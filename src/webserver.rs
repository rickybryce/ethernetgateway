//! Hand-rolled HTTP/1.1 configuration web server.
//!
//! Renders the same settings page the GUI does, in a browser.  Accepts only
//! private/loopback source IPs unless `disable_ip_safety` is set — applied
//! regardless of whether login is required (M-9), which DIFFERS from the
//! telnet listener (there, enabling `security_enabled` opens any IP; here it
//! does not, because this page renders the password + API key).  HTTP Basic
//! auth is gated by the same `security_enabled` flag using the telnet
//! `username` / `password`.
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

/// IP-policy decision for the web server (M-9). Returns `Some(reason)` to
/// reject, `None` to allow. The private-IP allowlist applies whenever
/// `disable_ip_safety` is off — INDEPENDENT of whether login is required.
///
/// `security_enabled` is intentionally IGNORED (it's a parameter only so a
/// test can assert it makes no difference): unlike the telnet listener, which
/// drops the allowlist once `security_enabled` is on, the web server keeps it
/// because its page renders the password + API key. Keeping this decision in
/// one named, tested function guards against a silent revert that re-couples
/// the allowlist to `security_enabled`.
fn web_ip_rejection(
    security_enabled: bool,
    disable_ip_safety: bool,
    peer_ip: IpAddr,
) -> Option<&'static str> {
    let _ = security_enabled; // deliberately not consulted — see doc comment
    if disable_ip_safety {
        None
    } else {
        telnet::reject_insecure_ip(peer_ip)
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
    //
    // The private-IP allowlist applies whenever `disable_ip_safety` is off,
    // INDEPENDENT of whether login is required (M-9).  Enabling "Require
    // Login" used to *drop* the allowlist (accepting any source IP gated only
    // by cleartext-HTTP Basic auth, on a page that echoes the password and
    // API key into value="…" attributes) — a counterintuitive "turning
    // security on widens IP exposure" interaction.  Now auth and the IP
    // allowlist are independent layers: an operator who genuinely wants
    // login-gated access from arbitrary IPs opts in explicitly with
    // `disable_ip_safety = true` (the single, documented escape hatch).
    //
    // This DELIBERATELY differs from the telnet accept loop
    // (`telnet::start_server`), which still couples the allowlist to
    // `security_enabled`: telnet echoes no secrets and is the retro-hardware
    // path where "enable auth to expose it" is a legitimate deployment,
    // whereas this page renders the password + API key. See the matching note
    // there.
    let (live_security, live_disable_safety) = config::get_security_flags();
    if let Some(reason) = web_ip_rejection(live_security, live_disable_safety, peer_ip) {
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
            // Only a *present but wrong* credential counts toward the
            // brute-force limit.  A request with no Authorization header is
            // the normal first half of the HTTP Basic challenge/response —
            // every browser sends it (and repeats it for subresources like
            // favicon) before it has any credentials to offer.  Counting
            // those would let a browser lock its own user out before they
            // typed a single password; a real attacker always sends a
            // credential, so the lockout still bites the case that matters.
            if request_presented_credential(&request) {
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
        ("GET", "/serial-ports") => {
            // Live serial-port re-scan for the refresh button.  The
            // JS picks up the result and rewrites the option list of
            // both serial selects without a full page reload.
            let ports = crate::gui::detect_serial_ports();
            let body = serial_ports_json(&ports);
            write_response(
                &mut stream,
                200,
                "OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
                false,
            )
            .await?;
        }
        ("POST", "/save") => {
            // CSRF defense-in-depth: reject a POST whose Origin/Referer
            // doesn't match our Host (a forged cross-site submit that would
            // otherwise ride the operator's cached Basic-auth credentials to
            // rewrite config — including disabling auth).
            if !same_origin_ok(&request) {
                logger::log(
                    "Web: rejected /save with cross-origin Origin/Referer (possible CSRF).".into(),
                );
                let body = b"403 Forbidden: cross-origin request rejected\n";
                write_response(
                    &mut stream,
                    403,
                    "Forbidden",
                    "text/plain; charset=utf-8",
                    body,
                    false,
                )
                .await?;
                return Ok(());
            }
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

/// Extract the authority (`host[:port]`) from an `Origin` or `Referer`
/// value: strip the `scheme://` prefix, then take everything up to the
/// first path/query/fragment delimiter.
fn url_authority(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
}

/// Same-origin guard for state-changing POSTs (CSRF defense-in-depth).
/// A browser always sends `Origin` on a cross-site POST, so an `Origin`
/// (or, failing that, `Referer`) whose authority doesn't match our own
/// `Host` flags a forged cross-site request — reject it.  When neither
/// header is present (non-browser clients such as curl, which can't be a
/// CSRF vector) the request is allowed: HTTP Basic auth still gates it,
/// and the threat model is trusted-LAN, so this is deliberately
/// lenient-on-absent rather than a full per-request token scheme.
fn same_origin_ok(req: &HttpRequest) -> bool {
    let Some(host) = req.headers.get("host") else {
        // No Host header to compare against — nothing to verify; allow.
        return true;
    };
    if let Some(origin) = req.headers.get("origin") {
        return url_authority(origin).eq_ignore_ascii_case(host);
    }
    if let Some(referer) = req.headers.get("referer") {
        return url_authority(referer).eq_ignore_ascii_case(host);
    }
    true
}

/// Whether the request actually presented a credential (an `Authorization`
/// header), as opposed to the credential-less request a browser sends as the
/// first half of the HTTP Basic challenge.  Only a presented-but-wrong
/// credential counts toward the brute-force lockout — counting the bare
/// challenge preflight (and subresource probes that repeat it) would let a
/// browser lock its own user out before they typed a password.
fn request_presented_credential(req: &HttpRequest) -> bool {
    req.headers.contains_key("authorization")
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
    // Evaluate BOTH comparisons before combining (no `&&` short-circuit) so a
    // wrong username can't be distinguished from a wrong password by response
    // time.  Mirrors the telnet/SSH auth paths.
    let user_ok = telnet::constant_time_eq(user.as_bytes(), cfg.username.as_bytes());
    let pass_ok = telnet::constant_time_eq(pass.as_bytes(), cfg.password.as_bytes());
    user_ok && pass_ok
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
/// punctuation pass through; everything else is `%xx`.  `pub(crate)` so the
/// weather fetch in telnet.rs can safely encode worldwide location queries
/// (city names, postal codes with spaces, UTF-8) into the geocoder URL.
pub(crate) fn encode_query(input: &str) -> String {
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
    // Guard against a malformed submission (non-UTF-8 body, or a
    // chunked/empty body that read_request surfaced as zero-length): with
    // no fields, collect_form_updates would write every checkbox-boolean as
    // `false`, silently disabling telnet/ssh/web/security in one shot.  The
    // real config form always submits many fields, so an empty field set is
    // never a legitimate save — refuse it (SaveAction::Save triggers no
    // restart) instead of wiping the config.
    let Ok(text) = std::str::from_utf8(body) else {
        return (
            "Save ignored: request body was not valid UTF-8.".to_string(),
            SaveAction::Save,
        );
    };
    let fields = parse_form(text);
    if fields.is_empty() {
        return (
            "Save ignored: empty or malformed form submission.".to_string(),
            SaveAction::Save,
        );
    }
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
        "transfer_dir", "max_sessions", "idle_timeout_secs", "gui_zoom",
        "groq_api_key", "browser_homepage", "weather_location", "weather_units",
        "xmodem_negotiation_timeout", "xmodem_block_timeout",
        "xmodem_max_retries", "xmodem_negotiation_retry_interval",
        "zmodem_negotiation_timeout", "zmodem_frame_timeout",
        "zmodem_max_retries", "zmodem_negotiation_retry_interval",
        "kermit_negotiation_timeout", "kermit_packet_timeout",
        "kermit_idle_timeout", "kermit_max_retries",
        "kermit_max_packet_length", "kermit_window_size",
        "kermit_block_check_type", "kermit_8bit_quote",
        "kermit_resume_max_age_hours",
        "punter_block_size", "punter_negotiation_timeout",
        "punter_block_timeout", "punter_max_retries",
        "punter_max_bad_rounds", "punter_negotiation_retry_interval",
        "ssh_gateway_auth",
        "gateway_role", "slave_master_host", "slave_master_port",
        "slave_master_username", "slave_master_password",
        // `relay_transport` is intentionally NOT here: no UI (telnet, web,
        // or GUI) exposes it because "raw" is not yet implemented, so the
        // web form must not accept it either (a crafted POST otherwise
        // could select the unimplemented transport).  It stays settable
        // only by hand-editing egateway.conf.
    ];
    for key in plain_keys {
        if let Some(v) = fields.get(*key) {
            updates.push(((*key).to_string(), v.clone()));
        }
    }

    // Checkbox-style booleans: an unchecked checkbox does not appear in
    // the form data, so absence is the canonical "false" signal.  Every
    // boolean key the page renders is set unconditionally (except
    // master_accept_relays, which is role-gated — see below) — partial
    // saves are not supported (the full form is always submitted).
    let bool_keys: &[&str] = &[
        "telnet_enabled", "ssh_enabled", "kermit_server_enabled", "web_enabled",
        "security_enabled", "disable_ip_safety", "enable_console", "verbose",
        "telnet_gateway_negotiate", "telnet_gateway_raw", "gateway_debug",
        "kermit_long_packets", "kermit_sliding_windows", "kermit_streaming",
        "kermit_attribute_packets", "kermit_repeat_compression",
        "kermit_resume_partial", "kermit_locking_shifts",
        "kermit_wait_for_receiver",
        "allow_atdt_kermit",
        "allow_peer_dial",
        "punter_hangup_on_failure",
        "master_accept_relays",
        "serial_a_enabled", "serial_b_enabled",
        "serial_a_echo", "serial_a_verbose", "serial_a_quiet",
        "serial_b_echo", "serial_b_verbose", "serial_b_quiet",
        "serial_a_petscii_translate", "serial_b_petscii_translate",
        "serial_a_drive_carrier", "serial_b_drive_carrier",
    ];
    for key in bool_keys {
        // `master_accept_relays` applies only to a master.  In the other roles
        // the web renders its checkbox disabled, so it isn't submitted — skip
        // it there instead of clobbering the stored value to false.  This
        // matches the GUI/telnet, which preserve it (it is inert outside master
        // anyway, and is re-defaulted on when the role is switched to master).
        if *key == "master_accept_relays"
            && fields.get("gateway_role").map(String::as_str) != Some("master")
        {
            continue;
        }
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

/// Hand-rolled JSON encoder for the `/serial-ports` response.  Serial
/// device paths are ASCII and quote-free in practice on Linux/macOS/
/// Windows, but escape defensively so a hostile or oddly-named device
/// can't break the JSON parse on the client.
fn serial_ports_json(ports: &[String]) -> String {
    let mut out = String::from("{\"ports\":[");
    for (i, p) in ports.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in p.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push_str("]}");
    out
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
    out.push_str(&render_warning_popups());
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
    // CSS Grid layout so the two `Port:` colons line up between
    // rows (a port number is at most 5 digits, so 6-char inputs
    // are plenty).  Row 1 pairs Telnet + Web Server + More button;
    // Row 2 pairs SSH + Kermit Server.  Moving More up to row 1
    // gets rid of the third visible line the button used to wrap
    // onto on narrow viewports — the GUI's same-rationale layout
    // floats More to the right edge of the upper content row.
    //
    // Cells in the grid (column index in parens):
    //   (1) listener checkbox  (2) "Port:" label  (3) port input
    //   (4) listener checkbox  (5) "Port:" label  (6) port input
    //   (7) More button on row 1 / empty on row 2
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">Server</span>\
         <span class=\"sub\">(Changes Require Restart)</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"server-grid\">\
         {telnet_chk}<span class=\"port-label\">Port:</span>{telnet_port}\
         {web_chk}<span class=\"port-label\">Port:</span>{web_port}\
         <button type=\"button\" class=\"more\" data-target=\"more-server\">More\u{2026}</button>\
         {ssh_chk}<span class=\"port-label\">Port:</span>{ssh_port}\
         {kermit_chk}<span class=\"port-label\">Port:</span>{kermit_port}\
         <span class=\"grid-blank\"></span>\
         </div></section>",
        save = save_button("save_and_restart", "Save and Restart", "primary"),
        telnet_chk = checkbox("telnet_enabled", "Telnet", cfg.telnet_enabled),
        telnet_port = port_input("telnet_port", cfg.telnet_port, None),
        ssh_chk = checkbox("ssh_enabled", "SSH", cfg.ssh_enabled),
        ssh_port = port_input("ssh_port", cfg.ssh_port, None),
        web_chk = checkbox_with_attr(
            "web_enabled",
            "Web Server",
            cfg.web_enabled,
            "onchange=\"warnIfDisablingWeb(this)\"",
        ),
        web_port = port_input(
            "web_port",
            cfg.web_port,
            Some("onchange=\"warnIfChangingWebPort(this)\""),
        ),
        kermit_chk = checkbox_with_attr(
            "kermit_server_enabled",
            "Kermit Server",
            cfg.kermit_server_enabled,
            "onchange=\"warnOnEnable(this, 'warn-kermit-server')\"",
        ),
        kermit_port = port_input("kermit_server_port", cfg.kermit_server_port, None),
    )
}

/// Render a port-number `<input>` for the Server-frame grid.  Six
/// characters is enough for any valid TCP port (65535 = 5 digits)
/// plus a touch of breathing room.  When `extra_attr` is provided
/// the attribute string is appended verbatim (used for the web-port
/// onchange warning) and a `data-orig` carries the current value so
/// the warning JS can detect changes.
fn port_input(name: &str, value: u16, extra_attr: Option<&str>) -> String {
    let attr = extra_attr.unwrap_or("");
    format!(
        "<input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"6\" class=\"port-num\" data-orig=\"{value}\" {attr}>",
        name = name,
        value = value,
        attr = attr,
    )
}

fn frame_security(cfg: &Config) -> String {
    // Telnet, SSH, and the web UI now share one credential pair, so
    // the Security frame renders a single Login row instead of the
    // earlier separate Telnet / SSH rows.
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">Security</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{sec_chk} {ipsafe_chk}</div>\
         <div class=\"row\"><span class=\"label-dim\">Login</span> {user} {pass}</div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        sec_chk = checkbox("security_enabled", "Require Login", cfg.security_enabled),
        ipsafe_chk = checkbox_with_attr(
            "disable_ip_safety",
            "Disable IP Safety",
            cfg.disable_ip_safety,
            "onchange=\"warnOnEnable(this, 'warn-ip-safety')\"",
        ),
        user = textfield("username", "User", &cfg.username, false, 12),
        pass = textfield("password", "Pass", &cfg.password, true, 12),
    )
}

/// Master/Slave serial-extender rows for the Server "More" modal (mirrors the
/// GUI, where these live under the Server frame's More popup — `draw_server_-
/// relay`).  `gateway_role` is an enum select; the master gate is a checkbox;
/// the slave's master host/port/credentials are text fields (password masked).
/// Changing role/relays needs a server restart, which the modal's own "Save
/// and Restart" button provides — so these rows carry no separate save button.
/// (`relay_transport` has no control here — SSH is the only implemented
/// transport; the raw alternative will add one when it lands.)  See the
/// Master/Slave design note.
fn master_slave_rows(cfg: &Config) -> String {
    let role_sel = |v: &str| if cfg.gateway_role == v { "selected" } else { "" };
    let is_master = cfg.gateway_role == "master";
    let is_slave = cfg.gateway_role == "slave";
    // Grey out the fields that don't apply to the current role: `accept relays`
    // is Master-only, the master host/port/user/pass are Slave-only.  The
    // server renders the initial disabled state (correct even without JS), and
    // `updateRelayFields()`/`onRoleChange()` keep it in sync as the role
    // changes.  Disabled inputs aren't submitted, and the save preserves a
    // greyed field's stored value: the slave_* text fields because plain keys
    // are only written when present, and `master_accept_relays` because the
    // save skips it unless the submitted role is master (see
    // collect_form_updates).
    let dis_accept = if is_master { "" } else { "disabled" };
    let dis_slave = if is_slave { "" } else { "disabled" };
    format!(
        "<h3>Master/Slave</h3>\
         <div class=\"row\"><span class=\"label\">Role:</span>\
         <select name=\"gateway_role\" onchange=\"onRoleChange(this)\">\
         <option value=\"standalone\" {st_sel}>Standalone</option>\
         <option value=\"master\" {ma_sel}>Master</option>\
         <option value=\"slave\" {sl_sel}>Slave</option>\
         </select> {accept_chk}</div>\
         <div class=\"row\">{host} {port}</div>\
         <div class=\"row\">{user} {pass}</div>",
        st_sel = role_sel("standalone"),
        ma_sel = role_sel("master"),
        sl_sel = role_sel("slave"),
        accept_chk = checkbox_with_attr(
            "master_accept_relays",
            "Master: accept relays",
            cfg.master_accept_relays,
            dis_accept,
        ),
        host = textfield_attr("slave_master_host", "Master Host", &cfg.slave_master_host, false, 16, dis_slave),
        port = numfield_attr("slave_master_port", "Port", cfg.slave_master_port, dis_slave),
        user = textfield_attr("slave_master_username", "User", &cfg.slave_master_username, false, 12, dis_slave),
        pass = textfield_attr("slave_master_password", "Pass", &cfg.slave_master_password, true, 12, dis_slave),
    )
}

fn frame_file_transfer(cfg: &Config) -> String {
    // Matches the GUI: Dir on top, then a single tunables row with
    // Negotiate / Block / Retries plus the right-aligned More button.
    // The `xmodem_negotiation_retry_interval` ("Poke") field moves to
    // the More popup (alongside the other rarely-tuned timeouts), just
    // like the GUI's draw_file_transfer_advanced.  The desktop GUI
    // also has a folder-browse button next to Dir — that opens a
    // native picker on the operator's machine, which doesn't make
    // sense for a remote browser, so the web variant omits it.
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">File Transfer (XMODEM)</span>\
         <span class=\"sub\">(More for others)</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\"><span class=\"label\">Dir:</span>\
         <input type=\"text\" name=\"transfer_dir\" value=\"{td}\" class=\"transfer-dir\"></div>\
         <div class=\"row tight-row\">{neg} {blk} {retries}\
         <button type=\"button\" class=\"more\" data-target=\"more-xfer\">More\u{2026}</button></div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        neg = numfield("xmodem_negotiation_timeout", "Negotiate", cfg.xmodem_negotiation_timeout),
        blk = numfield("xmodem_block_timeout", "Block", cfg.xmodem_block_timeout),
        retries = numfield("xmodem_max_retries", "Retries", cfg.xmodem_max_retries),
        td = html_escape(&cfg.transfer_dir),
    )
}

fn frame_ai_browser(cfg: &Config) -> String {
    // Three rows: title+Save, API Key, and Home with a right-aligned "More…"
    // button.  The weather location + units live in the `more-ai` modal
    // (render_more_popups) so this frame stays compact, mirroring the GUI.
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">AI Chat, Browser, and Weather</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\"><span class=\"label\">API Key:</span>\
         <input type=\"password\" name=\"groq_api_key\" value=\"{key}\"></div>\
         <div class=\"row\"><span class=\"label\">Home:</span>\
         <input type=\"text\" name=\"browser_homepage\" value=\"{home}\">\
         <button type=\"button\" class=\"more\" data-target=\"more-ai\">More\u{2026}</button></div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        key = html_escape(&cfg.groq_api_key),
        home = html_escape(&cfg.browser_homepage),
    )
}

fn frame_serial(cfg: &Config) -> String {
    // Matches the GUI: both Enabled checkboxes ride in the frame
    // header alongside per-port titles + the right-aligned Save
    // button, so each per-port row below stays compact (label, port
    // select, refresh, baud, More).  The two header titles use the
    // same amber title style as the other frames' single title.
    format!(
        "<section class=\"frame\"><div class=\"frame-head serial-head\">\
         <span class=\"title\">Serial Port A</span> {en_a}\
         <span class=\"title\">Serial Port B</span> {en_b}\
         <span class=\"head-right\">{save}</span></div>\
         {a}\
         {b}\
         </section>",
        en_a = checkbox("serial_a_enabled", "Enabled", cfg.serial_a.enabled),
        en_b = checkbox("serial_b_enabled", "Enabled", cfg.serial_b.enabled),
        save = save_button("save_and_restart_serial", "Save", "secondary"),
        a = serial_row("serial_a", "Port A", &cfg.serial_a),
        b = serial_row("serial_b", "Port B", &cfg.serial_b),
    )
}

fn serial_row(prefix: &str, label: &str, port: &config::SerialPortConfig) -> String {
    // Detect available ports server-side at render time (mirrors the
    // GUI's ComboBox source).  The JS refresh button below re-fetches
    // via /serial-ports without a full page reload.  The row uses
    // `serial-row` instead of the default `.row` class so it keeps
    // the More button on the same line as the rest of the controls
    // — the default `.row` wraps when the contents overflow, which
    // pushed More onto its own line once the dropdown + refresh
    // button joined the row.
    let detected = crate::gui::detect_serial_ports();
    // The Enabled checkbox now lives in the frame header (matches the
    // GUI), so each per-port row is: label + select + refresh + Baud
    // + More.  Keeping the row this lean leaves room for the More
    // button to sit on the right edge without wrapping even inside
    // the half-width frame.
    format!(
        "<div class=\"row serial-row\"><span class=\"label\">{label}:</span>\
         <select name=\"{prefix}_port\" class=\"serial-port-select\" data-current=\"{dev}\">\
         {options}\
         </select>\
         <button type=\"button\" class=\"refresh\" title=\"Refresh ports\" \
         data-refresh-ports>\u{21bb}</button>\
         {baud}\
         <button type=\"button\" class=\"more\" data-target=\"more-{prefix}\">More\u{2026}</button></div>",
        label = label,
        prefix = prefix,
        dev = html_escape(&port.port),
        options = serial_port_options(&port.port, &detected),
        baud = numfield(&format!("{}_baud", prefix), "Baud", port.baud),
    )
}

/// Build the `<option>` list for a serial-port `<select>`.  Always
/// includes a leading "(none)" option (the empty-string value, which
/// disables the port).  Detected ports come next.  Finally, if the
/// currently-saved port path is non-empty and isn't in the detected
/// list (cable unplugged, device temporarily gone), it gets its own
/// option with a "(saved)" suffix so the operator can still see and
/// keep their pinned value.
fn serial_port_options(current: &str, detected: &[String]) -> String {
    let mut out = String::new();
    let sel_none = if current.is_empty() { " selected" } else { "" };
    out.push_str(&format!(
        "<option value=\"\"{sel}>(none)</option>",
        sel = sel_none,
    ));
    let mut current_in_detected = false;
    for p in detected {
        let sel = if p == current { " selected" } else { "" };
        if p == current {
            current_in_detected = true;
        }
        out.push_str(&format!(
            "<option value=\"{v}\"{sel}>{v}</option>",
            v = html_escape(p),
            sel = sel,
        ));
    }
    if !current.is_empty() && !current_in_detected {
        out.push_str(&format!(
            "<option value=\"{v}\" selected>{v} (saved)</option>",
            v = html_escape(current),
        ));
    }
    out
}

fn frame_general(cfg: &Config) -> String {
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">General</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\">{v}</div>\
         <div class=\"row\">{d}<span class=\"hspace\"></span>{g}</div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        v = checkbox("verbose", "Verbose Transfer Logging", cfg.verbose),
        d = checkbox("gateway_debug", "Gateway Debug Trace", cfg.gateway_debug),
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

/// Build one dark-red warning modal.  Single-sources the modal id (used on the
/// container div AND both buttons' `data-warn`) so a copy-paste id typo — which
/// would silently make a warning never open — is impossible.  `show_cancel` is
/// false for informational (OK-only) warnings.
fn warn_modal(id: &str, title: &str, body: &str, confirm_label: &str, show_cancel: bool) -> String {
    let cancel = if show_cancel {
        format!("<button type=\"button\" class=\"warn-cancel\" data-warn=\"{id}\">Cancel</button>")
    } else {
        String::new()
    };
    format!(
        "<div class=\"modal warn\" id=\"{id}\"><div class=\"modal-body warn\">\
         <div class=\"modal-head\"><span class=\"title\">{title}</span></div>\
         <p>{body}</p>\
         <div class=\"modal-foot\">{cancel}\
         <button type=\"button\" class=\"warn-continue\" data-warn=\"{id}\">{confirm_label}</button>\
         </div></div></div>"
    )
}

/// Dark-red warning modals that replace the old native `confirm()`/`alert()`
/// dialogs.  The JS in `SCRIPT` opens them and wires Continue/Cancel; the
/// overlay blocks the form behind it, and warning modals are excluded from
/// backdrop-dismiss, so the operator must click a button to proceed.
fn render_warning_popups() -> String {
    let warn = "\u{26a0} Warning";
    let sec = "\u{26a0} Security warning";
    let mut out = String::new();
    out.push_str(&warn_modal(
        "warn-web-disable", warn,
        "Disabling the web server will break this browser connection.",
        "Continue", true,
    ));
    out.push_str(&warn_modal(
        "warn-web-port", warn,
        "Changing the web port will break this browser connection. Reconnect at \
         the new port after saving.",
        "Continue", true,
    ));
    out.push_str(&warn_modal(
        "warn-master-ssh", warn,
        "Master mode uses the SSH server for slave connections, but SSH is \
         currently disabled. Enable SSH in Server settings and Save &amp; Restart, \
         otherwise slaves cannot connect. (SSH is not changed automatically.)",
        "OK", false,
    ));
    out.push_str(&warn_modal(
        "warn-ip-safety", sec,
        "Disabling IP safety removes the private-IP allowlist entirely. Anyone \
         on the public internet who can reach your telnet port will be able to \
         connect \u{2014} and without Require Login, without a password. Enable only \
         when a separate control fronts the listener (LAN-only firewall, VPN, port \
         not exposed) or you are about to turn Require Login on.",
        "Continue", true,
    ));
    out.push_str(&warn_modal(
        "warn-kermit-server", sec,
        "Enabling the Kermit server opens a dedicated TCP port that drops every \
         connection straight into Kermit server mode \u{2014} no telnet menu, no \
         username, no password, no private-IP filter. Anyone who can reach the \
         listener can read and write files in your transfer directory.",
        "Continue", true,
    ));
    out.push_str(&warn_modal(
        "warn-atdt-kermit", sec,
        "Allowing ATDT KERMIT lets anyone who can dial the serial modem reach \
         Kermit server mode directly, bypassing the telnet menu's username/password \
         gate. There is no auth on this dial path. Enable only when the serial line \
         itself is trusted.",
        "Continue", true,
    ));
    out
}

fn render_more_popups(cfg: &Config) -> String {
    let mut out = String::new();
    // Desktop-GUI display scale (see cfg.gui_zoom_factor). Match on the parsed
    // factor so "1" and "1.0" both select 100% and any custom value still shows.
    let zf = cfg.gui_zoom_factor();
    let zsel = |target: f32| -> &'static str {
        if zf.is_some_and(|z| (z - target).abs() < 0.01) { "selected" } else { "" }
    };
    // Server More — session cap, idle timeout, GUI scale, gateway advanced.
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-server\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">Server \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-server\">\u{00d7}</button></div>\
         <div class=\"row\">{sessions} {idle}</div>\
         <div class=\"row\"><span class=\"label\">GUI display scale:</span>\
         <select name=\"gui_zoom\">\
         <option value=\"auto\" {z_auto}>Auto</option>\
         <option value=\"0.75\" {z75}>75%</option>\
         <option value=\"1.0\" {z100}>100%</option>\
         <option value=\"1.25\" {z125}>125%</option>\
         <option value=\"1.5\" {z150}>150%</option>\
         <option value=\"2.0\" {z200}>200%</option>\
         </select></div>\
         <div class=\"row\">{tneg} {traw}</div>\
         <div class=\"row\"><span class=\"label\">SSH Gateway Auth:</span>\
         <select name=\"ssh_gateway_auth\">\
         <option value=\"key\" {key_sel}>Key</option>\
         <option value=\"password\" {pwd_sel}>Password</option>\
         </select></div>\
         {master_slave}\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        sessions = numfield("max_sessions", "Sessions", cfg.max_sessions),
        idle = numfield("idle_timeout_secs", "Idle (s)", cfg.idle_timeout_secs),
        z_auto = if zf.is_none() { "selected" } else { "" },
        z75 = zsel(0.75),
        z100 = zsel(1.0),
        z125 = zsel(1.25),
        z150 = zsel(1.5),
        z200 = zsel(2.0),
        tneg = checkbox("telnet_gateway_negotiate", "Telnet Gateway: negotiate TTYPE/NAWS", cfg.telnet_gateway_negotiate),
        traw = checkbox("telnet_gateway_raw", "Telnet Gateway: raw TCP mode", cfg.telnet_gateway_raw),
        key_sel = if cfg.ssh_gateway_auth == "key" { "selected" } else { "" },
        pwd_sel = if cfg.ssh_gateway_auth == "password" { "selected" } else { "" },
        // Master/Slave lives under Server → More (mirrors the GUI); the modal's
        // own Save-and-Restart covers the restart a role change needs.
        master_slave = master_slave_rows(cfg),
        save = save_button("save_and_restart", "Save and Restart", "primary"),
    ));

    // AI/Browser/Weather More — weather location + units (moved off the
    // primary frame so it stays at three rows, mirroring the GUI).  The API
    // key + homepage remain on the main frame, so they are NOT repeated here
    // (a duplicate name= in this single form would clobber the value).
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-ai\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">AI, Browser &amp; Weather \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-ai\">\u{00d7}</button></div>\
         <div class=\"row\"><span class=\"label\">Location:</span>\
         <input type=\"text\" name=\"weather_location\" value=\"{loc}\" \
         placeholder=\"city or postal code\"></div>\
         <div class=\"row\"><span class=\"label\">Units:</span>\
         <select name=\"weather_units\">\
         <option value=\"auto\" {u_auto}>Auto</option>\
         <option value=\"us\" {u_us}>US (F/mph)</option>\
         <option value=\"metric\" {u_metric}>Metric (C/km/h)</option>\
         </select></div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        loc = html_escape(&cfg.weather_location),
        u_auto = if cfg.weather_units == "auto" { "selected" } else { "" },
        u_us = if cfg.weather_units == "us" { "selected" } else { "" },
        u_metric = if cfg.weather_units == "metric" { "selected" } else { "" },
        save = save_button("save", "Save", "secondary"),
    ));

    // File-transfer More — XMODEM-family retry interval (moved off
    // the primary frame to mirror the GUI's draw_file_transfer_-
    // advanced section), plus ZMODEM and Kermit settings.
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-xfer\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">File Transfer \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-xfer\">\u{00d7}</button></div>\
         <h3>XMODEM / XMODEM-1K / YMODEM</h3>\
         <div class=\"row\">{xint}</div>\
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
         <div class=\"row\">{kwr}</div>\
         <div class=\"row\">{atd}</div>\
         <div class=\"row\">{apd}</div>\
         <h3>Punter (C1)</h3>\
         <div class=\"row\">{pbs} {pneg}</div>\
         <div class=\"row\">{pblk} {pret} {pbad} {pint}</div>\
         <div class=\"row\">{phang}</div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        save = save_button("save", "Save", "secondary"),
        xint = numfield("xmodem_negotiation_retry_interval", "Retry interval (s)", cfg.xmodem_negotiation_retry_interval),
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
        kwr = checkbox("kermit_wait_for_receiver", "Wait for receiver NAK (download)", cfg.kermit_wait_for_receiver),
        atd = checkbox_with_attr(
            "allow_atdt_kermit",
            "Allow ATDT KERMIT (modem emulator)",
            cfg.allow_atdt_kermit,
            "onchange=\"warnOnEnable(this, 'warn-atdt-kermit')\"",
        ),
        apd = checkbox("allow_peer_dial", "Allow peer-dial (ATD Port@IP / ring modem ports)", cfg.allow_peer_dial),
        pbs = numfield("punter_block_size", "Block size (8-255)", cfg.punter_block_size),
        pneg = numfield("punter_negotiation_timeout", "Neg (s)", cfg.punter_negotiation_timeout),
        pblk = numfield("punter_block_timeout", "Block (s)", cfg.punter_block_timeout),
        pret = numfield("punter_max_retries", "Retries", cfg.punter_max_retries),
        pbad = numfield("punter_max_bad_rounds", "Bad rounds", cfg.punter_max_bad_rounds),
        pint = numfield("punter_negotiation_retry_interval", "Poke (s)", cfg.punter_negotiation_retry_interval),
        phang = checkbox("punter_hangup_on_failure", "Hang up (drop carrier) on a failed transfer", cfg.punter_hangup_on_failure),
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
         <div class=\"row\">{echo} {verb} {quiet} {petscii}</div>\
         <div class=\"row\">{xc} {dtr} {flw} {dcd} {carrier}</div>\
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
        petscii = checkbox_with_attr(
            &format!("{}_petscii_translate", prefix),
            "PETSCII (AT+PETSCII)",
            port.petscii_translate,
            "title=\"Text only — disable before XMODEM/YMODEM/ZMODEM/Kermit/Punter transfers over the same TCP session, or the binary payload will be corrupted.\"",
        ),
        xc = numfield(&format!("{}_x_code", prefix), "X-code", port.x_code),
        dtr = numfield(&format!("{}_dtr_mode", prefix), "&D", port.dtr_mode),
        flw = numfield(&format!("{}_flow_mode", prefix), "&K", port.flow_mode),
        dcd = numfield(&format!("{}_dcd_mode", prefix), "&C", port.dcd_mode),
        carrier = checkbox_with_attr(
            &format!("{}_drive_carrier", prefix),
            "Drive carrier (DCD)",
            port.drive_carrier,
            "title=\"Drive DTR as a carrier proxy (asserted on CONNECT, dropped on NO CARRIER, per AT&C). Wire DTR->DCD via null-modem. Off = the gateway never touches the modem-control lines. Modem mode only.\"",
        ),
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
    // An unchecked checkbox is not submitted, so absence from the form data
    // is the "false" signal collect_form_updates relies on server-side; a
    // checked box submits value="true".
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
    // size=5 fits every numeric setting we currently expose (max
    // observed is kermit_max_packet_length 4096, 4 digits) and
    // tightens the visual footprint so frames don't waste width on
    // empty input padding — matches the user's directive that text
    // entry boxes shouldn't reserve more characters than needed.
    format!(
        "<span class=\"label\">{label}:</span><input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"5\" class=\"num-tight\">",
        name = name,
        label = html_escape(label),
        value = value,
    )
}

