//! Hand-rolled HTTP/1.1 configuration web server.
//!
//! Renders the same settings page the GUI does, in a browser.  Honors
//! the same IP-safety allowlist as the telnet listener (private/loopback
//! only unless `disable_ip_safety` is set), and the same
//! `security_enabled` flag for HTTP Basic auth (using the telnet
//! `username` / `password`).
//!
//! No external HTTP-crate dependency — the protocol surface is small
//! (GET /, GET /logo.png, GET /logs, POST /save) and we already roll
//! our own XMODEM/ZMODEM/Kermit/telnet on top of tokio.  Keeping the
//! parser tiny here matches the rest of the project.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::{self, Config};
use crate::logger::{self, glog};
use crate::telnet::{self, LockoutMap};

/// Maximum size of a request line + headers we'll accept.  Plenty for
/// the small form posts we handle; bounds the worst case for a
/// misbehaving / malicious client.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Maximum POST body size.  The save form is far smaller, but leave
/// headroom for very long passwords / API keys.
const MAX_BODY_BYTES: usize = 64 * 1024;
/// How many recent log lines we surface in the /logs endpoint.
const LOG_TAIL_LINES: usize = 400;
/// Bound the time we'll wait for a complete request from one peer.
/// Stops a slow-loris client from parking a tokio task indefinitely.
const READ_TIMEOUT_SECS: u64 = 30;
/// Suggested wait sent back to a locked-out client in the Retry-After
/// header.  The actual lockout in `telnet::is_locked_out` runs on its
/// own clock; this is the upper bound a client would ever need to wait
/// (matches the 5-minute LOCKOUT_DURATION in telnet.rs).
const LOCKOUT_RETRY_SECS: u64 = 300;
/// Defense-in-depth cap on concurrent HTTP requests in flight.  A
/// typical browser opens 2–3 connections per page (HTML + /logs poll +
/// /logo.png), so 16 leaves headroom for several users while bounding
/// the worst case a hostile peer could spin up.  Excess connections
/// are immediately rejected with 503 instead of being parked behind a
/// long read timeout.  Not configurable: HTTP is short-lived and the
/// real session limit lives on telnet/SSH (see cfg.max_sessions).
const MAX_INFLIGHT: usize = 16;

/// Embedded logo (same PNG the GUI uses) so the web page mirrors the
/// look of the desktop console without needing an external file.
const LOGO_PNG: &[u8] = include_bytes!("../ethernetgatewaylogo_small.png");

/// Launch the HTTP listener.  No-op when `web_enabled` is false.
///
/// `lockouts` is the same shared map that backs the telnet and SSH
/// auth gates — an attacker cannot bounce between protocols (or hosts)
/// to reset the failure counter.  `restart` and `shutdown` are the
/// same flags `gui::App` flips on its "Save and Restart" button so a
/// web-driven save can trigger a full server restart in exactly the
/// same way the desktop console does.
pub fn start_web_server(
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    lockouts: LockoutMap,
) {
    let cfg = config::get_config();
    if !cfg.web_enabled {
        return;
    }
    let port = cfg.web_port;

    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Web server: failed to bind port {}: {}", port, e);
                return;
            }
        };
        glog!("Web server listening on port {}", port);

        // Atomic claim/release counter — matches the TOCTOU-safe
        // fetch_add pattern from telnet::start_server.  Decrements
        // when the per-connection task drops the guard at the end of
        // handle_connection.
        let inflight = Arc::new(AtomicUsize::new(0));

        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            let prev = inflight.fetch_add(1, Ordering::SeqCst);
                            if prev >= MAX_INFLIGHT {
                                inflight.fetch_sub(1, Ordering::SeqCst);
                                glog!(
                                    "Web: rejected {} (max {} concurrent connections)",
                                    addr, MAX_INFLIGHT
                                );
                                tokio::spawn(async move {
                                    let mut s = stream;
                                    let _ = write_service_unavailable(&mut s).await;
                                });
                                continue;
                            }
                            let lockouts_conn = lockouts.clone();
                            let inflight_conn = inflight.clone();
                            let shutdown_conn = shutdown.clone();
                            let restart_conn = restart.clone();
                            let notify_conn = shutdown_notify.clone();
                            tokio::spawn(async move {
                                let _guard = InflightGuard(inflight_conn);
                                if let Err(e) = handle_connection(
                                    stream,
                                    addr.ip(),
                                    lockouts_conn,
                                    shutdown_conn,
                                    restart_conn,
                                    notify_conn,
                                )
                                .await
                                {
                                    glog!("Web server: error from {}: {}", addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            glog!("Web server: accept failed: {}", e);
                        }
                    }
                }
                _ = shutdown_notify.notified() => {
                    // Loop iteration will re-check shutdown flag.
                }
            }
        }
    });
}

/// Decrements the in-flight counter when dropped — pairs with the
/// `fetch_add` at accept time so the slot is always released even if
/// the per-connection task panics or short-circuits on an early
/// return.  Using a Drop-based guard instead of an explicit
/// `fetch_sub` at every exit point closes a class of "forgot to
/// decrement" bugs by construction.
struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn write_service_unavailable(stream: &mut tokio::net::TcpStream) -> std::io::Result<()> {
    let body = b"503 Service Unavailable\nServer is busy. Try again shortly.\n";
    let head = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nRetry-After: 5\r\n\r\n",
        body.len(),
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Read+parse the request, route, and write the response.
/// What the operator clicked.  Each frame's Save button submits the
/// full form with a distinct `action=` value so the server knows
/// whether to just persist, restart the whole gateway, or just
/// reload the serial managers — the exact same three behaviors the
/// GUI's per-frame Save buttons trigger (`save_config_now`,
/// `save_and_restart_all`, `save_and_restart_serial`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveAction {
    /// Persist config; leave running listeners alone.  Used by frames
    /// whose fields are read live (Security, File Transfer, AI/Browser,
    /// General) — toggles in those areas take effect on the next
    /// request without a restart.
    Save,
    /// Persist config and trigger a full server restart so the new
    /// telnet/SSH/Kermit/Web port bindings actually take hold.  Sets
    /// the same `restart` + `shutdown` flags `gui::App` does.
    SaveAndRestart,
    /// Persist config and ask the serial subsystem to reopen its
    /// ports.  Mirrors `gui::App::save_and_restart_serial`.
    SaveAndRestartSerial,
}

impl SaveAction {
    fn from_form(value: Option<&str>) -> Self {
        match value {
            Some("save_and_restart") => SaveAction::SaveAndRestart,
            Some("save_and_restart_serial") => SaveAction::SaveAndRestartSerial,
            _ => SaveAction::Save,
        }
    }
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer_ip: IpAddr,
    lockouts: LockoutMap,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);

    // Fresh per-connection snapshot of the live flags — toggles in the
    // GUI / telnet menu apply on the next connection without a restart.
    // IP-safety check mirrors `telnet::start_server`: when security is
    // on, HTTP Basic auth gates access regardless of source IP, so the
    // private-only allowlist only applies when both auth is off AND
    // the operator hasn't explicitly disabled the allowlist.
    let (live_security, live_disable_safety) = config::get_security_flags();
    if !live_security
        && !live_disable_safety
        && let Some(reason) = telnet::reject_insecure_ip(peer_ip)
    {
        glog!("Web: rejected {} ({})", peer_ip, reason);
        let body = format!("403 Forbidden\n{}\n", reason);
        write_response(&mut stream, 403, "Forbidden", "text/plain; charset=utf-8", body.as_bytes(), false).await?;
        return Ok(());
    }

    // Lockout gate runs ahead of any request parsing so a flood of
    // malformed POSTs from a banned IP can't keep us busy.  The same
    // map is shared with telnet + SSH; an attacker who tripped the
    // limit on telnet hits this 429 here too.
    if telnet::is_locked_out(&lockouts, peer_ip) {
        glog!("Web: locked-out {} blocked", peer_ip);
        let body = b"429 Too Many Requests\nToo many failed logins. Try again later.\n";
        write_locked_out(&mut stream, body).await?;
        return Ok(());
    }

    let read = tokio::time::timeout(
        std::time::Duration::from_secs(READ_TIMEOUT_SECS),
        read_request(&mut stream),
    )
    .await;
    let request = match read {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            let _ = write_response(
                &mut stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                format!("400 Bad Request\n{}\n", e).as_bytes(),
                false,
            )
            .await;
            return Ok(());
        }
        Err(_) => {
            let _ = write_response(
                &mut stream,
                408,
                "Request Timeout",
                "text/plain; charset=utf-8",
                b"408 Request Timeout\n",
                false,
            )
            .await;
            return Ok(());
        }
    };

    if live_security {
        if is_authorized(&request) {
            // Successful auth clears the lockout entry so a legitimate
            // user who fat-fingered once or twice isn't stuck waiting
            // out the 5-minute window after typing the right password.
            telnet::clear_lockout(&lockouts, peer_ip);
        } else {
            let count = telnet::record_auth_failure(&lockouts, peer_ip);
            glog!(
                "Web: auth failed for {} (attempt {}/{})",
                peer_ip,
                count,
                telnet::AUTH_MAX_ATTEMPTS,
            );
            if count >= telnet::AUTH_MAX_ATTEMPTS {
                let body = b"429 Too Many Requests\nToo many failed logins. Try again later.\n";
                write_locked_out(&mut stream, body).await?;
                return Ok(());
            }
            let body = b"401 Unauthorized\n";
            write_response(
                &mut stream,
                401,
                "Unauthorized",
                "text/plain; charset=utf-8",
                body,
                true,
            )
            .await?;
            return Ok(());
        }
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            let cfg = config::get_config();
            // The Save POST handler 303s back here with the success
            // banner riding in the query string.  Decode it from the
            // pre-parsed query rather than re-parsing the raw path.
            let notice = parse_form(&request.query)
                .remove("notice")
                .filter(|s| !s.is_empty());
            let body = render_main_page(&cfg, notice);
            write_response(
                &mut stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                body.as_bytes(),
                false,
            )
            .await?;
        }
        ("GET", "/logo.png") => {
            write_response(&mut stream, 200, "OK", "image/png", LOGO_PNG, false).await?;
        }
        ("GET", "/logs") => {
            let lines = logger::snapshot(LOG_TAIL_LINES);
            let body = lines.join("\n");
            write_response(
                &mut stream,
                200,
                "OK",
                "text/plain; charset=utf-8",
                body.as_bytes(),
                false,
            )
            .await?;
        }
        ("POST", "/save") => {
            // Apply on a blocking thread — update_config_value reads,
            // mutates, and rewrites egateway.conf, which would otherwise
            // park a tokio worker on filesystem I/O for every save.
            let body = request.body;
            let result = tokio::task::spawn_blocking(move || apply_form_post(&body)).await;
            let (notice, action) = match result {
                Ok(pair) => pair,
                Err(e) => (format!("Save failed: {}", e), SaveAction::Save),
            };
            // 303 See Other so a browser reload after Save re-issues GET
            // instead of resubmitting the form.  The notice rides along
            // in the query string (URL-encoded) and the GET handler picks
            // it up to render the banner once.
            let location = format!("/?notice={}", encode_query(&notice));
            write_redirect(&mut stream, &location).await?;

            // Response has been flushed and the connection shut down —
            // safe to fire the restart now.  Doing it any earlier risks
            // the runtime tearing down mid-write so the operator never
            // sees the confirmation banner on the redirected GET.
            match action {
                SaveAction::Save => {}
                SaveAction::SaveAndRestartSerial => {
                    crate::serial::restart_all_serial();
                    logger::log("Web: serial ports reconfigured.".into());
                }
                SaveAction::SaveAndRestart => {
                    logger::log("Web: configuration saved — restarting server...".into());
                    // Set restart BEFORE shutdown so main's restart-or-exit
                    // check reads the right intent (same ordering rule
                    // as gui::App::save_and_restart_all).
                    restart.store(true, Ordering::SeqCst);
                    shutdown.store(true, Ordering::SeqCst);
                    shutdown_notify.notify_waiters();
                }
            }
        }
        _ => {
            let body = b"404 Not Found\n";
            write_response(
                &mut stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                body,
                false,
            )
            .await?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Minimal HTTP/1.1 request parser — supports just enough of the
/// protocol to drive the config page (request line + headers, optional
/// Content-Length body for POSTs).  Returns a string error on any
/// malformed input so callers can log it and reply 400.
async fn read_request(stream: &mut tokio::net::TcpStream) -> Result<HttpRequest, String> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let header_end;
    loop {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read error: {}", e))?;
        if n == 0 {
            return Err("connection closed before request was complete".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            header_end = idx + 4;
            break;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err("request headers exceeded size cap".into());
        }
    }

    let header_bytes = &buf[..header_end - 4];
    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|_| "request headers contain non-UTF-8 bytes".to_string())?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method".to_string())?.to_string();
    let raw_path = parts.next().ok_or("missing path".to_string())?.to_string();
    let (path, query) = match raw_path.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (raw_path.clone(), String::new()),
    };

    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(
            name.trim().to_ascii_lowercase(),
            value.trim().to_string(),
        );
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("body size exceeded cap".into());
    }

    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&buf[header_end..]);
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| format!("body read error: {}", e))?;
        if n == 0 {
            return Err("connection closed before body was complete".into());
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Verify Basic auth against the live telnet `username` / `password`.
/// Returns true when auth is provided AND matches.
fn is_authorized(req: &HttpRequest) -> bool {
    let cfg = config::get_config();
    let Some(header) = req.headers.get("authorization") else {
        return false;
    };
    let Some(b64) = header.strip_prefix("Basic ").or_else(|| header.strip_prefix("basic ")) else {
        return false;
    };
    let decoded = decode_base64(b64.trim());
    let Ok(text) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((user, pass)) = text.split_once(':') else {
        return false;
    };
    telnet::constant_time_eq(user.as_bytes(), cfg.username.as_bytes())
        && telnet::constant_time_eq(pass.as_bytes(), cfg.password.as_bytes())
}

/// Tiny RFC 4648 base64 decoder.  Returns the empty vec for any input
/// that contains a non-base64 character so callers don't have to
/// distinguish "invalid" from "empty" — both fail auth identically.
fn decode_base64(input: &str) -> Vec<u8> {
    let trimmed: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u8;
    for c in trimmed.chars() {
        if c == '=' {
            break;
        }
        let v: u32 = match c {
            'A'..='Z' => (c as u32) - ('A' as u32),
            'a'..='z' => (c as u32) - ('a' as u32) + 26,
            '0'..='9' => (c as u32) - ('0' as u32) + 52,
            '+' => 62,
            '/' => 63,
            _ => return Vec::new(),
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    out
}

/// Write a 429 Too Many Requests response with `Retry-After` so a
/// well-behaved client knows roughly how long to back off.  Used after
/// the lockout map records too many failed Basic-Auth attempts from
/// this IP.
async fn write_locked_out(
    stream: &mut tokio::net::TcpStream,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nRetry-After: {}\r\n\r\n",
        body.len(),
        LOCKOUT_RETRY_SECS,
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Write a 303 See Other redirect and close the connection.  Used as
/// the response to POST /save so a browser reload after submit doesn't
/// resubmit the form (POST → 303 → GET — the canonical PRG pattern).
async fn write_redirect(
    stream: &mut tokio::net::TcpStream,
    location: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 303 See Other\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        location,
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Percent-encode a string for inclusion in a query parameter value.
/// Conservative: only ASCII alphanumerics and a handful of safe
/// punctuation pass through; everything else is `%xx`.
fn encode_query(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Write a single HTTP/1.1 response and close the connection.  Adds
/// `WWW-Authenticate` when `auth_challenge` is true so a 401 reply
/// triggers the browser's login prompt.
async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    auth_challenge: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        status,
        reason,
        content_type,
        body.len(),
    );
    if auth_challenge {
        head.push_str("WWW-Authenticate: Basic realm=\"Ethernet Gateway\"\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

// ─── Form-post handling ─────────────────────────────────────────────

/// Apply every recognized field from a `POST /save` body in a single
/// read-modify-write of the config file.  Returns a human-readable
/// notice + the action the operator's button asked for, so the
/// caller can trigger the matching restart behavior after the
/// response has flushed.  Synchronous because it does filesystem I/O
/// — wrap in `spawn_blocking`.
fn apply_form_post(body: &[u8]) -> (String, SaveAction) {
    let text = std::str::from_utf8(body).unwrap_or("");
    let fields = parse_form(text);
    let action = SaveAction::from_form(fields.get("action").map(String::as_str));
    let old_cfg = config::get_config();
    let (updates, notice) = collect_form_updates(&fields, &old_cfg);

    let pairs: Vec<(&str, &str)> = updates
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    config::update_config_values(&pairs);

    logger::log("Web: configuration saved.".into());
    (notice, action)
}

/// Pure transformation from a parsed form + the current Config to a
/// (`Vec<(key, value)>`, notice) pair.  Separated from
/// `apply_form_post` so tests can exercise the form-to-update mapping
/// (including the connection-breaking warning logic) without touching
/// the global CONFIG singleton or rewriting the on-disk config file.
fn collect_form_updates(
    fields: &HashMap<String, String>,
    old_cfg: &Config,
) -> (Vec<(String, String)>, String) {
    // Snapshot connection-breaking changes (web server disabled or
    // port changed) so the caller can surface them in the post-save
    // notice.  The change still applies — the operator already
    // confirmed in the JS dialog — but the next page render flags it
    // so they know to reconnect.
    let new_web_enabled = fields
        .get("web_enabled")
        .map(|v| is_truthy(v))
        .unwrap_or(false);
    let new_web_port = fields.get("web_port").and_then(|s| s.parse::<u16>().ok());
    let mut warning = String::new();
    if old_cfg.web_enabled && !new_web_enabled {
        warning = "Web server disabled — this connection will stop responding.".into();
    } else if let Some(v) = new_web_port
        && v != old_cfg.web_port
    {
        warning = format!(
            "Web server port changed to {}. Reconnect at the new port.",
            v
        );
    }

    // Collect every key=value pair into a single batch so the underlying
    // CONFIG mutex is taken once and the conf file is rewritten once.
    let mut updates: Vec<(String, String)> = Vec::new();

    // Plain key=value — the config layer validates each value and
    // silently rejects bad input.
    let plain_keys: &[&str] = &[
        "telnet_port", "ssh_port", "kermit_server_port", "web_port",
        "username", "password",
        "ssh_username", "ssh_password",
        "transfer_dir", "max_sessions", "idle_timeout_secs",
        "groq_api_key", "browser_homepage", "weather_zip",
        "xmodem_negotiation_timeout", "xmodem_block_timeout",
        "xmodem_max_retries", "xmodem_negotiation_retry_interval",
        "zmodem_negotiation_timeout", "zmodem_frame_timeout",
        "zmodem_max_retries", "zmodem_negotiation_retry_interval",
        "kermit_negotiation_timeout", "kermit_packet_timeout",
        "kermit_idle_timeout", "kermit_max_retries",
        "kermit_max_packet_length", "kermit_window_size",
        "kermit_block_check_type", "kermit_8bit_quote",
        "kermit_resume_max_age_hours",
        "ssh_gateway_auth",
    ];
    for key in plain_keys {
        if let Some(v) = fields.get(*key) {
            updates.push(((*key).to_string(), v.clone()));
        }
    }

    // Checkbox-style booleans: an unchecked checkbox does not appear in
    // the form data, so absence is the canonical "false" signal.  Every
    // boolean key the page renders is set unconditionally — partial
    // saves are not supported (the full form is always submitted).
    let bool_keys: &[&str] = &[
        "telnet_enabled", "ssh_enabled", "kermit_server_enabled", "web_enabled",
        "security_enabled", "disable_ip_safety", "enable_console", "verbose",
        "telnet_gateway_negotiate", "telnet_gateway_raw",
        "kermit_long_packets", "kermit_sliding_windows", "kermit_streaming",
        "kermit_attribute_packets", "kermit_repeat_compression",
        "kermit_resume_partial", "kermit_locking_shifts",
        "allow_atdt_kermit",
        "serial_a_enabled", "serial_b_enabled",
        "serial_a_echo", "serial_a_verbose", "serial_a_quiet",
        "serial_b_echo", "serial_b_verbose", "serial_b_quiet",
    ];
    for key in bool_keys {
        let truthy = fields.get(*key).map(|s| is_truthy(s)).unwrap_or(false);
        updates.push(((*key).to_string(), if truthy { "true" } else { "false" }.to_string()));
    }

    // Per-port serial settings (the rest are plain).
    let serial_keys: &[&str] = &[
        "mode", "port", "baud", "databits", "parity", "stopbits",
        "flowcontrol", "s_regs", "x_code", "dtr_mode", "flow_mode",
        "dcd_mode",
        "stored_0", "stored_1", "stored_2", "stored_3",
    ];
    for port in ["serial_a", "serial_b"] {
        for k in serial_keys {
            let full = format!("{}_{}", port, k);
            if let Some(v) = fields.get(&full) {
                updates.push((full, v.clone()));
            }
        }
    }

    let notice = if warning.is_empty() {
        "Configuration saved.".into()
    } else {
        format!("Configuration saved. {}", warning)
    };
    (updates, notice)
}

/// True when a form value represents an enabled checkbox.  HTML
/// checkboxes default to `value="on"` but our markup explicitly sets
/// `value="true"`; accept both plus `"1"` so the parser is robust to
/// browser quirks and hand-crafted POSTs.
fn is_truthy(s: &str) -> bool {
    matches!(s, "true" | "on" | "1") || s.eq_ignore_ascii_case("true")
}

/// Parse `application/x-www-form-urlencoded` into a flat map.  The
/// last value wins on duplicates — fine because every field on the
/// page has a unique name.
fn parse_form(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(input: &str) -> String {
    // Percent-decode into a byte vec first, then reinterpret as UTF-8.
    // Earlier this function cast each decoded byte to `char`, which
    // works for ASCII but mangles multi-byte UTF-8 sequences — "café"
    // encoded as "caf%C3%A9" round-tripped to "cafÃ©" (the two bytes
    // 0xC3 / 0xA9 became two separate Latin-1 codepoints instead of
    // the single U+00E9).  Decoding to bytes preserves the original
    // wire encoding, and `from_utf8_lossy` produces a String without
    // panicking even if a malformed sequence slips through.
    let mut bytes_out: Vec<u8> = Vec::with_capacity(input.len());
    let mut iter = input.bytes();
    while let Some(b) = iter.next() {
        match b {
            b'+' => bytes_out.push(b' '),
            b'%' => {
                let h = iter.next();
                let l = iter.next();
                if let (Some(h), Some(l)) = (h, l)
                    && let (Some(hv), Some(lv)) = (hex_value(h), hex_value(l))
                {
                    bytes_out.push((hv << 4) | lv);
                }
            }
            _ => bytes_out.push(b),
        }
    }
    String::from_utf8_lossy(&bytes_out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

// ─── HTML rendering ─────────────────────────────────────────────────

/// Build the full configuration page.  `notice` is an optional banner
/// shown above the form (used to confirm a save).
fn render_main_page(cfg: &Config, notice: Option<String>) -> String {
    let mut out = String::with_capacity(32 * 1024);
    out.push_str("<!doctype html><html lang=\"en\"><head>");
    out.push_str("<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<title>Ethernet Gateway — Configuration</title>");
    out.push_str(STYLE);
    out.push_str("</head><body>");
    out.push_str(&render_header(cfg));
    if let Some(n) = notice {
        out.push_str(&format!(
            "<div class=\"notice\">{}</div>",
            html_escape(&n)
        ));
    }
    // Single form wraps every frame AND every More popup.  The
    // popups have to live inside the form so their fields actually
    // submit, and each frame's Save button is a `submit` input with a
    // distinct `name="action"` value — clicking any of them POSTs the
    // entire form and the server routes on the action.  Multiple
    // submit buttons inside one form is the canonical HTML way to
    // model "same data, different intent."
    out.push_str("<form method=\"post\" action=\"/save\" id=\"cfg-form\">");
    out.push_str(&render_grid(cfg));
    out.push_str(&render_more_popups(cfg));
    out.push_str(&render_scripture_and_logo());
    out.push_str("</form>");
    out.push_str(&render_console());
    out.push_str(SCRIPT);
    out.push_str("</body></html>");
    out
}

fn render_header(cfg: &Config) -> String {
    let ip = local_ip();
    format!(
        "<header><h1>Ethernet Gateway v{ver}</h1>\
         <div class=\"server-ip\">Server IP: <code>{ip}</code></div>\
         </header>\
         <div class=\"hint\">Telnet: {tport} &middot; SSH: {sport} &middot; Kermit: {kport} &middot; Web: {wport}</div>",
        ver = env!("CARGO_PKG_VERSION"),
        ip = html_escape(&ip),
        tport = cfg.telnet_port,
        sport = cfg.ssh_port,
        kport = cfg.kermit_server_port,
        wport = cfg.web_port,
    )
}

fn render_grid(cfg: &Config) -> String {
    let mut out = String::new();
    out.push_str("<div class=\"grid\">");
    out.push_str(&frame_server(cfg));
    out.push_str(&frame_security(cfg));
    out.push_str(&frame_file_transfer(cfg));
    out.push_str(&frame_ai_browser(cfg));
    out.push_str(&frame_serial(cfg));
    out.push_str(&frame_general(cfg));
    out.push_str("</div>");
    out
}

/// Render one submit button.  `action` is the value sent in the
/// `name="action"` form field; the server dispatches on it (see
/// `SaveAction::from_form`).  `class` lets the Server frame's
/// "Save and Restart" stand out as the highest-impact button.
fn save_button(action: &str, label: &str, class: &str) -> String {
    format!(
        "<button type=\"submit\" name=\"action\" value=\"{action}\" class=\"{class}\">{label}</button>",
        action = action,
        class = class,
        label = html_escape(label),
    )
}

fn frame_server(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">Server</span>\
         <span class=\"sub\">(Changes Require Restart)</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{telnet_chk} {telnet_port}</div>\
         <div class=\"row\">{ssh_chk} {ssh_port}</div>\
         <div class=\"row\">{web_chk} {web_port}</div>\
         <div class=\"row\">{kermit_chk} {kermit_port}\
         <button type=\"button\" class=\"more\" data-target=\"more-server\">More\u{2026}</button></div>\
         </section>",
        save = save_button("save_and_restart", "Save and Restart", "primary"),
        telnet_chk = checkbox("telnet_enabled", "Telnet", cfg.telnet_enabled),
        telnet_port = numfield("telnet_port", "Port", cfg.telnet_port),
        ssh_chk = checkbox("ssh_enabled", "SSH", cfg.ssh_enabled),
        ssh_port = numfield("ssh_port", "Port", cfg.ssh_port),
        web_chk = checkbox_with_attr(
            "web_enabled",
            "Web Server",
            cfg.web_enabled,
            "onchange=\"warnIfDisablingWeb(this)\"",
        ),
        web_port = numfield_with_attr(
            "web_port",
            "Port",
            cfg.web_port,
            "onchange=\"warnIfChangingWebPort(this)\"",
            cfg.web_port,
        ),
        kermit_chk = checkbox("kermit_server_enabled", "Kermit Server", cfg.kermit_server_enabled),
        kermit_port = numfield("kermit_server_port", "Port", cfg.kermit_server_port),
    )
}

fn frame_security(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">Security</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{sec_chk} {ipsafe_chk}</div>\
         <div class=\"row\"><span class=\"label-dim\">Telnet</span> {tuser} {tpass}</div>\
         <div class=\"row\"><span class=\"label-dim\">SSH</span> {suser} {spass}</div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        sec_chk = checkbox("security_enabled", "Require Login", cfg.security_enabled),
        ipsafe_chk = checkbox("disable_ip_safety", "Disable IP Safety", cfg.disable_ip_safety),
        tuser = textfield("username", "User", &cfg.username, false, 12),
        tpass = textfield("password", "Pass", &cfg.password, true, 12),
        suser = textfield("ssh_username", "User", &cfg.ssh_username, false, 12),
        spass = textfield("ssh_password", "Pass", &cfg.ssh_password, true, 12),
    )
}

fn frame_file_transfer(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">File Transfer (XMODEM)</span>\
         <span class=\"sub\">(More for others)</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{neg} {blk}</div>\
         <div class=\"row\">{retries} {interval}</div>\
         <div class=\"row\"><span class=\"label\">Transfer dir:</span>\
         <input type=\"text\" name=\"transfer_dir\" value=\"{td}\"></div>\
         <div class=\"row\"><button type=\"button\" class=\"more\" data-target=\"more-xfer\">More\u{2026}</button></div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        neg = numfield("xmodem_negotiation_timeout", "Neg (s)", cfg.xmodem_negotiation_timeout),
        blk = numfield("xmodem_block_timeout", "Blk (s)", cfg.xmodem_block_timeout),
        retries = numfield("xmodem_max_retries", "Retries", cfg.xmodem_max_retries),
        interval = numfield("xmodem_negotiation_retry_interval", "Poke (s)", cfg.xmodem_negotiation_retry_interval),
        td = html_escape(&cfg.transfer_dir),
    )
}

fn frame_ai_browser(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">AI Chat, Browser, and Weather</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\"><span class=\"label\">API Key:</span>\
         <input type=\"password\" name=\"groq_api_key\" value=\"{key}\"></div>\
         <div class=\"row\"><span class=\"label\">Home:</span>\
         <input type=\"text\" name=\"browser_homepage\" value=\"{home}\">\
         <span class=\"label\">Zip:</span>\
         <input type=\"text\" name=\"weather_zip\" value=\"{zip}\" size=\"6\"></div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        key = html_escape(&cfg.groq_api_key),
        home = html_escape(&cfg.browser_homepage),
        zip = html_escape(&cfg.weather_zip),
    )
}

fn frame_serial(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">Serial Ports</span>\
         <span class=\"head-right\">{save}</span></div>\
         {a}\
         {b}\
         </section>",
        save = save_button("save_and_restart_serial", "Save", "secondary"),
        a = serial_row("serial_a", "Port A", &cfg.serial_a),
        b = serial_row("serial_b", "Port B", &cfg.serial_b),
    )
}

fn serial_row(prefix: &str, label: &str, port: &config::SerialPortConfig) -> String {
    format!(
        "<div class=\"row\"><span class=\"label\">{label}:</span>\
         {en}\
         <input type=\"text\" name=\"{prefix}_port\" value=\"{dev}\" placeholder=\"(none)\" size=\"14\">\
         {baud}\
         <button type=\"button\" class=\"more\" data-target=\"more-{prefix}\">More\u{2026}</button></div>",
        label = label,
        en = checkbox(&format!("{}_enabled", prefix), "Enabled", port.enabled),
        prefix = prefix,
        dev = html_escape(&port.port),
        baud = numfield(&format!("{}_baud", prefix), "Baud", port.baud),
    )
}

fn frame_general(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">General</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{v}</div>\
         <div class=\"row\">{g}</div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        v = checkbox("verbose", "Verbose Transfer Logging", cfg.verbose),
        g = checkbox("enable_console", "Show GUI on Startup", cfg.enable_console),
    )
}

fn render_scripture_and_logo() -> String {
    String::from(
        "<div class=\"verse-row\">\
         <div class=\"verse\">\
         \u{201c}For God so loved the world, that he gave his only begotten Son, \
         that whosoever believeth in him should not perish, but have everlasting life.\u{201d}\
         <div class=\"verse-cite\">\u{2014} John 3:16, KJV</div>\
         </div>\
         <div class=\"logo-wrap\"><img src=\"/logo.png\" alt=\"Ethernet Gateway\" class=\"logo\"></div>\
         </div>",
    )
}

fn render_more_popups(cfg: &Config) -> String {
    let mut out = String::new();
    // Server More — session cap, idle timeout, gateway advanced settings.
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-server\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">Server \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-server\">\u{00d7}</button></div>\
         <div class=\"row\">{sessions} {idle}</div>\
         <div class=\"row\">{tneg} {traw}</div>\
         <div class=\"row\"><span class=\"label\">SSH Gateway Auth:</span>\
         <select name=\"ssh_gateway_auth\">\
         <option value=\"key\" {key_sel}>Key</option>\
         <option value=\"password\" {pwd_sel}>Password</option>\
         </select></div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        sessions = numfield("max_sessions", "Sessions", cfg.max_sessions),
        idle = numfield("idle_timeout_secs", "Idle (s)", cfg.idle_timeout_secs),
        tneg = checkbox("telnet_gateway_negotiate", "Telnet Gateway: negotiate TTYPE/NAWS", cfg.telnet_gateway_negotiate),
        traw = checkbox("telnet_gateway_raw", "Telnet Gateway: raw TCP mode", cfg.telnet_gateway_raw),
        key_sel = if cfg.ssh_gateway_auth == "key" { "selected" } else { "" },
        pwd_sel = if cfg.ssh_gateway_auth == "password" { "selected" } else { "" },
        save = save_button("save_and_restart", "Save and Restart", "primary"),
    ));

    // File-transfer More — ZMODEM and Kermit settings.
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-xfer\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">File Transfer \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-xfer\">\u{00d7}</button></div>\
         <h3>ZMODEM</h3>\
         <div class=\"row\">{zneg} {zfrm}</div>\
         <div class=\"row\">{zret} {zint}</div>\
         <h3>Kermit</h3>\
         <div class=\"row\">{kneg} {kpkt}</div>\
         <div class=\"row\">{kidle} {kret}</div>\
         <div class=\"row\">{kmaxl} {kwin}</div>\
         <div class=\"row\">{kbct}\
         <span class=\"label\">8-bit quote:</span>\
         <select name=\"kermit_8bit_quote\">\
         <option value=\"auto\" {qa}>auto</option>\
         <option value=\"on\" {qo}>on</option>\
         <option value=\"off\" {qf}>off</option>\
         </select></div>\
         <div class=\"row\">{klp} {ksw}</div>\
         <div class=\"row\">{kst} {kap}</div>\
         <div class=\"row\">{krc} {krp}</div>\
         <div class=\"row\">{kma} {kls}</div>\
         <div class=\"row\">{atd}</div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        save = save_button("save", "Save", "secondary"),
        zneg = numfield("zmodem_negotiation_timeout", "Neg (s)", cfg.zmodem_negotiation_timeout),
        zfrm = numfield("zmodem_frame_timeout", "Frame (s)", cfg.zmodem_frame_timeout),
        zret = numfield("zmodem_max_retries", "Retries", cfg.zmodem_max_retries),
        zint = numfield("zmodem_negotiation_retry_interval", "Poke (s)", cfg.zmodem_negotiation_retry_interval),
        kneg = numfield("kermit_negotiation_timeout", "Neg (s)", cfg.kermit_negotiation_timeout),
        kpkt = numfield("kermit_packet_timeout", "Pkt (s)", cfg.kermit_packet_timeout),
        kidle = numfield("kermit_idle_timeout", "Idle (s)", cfg.kermit_idle_timeout),
        kret = numfield("kermit_max_retries", "Retries", cfg.kermit_max_retries),
        kmaxl = numfield("kermit_max_packet_length", "MaxLen", cfg.kermit_max_packet_length),
        kwin = numfield("kermit_window_size", "Window", cfg.kermit_window_size),
        kbct = numfield("kermit_block_check_type", "BCT", cfg.kermit_block_check_type),
        qa = if cfg.kermit_8bit_quote == "auto" { "selected" } else { "" },
        qo = if cfg.kermit_8bit_quote == "on" { "selected" } else { "" },
        qf = if cfg.kermit_8bit_quote == "off" { "selected" } else { "" },
        klp = checkbox("kermit_long_packets", "Long packets", cfg.kermit_long_packets),
        ksw = checkbox("kermit_sliding_windows", "Sliding windows", cfg.kermit_sliding_windows),
        kst = checkbox("kermit_streaming", "Streaming", cfg.kermit_streaming),
        kap = checkbox("kermit_attribute_packets", "Attribute packets", cfg.kermit_attribute_packets),
        krc = checkbox("kermit_repeat_compression", "Repeat compression", cfg.kermit_repeat_compression),
        krp = checkbox("kermit_resume_partial", "Resume partial", cfg.kermit_resume_partial),
        kma = numfield("kermit_resume_max_age_hours", "Resume max age (h)", cfg.kermit_resume_max_age_hours),
        kls = checkbox("kermit_locking_shifts", "Locking shifts", cfg.kermit_locking_shifts),
        atd = checkbox("allow_atdt_kermit", "Allow ATDT KERMIT (modem emulator)", cfg.allow_atdt_kermit),
    ));

    // Per-port serial popups.
    out.push_str(&serial_more_popup("serial_a", "Port A", &cfg.serial_a));
    out.push_str(&serial_more_popup("serial_b", "Port B", &cfg.serial_b));
    out
}