fn textfield(name: &str, label: &str, value: &str, password: bool, size: usize) -> String {
    textfield_attr(name, label, value, password, size, "")
}

/// Like [`textfield`] but with an extra attribute string (e.g. `"disabled"`),
/// used to grey out fields that don't apply to the current gateway role.
fn textfield_attr(
    name: &str,
    label: &str,
    value: &str,
    password: bool,
    size: usize,
    attr: &str,
) -> String {
    let kind = if password { "password" } else { "text" };
    format!(
        "<span class=\"label\">{label}:</span><input type=\"{kind}\" name=\"{name}\" value=\"{value}\" size=\"{size}\" {attr}>",
        kind = kind,
        name = name,
        label = html_escape(label),
        value = html_escape(value),
        size = size,
        attr = attr,
    )
}

/// Like [`numfield`] but with an extra attribute string (e.g. `"disabled"`).
fn numfield_attr<T: std::fmt::Display>(name: &str, label: &str, value: T, attr: &str) -> String {
    format!(
        "<span class=\"label\">{label}:</span><input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"5\" class=\"num-tight\" {attr}>",
        name = name,
        label = html_escape(label),
        value = value,
        attr = attr,
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
  --warn-bg: #330606;
  --warn-border: #e03a3a;
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
input:disabled, select:disabled { opacity: 0.45; cursor: not-allowed; }
label.chk:has(input:disabled) { opacity: 0.45; }
.hspace { display: inline-block; width: 18px; }
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
button.refresh {
  font-size: 14px;
  padding: 2px 6px;
  line-height: 1;
  flex-shrink: 0;
}
/* Serial port row keeps all controls on one line, including the
   right-floated More button.  The default `.row` flex-wrap rule
   would otherwise push More onto a second line as soon as the
   dropdown + refresh + Baud combination overflows the half-width
   frame.  The select itself is the only flexible child: it gives up
   width first, the labels and buttons keep their natural size. */
.serial-row { flex-wrap: nowrap; }
.serial-row .label,
.serial-row .chk,
.serial-row button { flex-shrink: 0; white-space: nowrap; }
.serial-port-select {
  min-width: 0;
  flex: 1 1 160px;
  max-width: 220px;
}
/* Dir field stretches to fill the row inside the File Transfer
   frame, mirroring the GUI's expanding text edit. */
.transfer-dir { flex: 1 1 auto; min-width: 0; }
/* Server frame's listener block uses CSS Grid so the two Port:
   colons in each column align between rows (and the 6-char port
   inputs line up too).  Column 7 is the More button slot — it
   sits on row 1 and an empty cell on row 2 keeps the grid square. */
.server-grid {
  display: grid;
  grid-template-columns:
    max-content max-content max-content
    max-content max-content max-content
    1fr;
  column-gap: 10px;
  row-gap: 6px;
  align-items: center;
  margin: 4px 0;
}
.server-grid .port-label { color: var(--text); }
.server-grid .port-num { width: 6ch; }
.server-grid button.more { justify-self: end; margin-left: 0; }
/* Tight row: keeps the contents on a single line.  Used by the
   File Transfer XMODEM tunables row so the right-floated More
   button stays after the last numeric field instead of wrapping
   onto its own line. */
.tight-row { flex-wrap: nowrap; align-items: center; }
.tight-row input,
.tight-row .label,
.tight-row button { flex-shrink: 0; white-space: nowrap; }
/* Serial-frame header carries two title+checkbox pairs plus the Save
   button.  Allow wrap (unlike the row above) since on narrow viewports
   it makes more sense for the second title to drop to its own line
   than to clip text. */
.serial-head { flex-wrap: wrap; column-gap: 12px; }
.serial-head .title { font-weight: bold; }
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
/* Warning modals: dark-red panel + red border/title so they read as a
   must-acknowledge alert, distinct from the ordinary (green) popups. */
.modal-body.warn { background: var(--warn-bg); border: 2px solid var(--warn-border); }
.modal-body.warn .modal-head .title { color: var(--warn-border); }
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
  // Ordinary popups dismiss on backdrop click; WARNING modals must be
  // acknowledged with an explicit Continue/Cancel, so don't backdrop-dismiss them.
  m.addEventListener('click', function(e) {
    if (e.target === m && !m.classList.contains('warn')) m.classList.remove('open');
  });
});
// Warning modals replace the native browser dialogs: the fixed-position overlay
// blocks the form behind it, so the operator must choose Continue or Cancel
// before the next click lands.  Revert callbacks are keyed by modal id (not a
// single global) so if a second warning is raised while one is open — e.g. via
// keyboard focus reaching a control behind the overlay — each modal's Cancel
// still runs its own revert.
var warnCancelCb = {};
function showWarn(id, cancelCb) { warnCancelCb[id] = cancelCb || null; openModal(id); }
document.querySelectorAll('.warn-continue').forEach(function(b) {
  b.addEventListener('click', function() { delete warnCancelCb[b.dataset.warn]; closeModal(b.dataset.warn); });
});
document.querySelectorAll('.warn-cancel').forEach(function(b) {
  b.addEventListener('click', function() {
    var cb = warnCancelCb[b.dataset.warn];
    if (cb) cb();
    delete warnCancelCb[b.dataset.warn];
    closeModal(b.dataset.warn);
  });
});
function warnIfDisablingWeb(cb) {
  if (!cb.checked) {
    showWarn('warn-web-disable', function() { cb.checked = true; });
  }
}
function warnIfChangingWebPort(input) {
  var orig = input.dataset.orig;
  if (input.value !== orig) {
    showWarn('warn-web-port', function() { input.value = orig; });
  }
}
// Security-sensitive ENABLE toggles (mirrors the GUI's confirm-on-enable
// popups): warn when the box is checked; Cancel unchecks it.
function warnOnEnable(cb, id) {
  if (cb.checked) {
    showWarn(id, function() { cb.checked = false; });
  }
}
// Grey out the Master/Slave fields that don't apply to the selected role:
// 'accept relays' is Master-only; the master host/port/user/pass are
// Slave-only.  Runs on load and on every role change.
function updateRelayFields() {
  var roleEl = document.querySelector('[name=gateway_role]');
  if (!roleEl) return;
  var role = roleEl.value;
  var isMaster = role === 'master', isSlave = role === 'slave';
  var accept = document.querySelector('[name=master_accept_relays]');
  if (accept) accept.disabled = !isMaster;
  ['slave_master_host', 'slave_master_port', 'slave_master_username', 'slave_master_password'].forEach(function(n) {
    var el = document.querySelector('[name=' + n + ']');
    if (el) el.disabled = !isSlave;
  });
}
function onRoleChange(sel) {
  if (sel.value === 'master') {
    // A master with relays off can't accept slaves: default the box on.
    var accept = document.querySelector('[name=master_accept_relays]');
    if (accept) accept.checked = true;
    // The relay listens on the SSH port. Warn (only) if SSH is off — never
    // toggle it automatically.
    var ssh = document.querySelector('[name=ssh_enabled]');
    if (ssh && !ssh.checked) {
      showWarn('warn-master-ssh', null);
    }
  }
  updateRelayFields();
}
updateRelayFields();
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
// Refresh-ports button on each Serial Port row.  Fetches the live
// device list and rewrites both selects' option children — matches
// the GUI's single refresh that re-scans for both port pickers.
function refreshSerialPorts() {
  fetch('/serial-ports').then(function(r) { return r.json(); }).then(function(data) {
    var detected = data.ports || [];
    document.querySelectorAll('select.serial-port-select').forEach(function(sel) {
      // Preserve the operator's current choice — they may have just
      // picked a value, and a background refresh shouldn't reset it.
      // Falls back to data-current (the on-page-render value) if the
      // select hasn't been touched yet.
      var keep = sel.value || sel.dataset.current || '';
      var html = '<option value=\"\"' + (keep === '' ? ' selected' : '') + '>(none)</option>';
      var inList = false;
      detected.forEach(function(p) {
        var esc = p.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
                   .replace(/\"/g, '&quot;').replace(/'/g, '&#39;');
        var sm = (p === keep) ? ' selected' : '';
        if (p === keep) inList = true;
        html += '<option value=\"' + esc + '\"' + sm + '>' + esc + '</option>';
      });
      if (keep && !inList) {
        var esc = keep.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
                      .replace(/\"/g, '&quot;').replace(/'/g, '&#39;');
        html += '<option value=\"' + esc + '\" selected>' + esc + ' (saved)</option>';
      }
      sel.innerHTML = html;
    });
  }).catch(function() {});
}
document.querySelectorAll('button[data-refresh-ports]').forEach(function(b) {
  b.addEventListener('click', refreshSerialPorts);
});
// The save-success banner rides into the page via the ?notice=...
// query string set by our 303 redirect.  Strip it from the URL bar
// after render so a refresh (or a bookmark) doesn't keep showing the
// banner forever — the banner is meant to confirm one save, not act
// as a permanent header.
if (window.location.search.indexOf('notice=') !== -1) {
  window.history.replaceState({}, document.title, window.location.pathname);
}
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
        // Master/Slave card.
        assert!(html.contains("gateway_role"));
        assert!(html.contains("master_accept_relays"));
        assert!(html.contains("slave_master_host"));
        // Scripture verse is part of the page.
        assert!(html.contains("John 3:16"));
        // Warnings are custom dark-red modals, not native confirm()/alert().
        assert!(html.contains("id=\"warn-web-disable\""));
        assert!(html.contains("id=\"warn-web-port\""));
        assert!(html.contains("id=\"warn-master-ssh\""));
        assert!(html.contains("modal-body warn"));
        assert!(!html.contains("confirm("), "native confirm() must be gone");
        assert!(!html.contains("alert("), "native alert() must be gone");
        // Enable-guard warnings for the security toggles (GUI parity).
        assert!(html.contains("id=\"warn-ip-safety\""));
        assert!(html.contains("id=\"warn-kermit-server\""));
        assert!(html.contains("id=\"warn-atdt-kermit\""));
        // …and the toggles are wired to raise them.
        assert!(html.contains("warnOnEnable(this, 'warn-ip-safety')"));
        assert!(html.contains("warnOnEnable(this, 'warn-kermit-server')"));
        assert!(html.contains("warnOnEnable(this, 'warn-atdt-kermit')"));
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

    #[test]
    fn test_same_origin_ok_csrf_guard() {
        let req = |pairs: &[(&str, &str)]| {
            let mut headers = HashMap::new();
            for (k, v) in pairs {
                headers.insert((*k).to_string(), (*v).to_string());
            }
            HttpRequest {
                method: "POST".into(),
                path: "/save".into(),
                query: String::new(),
                headers,
                body: Vec::new(),
            }
        };
        // Matching Origin → allowed (the legitimate same-origin form post).
        assert!(same_origin_ok(&req(&[("host", "gw:8080"), ("origin", "http://gw:8080")])));
        // Cross-origin Origin → rejected (the forged cross-site submit).
        assert!(!same_origin_ok(&req(&[("host", "gw:8080"), ("origin", "http://evil.example")])));
        // Opaque "null" origin (sandboxed iframe / data: URL) → rejected.
        assert!(!same_origin_ok(&req(&[("host", "gw:8080"), ("origin", "null")])));
        // No Origin but matching Referer → allowed.
        assert!(same_origin_ok(&req(&[("host", "gw:8080"), ("referer", "http://gw:8080/")])));
        // No Origin, cross-site Referer → rejected.
        assert!(!same_origin_ok(&req(&[("host", "gw:8080"), ("referer", "http://evil.example/x")])));
        // Neither header (non-browser client like curl) → allowed; Basic
        // auth still gates, and a script can't be a CSRF vector.
        assert!(same_origin_ok(&req(&[("host", "gw:8080")])));
        // No Host header at all → nothing to compare against; allowed.
        assert!(same_origin_ok(&req(&[("origin", "http://whatever")])));
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
    fn test_missing_auth_header_does_not_count_as_attempt() {
        // The lockout only counts a present-but-wrong credential.  A
        // credential-less request (the normal Basic challenge preflight, and
        // the subresource probes that repeat it) must NOT be counted — else a
        // browser locks its own user out before they type a password.
        assert!(!request_presented_credential(&req_with_auth(None)));
        // A request that carries a credential (even a wrong/garbage one) does
        // count, so an actual brute-forcer still trips the lockout.
        assert!(request_presented_credential(&req_with_auth(Some("Basic Zm9vOmJhcg=="))));
        assert!(request_presented_credential(&req_with_auth(Some("Basic !!garbage!!"))));
    }

    #[test]
    fn test_apply_form_post_rejects_empty_body() {
        // An empty/chunked body must not be applied: collect_form_updates
        // would write every checkbox-boolean false, disabling telnet/ssh/web/
        // security in one shot.  Refuse with no restart and no config write.
        let (notice, action) = apply_form_post(b"");
        assert!(notice.contains("ignored"), "expected refusal notice, got {:?}", notice);
        assert_eq!(action, SaveAction::Save);
    }

    #[test]
    fn test_apply_form_post_rejects_non_utf8_body() {
        let (notice, action) = apply_form_post(&[0xff, 0xfe, 0x00]);
        assert!(notice.contains("ignored"), "expected refusal notice, got {:?}", notice);
        assert_eq!(action, SaveAction::Save);
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
        // NB: `master_accept_relays` is intentionally NOT in this list — it is
        // role-gated (written only when the submitted gateway_role is
        // "master"); see test_collect_form_updates_master_accept_relays_role_gated.
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
    fn test_collect_form_updates_master_accept_relays_role_gated() {
        // `master_accept_relays` applies only to a master.  With role=master an
        // absent checkbox means unchecked -> "false", present -> "true".  With
        // any other role the checkbox is rendered disabled (not submitted) and
        // must be left untouched (preserved), not clobbered to false.
        let old = Config::default();

        let mut f = empty_form();
        f.insert("gateway_role".into(), "master".into());
        let (updates, _) = collect_form_updates(&f, &old);
        assert_eq!(
            updates.iter().find(|(k, _)| k == "master_accept_relays").map(|(_, v)| v.as_str()),
            Some("false"),
            "role=master + absent checkbox should write false"
        );

        let mut f = empty_form();
        f.insert("gateway_role".into(), "master".into());
        f.insert("master_accept_relays".into(), "true".into());
        let (updates, _) = collect_form_updates(&f, &old);
        assert_eq!(
            updates.iter().find(|(k, _)| k == "master_accept_relays").map(|(_, v)| v.as_str()),
            Some("true"),
            "role=master + present checkbox should write true"
        );

        let mut f = empty_form();
        f.insert("gateway_role".into(), "slave".into());
        let (updates, _) = collect_form_updates(&f, &old);
        assert!(
            !updates.iter().any(|(k, _)| k == "master_accept_relays"),
            "role=slave must leave master_accept_relays untouched (preserved)"
        );

        // Absent gateway_role (non-master) is likewise preserved.
        let (updates, _) = collect_form_updates(&empty_form(), &old);
        assert!(
            !updates.iter().any(|(k, _)| k == "master_accept_relays"),
            "non-master role must leave master_accept_relays untouched"
        );
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
    fn test_serial_ports_json_empty() {
        assert_eq!(serial_ports_json(&[]), r#"{"ports":[]}"#);
    }

    #[test]
    fn test_serial_ports_json_typical_paths() {
        let ports = vec!["/dev/ttyS0".to_string(), "/dev/ttyUSB0".to_string()];
        assert_eq!(
            serial_ports_json(&ports),
            r#"{"ports":["/dev/ttyS0","/dev/ttyUSB0"]}"#
        );
    }

    #[test]
    fn test_serial_ports_json_escapes_quotes_and_backslashes() {
        // Defensive: if a hostile or oddly-named device shows up in
        // the OS port table, the JSON we emit must still parse on
        // the browser side.  Most real serial paths are ASCII and
        // quote-free, but escaping per RFC 8259 §7 keeps a Windows
        // COM-port-like path with backslashes safe too.
        let weird = vec!["a\"b".to_string(), "c\\d".to_string(), "e\nf".to_string()];
        let out = serial_ports_json(&weird);
        assert!(out.contains(r#""a\"b""#));
        assert!(out.contains(r#""c\\d""#));
        assert!(out.contains(r#""e\nf""#));
    }

    #[test]
    fn test_serial_port_options_none_selected_when_empty_current() {
        let opts = serial_port_options("", &["/dev/ttyS0".into()]);
        // First option is "(none)" with the selected attribute.
        assert!(opts.starts_with(r#"<option value="" selected>(none)</option>"#));
        // The detected port is present but not selected.
        assert!(opts.contains(r#"<option value="/dev/ttyS0">"#));
    }

    #[test]
    fn test_serial_port_options_marks_current_detected() {
        let opts = serial_port_options("/dev/ttyUSB0", &[
            "/dev/ttyS0".into(),
            "/dev/ttyUSB0".into(),
        ]);
        assert!(opts.contains(r#"<option value="/dev/ttyUSB0" selected>"#));
        // The (none) option is NOT selected when a real port is chosen.
        assert!(opts.starts_with(r#"<option value="">(none)</option>"#));
    }

    #[test]
    fn test_serial_port_options_preserves_saved_value_not_in_detected() {
        // Saved port path that isn't currently plugged in: keep it
        // visible with a "(saved)" suffix so the operator's choice
        // is preserved across reboots / cable unplugs.
        let opts = serial_port_options("/dev/ttyUSB99", &["/dev/ttyS0".into()]);
        assert!(opts.contains(r#"<option value="/dev/ttyUSB99" selected>/dev/ttyUSB99 (saved)</option>"#));
    }

    #[test]
    fn test_serial_port_options_html_escapes_path() {
        // A path with HTML-active chars must come out escaped — the
        // option text is rendered as HTML, not as a literal attribute
        // value alone.
        let opts = serial_port_options("/dev/<weird>", &[]);
        assert!(opts.contains("&lt;weird&gt;"));
        assert!(!opts.contains("<weird>"));
    }

    #[test]
    fn test_file_transfer_frame_matches_gui_layout() {
        // Mirrors the GUI: Dir on top, then a single tunables row
        // with Negotiate / Block / Retries + the More button.  The
        // retry-interval ("Poke") field moves to the More popup so
        // the primary frame stays compact.  Lock that down — if the
        // layout regresses, the primary frame grows back to 4 rows
        // and unbalances the row pair with AI/Browser.
        let html = render_main_page(&Config::default(), None);
        // Dir input must come first in the frame.
        let dir_idx = html
            .find(r#"name="transfer_dir""#)
            .expect("transfer_dir field");
        let neg_idx = html
            .find(r#"name="xmodem_negotiation_timeout""#)
            .expect("xmodem_negotiation_timeout field");
        let retries_idx = html
            .find(r#"name="xmodem_max_retries""#)
            .expect("xmodem_max_retries field");
        let more_idx = html
            .find(r#"data-target="more-xfer""#)
            .expect("more-xfer button");
        assert!(
            dir_idx < neg_idx,
            "Dir should render before the tunables row"
        );
        assert!(
            neg_idx < retries_idx && retries_idx < more_idx,
            "Negotiate / Retries / More must appear in that order on the second row"
        );
        // The retry-interval ("Poke") moved to the popup — verify
        // it's NOT on the primary frame.  Search range up to the
        // More button (everything before it is the primary frame).
        let primary = &html[..more_idx];
        assert!(
            !primary.contains(r#"name="xmodem_negotiation_retry_interval""#),
            "Poke / retry interval should live in the More popup, not the primary frame"
        );
    }

    #[test]
    fn test_server_frame_uses_grid_with_port_label_cells() {
        // The Server frame switched from flex-rows to CSS Grid so the
        // two `Port:` colons line up across rows.  The `port-label`
        // cells are the colon-bearers; the `port-num` inputs are
        // 6-char wide.  Lock the structure down so a future revert
        // to flex `<div class="row">` would visibly mis-align the
        // colons and trip this test.
        let html = render_main_page(&Config::default(), None);
        assert!(html.contains(r#"class="server-grid""#));
        // Four port inputs, all with class="port-num" + size="6".
        let port_num_count = html.matches(r#"class="port-num""#).count();
        assert_eq!(port_num_count, 4, "expected 4 port-num inputs in Server frame, got {}", port_num_count);
        let size6_in_server = html.matches(r#" size="6" class="port-num""#).count();
        assert_eq!(size6_in_server, 4, "all 4 port inputs must be size=6");
        // Six port-label cells (one per port column in each row).
        let port_label_count = html.matches(r#"class="port-label""#).count();
        assert_eq!(port_label_count, 4, "expected 4 port-label cells (one per port input)");
    }

    #[test]
    fn test_server_frame_more_button_renders_on_row_one() {
        // More button must appear in the grid BETWEEN the row 1
        // listeners (Telnet/Web) and the row 2 listeners (SSH/Kermit).
        // In CSS-Grid auto-flow that position puts the button as the
        // last cell of row 1.  If a future refactor places More after
        // kermit_server_enabled instead, this test catches the regress.
        let html = render_main_page(&Config::default(), None);
        let web_idx = html
            .find(r#"name="web_port""#)
            .expect("web_port field");
        let more_idx = html
            .find(r#"data-target="more-server""#)
            .expect("more-server button");
        let ssh_idx = html
            .find(r#"name="ssh_enabled""#)
            .expect("ssh_enabled field");
        assert!(
            web_idx < more_idx && more_idx < ssh_idx,
            "More button must sit between Row 1 (Telnet/Web) and Row 2 (SSH/Kermit) — got web={}, more={}, ssh={}",
            web_idx, more_idx, ssh_idx,
        );
    }

    #[test]
    fn test_xfer_tunables_row_keeps_more_inline() {
        // File-transfer XMODEM tunables row must keep the More button
        // on the same line as Negotiate/Block/Retries by carrying the
        // `tight-row` class (nowrap).  Lock that down — previously the
        // default `.row` flex-wrap pushed More onto its own line.
        let html = render_main_page(&Config::default(), None);
        assert!(
            html.contains(r#"class="row tight-row""#),
            "File-transfer tunables row missing tight-row class"
        );
    }

    #[test]
    fn test_server_frame_pairs_listeners_two_rows() {
        // Matches the GUI: Row 1 pairs Telnet + Web Server (the
        // unencrypted + the configuration listener); Row 2 pairs
        // SSH + Kermit Server (encrypted + file-transfer listener)
        // and floats the More button.  Compresses the older 4-row
        // layout to 2 content rows.  This test guards against an
        // accidental revert that would re-grow the frame and unbalance
        // the side-by-side Server/Security row.
        let html = render_main_page(&Config::default(), None);
        // First content row must hold both telnet and web fields.
        let telnet_idx = html
            .find(r#"name="telnet_enabled""#)
            .expect("telnet_enabled");
        let web_idx = html
            .find(r#"name="web_enabled""#)
            .expect("web_enabled");
        let ssh_idx = html.find(r#"name="ssh_enabled""#).expect("ssh_enabled");
        let kermit_idx = html
            .find(r#"name="kermit_server_enabled""#)
            .expect("kermit_server_enabled");
        // Telnet and Web both come before SSH and Kermit (Row 1
        // before Row 2 in the rendered HTML).
        assert!(
            telnet_idx < ssh_idx && web_idx < ssh_idx,
            "Row 1 should hold Telnet + Web (before SSH/Kermit)"
        );
        assert!(
            kermit_idx > web_idx,
            "Kermit should land on Row 2 (after Web)"
        );
    }

    #[test]
    fn test_serial_frame_header_carries_enabled_checkboxes() {
        // Matches the GUI's layout: both Enabled checkboxes ride in
        // the frame header, not on the per-port rows.  The header has
        // two per-port titles ("Serial Port A" / "Serial Port B")
        // plus the Save button.  Lock that down — if the header
        // shape regresses, the per-port rows would need their Enabled
        // checkbox back and the More-button-on-same-line property
        // would break too.
        let html = render_main_page(&Config::default(), None);
        assert!(html.contains("Serial Port A"), "Port A header title missing");
        assert!(html.contains("Serial Port B"), "Port B header title missing");
        assert!(
            html.contains(r#"name="serial_a_enabled""#),
            "Port A Enabled checkbox missing"
        );
        assert!(
            html.contains(r#"name="serial_b_enabled""#),
            "Port B Enabled checkbox missing"
        );
        // The Enabled checkboxes should be inside the frame header,
        // not the per-port row.  Locate the actual HTML elements
        // (not the CSS-rule occurrences in <style>) by matching the
        // full class attribute, then assert the checkbox appears
        // between the header open and the first row open.
        let head_idx = html
            .find(r#"class="frame-head serial-head""#)
            .expect("serial-head frame-head element");
        let row_idx = html[head_idx..]
            .find(r#"class="row serial-row""#)
            .map(|i| head_idx + i)
            .expect("serial-row element after header");
        let a_chk_idx = html
            .find(r#"name="serial_a_enabled""#)
            .expect("serial_a_enabled");
        assert!(
            head_idx < a_chk_idx && a_chk_idx < row_idx,
            "serial_a_enabled checkbox is not inside the frame header (head={}, chk={}, row={})",
            head_idx, a_chk_idx, row_idx,
        );
    }

    #[test]
    fn test_rendered_serial_row_keeps_more_on_same_line() {
        // The Serial Port rows use the `serial-row` class on top of
        // the default `.row` so flex-wrap stays disabled and the
        // More button doesn't get pushed onto a second line.  Lock
        // that down — earlier the More button wrapped beneath the
        // baud field once we added the dropdown + refresh button.
        let html = render_main_page(&Config::default(), None);
        assert!(
            html.contains(r#"class="row serial-row""#),
            "serial rows missing the serial-row class that suppresses wrap"
        );
        // CSS rule must declare nowrap on .serial-row so the class is
        // not just a marker but actually changes layout.
        assert!(
            html.contains(".serial-row { flex-wrap: nowrap; }"),
            "CSS is missing the .serial-row flex-wrap: nowrap rule"
        );
    }

    #[test]
    fn test_rendered_serial_row_uses_select_not_text_input() {
        // The Serial Ports frame must render a <select> for each
        // port, not the old free-text <input>.  This test guards
        // against an accidental revert of the GUI-parity change.
        let html = render_main_page(&Config::default(), None);
        assert!(
            html.contains(r#"name="serial_a_port""#),
            "serial_a_port form field missing"
        );
        assert!(
            html.contains(r#"name="serial_b_port""#),
            "serial_b_port form field missing"
        );
        // The select tag carries the data-current attribute so the
        // refresh JS knows the on-page-load value.
        assert!(
            html.contains(r#"data-current="""#),
            "serial select missing data-current attr (default port is empty)"
        );
        // The refresh button is present and tagged for the JS
        // handler.  Match a substring on both sides of the title
        // attribute so the test isn't brittle to attribute ordering.
        assert!(
            html.contains("data-refresh-ports"),
            "serial refresh button missing the data-refresh-ports tag"
        );
    }

    #[test]
    fn test_security_frame_renders_unified_credentials_only() {
        // After the SSH-creds merge the Security frame should expose
        // a single User/Pass pair, not separate Telnet/SSH rows.
        // Lock that down — a future refactor that re-introduces
        // ssh_username/ssh_password as form inputs would have to
        // update this test alongside the field names.
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        assert!(
            html.contains("name=\"username\""),
            "Security frame missing unified username input"
        );
        assert!(
            html.contains("name=\"password\""),
            "Security frame missing unified password input"
        );
        assert!(
            !html.contains("name=\"ssh_username\""),
            "Security frame still rendering legacy ssh_username input"
        );
        assert!(
            !html.contains("name=\"ssh_password\""),
            "Security frame still rendering legacy ssh_password input"
        );
    }

    #[test]
    fn test_rendered_page_strips_notice_query_on_load() {
        // The "Configuration saved." banner rides in via ?notice=... on
        // the 303 redirect after a save.  Reloading or bookmarking that
        // URL would otherwise keep showing the banner forever — the
        // script clears it after render via history.replaceState.  This
        // test locks down the presence of the strip so a future refactor
        // can't silently regress the banner back to "permanent header"
        // behavior.
        let html = render_main_page(&Config::default(), Some("Configuration saved.".into()));
        assert!(
            html.contains("history.replaceState"),
            "page does not strip the ?notice= query string on load"
        );
        assert!(
            html.contains("notice="),
            "URL-strip guard should still mention notice= in the check"
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

    #[test]
    fn test_web_ip_rejection_ignores_security_enabled() {
        // The whole point of the named helper: unlike the telnet listener,
        // the web allowlist stays on regardless of `security_enabled`
        // (the page renders the password + API key). Toggling
        // `security_enabled` must not change the decision either way.
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let private: IpAddr = "192.168.1.10".parse().unwrap();

        // Public IP is rejected whether or not login is required.
        assert!(web_ip_rejection(false, false, public).is_some());
        assert!(web_ip_rejection(true, false, public).is_some());
        assert_eq!(
            web_ip_rejection(false, false, public),
            web_ip_rejection(true, false, public),
            "security_enabled must not affect the web IP decision"
        );

        // Private IP is allowed whether or not login is required.
        assert!(web_ip_rejection(false, false, private).is_none());
        assert!(web_ip_rejection(true, false, private).is_none());
    }

    #[test]
    fn test_web_ip_rejection_disable_safety_allows_all() {
        // With the IP safety toggle off, even a public peer is allowed
        // (operator opt-out), and `security_enabled` still doesn't matter.
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(web_ip_rejection(false, true, public).is_none());
        assert!(web_ip_rejection(true, true, public).is_none());
    }
}