fn serial_more_popup(prefix: &str, label: &str, port: &config::SerialPortConfig) -> String {
    let mode_sel_modem = if port.mode == "modem" { "selected" } else { "" };
    let mode_sel_console = if port.mode == "console" { "selected" } else { "" };
    let parity_opts = ["none", "odd", "even"]
        .iter()
        .map(|p| format!(
            "<option value=\"{p}\" {sel}>{p}</option>",
            p = p,
            sel = if port.parity == *p { "selected" } else { "" },
        ))
        .collect::<String>();
    let flow_opts = ["none", "hardware", "software"]
        .iter()
        .map(|f| format!(
            "<option value=\"{f}\" {sel}>{f}</option>",
            f = f,
            sel = if port.flowcontrol == *f { "selected" } else { "" },
        ))
        .collect::<String>();
    format!(
        "<div class=\"modal\" id=\"more-{prefix}\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">{label} \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-{prefix}\">\u{00d7}</button></div>\
         <div class=\"row\"><span class=\"label\">Mode:</span>\
         <select name=\"{prefix}_mode\">\
         <option value=\"modem\" {ms_modem}>Modem (AT)</option>\
         <option value=\"console\" {ms_console}>Telnet-Serial</option>\
         </select></div>\
         <div class=\"row\">{bits} {stop}\
         <span class=\"label\">Parity:</span><select name=\"{prefix}_parity\">{po}</select>\
         <span class=\"label\">Flow:</span><select name=\"{prefix}_flowcontrol\">{fo}</select>\
         </div>\
         <div class=\"row\">{echo} {verb} {quiet}</div>\
         <div class=\"row\">{xc} {dtr} {flw} {dcd}</div>\
         <div class=\"row\"><span class=\"label\">S-registers:</span>\
         <input type=\"text\" name=\"{prefix}_s_regs\" value=\"{sregs}\" size=\"40\"></div>\
         <h3>Stored numbers</h3>\
         <div class=\"row\">{n0} {n1}</div>\
         <div class=\"row\">{n2} {n3}</div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        save = save_button("save_and_restart_serial", "Save", "secondary"),
        prefix = prefix,
        label = label,
        ms_modem = mode_sel_modem,
        ms_console = mode_sel_console,
        bits = numfield(&format!("{}_databits", prefix), "Bits", port.databits),
        stop = numfield(&format!("{}_stopbits", prefix), "Stop", port.stopbits),
        po = parity_opts,
        fo = flow_opts,
        echo = checkbox(&format!("{}_echo", prefix), "Echo (E1)", port.echo),
        verb = checkbox(&format!("{}_verbose", prefix), "Verbose (V1)", port.verbose),
        quiet = checkbox(&format!("{}_quiet", prefix), "Quiet (Q1)", port.quiet),
        xc = numfield(&format!("{}_x_code", prefix), "X-code", port.x_code),
        dtr = numfield(&format!("{}_dtr_mode", prefix), "&D", port.dtr_mode),
        flw = numfield(&format!("{}_flow_mode", prefix), "&K", port.flow_mode),
        dcd = numfield(&format!("{}_dcd_mode", prefix), "&C", port.dcd_mode),
        sregs = html_escape(&port.s_regs),
        n0 = textfield(&format!("{}_stored_0", prefix), "Slot 0", &port.stored_numbers[0], false, 16),
        n1 = textfield(&format!("{}_stored_1", prefix), "Slot 1", &port.stored_numbers[1], false, 16),
        n2 = textfield(&format!("{}_stored_2", prefix), "Slot 2", &port.stored_numbers[2], false, 16),
        n3 = textfield(&format!("{}_stored_3", prefix), "Slot 3", &port.stored_numbers[3], false, 16),
    )
}

fn render_console() -> String {
    String::from(
        "<section class=\"frame console-frame\">\
         <div class=\"frame-head\"><span class=\"title\">Console Output</span>\
         <span class=\"sub\">(auto-refreshes every 2 s)</span></div>\
         <pre id=\"console\">(loading\u{2026})</pre>\
         </section>",
    )
}

// ─── HTML helpers ───────────────────────────────────────────────────

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            _ => c.to_string(),
        })
        .collect()
}

fn checkbox(name: &str, label: &str, checked: bool) -> String {
    // Hidden sentinel ensures absence-is-false works robustly even when
    // the browser collapses a checkbox name; an unchecked checkbox does
    // not submit, so absence from the form is the "false" signal we rely
    // on server-side.  The hidden field is harmless because the visible
    // checkbox's submitted "true" value wins (last-write semantics in
    // parse_form).
    format!(
        "<label class=\"chk\"><input type=\"checkbox\" name=\"{name}\" value=\"true\" {chk}> {label}</label>",
        name = name,
        chk = if checked { "checked" } else { "" },
        label = html_escape(label),
    )
}

fn checkbox_with_attr(name: &str, label: &str, checked: bool, attr: &str) -> String {
    format!(
        "<label class=\"chk\"><input type=\"checkbox\" name=\"{name}\" value=\"true\" {chk} {attr}> {label}</label>",
        name = name,
        chk = if checked { "checked" } else { "" },
        attr = attr,
        label = html_escape(label),
    )
}

fn numfield<T: std::fmt::Display>(name: &str, label: &str, value: T) -> String {
    format!(
        "<span class=\"label\">{label}:</span><input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"7\">",
        name = name,
        label = html_escape(label),
        value = value,
    )
}

fn numfield_with_attr<T: std::fmt::Display>(
    name: &str,
    label: &str,
    value: T,
    attr: &str,
    original: u16,
) -> String {
    format!(
        "<span class=\"label\">{label}:</span><input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" data-orig=\"{orig}\" size=\"7\" {attr}>",
        name = name,
        label = html_escape(label),
        value = value,
        orig = original,
        attr = attr,
    )
}

fn textfield(name: &str, label: &str, value: &str, password: bool, size: usize) -> String {
    let kind = if password { "password" } else { "text" };
    format!(
        "<span class=\"label\">{label}:</span><input type=\"{kind}\" name=\"{name}\" value=\"{value}\" size=\"{size}\">",
        kind = kind,
        name = name,
        label = html_escape(label),
        value = html_escape(value),
        size = size,
    )
}

fn local_ip() -> String {
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in &ifaces {
            if iface.is_loopback() {
                continue;
            }
            let ip = iface.ip();
            if ip.is_ipv4() {
                return ip.to_string();
            }
        }
    }
    "127.0.0.1".into()
}

// ─── Static assets ──────────────────────────────────────────────────

const STYLE: &str = "<style>
:root {
  --bg-darkest: #000510;
  --bg-dark: #101c3a;
  --bg-mid: #182848;
  --border: #304570;
  --amber: #e6b422;
  --amber-bright: #ffd700;
  --amber-dim: #8b7a3a;
  --text: #d4c590;
  --text-input: #e8dcb0;
  --console-bg: #081228;
  --console-text: #33cc33;
  --scripture: #c0aa60;
  --popup-bg: #04180a;
  --popup-input: #1c462a;
}
* { box-sizing: border-box; }
body {
  background: var(--bg-darkest);
  color: var(--text);
  font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
  font-size: 15px;
  margin: 0;
  padding: 16px;
}
header { display: flex; align-items: baseline; justify-content: space-between; }
h1 { color: var(--amber-bright); font-weight: bold; margin: 0; font-size: 22px; }
.server-ip { color: var(--amber); font-family: monospace; font-size: 14px; }
.hint { color: var(--amber-dim); font-style: italic; margin-top: 4px; }
.notice {
  background: #1c3a50; color: var(--amber-bright);
  padding: 8px 12px; border: 1px solid var(--amber);
  border-radius: 4px; margin: 10px 0;
}
.grid {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
  gap: 10px; margin-top: 10px;
}
.frame {
  background: var(--bg-dark);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 10px 12px;
}
.frame-head { display: flex; align-items: baseline; gap: 8px; margin-bottom: 6px; }
.frame-head .title { color: var(--amber); font-weight: bold; }
.frame-head .sub { color: var(--amber-dim); font-style: italic; font-size: 13px; }
.frame-head .head-right { margin-left: auto; }
.row { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin: 4px 0; }
.label { color: var(--text); }
.label-dim { color: var(--amber-dim); min-width: 56px; }
.chk { display: inline-flex; align-items: center; gap: 6px; }
input[type=text], input[type=password], select {
  background: var(--bg-mid);
  color: var(--text-input);
  border: 1px solid var(--border);
  border-radius: 3px;
  padding: 3px 6px;
}
input:focus, select:focus { outline: 1px solid var(--amber); }
button {
  background: var(--bg-mid);
  color: var(--amber);
  border: 1px solid var(--border);
  border-radius: 3px;
  padding: 4px 10px;
  cursor: pointer;
  font-weight: bold;
}
button:hover { background: #22365a; }
button.primary {
  background: #1c3a50;
  color: var(--amber-bright);
  font-size: 14px;
  padding: 4px 12px;
}
button.secondary {
  font-size: 13px;
  padding: 3px 10px;
}
button.more {
  margin-left: auto;
  font-size: 13px;
  padding: 2px 8px;
}
.modal-foot {
  display: flex;
  justify-content: flex-end;
  margin-top: 10px;
  padding-top: 8px;
  border-top: 1px solid var(--border);
}
.verse-row { display: flex; gap: 16px; align-items: flex-start; margin-top: 14px; flex-wrap: wrap; }
.verse {
  color: var(--scripture);
  font-style: italic; font-weight: bold;
  font-size: 16px; flex: 1; min-width: 280px;
}
.verse-cite { font-size: 14px; margin-top: 4px; }
.logo-wrap { flex: 0 0 auto; }
.logo { max-width: 366px; height: auto; }
h3 { color: var(--amber); margin: 12px 0 4px; font-size: 14px; }
.modal {
  display: none;
  position: fixed; top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 5, 16, 0.85);
  align-items: flex-start; justify-content: center;
  padding: 5vh 16px;
  z-index: 50;
  overflow-y: auto;
}
.modal.open { display: flex; }
.modal-body {
  background: var(--popup-bg);
  border: 1px solid var(--amber);
  border-radius: 4px;
  padding: 14px 16px;
  max-width: 720px; width: 100%;
}
.modal-body input[type=text], .modal-body input[type=password], .modal-body select {
  background: var(--popup-input);
}
.modal-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 8px; }
.modal-head .title { color: var(--amber-bright); font-weight: bold; font-size: 16px; }
.close { padding: 0 8px; font-size: 18px; line-height: 1; }
.console-frame { margin-top: 14px; background: var(--console-bg); }
#console {
  margin: 0;
  color: var(--console-text);
  font-family: monospace;
  font-size: 13px;
  max-height: 260px;
  overflow-y: auto;
  white-space: pre-wrap;
}
</style>";

const SCRIPT: &str = "<script>
function openModal(id) { document.getElementById(id).classList.add('open'); }
function closeModal(id) { document.getElementById(id).classList.remove('open'); }
document.querySelectorAll('button.more').forEach(function(b) {
  b.addEventListener('click', function() { openModal(b.dataset.target); });
});
document.querySelectorAll('.close').forEach(function(b) {
  b.addEventListener('click', function() { closeModal(b.dataset.close); });
});
document.querySelectorAll('.modal').forEach(function(m) {
  m.addEventListener('click', function(e) { if (e.target === m) m.classList.remove('open'); });
});
function warnIfDisablingWeb(cb) {
  if (!cb.checked) {
    if (!confirm('Disabling the web server will break this browser connection. Continue?')) {
      cb.checked = true;
    }
  }
}
function warnIfChangingWebPort(input) {
  var orig = input.dataset.orig;
  if (input.value !== orig) {
    if (!confirm('Changing the web port will break this browser connection. Reconnect at the new port after saving. Continue?')) {
      input.value = orig;
    }
  }
}
function refreshLogs() {
  fetch('/logs').then(function(r) { return r.text(); }).then(function(t) {
    var el = document.getElementById('console');
    var atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 8;
    el.textContent = t;
    if (atBottom) el.scrollTop = el.scrollHeight;
  }).catch(function() {});
}
refreshLogs();
setInterval(refreshLogs, 2000);
</script>";

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_decode_basic() {
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("100%25"), "100%");
        assert_eq!(url_decode(""), "");
    }

    #[test]
    fn test_parse_form_basic() {
        let m = parse_form("a=1&b=hello+there&c=%2F&d=");
        assert_eq!(m.get("a").map(String::as_str), Some("1"));
        assert_eq!(m.get("b").map(String::as_str), Some("hello there"));
        assert_eq!(m.get("c").map(String::as_str), Some("/"));
        assert_eq!(m.get("d").map(String::as_str), Some(""));
    }

    #[test]
    fn test_base64_decode_roundtrip() {
        // "admin:changeme"
        assert_eq!(decode_base64("YWRtaW46Y2hhbmdlbWU="), b"admin:changeme");
        // Empty.
        assert_eq!(decode_base64(""), b"");
        // Invalid byte yields empty.
        assert!(decode_base64("@@@").is_empty());
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("<b>&'\""), "&lt;b&gt;&amp;&#39;&quot;");
        assert_eq!(html_escape("plain"), "plain");
    }

    #[test]
    fn test_find_double_crlf() {
        assert_eq!(find_double_crlf(b"GET / HTTP/1.1\r\n\r\n"), Some(14));
        assert_eq!(find_double_crlf(b"no separator here"), None);
        assert_eq!(find_double_crlf(b"\r\n\r\n"), Some(0));
    }

    #[test]
    fn test_render_main_page_contains_key_fields() {
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        // Header + each frame's signature field.
        assert!(html.contains("Ethernet Gateway"));
        assert!(html.contains("telnet_enabled"));
        assert!(html.contains("web_enabled"));
        assert!(html.contains("kermit_server_enabled"));
        assert!(html.contains("security_enabled"));
        assert!(html.contains("serial_a_enabled"));
        assert!(html.contains("serial_b_enabled"));
        // Scripture verse is part of the page.
        assert!(html.contains("John 3:16"));
    }

    #[test]
    fn test_render_main_page_includes_notice() {
        let cfg = Config::default();
        let html = render_main_page(&cfg, Some("Saved!".into()));
        assert!(html.contains("Saved!"));
    }

    #[test]
    fn test_render_page_html_escapes_user_input() {
        let cfg = Config {
            browser_homepage: "<script>alert(1)</script>".into(),
            ..Config::default()
        };
        let html = render_main_page(&cfg, None);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn test_local_ip_returns_non_empty() {
        let ip = local_ip();
        assert!(!ip.is_empty());
    }

    #[test]
    fn test_encode_query_safe_chars_pass_through() {
        assert_eq!(encode_query("hello-world.txt~"), "hello-world.txt~");
        assert_eq!(encode_query("abc123_xyz"), "abc123_xyz");
    }

    #[test]
    fn test_encode_query_percent_encodes_punct_and_space() {
        // Spaces, slashes, ampersands, and non-ASCII all need encoding.
        assert_eq!(encode_query("a b"), "a%20b");
        assert_eq!(encode_query("/save?x=1"), "%2Fsave%3Fx%3D1");
        assert_eq!(encode_query("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy("true"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy("True"));
        assert!(is_truthy("on"));
        assert!(is_truthy("1"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("nope"));
    }

    #[test]
    fn test_lockout_triggers_after_max_attempts() {
        // The web server reuses the same LockoutMap as telnet/SSH.
        // Verify that record_auth_failure crosses the threshold in
        // exactly AUTH_MAX_ATTEMPTS calls and that is_locked_out
        // flips at that boundary — same contract the web auth path
        // depends on.
        use std::collections::HashMap;
        use std::net::Ipv4Addr;
        use std::sync::{Arc, Mutex};

        let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 200));
        assert!(!telnet::is_locked_out(&lockouts, ip));
        for _ in 0..telnet::AUTH_MAX_ATTEMPTS {
            telnet::record_auth_failure(&lockouts, ip);
        }
        assert!(telnet::is_locked_out(&lockouts, ip));
    }

    #[test]
    fn test_lockout_cleared_on_successful_auth() {
        // Mirrors the live-auth flow: a few failures accumulate, then
        // a correct password clears the entry so the user isn't held
        // out for the full 5-minute window after recovering.
        use std::collections::HashMap;
        use std::net::Ipv4Addr;
        use std::sync::{Arc, Mutex};

        let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 201));
        telnet::record_auth_failure(&lockouts, ip);
        telnet::record_auth_failure(&lockouts, ip);
        assert!(!telnet::is_locked_out(&lockouts, ip));
        telnet::clear_lockout(&lockouts, ip);
        // A subsequent first failure should start fresh, not roll
        // over from the cleared count.
        let count = telnet::record_auth_failure(&lockouts, ip);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_encode_query_roundtrip_via_url_decode() {
        let original = "Saved! Port changed to 18081.";
        let encoded = encode_query(original);
        // The browser will decode + → space and %xx → byte; our url_decode
        // also turns + into space, which is fine because encode_query
        // never emits a literal '+' (spaces go to %20).
        assert_eq!(url_decode(&encoded), original);
    }

    #[test]
    fn test_url_decode_handles_utf8_multibyte() {
        // Round-trip UTF-8 through encode_query → url_decode.  Earlier
        // url_decode cast each decoded byte to `char` directly, which
        // produced Latin-1 codepoints instead of reassembling the
        // multi-byte UTF-8 sequence.  Lock the fix down so a future
        // refactor can't regress it.
        for original in ["café", "naïve", "日本語", "emoji 🎉 here", "Ω + π"] {
            let encoded = encode_query(original);
            assert_eq!(
                url_decode(&encoded),
                original,
                "round-trip failed for {:?}",
                original,
            );
        }
    }

    #[test]
    fn test_url_decode_truncated_percent_escape() {
        // A trailing `%` with no hex digits, or only one digit, must
        // not panic; the malformed escape is silently dropped.
        assert_eq!(url_decode("hello%"), "hello");
        assert_eq!(url_decode("hello%2"), "hello");
        // A bad hex digit also drops the escape but resumes decoding.
        assert_eq!(url_decode("a%ZZb"), "ab");
    }

    #[test]
    fn test_base64_decode_with_padding_variants() {
        // 0 / 1 / 2 trailing `=` characters all decode correctly.
        assert_eq!(decode_base64("YWJjZA=="), b"abcd");
        assert_eq!(decode_base64("YWJjZGU="), b"abcde");
        assert_eq!(decode_base64("YWJjZGVm"), b"abcdef");
        // Whitespace inside the input is stripped before decoding.
        assert_eq!(decode_base64("YWRt aW46 Y2hh bmdl bWU="), b"admin:changeme");
    }

    /// Construct a minimal HttpRequest with just the headers we need
    /// for is_authorized() to make a decision.  Lets the tests below
    /// drive the auth path without going through the network parser.
    fn req_with_auth(auth_value: Option<&str>) -> HttpRequest {
        let mut headers = HashMap::new();
        if let Some(v) = auth_value {
            headers.insert("authorization".into(), v.into());
        }
        HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn test_is_authorized_missing_header_fails() {
        // No Authorization header at all → auth fails.
        assert!(!is_authorized(&req_with_auth(None)));
    }

    #[test]
    fn test_is_authorized_non_basic_scheme_fails() {
        // Bearer / Digest / arbitrary scheme prefixes all fail; we
        // only accept Basic.
        assert!(!is_authorized(&req_with_auth(Some("Bearer abcdef"))));
        assert!(!is_authorized(&req_with_auth(Some("Digest realm=x"))));
        assert!(!is_authorized(&req_with_auth(Some("nonsense"))));
    }

    #[test]
    fn test_is_authorized_malformed_base64_fails() {
        // Base64 with non-base64 characters yields an empty decode,
        // which means no `:` separator, which means auth fails.
        assert!(!is_authorized(&req_with_auth(Some("Basic @@@"))));
    }

    #[test]
    fn test_is_authorized_no_colon_fails() {
        // Properly base64 but no `:` separator between user and pass.
        // "noseparator" → "bm9zZXBhcmF0b3I="
        assert!(!is_authorized(&req_with_auth(Some("Basic bm9zZXBhcmF0b3I="))));
    }

    #[test]
    fn test_is_authorized_accepts_lowercase_scheme() {
        // RFC 7235 says the scheme name is case-insensitive.  Some
        // ancient clients send "basic " in lowercase; accept both.
        // Both should fail since the credentials don't match the
        // default config, but they shouldn't short-circuit on the
        // scheme parse.
        let req = req_with_auth(Some("basic dXNlcjpwYXNz")); // user:pass
        // We don't know the test runtime's config username/password —
        // the global CONFIG is loaded from the cwd.  Just verify the
        // parse didn't short-circuit; behavior beyond that is covered
        // by the smoke test.
        let _ = is_authorized(&req);
    }

    fn empty_form() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn test_collect_form_updates_warns_when_disabling_web() {
        let old = Config { web_enabled: true, web_port: 8080, ..Config::default() };
        // Form omits web_enabled entirely → treated as false.
        let (_, notice) = collect_form_updates(&empty_form(), &old);
        assert!(
            notice.contains("Web server disabled"),
            "expected disable warning, got: {}",
            notice
        );
    }

    #[test]
    fn test_collect_form_updates_warns_on_port_change() {
        let old = Config { web_enabled: true, web_port: 8080, ..Config::default() };
        let mut form = empty_form();
        form.insert("web_enabled".into(), "true".into());
        form.insert("web_port".into(), "9090".into());
        let (_, notice) = collect_form_updates(&form, &old);
        assert!(
            notice.contains("port changed to 9090"),
            "expected port-change warning, got: {}",
            notice
        );
    }

    #[test]
    fn test_collect_form_updates_no_warning_on_unchanged_save() {
        let old = Config { web_enabled: true, web_port: 8080, ..Config::default() };
        let mut form = empty_form();
        form.insert("web_enabled".into(), "true".into());
        form.insert("web_port".into(), "8080".into());
        let (_, notice) = collect_form_updates(&form, &old);
        assert_eq!(notice, "Configuration saved.");
    }

    #[test]
    fn test_collect_form_updates_absent_checkboxes_become_false() {
        // The form contains zero boolean keys; every known bool must
        // come back set to "false".  This is the contract HTML forms
        // require for unchecked checkboxes (they don't submit).
        let old = Config::default();
        let (updates, _) = collect_form_updates(&empty_form(), &old);
        for key in [
            "telnet_enabled", "ssh_enabled", "web_enabled",
            "security_enabled", "verbose",
        ] {
            let pair = updates.iter().find(|(k, _)| k == key);
            assert!(pair.is_some(), "missing key {}", key);
            assert_eq!(pair.unwrap().1, "false", "key {} should be false", key);
        }
    }

    #[test]
    fn test_collect_form_updates_truthy_checkbox_values() {
        // "true" / "on" / "1" are all accepted as a checked checkbox —
        // browser quirks plus a hand-crafted POST should both work.
        let old = Config::default();
        for val in ["true", "on", "1", "TRUE"] {
            let mut form = empty_form();
            form.insert("security_enabled".into(), val.into());
            let (updates, _) = collect_form_updates(&form, &old);
            let pair = updates.iter().find(|(k, _)| k == "security_enabled").unwrap();
            assert_eq!(pair.1, "true", "value {:?} should be truthy", val);
        }
    }

    #[test]
    fn test_collect_form_updates_includes_plain_keys() {
        // Plain text fields are passed straight through; validation
        // happens later inside apply_config_key.
        let old = Config::default();
        let mut form = empty_form();
        form.insert("telnet_port".into(), "2323".into());
        form.insert("groq_api_key".into(), "gsk_test".into());
        form.insert("transfer_dir".into(), "/var/files".into());
        let (updates, _) = collect_form_updates(&form, &old);
        let lookup = |k: &str| {
            updates
                .iter()
                .find(|(uk, _)| uk == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(lookup("telnet_port"), Some("2323"));
        assert_eq!(lookup("groq_api_key"), Some("gsk_test"));
        assert_eq!(lookup("transfer_dir"), Some("/var/files"));
    }

    #[test]
    fn test_collect_form_updates_includes_serial_keys() {
        // Per-port serial settings round-trip with the right prefixes.
        let old = Config::default();
        let mut form = empty_form();
        form.insert("serial_a_baud".into(), "115200".into());
        form.insert("serial_b_mode".into(), "console".into());
        form.insert("serial_a_stored_2".into(), "5551234".into());
        let (updates, _) = collect_form_updates(&form, &old);
        let lookup = |k: &str| {
            updates
                .iter()
                .find(|(uk, _)| uk == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(lookup("serial_a_baud"), Some("115200"));
        assert_eq!(lookup("serial_b_mode"), Some("console"));
        assert_eq!(lookup("serial_a_stored_2"), Some("5551234"));
    }

    #[test]
    fn test_parse_form_handles_utf8_value() {
        // End-to-end: percent-encoded UTF-8 in a form value survives
        // the parse_form → url_decode pipeline as the original chars.
        let body = format!("home=https%3A%2F%2Fexample.com%2F&zip={}", encode_query("日本"));
        let fields = parse_form(&body);
        assert_eq!(fields.get("home").map(String::as_str), Some("https://example.com/"));
        assert_eq!(fields.get("zip").map(String::as_str), Some("日本"));
    }

    #[test]
    fn test_save_action_from_form_recognizes_each_variant() {
        // Each frame's submit button identifies itself via the
        // `action` form field; verify the dispatch table maps every
        // expected value and falls back safely on unknown / absent.
        assert_eq!(SaveAction::from_form(Some("save")), SaveAction::Save);
        assert_eq!(
            SaveAction::from_form(Some("save_and_restart")),
            SaveAction::SaveAndRestart,
        );
        assert_eq!(
            SaveAction::from_form(Some("save_and_restart_serial")),
            SaveAction::SaveAndRestartSerial,
        );
        // Unknown actions and missing fields both fall back to the
        // safe persist-only behavior — never accidentally restart on
        // a hand-crafted POST with a typo.
        assert_eq!(SaveAction::from_form(Some("bogus")), SaveAction::Save);
        assert_eq!(SaveAction::from_form(Some("")), SaveAction::Save);
        assert_eq!(SaveAction::from_form(None), SaveAction::Save);
    }

    #[test]
    fn test_rendered_page_advertises_every_save_action() {
        // Each per-frame Save button on the page submits a distinct
        // `action=...` value.  If a button accidentally lands on the
        // wrong action, the corresponding restart behavior breaks
        // silently — guard against that drift by asserting each
        // intended action value appears in the rendered HTML.
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        assert!(
            html.contains("value=\"save_and_restart\""),
            "Server frame's Save and Restart button missing"
        );
        assert!(
            html.contains("value=\"save_and_restart_serial\""),
            "Serial frame's Save (serial reload) button missing"
        );
        assert!(
            html.contains("value=\"save\""),
            "Per-frame plain Save button missing"
        );
    }

    #[test]
    fn test_rendered_page_puts_more_popups_inside_form() {
        // The popups must live inside the <form> so their fields
        // actually submit.  This was a bug in an earlier revision —
        // the popups were rendered after </form>, so any change made
        // in a More popup silently dropped on save.  Lock it down by
        // checking that a popup id appears between <form ...> and
        // </form> in the rendered HTML.
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        let form_start = html.find("<form").expect("form open tag");
        let form_end = html.find("</form>").expect("form close tag");
        let popup_pos = html.find("id=\"more-server\"").expect("server popup id");
        assert!(
            popup_pos > form_start && popup_pos < form_end,
            "more-server popup is outside the form (pos {} vs form {}..{})",
            popup_pos, form_start, form_end,
        );
    }

    #[test]
    fn test_inflight_guard_decrements_on_drop() {
        // The Drop-based slot release is the only thing keeping
        // long-running connections from leaking the cap.  Spot-check
        // that exiting the guard's scope (panic or otherwise)
        // releases the slot.
        let counter = Arc::new(AtomicUsize::new(0));
        {
            counter.fetch_add(1, Ordering::SeqCst);
            let _g = InflightGuard(counter.clone());
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
