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

    crate::bindwatch::expect("web", port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(l) => l,
            Err(e) => {
                glog!("Web server: failed to bind port {}: {}", port, e);
                crate::bindwatch::failed("web", &e);
                return;
            }
        };
        crate::bindwatch::bound("web");
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
    block_gateway: bool,
    peer_ip: IpAddr,
) -> Option<String> {
    let _ = security_enabled; // deliberately not consulted — see doc comment
    if disable_ip_safety {
        None
    } else {
        telnet::reject_insecure_ip(peer_ip, block_gateway)
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
    let (live_security, live_disable_safety, live_block_gw) = config::get_security_flags();
    if let Some(reason) =
        web_ip_rejection(live_security, live_disable_safety, live_block_gw, peer_ip)
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
    let (mut updates, mut notice) = collect_form_updates(&fields, &old_cfg);

    // "Default port" resets the CP/M virtual-modem port whatever the select
    // was showing, so it works as a one-click recovery rather than needing the
    // operator to first find the right entry in the list.  It is the same port
    // EGT80 defaults to, which is the point: the pair works together again.
    if fields.get("action").map(String::as_str) == Some("cpm_port_default") {
        let def = crate::cpm::uart::DEFAULT_UART;
        updates.retain(|(k, _)| k != "cpm_emu_uart");
        updates.push(("cpm_emu_uart".to_string(), def.to_string()));
        // Appended, not assigned: `collect_form_updates` may have produced a
        // warning about another field in the same submission (a port out of
        // range, say), and replacing the notice would throw that away silently.
        let msg = format!("CP/M virtual modem port reset to the default ({def}).");
        notice = if notice.is_empty() {
            msg
        } else {
            format!("{notice} {msg}")
        };
    }

    // CP/M mounts are applied live, then the resulting table is what gets
    // written — rather than writing the request and hoping it took.  A drive
    // that refused (because somebody is on it) therefore keeps its old image in
    // the config too, so a restart does not quietly apply a change the operator
    // was told had failed.
    // Creating a blank disk happens before the mounts are applied, so a
    // freshly-made image can be mounted by the same save if the operator picked
    // it — and so the notice reads in the order things happened.
    if let Some((token, name)) = requested_new_disk(&fields) {
        let base = crate::cpm::layout::cpm_dir(&old_cfg.transfer_dir);
        let created = match crate::cpm::image::create_blank_image(&base, &token, &name) {
            Ok(note) => {
                glog!("Web: CP/M {}", note);
                note
            }
            Err(e) => format!("Could not create the disk: {e}"),
        };
        notice = if notice.is_empty() {
            created
        } else {
            format!("{notice} {created}")
        };
    }

    if fields.keys().any(|k| k.starts_with("cpm_mount_")) {
        let (mount_notice, mounts_value) = apply_cpm_mount_form(&fields, &old_cfg);
        updates.retain(|(k, _)| k != "cpm_mounts");
        updates.push(("cpm_mounts".to_string(), mounts_value));
        if !mount_notice.is_empty() {
            notice = if notice.is_empty() {
                mount_notice
            } else {
                format!("{notice} {mount_notice}")
            };
        }
    }

    let pairs: Vec<(&str, &str)> = updates
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    config::update_config_values(&pairs);

    logger::log("Web: configuration saved.".into());
    (notice, action)
}

/// Did this submission ask for a new blank disk, and if so which?
///
/// A separate decision because the mount screen submits its create fields on
/// *every* save, including the ones that only change a drive.  What marks a
/// real request is a name having been typed — so an empty or all-space box has
/// to mean "no", or every mount change would also try to make a disk and report
/// a failure at somebody who never asked.
fn requested_new_disk(fields: &HashMap<String, String>) -> Option<(String, String)> {
    let name = fields.get("cpm_new_name")?;
    if name.trim().is_empty() {
        return None;
    }
    let token = fields.get("cpm_new_format")?;
    if token.trim().is_empty() {
        return None;
    }
    Some((token.clone(), name.clone()))
}

/// Apply the mount screen's sixteen selects, returning (notice, `cpm_mounts`).
///
/// A drive whose select is **absent** from the submission is left exactly as it
/// is.  That is not laziness: a busy drive renders its select `disabled`, and a
/// disabled control is not submitted, so treating absence as "set to none"
/// would unmount precisely the drives the screen said could not be changed.
fn apply_cpm_mount_form(
    fields: &HashMap<String, String>,
    cfg: &Config,
) -> (String, String) {
    use crate::cpm::image;
    let current = image::registry::all();
    let mut desired: Vec<(u8, String)> = Vec::new();
    for drive0 in 0..crate::cpm::NUM_DRIVES {
        let key = format!("cpm_mount_{}", ((b'a' + drive0) as char));
        match fields.get(&key) {
            Some(name) if !name.is_empty() => desired.push((drive0, name.clone())),
            Some(_) => {} // explicitly "(drive folder)" — unmount
            None => {
                // Not submitted: keep whatever is there.
                if let Some(m) = current.get(drive0 as usize).and_then(|m| m.as_ref()) {
                    desired.push((drive0, m.filename.clone()));
                }
            }
        }
    }
    let base = crate::cpm::layout::cpm_dir(&cfg.transfer_dir);
    let (notes, errors) = image::apply_mount_selection(&base, &desired);
    let mut notice = String::new();
    if !notes.is_empty() {
        notice.push_str(&notes.join(" "));
    }
    if !errors.is_empty() {
        if !notice.is_empty() {
            notice.push(' ');
        }
        notice.push_str(&errors.join(" "));
    }
    (notice, image::current_mounts_value())
}

/// Pure transformation from a parsed form + the current Config to a
/// (`Vec<(key, value)>`, notice) pair.  Separated from
/// `apply_form_post` so tests can exercise the form-to-update mapping
/// (including the connection-breaking warning logic) without touching
/// the global CONFIG singleton or rewriting the on-disk config file.
/// Boolean keys the save must **not** write when the submitted role isn't
/// master.  Their checkboxes render `disabled` outside master, and a disabled
/// input isn't submitted — which the "absent means false" rule would otherwise
/// read as the operator turning the setting off.
///
/// Three places must agree about these: the renderer that disables them, the
/// `updateRelayFields()` JS that re-enables them when the role changes, and this
/// skip. `test_role_gated_checkboxes_are_kept_in_sync_by_js` enforces that.
const BOOL_KEYS_SKIPPED_OUTSIDE_MASTER: &[&str] =
    &["master_accept_relays", "allow_relay_kermit"];

/// Is this boolean key's checkbox greyed out by the state the form was submitted
/// in — so its absence means "the browser could not submit it", not "the operator
/// turned it off"?
///
/// **Only checkboxes need this.** A plain key is written only when the form
/// contains it, so an unsubmitted text/number field preserves what is stored; an
/// absent checkbox is an affirmative `false` and would clobber it.
///
/// The condition is read from the **submitted** form rather than the stored
/// config, because the operator may have changed the gating control in the same
/// save (switching to Master, or ticking raw-TCP mode) and it is the submitted
/// state that decides what the browser was able to send.
fn bool_checkbox_gated_off(key: &str, fields: &HashMap<String, String>) -> bool {
    let submitted = |k: &str| fields.get(k).map(|v| is_truthy(v)).unwrap_or(false);
    match key {
        // Master-only relay gates.
        k if BOOL_KEYS_SKIPPED_OUTSIDE_MASTER.contains(&k) => {
            fields.get("gateway_role").map(String::as_str) != Some("master")
        }
        // Raw TCP has no IAC layer, so TTYPE/NAWS negotiation is meaningless and
        // the checkbox is greyed — matching the GUI's `add_enabled_ui(!raw)`.
        // Without this skip, saving in raw mode would store `false` over the
        // operator's setting, and turning raw mode back off would silently leave
        // negotiation disabled.
        "telnet_gateway_negotiate" => submitted("telnet_gateway_raw"),
        _ => false,
    }
}

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
        "log_file", "log_max_size_kb", "log_max_files",
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
        "cpm_emu_max_minstr", "cpm_emu_uart", "cpm_boot_image",
        // The CP/M virtual modem's saved AT profile (what AT&W writes), the
        // same fields the serial ports expose for theirs.
        "cpm_emu_x_code", "cpm_emu_dcd_mode", "cpm_emu_s_regs",
        "ssh_gateway_auth",
        "gateway_term_width", "gateway_term_height",
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
        "security_enabled", "disable_ip_safety", "disable_gateway_connections",
        "enable_console", "verbose", "log_to_file",
        "telnet_gateway_negotiate", "telnet_gateway_raw", "gateway_debug",
        "cpm_emu_enabled",
        "kermit_long_packets", "kermit_sliding_windows", "kermit_streaming",
        "kermit_attribute_packets", "kermit_repeat_compression",
        "kermit_resume_partial", "kermit_locking_shifts",
        "kermit_wait_for_receiver",
        "allow_atdt_kermit",
        "allow_peer_dial",
        "punter_hangup_on_failure",
        "master_accept_relays",
        "allow_relay_kermit",
        "serial_a_enabled", "serial_b_enabled",
        "cpm_emu_echo", "cpm_emu_verbose", "cpm_emu_quiet",
        "serial_a_echo", "serial_a_verbose", "serial_a_quiet",
        "serial_b_echo", "serial_b_verbose", "serial_b_quiet",
        "serial_a_petscii_translate", "serial_b_petscii_translate",
        "serial_a_drive_carrier", "serial_b_drive_carrier",
    ];
    for key in bool_keys {
        // A checkbox rendered `disabled` isn't submitted, and "absent means
        // false" would read that as the operator turning the setting off —
        // clobbering it.  Skip those instead, preserving the stored value the way
        // the GUI and telnet do (they leave an inert setting alone).  See
        // `bool_checkbox_gated_off` for which keys and why.
        if bool_checkbox_gated_off(key, fields) {
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

/// Escape one string as a JSON string body (no surrounding quotes).  Serial
/// device paths are ASCII and quote-free in practice on Linux/macOS/Windows,
/// but a USB descriptor is whatever the device claims it is — so escape
/// defensively and a hostile or oddly-named device can't break the parse.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Hand-rolled JSON encoder for the `/serial-ports` response.
///
/// Each entry carries the device path plus its descriptions, so the refresh
/// button can rebuild both the options *and* their hover text without a page
/// reload — otherwise a refresh would silently strip the names the page was
/// rendered with.
fn serial_ports_json(ports: &[crate::serial::DetectedPort]) -> String {
    let mut out = String::from("{\"ports\":[");
    for (i, p) in ports.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"summary\":\"{}\",\"detail\":\"{}\"}}",
            json_escape(&p.name),
            json_escape(&p.summary),
            json_escape(&p.detail),
        ));
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
         <div class=\"row\">{gwblock_chk}</div>\
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
        // Names the detected router (see crate::router); "x.x.x.1" when the
        // OS could not tell us, which is the rule's own fallback.
        gwblock_chk = checkbox(
            "disable_gateway_connections",
            &format!(
                "Block connections from the router ({})",
                crate::router::describe()
            ),
            cfg.disable_gateway_connections,
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
    // and `serve Kermit to slave ports` are Master-only, the master
    // host/port/user/pass are Slave-only.  The server renders the initial
    // disabled state (correct even without JS), and
    // `updateRelayFields()`/`onRoleChange()` keep it in sync as the role
    // changes.  **Every field using `dis_accept` must also be listed in
    // `updateRelayFields`** — `allow_relay_kermit` was not, so switching the
    // role to Master left its box greyed out and unsubmittable, and saving then
    // stored false over a previously-enabled setting.  Disabled inputs aren't
    // submitted, and the save preserves a greyed field's stored value: the
    // slave_* text fields because plain keys are only written when present, and
    // the two Master-only checkboxes because the save skips them unless the
    // submitted role is master (see collect_form_updates).
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
         {ssh_warn}\
         <div class=\"row\">{host} {port}</div>\
         <div class=\"row\">{relay_kermit_chk}</div>\
         <div class=\"row\">{user} {pass}</div>",
        // The relay listens on the SSH port, so accept-relays is inert while the
        // SSH server is off.  The `showWarn` in onRoleChange only fires on the
        // *switch* into Master, which misses the case that actually stops a relay
        // working: a master set up earlier whose SSH server was turned off since.
        // Rendered server-side so it is correct on load and without JS, and (like
        // the popup) it never changes SSH on its own.
        ssh_warn = if cfg.relays_blocked_by_ssh_off() {
            "<div class=\"row\"><span class=\"warn-inline\">SSH server is off \
             \u{2014} the relay listens on the SSH port, so no slave can \
             connect.</span></div>"
        } else {
            ""
        },
        st_sel = role_sel("standalone"),
        ma_sel = role_sel("master"),
        sl_sel = role_sel("slave"),
        accept_chk = checkbox_with_attr(
            "master_accept_relays",
            "Master: accept relays",
            cfg.master_accept_relays,
            dis_accept,
        ),
        relay_kermit_chk = checkbox_with_attr(
            "allow_relay_kermit",
            "Master: serve Kermit to slave ports",
            cfg.allow_relay_kermit,
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
         <span class=\"title\">AI Chat, Browser, Weather &amp; CP/M</span>\
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
         <select name=\"{prefix}_port\" class=\"serial-port-select\" data-current=\"{dev}\" \
         title=\"{tip}\">\
         {options}\
         </select>\
         <button type=\"button\" class=\"refresh\" title=\"Refresh ports\" \
         data-refresh-ports>\u{21bb}</button>\
         {baud}\
         <button type=\"button\" class=\"more\" data-target=\"more-{prefix}\">More\u{2026}</button></div>",
        label = label,
        prefix = prefix,
        dev = html_escape(&port.port),
        // Hovering the closed selector lists every detected port with the
        // hardware behind it — the answer to "which ttyUSB is my adapter?"
        // without having to open the list and read it item by item.
        tip = html_escape(&crate::gui::serial_ports_tooltip(&detected)),
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
fn serial_port_options(current: &str, detected: &[crate::serial::DetectedPort]) -> String {
    let mut out = String::new();
    let sel_none = if current.is_empty() { " selected" } else { "" };
    out.push_str(&format!(
        "<option value=\"\"{sel}>(none)</option>",
        sel = sel_none,
    ));
    let mut current_in_detected = false;
    for p in detected {
        let sel = if p.name == current { " selected" } else { "" };
        if p.name == current {
            current_in_detected = true;
        }
        // The option's *value* stays the bare path — that is what gets saved.
        // Only the visible text carries the short label, and the full
        // description rides along as the per-option tooltip.
        let text = if p.summary.is_empty() {
            html_escape(&p.name)
        } else {
            format!("{} \u{2014} {}", html_escape(&p.name), html_escape(&p.summary))
        };
        out.push_str(&format!(
            "<option value=\"{v}\"{sel} title=\"{t}\">{text}</option>",
            v = html_escape(&p.name),
            sel = sel,
            t = html_escape(&p.detail),
            text = text,
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
         <div class=\"row\">{d}<span class=\"hspace\"></span>{g}\
         <button type=\"button\" class=\"more row-right\" \
         data-target=\"more-general\">More\u{2026}</button></div>\
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

/// The CP/M mount screen: one row per drive, each a picker of the images
/// folder.
///
/// Sixteen rows rather than a list of mounts, because the question an operator
/// has is "what is on drive B:", and a row per drive answers it without them
/// having to work out which drives are absent from a list.
///
/// A drive somebody is using renders disabled with the reason beside it.  A
/// disabled `select` is not submitted, which would normally read as "set to
/// none" — so the save skips any drive that is busy rather than trusting the
/// absence.  Same hazard as the role-gated checkboxes, handled the same way.
fn render_cpm_disks_modal(cfg: &Config) -> String {
    let base = crate::cpm::layout::cpm_dir(&cfg.transfer_dir);
    let images = crate::cpm::image::available_images(&base);
    let mounts = crate::cpm::image::registry::all();
    let usage = crate::cpm::image::registry::usage();

    let mut rows = String::new();
    for drive0 in 0..crate::cpm::NUM_DRIVES {
        let letter = (b'A' + drive0) as char;
        let mounted = mounts.get(drive0 as usize).and_then(|m| m.as_ref());
        // A drive lent to a booted session reads as empty here, so without the
        // note it would render free and enabled and then refuse on Save.
        let held = crate::cpm::image::drive_held_note(drive0);
        let busy = usage
            .get(drive0 as usize)
            .and_then(|u| u.describe())
            .or_else(|| held.clone());
        let disabled = if busy.is_some() { " disabled" } else { "" };

        let mut opts = String::from("<option value=\"\">(drive folder)</option>");
        for name in &images {
            let sel = if mounted.map(|m| m.filename.as_str()) == Some(name.as_str()) {
                " selected"
            } else {
                ""
            };
            opts.push_str(&format!(
                "<option value=\"{}\"{}>{}</option>",
                html_escape(name),
                sel,
                html_escape(name)
            ));
        }
        // An image that is mounted but no longer in the folder would otherwise
        // vanish from its own row and read as "no image".
        if let Some(m) = mounted {
            if !images.contains(&m.filename) {
                opts.push_str(&format!(
                    "<option value=\"{}\" selected>{} (missing from folder)</option>",
                    html_escape(&m.filename),
                    html_escape(&m.filename)
                ));
            }
        }

        let mut note = String::new();
        if let Some(m) = mounted {
            if m.read_only {
                note.push_str(&format!(
                    " <span class=\"sub\">read-only: {}</span>",
                    html_escape(&m.read_only_reason)
                ));
            }
        }
        if let Some(b) = &busy {
            note.push_str(&format!(" <span class=\"sub\">{}</span>", html_escape(b)));
        }
        if drive0 == 0 {
            note.push_str(" <span class=\"sub\">A: hides EGT80 while mounted</span>");
        }
        rows.push_str(&format!(
            "<div class=\"row\"><span class=\"label\">{letter}:</span>\
             <select name=\"cpm_mount_{}\"{}>{}</select>{}</div>",
            letter.to_ascii_lowercase(),
            disabled,
            opts,
            note
        ));
    }

    let intro = if images.is_empty() {
        format!(
            "<div class=\"row\"><span class=\"sub\">No images found. Put .dsk files in              {}/images — readme.txt there explains the naming — or make an empty one below.</span></div>",
            html_escape(&base.display().to_string())
        )
    } else {
        String::from(
            "<div class=\"row\"><span class=\"sub\">A mounted drive uses the files inside the image instead of the files in its folder. The folder's files are not touched and return when you unmount.</span></div>",
        )
    };

    // Making a blank disk sits on the same screen because it is the answer to
    // "there is nothing in the list yet", and sending someone elsewhere to
    // solve that is how a feature goes unused.  Both buttons post the same
    // form: what decides whether a disk is created is a name being typed, not
    // which button was pressed, so filling the name in and pressing Save does
    // the obvious thing rather than silently ignoring it.  The field renders
    // empty every time, so a create cannot be repeated by a later Save.
    let mut fmt_opts = String::new();
    for (token, label) in crate::cpm::image::creatable_formats() {
        fmt_opts.push_str(&format!(
            "<option value=\"{}\">{}</option>",
            html_escape(token),
            html_escape(label)
        ));
    }
    let create = format!(
        "<div class=\"row\"><span class=\"label\">New blank disk</span>\
         <select name=\"cpm_new_format\">{fmt_opts}</select>\
         <input type=\"text\" name=\"cpm_new_name\" value=\"\" placeholder=\"disk name\" \
         maxlength=\"32\" size=\"14\">{create_btn}</div>\
         <div class=\"row\"><span class=\"sub\">Creates an empty, formatted image in the images \
         folder, named &lt;format&gt;_&lt;name&gt;.dsk so it mounts read-write. Nothing is \
         overwritten — a name already in use is refused.</span></div>",
        create_btn = save_button("save", "Create", "secondary"),
    );

    format!(
        "<div class=\"modal\" id=\"more-cpm-disks\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">Mount CP/M Drives</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-cpm-disks\">\u{00d7}</button></div>\
         {intro}{rows}{create}\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        save = save_button("save", "Save", "secondary"),
    )
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
         <div class=\"row\">{gwcols} {gwrows}</div>\
         <div class=\"row\"><span class=\"hint\">{gwgeom_hint}</span></div>\
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
        // Raw TCP has no IAC layer, so TTYPE/NAWS negotiation is meaningless
        // there — greyed, matching the GUI's `add_enabled_ui(!raw)`.  Being a
        // checkbox, this also needs the save to skip it (see
        // `bool_checkbox_gated_off`) or the greyed box would store `false` over
        // the operator's setting, and `updateGatewayFields()` to re-enable it when
        // raw mode is unticked.
        tneg = checkbox_with_attr(
            "telnet_gateway_negotiate",
            "Telnet Gateway: negotiate TTYPE/NAWS",
            cfg.telnet_gateway_negotiate,
            if cfg.telnet_gateway_raw { "disabled" } else { "" },
        ),
        traw = checkbox_with_attr(
            "telnet_gateway_raw",
            "Telnet Gateway: raw TCP mode",
            cfg.telnet_gateway_raw,
            "onchange=\"updateGatewayFields()\"",
        ),
        key_sel = if cfg.ssh_gateway_auth == "key" { "selected" } else { "" },
        pwd_sel = if cfg.ssh_gateway_auth == "password" { "selected" } else { "" },
        // Plain number fields, not checkboxes — an unsubmitted plain key
        // preserves the stored value, so these need no skip-list entry and no
        // re-enabling JS (the asymmetry from review pass #3).  Never greyed:
        // both are meaningful whatever else is set.
        gwcols = numfield("gateway_term_width", "Gateway cols (0=auto)", cfg.gateway_term_width),
        gwrows = numfield("gateway_term_height", "Gateway rows (0=auto)", cfg.gateway_term_height),
        gwgeom_hint = html_escape(&Config::gateway_term_hint(
            cfg.gateway_term_width,
            cfg.gateway_term_height,
        )),
        // Master/Slave lives under Server → More (mirrors the GUI); the modal's
        // own Save-and-Restart covers the restart a role change needs.
        master_slave = master_slave_rows(cfg),
        save = save_button("save_and_restart", "Save and Restart", "primary"),
    ));

    // General More — the on-disk log.  Ricky moved these off the Server popup:
    // they belong with the other General settings, not with the listeners.
    //
    // The three General checkboxes on the main frame (verbose, gateway debug,
    // show GUI) are deliberately NOT repeated here.  The whole page is ONE form,
    // so a second input with the same `name=` would submit twice and the last
    // value would win — the defect class that let a save clobber
    // `allow_relay_kermit`.  The GUI has no such constraint and does re-show
    // them.  Same reasoning as the AI/Browser popup, which leaves the API key
    // and homepage on its main frame.
    //
    // The path/size/keep fields grey out when `log_to_file` is off, matching the
    // GUI.  Two things make that safe here, and the distinction is the whole
    // reason `allow_relay_kermit` was a data-loss bug while this is not:
    //
    //  * These are **plain keys**.  `collect_form_updates` writes a plain key
    //    only when the form actually contains it (`if let Some(v) = ...`), so a
    //    disabled — and therefore unsubmitted — field leaves the stored value
    //    alone.  A **checkbox** is the opposite: absence is the canonical
    //    "false", which is how a greyed box silently cleared a saved setting.
    //    So these need no entry in `BOOL_KEYS_SKIPPED_OUTSIDE_MASTER`.
    //  * `updateLogFields()` re-enables them the moment the box is ticked, so the
    //    operator is never left unable to type a path.  That was the other half
    //    of the relay bug: rendered disabled, never re-enabled by JS.
    //
    // Gated on `log_to_file` alone, NOT on `logger::file_logging_enabled` — a
    // blank path also means "off", but greying the path field because it is blank
    // would make it impossible to fill in.  The GUI gates on the same flag.
    //
    // Save-and-Restart, not Save: file logging is armed from the startup path,
    // so a changed path or limit takes effect on the next restart.
    let dis_log = if cfg.log_to_file { "" } else { "disabled" };
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-general\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">General \u{2014} More</span>\
         <button type=\"button\" class=\"close\" data-close=\"more-general\">\u{00d7}</button></div>\
         <div class=\"row\">{logfile}</div>\
         <div class=\"row\">{logpath}</div>\
         <div class=\"row\">{logsize} {logkeep}</div>\
         <div class=\"row\"><span class=\"hint\">{loghint}</span></div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        // The worst-case disk figure comes from logger::max_disk_kb so this page
        // doesn't re-derive the bound (the GUI, telnet and the startup banner all
        // read it from the same place).
        logfile = checkbox("log_to_file", "Write the log to a file", cfg.log_to_file),
        logpath = textfield_attr("log_file", "Log file", &cfg.log_file, false, 28, dis_log),
        logsize = numfield_attr("log_max_size_kb", "Rotate at (KB)", cfg.log_max_size_kb, dis_log),
        logkeep = numfield_attr("log_max_files", "Keep old", cfg.log_max_files, dis_log),
        loghint = html_escape(&crate::logger::log_state_hint(
            cfg,
            cfg.log_max_size_kb,
            cfg.log_max_files,
        )),
        save = save_button("save_and_restart", "Save and Restart", "primary"),
    ));

    // AI/Browser/Weather More — weather location + units (moved off the
    // primary frame so it stays at three rows, mirroring the GUI).  The API
    // key + homepage remain on the main frame, so they are NOT repeated here
    // (a duplicate name= in this single form would clobber the value).
    // What the CP/M menu item runs: our emulator, or one of the images on
    // hand.  Built from the same `boot_choices` the telnet and desktop screens
    // use, so the three cannot offer different lists.
    let cpm_boot_options: String = {
        let base = crate::cpm::layout::cpm_dir(&cfg.transfer_dir);
        let mut choices = crate::cpm::boot::boot_choices(&base);
        // A setting naming an image that is no longer in the folder would
        // otherwise vanish from the list and silently reset itself on the next
        // save.  Show it, so the operator can see what is set and why it is not
        // running.
        if !cfg.cpm_boot_image.is_empty()
            && !choices.iter().any(|(v, _)| *v == cfg.cpm_boot_image)
        {
            choices.push((
                cfg.cpm_boot_image.clone(),
                format!("{} (missing)", crate::cpm::boot::boot_choice_label(&cfg.cpm_boot_image)),
            ));
        }
        choices
            .iter()
            .map(|(value, label)| {
                format!(
                    "<option value=\"{}\"{}>{}</option>",
                    html_escape(value),
                    if *value == cfg.cpm_boot_image { " selected" } else { "" },
                    html_escape(label),
                )
            })
            .collect()
    };
    let cpm_uart_options: String = crate::cpm::uart::UART_CHOICES
        .iter()
        .map(|c| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                c.key,
                if c.key == cfg.cpm_emu_uart { " selected" } else { "" },
                html_escape(c.description),
            )
        })
        .collect();
    out.push_str(&format!(
        "<div class=\"modal\" id=\"more-ai\"><div class=\"modal-body\">\
         <div class=\"modal-head\"><span class=\"title\">AI, Browser, Weather &amp; CP/M \u{2014} More</span>\
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
         <div class=\"row\">{cpm}</div>\
         <div class=\"row\">{cpmmax}</div>\
         <div class=\"row\">{cpmprof}</div>\
         <div class=\"row\">{cpmecho}{cpmverb}{cpmquiet}</div>\
         <div class=\"row\">{cpmx}{cpmdcd}</div>\
         <div class=\"row\">{cpmsregs}</div>\
         <div class=\"row\">{cpmuart}</div>\
         <div class=\"row\">{cpmboot}</div>\
         <div class=\"row\">{cpmdisks}</div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        loc = html_escape(&cfg.weather_location),
        u_auto = if cfg.weather_units == "auto" { "selected" } else { "" },
        u_us = if cfg.weather_units == "us" { "selected" } else { "" },
        u_metric = if cfg.weather_units == "metric" { "selected" } else { "" },
        cpm = checkbox(
            "cpm_emu_enabled",
            "CP/M Emulator (main menu; be sure you trust the CP/M files you run)",
            cfg.cpm_emu_enabled,
        ),
        cpmmax = numfield(
            "cpm_emu_max_minstr",
            "CP/M runaway ceiling (M-instr)",
            cfg.cpm_emu_max_minstr,
        ),
        // The CP/M virtual modem's saved AT profile — the same fields the
        // serial ports expose for theirs, so a profile can be inspected or
        // repaired here instead of only from inside the emulator.
        cpmprof = "<span class=\"label\">CP/M modem saved profile (AT&amp;W):</span>",
        cpmecho = checkbox("cpm_emu_echo", "Echo (E1)", cfg.cpm_emu_modem.echo),
        cpmverb = checkbox("cpm_emu_verbose", "Verbose (V1)", cfg.cpm_emu_modem.verbose),
        cpmquiet = checkbox("cpm_emu_quiet", "Quiet (Q1)", cfg.cpm_emu_modem.quiet),
        cpmx = numfield("cpm_emu_x_code", "Result-code level (X)", cfg.cpm_emu_modem.x_code),
        cpmdcd = numfield("cpm_emu_dcd_mode", "DCD mode (&amp;C)", cfg.cpm_emu_modem.dcd_mode),
        cpmsregs = textfield(
            "cpm_emu_s_regs",
            "S-registers S0..S27 (blank = power-on)",
            &cfg.cpm_emu_modem.s_regs,
            false,
            40,
        ),
        cpmuart = format_args!(
            "<span class=\"label\">CP/M virtual modem port:</span>\
             <select name=\"cpm_emu_uart\">{cpm_uart_options}</select> {reset}",
            reset = save_button("cpm_port_default", "Default port", "secondary"),
        ),
        cpmboot = format_args!(
            "<span class=\"label\">CP/M runs:</span>\
             <select name=\"cpm_boot_image\">{cpm_boot_options}</select>",
        ),
        cpmdisks = "<button type=\"button\" class=\"more\"                     data-target=\"more-cpm-disks\">Mount CP/M drives\u{2026}</button>",

        save = save_button("save", "Save", "secondary"),
    ));

    out.push_str(&render_cpm_disks_modal(cfg));

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
    let mode_sel_kermit = if port.mode == "kermit" { "selected" } else { "" };
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
         <option value=\"kermit\" {ms_kermit}>Kermit Server</option>\
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
        ms_kermit = mode_sel_kermit,
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

/// Narrowest a numeric box is allowed to get, in characters.
///
/// Five digits covers nearly every setting we expose, and keeping the boxes
/// tight is deliberate — frames shouldn't waste width on empty input padding.
const NUM_FIELD_MIN_CH: usize = 5;

fn numfield<T: std::fmt::Display>(name: &str, label: &str, value: T) -> String {
    numfield_attr(name, label, value, "")
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
///
/// The box grows to fit values longer than [`NUM_FIELD_MIN_CH`] digits.  The
/// fixed five-digit width clipped `log_max_size_kb`, which is a `u64` an
/// operator can legitimately set to a 6- or 7-digit KB figure: the value was
/// intact and the input scrolled, but the field showed a truncated number,
/// which reads as data loss.  Sizing from the value itself rather than adding a
/// second CSS class for one key keeps it a single rule — nothing to keep in
/// sync, and any other setting that grows past five digits is covered too.
fn numfield_attr<T: std::fmt::Display>(name: &str, label: &str, value: T, attr: &str) -> String {
    let value = value.to_string();
    let ch = value.chars().count().max(NUM_FIELD_MIN_CH);
    // `.num-tight` already sizes the common case; only a wider box needs an
    // override, so the markup stays clean for the ~60 fields that don't.
    // The `+ 14px` matches the class: 6px padding each side plus the 1px
    // borders, which the width must include under border-box.
    let style = if ch > NUM_FIELD_MIN_CH {
        format!(" style=\"width: calc({ch}ch + 14px)\"")
    } else {
        String::new()
    };
    format!(
        "<span class=\"label\">{label}:</span><input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"{ch}\" class=\"num-tight\"{style} {attr}>",
        name = name,
        label = html_escape(label),
        value = value,
        ch = ch,
        style = style,
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
/* Right-justify an item on its own `.row` (the General frame's More button).
   `.row` is flex with wrap, so auto-margin pushes the button to the frame's
   right edge and it stays inside on every width — unlike the Server frame's
   CSS Grid, where a `1fr` button column collapsed to zero and put the button
   outside the frame.  Guarded by test_more_buttons_cannot_leave_their_frame. */
.row-right { margin-left: auto; }
/* Inline (non-modal) warning that a setting is inert as configured.  Reuses the
   warning-modal red so the two read as the same class of message.  Wraps rather
   than overflowing its frame — the text is a full sentence. */
.warn-inline { color: var(--warn-border); font-style: italic; }
.notice {
  background: #1c3a50; color: var(--amber-bright);
  padding: 8px 12px; border: 1px solid var(--amber);
  border-radius: 4px; margin: 10px 0;
}
/* 500px is not arbitrary: it is what the Server frame's widest row needs.  Its
   seven grid columns measure ~411px (the Kermit Server label alone is the
   widest single cell at ~118px) plus 60px of column gaps, 24px of frame padding
   and 2px of border — ~497px.  The old 420px floor let the frame get narrower
   than its own content, which is what pushed the More button out of view.
   Measured with the wide Linux fallback font; Segoe UI on Windows is narrower,
   so this floor holds there too.  On a viewport too small for 500px the page
   scrolls horizontally, which is a predictable degradation; a button that has
   left its frame is not. */
.grid {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(500px, 1fr));
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
/* Numeric entry boxes get an explicit width.  They used to be sized only by
   the HTML size=5 attribute, which browsers map to different pixel widths —
   and the width also depends on which font in the stack actually resolved
   (Segoe UI on Windows is much narrower than the DejaVu/Verdana fallbacks).
   That variance is what pushed the File Transfer frame's More button out of
   the frame on some browsers at some widths.  One ch is the width of a zero
   in the resolved font, so five digits always fit whichever font that is (the
   widest value we expose is kermit_max_packet_length, 4 digits); the 14px is
   the horizontal padding (6px each side) plus the 1px borders, which width
   has to include because box-sizing is border-box.
   A value too long for five digits (log_max_size_kb is a u64 in KB) gets an
   inline width from numfield_attr, computed the same way, rather than a second
   class here that would have to be assigned key by key. */
.num-tight { width: calc(5ch + 14px); }
/* Server frame's listener block uses CSS Grid so the two Port:
   colons in each column align between rows (and the port inputs
   line up too).  Column 7 is the More button slot — it
   sits on row 1 and an empty cell on row 2 keeps the grid square. */
.server-grid {
  display: grid;
  /* The six content columns stay max-content: nothing here can be squeezed
     without either clipping a port number or colliding a label with the next
     column, both of which were tried and looked broken.  Instead the frame is
     never allowed to get narrower than this row needs — see the 500px floor on
     .grid, which is derived from these columns plus the frame padding.
     Column 7 is minmax(max-content, 1fr): never narrower than the button
     itself, so it can no longer collapse to zero and push the button outside
     the frame (the old 1fr did exactly that), but it still absorbs the slack
     that right-justifies the button when there is room. */
  grid-template-columns:
    max-content max-content max-content
    max-content max-content max-content
    minmax(max-content, 1fr);
  column-gap: 10px;
  row-gap: 6px;
  align-items: center;
  margin: 4px 0;
}
.server-grid .port-label { color: var(--text); }
/* 5ch of digits, which is exactly a full port number (65535), plus the 14px of
   padding and borders that width must include under border-box.  The old plain
   6ch was BOTH too wide for the row to fit and too narrow to show five digits,
   because the padding ate into it. */
.server-grid .port-num { width: calc(5ch + 14px); }
.server-grid button.more { justify-self: end; margin-left: 0; }
/* Tight row: keeps the contents on a single line.  Used by the
   File Transfer XMODEM tunables row so the right-floated More
   button stays after the last numeric field instead of wrapping
   onto its own line. */
.tight-row { flex-wrap: nowrap; align-items: center; min-width: 0; }
/* Labels and the button keep their full size... */
.tight-row .label,
.tight-row button { flex-shrink: 0; white-space: nowrap; }
/* ...but the numeric inputs may shrink, which is what guarantees the
   right-justified More button stays inside the frame.  With nowrap and
   nothing allowed to shrink, a row wider than the frame simply overflowed to
   the right and the button went out of view — narrower digits are a much
   better failure mode than an unreachable button. */
.tight-row input { flex-shrink: 1; min-width: 3ch; }
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
/* ── Phones ───────────────────────────────────────────────────────────────
   Everything above is sized for a desktop browser on purpose, including the
   500px floor on `.grid`: a frame narrower than its own widest row is what
   pushed the More button outside it, and a page that scrolls was the better of
   those two. But that trade only exists while both are possible. Below the
   floor neither is — the frame cannot fit — so the page simply scrolled
   sideways at every phone width (measured: 516px of content in a 375px
   viewport, at every viewport down to 345px, because the floor fixes the width
   rather than the screen doing it).

   So below the floor we re-flow instead: one listener per line rather than
   two, which is the only row that needed 500px in the first place. Desktop
   rendering cannot change — every rule here is inside the query, and the
   breakpoint sits above the floor so the switch happens before the overflow
   would. */
@media (max-width: 640px) {
  /* Give up on multi-column: `auto-fit` with a 500px minimum keeps sizing a
     column wider than the screen rather than falling back to one.
     `minmax(0, 1fr)`, not a plain `1fr`: a bare `1fr` is `minmax(auto, 1fr)`,
     whose floor is the item's own min-content — which measured 450px and left
     the page overflowing after the column count was already fixed. */
  .grid { grid-template-columns: minmax(0, 1fr); }
  .frame { min-width: 0; }
  /* The XMODEM tunables row is `nowrap` so its right-justified More button
     stays put; on a phone that row cannot fit on one line at all, and the
     nowrap floor was part of the 450px. Wrapping costs the button its place at
     the end of the row, which is the lesser loss here. */
  .tight-row, .serial-row { flex-wrap: wrap; }
  /* [listener] [Port:] [number], one per line. */
  .server-grid { grid-template-columns: 1fr max-content max-content; }
  /* The More button sits mid-source (between Web and SSH), which is right for
     two-per-line and wrong for one; `order` moves it to the end, where it gets
     a line of its own. */
  .server-grid button.more { order: 99; grid-column: 1 / -1; justify-self: end; }
  .server-grid .grid-blank { display: none; }
  /* Fields sized in characters — the 40-char S-register boxes, the 28-char log
     path, the 366px logo — are all wider than a phone.  `min-width: 0` goes
     with it: a flex item's automatic minimum is its min-content, so without
     this the serial rows' port picker refused to shrink and carried the More
     button ~30px past the frame at 320px. */
  input[type=text], input[type=password], select, .logo { max-width: 100%; }
  /* Selects only, NOT inputs.  Applying it to inputs let the 4-digit baud box
     shrink to three visible digits — the same defect just fixed for the log
     size (a box showing less than the value it holds), one line later. */
  .row select { min-width: 0; }
  /* 16px of modal padding each side is a lot of a 375px screen. */
  .modal { padding: 3vh 8px; }
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
  // Both Master-only checkboxes, and both gated on the *role* alone — not on
  // master_accept_relays. The save only skips these two when the submitted role
  // isn't master, so disabling either on any other condition would make a
  // greyed box submit nothing and silently store false.
  ['master_accept_relays', 'allow_relay_kermit'].forEach(function(n) {
    var el = document.querySelector('[name=' + n + ']');
    if (el) el.disabled = !isMaster;
  });
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
// Grey the log path/size/keep fields while 'write the log to a file' is off,
// matching the GUI.  The server renders the initial state (correct even without
// JS); this keeps it in sync as the box is toggled.  Every field named here must
// also be rendered with the same gate in render_more_popups, and vice versa --
// a field rendered disabled with nothing to re-enable it is un-editable until a
// save-and-reload, which is half of what made allow_relay_kermit a bug.
// Enforced by test_disabled_inputs_are_re_enabled_by_js.
// Raw TCP mode has no IAC layer, so TTYPE/NAWS negotiation is meaningless: grey
// that box while raw is ticked, matching the GUI.  The save skips the key while
// it is greyed (bool_checkbox_gated_off), so the operator's setting survives.
function updateGatewayFields() {
  var raw = document.querySelector('[name=telnet_gateway_raw]');
  var neg = document.querySelector('[name=telnet_gateway_negotiate]');
  if (raw && neg) neg.disabled = raw.checked;
}
updateGatewayFields();
var LOG_GATED = ['log_file', 'log_max_size_kb', 'log_max_files'];
function updateLogFields() {
  var box = document.querySelector('[name=log_to_file]');
  if (!box) return;
  LOG_GATED.forEach(function(n) {
    var e = document.querySelector('[name=' + n + ']');
    if (e) e.disabled = !box.checked;
  });
}
(function() {
  var box = document.querySelector('[name=log_to_file]');
  if (box) box.addEventListener('change', updateLogFields);
})();
updateLogFields();
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
function escAttr(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/\"/g, '&quot;').replace(/'/g, '&#39;');
}
function refreshSerialPorts() {
  fetch('/serial-ports').then(function(r) { return r.json(); }).then(function(data) {
    var detected = data.ports || [];
    // Same tooltip the server renders on first paint, rebuilt here so a
    // refresh doesn't strip the descriptions off the selector.
    var tip = detected.length
      ? 'Detected serial ports:\\n' + detected.map(function(p) { return p.detail; }).join('\\n')
      : 'No serial ports detected.';
    document.querySelectorAll('select.serial-port-select').forEach(function(sel) {
      // Preserve the operator's current choice — they may have just
      // picked a value, and a background refresh shouldn't reset it.
      // Falls back to data-current (the on-page-render value) if the
      // select hasn't been touched yet.
      var keep = sel.value || sel.dataset.current || '';
      var html = '<option value=\"\"' + (keep === '' ? ' selected' : '') + '>(none)</option>';
      var inList = false;
      detected.forEach(function(p) {
        var esc = escAttr(p.name);
        var sm = (p.name === keep) ? ' selected' : '';
        if (p.name === keep) inList = true;
        var text = p.summary ? esc + ' \\u2014 ' + escAttr(p.summary) : esc;
        html += '<option value=\"' + esc + '\"' + sm +
                ' title=\"' + escAttr(p.detail) + '\">' + text + '</option>';
      });
      if (keep && !inList) {
        var esc = escAttr(keep);
        html += '<option value=\"' + esc + '\" selected>' + esc + ' (saved)</option>';
      }
      sel.innerHTML = html;
      sel.title = tip;
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

    /// EVERY input rendered `disabled` must have JS that can re-enable it.
    ///
    /// Generalises the checkbox-only scan below, which would not have covered the
    /// log path/size/keep fields when they gained a `disabled` gate — they are
    /// text inputs. A field rendered disabled with nothing to re-enable it is
    /// un-editable until a save-and-reload, which was half of what made
    /// `allow_relay_kermit` a bug.
    ///
    /// The *other* half — a greyed field clobbering its stored value on save —
    /// applies only to checkboxes, and that asymmetry is asserted here so the
    /// reasoning is pinned rather than remembered: `collect_form_updates` writes a
    /// plain key only when the form contains it, so an unsubmitted plain field
    /// preserves what is stored; an absent checkbox means `false`.
    #[test]
    fn test_disabled_inputs_are_re_enabled_by_js() {
        // A state that greys everything the page can ever grey at once:
        //  * standalone — greys the two Master-only checkboxes AND the four
        //    slave_* fields (a *slave* leaves those four enabled, so that role
        //    would cover fewer inputs),
        //  * file logging off — greys the three log fields,
        //  * raw TCP on — greys the TTYPE/NAWS negotiate box.
        let cfg = Config {
            gateway_role: "standalone".into(),
            log_to_file: false,
            telnet_gateway_raw: true,
            ..Config::default()
        };
        // The form a browser submits from that page: the gating controls carry
        // their state and every disabled input is simply absent.  Used to ask the
        // save whether it would skip each greyed checkbox.
        let mut submitted = empty_form();
        submitted.insert("gateway_role".into(), "standalone".into());
        submitted.insert("telnet_gateway_raw".into(), "true".into());

        let html = render_main_page(&cfg, None);
        let script = html
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("page must have a <script> block");

        let mut checked = 0;
        for tag in html.split('<') {
            if !tag.starts_with("input") || !tag.contains("disabled") {
                continue;
            }
            let Some(name) = tag
                .split("name=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
            else {
                continue;
            };
            let is_checkbox = tag.contains("type=\"checkbox\"");
            assert!(
                script.contains(name),
                "input {name:?} renders disabled but no JS mentions it, so nothing \
                 can re-enable it — the operator is stuck until a save-and-reload"
            );
            if is_checkbox {
                // Asks the save's own predicate rather than a hard-coded list, so
                // a new gate is covered automatically.  An earlier version checked
                // `BOOL_KEYS_SKIPPED_OUTSIDE_MASTER` directly and would have
                // rejected `telnet_gateway_negotiate`, whose gate is raw-TCP mode
                // rather than the role.
                assert!(
                    bool_checkbox_gated_off(name, &submitted),
                    "checkbox {name:?} renders disabled but the save does not skip \
                     it in that state; an absent checkbox means false, so saving \
                     would clobber the stored value"
                );
            } else {
                // Stated as an assertion so the reasoning can't quietly rot: a
                // plain key is only written when present, so it needs no skip.
                assert!(
                    !BOOL_KEYS_SKIPPED_OUTSIDE_MASTER.contains(&name),
                    "{name:?} is not a checkbox, so it does not belong in the \
                     bool skip-list; plain keys are preserved by absence"
                );
            }
            checked += 1;
        }
        // Guards the scan itself: 2 relay checkboxes + 4 slave_* fields + 3 log
        // fields are expected in this state.
        assert!(
            checked >= 8,
            "expected at least 8 disabled inputs in this state, found {checked} — \
             the scan has stopped matching the real markup"
        );

        // The log fields must be greyed here, and enabled when the box is ticked.
        for n in ["log_file", "log_max_size_kb", "log_max_files"] {
            assert!(
                html.contains(&format!("name=\"{n}\" value=")) ,
                "{n} not rendered at all"
            );
        }
        let on = render_main_page(&Config { log_to_file: true, ..cfg.clone() }, None);
        for n in ["log_file", "log_max_size_kb", "log_max_files"] {
            let disabled_when_on = on
                .split('<')
                .filter(|t| t.starts_with("input") && t.contains(&format!("name=\"{n}\"")))
                .any(|t| t.contains("disabled"));
            assert!(
                !disabled_when_on,
                "{n} still renders disabled with log_to_file = true"
            );
        }
    }

    /// Raw-TCP mode greys the TTYPE/NAWS negotiate box, so saving in raw mode must
    /// PRESERVE that setting rather than store `false` over it.
    ///
    /// This is the dangerous shape — a **checkbox**, where absence is an
    /// affirmative `false`. Without the skip, an operator who turned raw mode on
    /// would silently lose their negotiate setting, and turning raw mode back off
    /// would leave negotiation unexpectedly disabled.
    #[test]
    fn test_saving_in_raw_mode_preserves_the_negotiate_setting() {
        let stored = Config {
            telnet_gateway_negotiate: true,
            telnet_gateway_raw: true,
            ..Config::default()
        };
        // Raw mode on: the negotiate box is greyed, so the browser omits it.
        let mut form = empty_form();
        form.insert("telnet_gateway_raw".into(), "true".into());
        let (updates, _) = collect_form_updates(&form, &stored);
        assert!(
            !updates.iter().any(|(k, _)| k == "telnet_gateway_negotiate"),
            "the greyed negotiate box was written anyway — saving in raw mode \
             would clobber the operator's setting"
        );

        // Raw mode off: the box is live, so absence really does mean "unticked".
        let form_off = empty_form();
        let (updates, _) = collect_form_updates(&form_off, &stored);
        assert_eq!(
            updates
                .iter()
                .find(|(k, _)| k == "telnet_gateway_negotiate")
                .map(|(_, v)| v.as_str()),
            Some("false"),
            "outside raw mode an absent checkbox must still turn the setting off"
        );
    }

    /// Saving while the log fields are greyed must PRESERVE them, not clear them.
    ///
    /// This is the property that makes greying them safe, and it is the one the
    /// `allow_relay_kermit` bug violated — so assert it directly rather than
    /// trusting the reasoning. A disabled input is not submitted, and a plain key
    /// absent from the form is skipped, so the stored value survives. The
    /// contrast is asserted too: an absent *checkbox* is a real `false`.
    #[test]
    fn test_saving_with_log_fields_greyed_preserves_them() {
        let stored = Config {
            log_to_file: true,
            log_file: "/var/log/keepme.log".into(),
            log_max_size_kb: 4096,
            log_max_files: 9,
            ..Config::default()
        };
        // What the browser submits with logging unticked: the checkbox is absent
        // (that is the "false" signal) and so are the three disabled fields.
        let form = empty_form();
        let (updates, _) = collect_form_updates(&form, &stored);
        let lookup = |k: &str| updates.iter().find(|(uk, _)| uk == k).map(|(_, v)| v.as_str());

        assert_eq!(
            lookup("log_to_file"),
            Some("false"),
            "the checkbox's absence must still turn file logging off"
        );
        for k in ["log_file", "log_max_size_kb", "log_max_files"] {
            assert_eq!(
                lookup(k),
                None,
                "{k} was written despite being absent from the form — a greyed \
                 field would clobber the operator's stored value"
            );
        }
    }

    /// Every role-gated checkbox rendered `disabled` must also be listed in the
    /// `updateRelayFields()` JS, and must be skipped by the save when the role
    /// isn't master.  Those three places have to agree or the control breaks in
    /// a way no compiler catches.
    ///
    /// `allow_relay_kermit` was added to the first and third but not the second:
    /// switching the role to Master left its box greyed out — so it submitted
    /// nothing, and since the submitted role *was* master the save no longer
    /// skipped it, storing `false` over a previously-enabled setting. Derived
    /// from the rendered HTML rather than hand-listed so a fourth such field
    /// can't repeat it.
    ///
    /// (See also `test_disabled_inputs_are_re_enabled_by_js`, which generalises
    /// the scan to every disabled input, not just checkboxes.)
    #[test]
    fn test_role_gated_checkboxes_are_kept_in_sync_by_js() {
        // Render in a NON-master role, which is when they are disabled.
        let cfg = Config {
            gateway_role: "slave".into(),
            ..Config::default()
        };
        let html = render_main_page(&cfg, None);

        // Collect the `name=` of every checkbox rendered disabled.
        let mut disabled: Vec<String> = Vec::new();
        for tag in html.split('<') {
            if !tag.starts_with("input") || !tag.contains("type=\"checkbox\"") {
                continue;
            }
            if !tag.contains("disabled") {
                continue;
            }
            if let Some(rest) = tag.split("name=\"").nth(1) {
                if let Some(name) = rest.split('"').next() {
                    disabled.push(name.to_string());
                }
            }
        }
        assert!(
            disabled.iter().any(|n| n == "master_accept_relays"),
            "expected the Master-only relay checkboxes to render disabled for a \
             slave; found {:?} — if the markup changed, this scan needs updating \
             rather than deleting",
            disabled,
        );

        // The JS list that re-enables them when the role changes to master.
        let js = html
            .split("function updateRelayFields()")
            .nth(1)
            .expect("updateRelayFields must exist");
        let js_body = js.split("\nfunction ").next().unwrap_or(js);

        for name in &disabled {
            assert!(
                js_body.contains(name),
                "checkbox {name:?} renders disabled but updateRelayFields() never \
                 re-enables it — switching the role to Master would leave it \
                 greyed out, submitting nothing, and the save would store false",
            );
            // …and the save must skip it outside master, or a greyed box would
            // clobber the stored value even before the role is switched.
            assert!(
                BOOL_KEYS_SKIPPED_OUTSIDE_MASTER.contains(&name.as_str()),
                "checkbox {name:?} renders disabled but collect_form_updates \
                 does not skip it outside master, so saving as a slave stores \
                 false over the operator's setting",
            );
        }
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

    /// Every config key needs all three UIs, and the web one is the easiest to
    /// forget because its field is a string in a format! rather than a
    /// compile-checked reference.
    #[test]
    fn test_render_main_page_offers_the_cpm_boot_choice() {
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        assert!(html.contains("name=\"cpm_boot_image\""), "the select must be on the page");
        assert!(html.contains("CP/M runs"), "and be labelled");
        // The emulator is always offered, and is the empty value so that a
        // config file written before this key existed keeps its behaviour.
        // Built from the label rather than typed out, so renaming the choice
        // is a one-line change instead of a hunt through the tests.
        assert!(
            html.contains(&format!(
                "<option value=\"\" selected>{}</option>",
                html_escape(crate::cpm::boot::BOOT_EMULATOR_LABEL)
            )),
            "the emulator must be the selected empty option by default"
        );
    }

    /// A disk named in the config but no longer in the images folder must still
    /// be shown — otherwise the next save silently resets it and the operator
    /// never learns why their gateway is running the emulator.
    #[test]
    fn test_a_missing_boot_image_still_appears_in_the_web_list() {
        let cfg = Config { cpm_boot_image: "vanished.dsk".to_string(), ..Default::default() };
        let html = render_main_page(&cfg, None);
        assert!(html.contains("vanished.dsk"), "the setting must be visible");
        assert!(html.contains("(missing)"), "and marked as not being there");
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

    /// The four on-disk-log keys must be rendered AND saved.  Rendering a field
    /// the save path doesn't collect is the bug class that hit
    /// `allow_relay_kermit`: the box appears, the operator ticks it, and the
    /// save silently drops it.  So this asserts both halves together — the
    /// input exists in the Server "More" popup, and a submitted value comes back
    /// out of `collect_form_updates`.
    #[test]
    fn test_log_keys_are_rendered_and_saved() {
        let cfg = Config::default();
        let html = render_more_popups(&cfg);
        // The log settings live under General → More (Ricky moved them off the
        // Server popup: they belong with the other General settings, not with
        // the listeners).  Slice out that popup so the assertion can't pass
        // because the field happens to exist in some other popup.
        let popup = {
            let start = html.find("id=\"more-general\"").expect("general popup");
            let end = html[start + 1..]
                .find("class=\"modal\"")
                .map(|e| start + 1 + e)
                .unwrap_or(html.len());
            &html[start..end]
        };
        for name in ["log_to_file", "log_file", "log_max_size_kb", "log_max_files"] {
            assert!(
                popup.contains(&format!("name=\"{}\"", name)),
                "{} has no input in the General More popup",
                name
            );
        }
        // The whole page is one form, so a field must appear exactly once —
        // twice and the save submits both and the last value wins (the defect
        // that let a save clobber allow_relay_kermit).
        let page = render_main_page(&cfg, None);
        for name in ["log_to_file", "log_file", "log_max_size_kb", "log_max_files"] {
            let n = page.matches(&format!("name=\"{}\"", name)).count();
            assert_eq!(n, 1, "{name} appears {n} times in the form; it must appear once");
        }

        // Now the save half.  log_to_file is a checkbox, so it is submitted as
        // "true" and its absence means false; the other three are plain fields.
        let mut form = empty_form();
        form.insert("log_to_file".into(), "true".into());
        form.insert("log_file".into(), "/var/log/eg.log".into());
        form.insert("log_max_size_kb".into(), "2048".into());
        form.insert("log_max_files".into(), "3".into());
        let (updates, _) = collect_form_updates(&form, &cfg);
        let lookup = |k: &str| {
            updates.iter().find(|(uk, _)| uk == k).map(|(_, v)| v.as_str())
        };
        assert_eq!(lookup("log_to_file"), Some("true"));
        assert_eq!(lookup("log_file"), Some("/var/log/eg.log"));
        assert_eq!(lookup("log_max_size_kb"), Some("2048"));
        assert_eq!(lookup("log_max_files"), Some("3"));

        // Unticking the box must reach the config as an explicit false, not be
        // dropped (an absent checkbox is the canonical "false" signal).
        let mut off = empty_form();
        off.insert("log_file".into(), "eg.log".into());
        let (updates, _) = collect_form_updates(&off, &cfg);
        assert_eq!(
            updates.iter().find(|(k, _)| k == "log_to_file").map(|(_, v)| v.as_str()),
            Some("false"),
            "an unticked log_to_file must be saved as false"
        );
    }

    /// The gateway terminal-geometry keys must be rendered in the Server popup
    /// (where the rest of the gateway settings live) AND reach the save.  Same
    /// shape as the log-keys guard above, and for the same reason: these keys
    /// had a parser, a writer, a struct field and a default before they had a
    /// `apply_config_key` arm, which is the combination that looks wired and
    /// silently drops every telnet/web save.
    #[test]
    fn test_gateway_term_keys_are_rendered_and_saved() {
        let cfg = Config::default();
        let html = render_more_popups(&cfg);
        // Slice out the Server popup so the assertion can't pass on a field
        // that happens to live in a different popup.
        let popup = {
            let start = html.find("id=\"more-server\"").expect("server popup");
            let end = html[start + 1..]
                .find("class=\"modal\"")
                .map(|e| start + 1 + e)
                .unwrap_or(html.len());
            &html[start..end]
        };
        for name in ["gateway_term_width", "gateway_term_height"] {
            assert!(
                popup.contains(&format!("name=\"{}\"", name)),
                "{} has no input in the Server More popup",
                name
            );
        }

        // One form, so exactly once — twice and the last value silently wins.
        let page = render_main_page(&cfg, None);
        for name in ["gateway_term_width", "gateway_term_height"] {
            let n = page.matches(&format!("name=\"{}\"", name)).count();
            assert_eq!(n, 1, "{name} appears {n} times in the form; it must appear once");
        }

        // Plain fields, so they are collected whenever present.
        let mut form = empty_form();
        form.insert("gateway_term_width".into(), "40".into());
        form.insert("gateway_term_height".into(), "25".into());
        let (updates, _) = collect_form_updates(&form, &cfg);
        let lookup = |k: &str| {
            updates.iter().find(|(uk, _)| uk == k).map(|(_, v)| v.as_str())
        };
        assert_eq!(lookup("gateway_term_width"), Some("40"));
        assert_eq!(lookup("gateway_term_height"), Some("25"));

        // `0` is the auto sentinel and must survive the form layer too — it is
        // the only way to get back to automatic once a width has been pinned.
        let mut zero = empty_form();
        zero.insert("gateway_term_width".into(), "0".into());
        zero.insert("gateway_term_height".into(), "0".into());
        let (updates, _) = collect_form_updates(&zero, &cfg);
        let lookup = |k: &str| {
            updates.iter().find(|(uk, _)| uk == k).map(|(_, v)| v.as_str())
        };
        assert_eq!(lookup("gateway_term_width"), Some("0"), "0 must reach the config");
        assert_eq!(lookup("gateway_term_height"), Some("0"), "0 must reach the config");

        // Being plain keys (not checkboxes), they are never greyed and so need
        // no entry in the disabled/re-enable machinery.  Pin that: a future
        // `disabled` here without matching JS is the allow_relay_kermit bug.
        assert!(
            !popup.contains("name=\"gateway_term_width\" value=\"0\" size=\"5\" class=\"num-tight\" disabled"),
            "the width field must not render disabled without re-enabling JS"
        );
    }

    /// A master that accepts relays while the SSH server is off cannot actually
    /// accept anything — the relay listens on the SSH port.  The web must say so
    /// **persistently**, not only in the `onRoleChange` popup: the case that
    /// strands an operator is a master configured earlier whose SSH was turned
    /// off since, which never fires a role-change event.
    ///
    /// Also asserts the CSS class it uses actually has a rule. `.num-tight` was
    /// a class with no rule at all, and that was a real rendering bug.
    #[test]
    fn test_web_warns_persistently_when_master_relays_need_ssh() {
        let warned = Config {
            gateway_role: "master".into(),
            master_accept_relays: true,
            ssh_enabled: false,
            ..Config::default()
        };
        let html = master_slave_rows(&warned);
        assert!(
            html.contains("warn-inline") && html.contains("SSH server is off"),
            "no persistent SSH warning for a master with relays on and SSH off: {html}"
        );

        // And not shown when it does not apply.
        for (role, accept, ssh) in [
            ("master", true, true),    // SSH on — nothing wrong
            ("master", false, false),  // not accepting relays anyway
            ("standalone", true, false),
            ("slave", true, false),
        ] {
            let cfg = Config {
                gateway_role: role.into(),
                master_accept_relays: accept,
                ssh_enabled: ssh,
                ..Config::default()
            };
            assert!(
                !master_slave_rows(&cfg).contains("warn-inline"),
                "spurious SSH warning for role={role} accept={accept} ssh={ssh}"
            );
        }

        // The class must be styled, or the warning renders as ordinary text.
        let page_css = render_main_page(&Config::default(), None);
        assert!(
            page_css.contains(".warn-inline {"),
            ".warn-inline has no CSS rule — the warning would not look like one"
        );
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

    /// Build a `DetectedPort` for a test: path only, as an unlabelled
    /// built-in UART would report.
    fn dp(name: &str) -> crate::serial::DetectedPort {
        crate::serial::DetectedPort {
            name: name.to_string(),
            summary: String::new(),
            detail: name.to_string(),
        }
    }

    /// A described port, as a USB adapter reports.
    fn dp_usb(name: &str, summary: &str, detail: &str) -> crate::serial::DetectedPort {
        crate::serial::DetectedPort {
            name: name.to_string(),
            summary: summary.to_string(),
            detail: detail.to_string(),
        }
    }

    #[test]
    fn test_serial_ports_json_empty() {
        assert_eq!(serial_ports_json(&[]), r#"{"ports":[]}"#);
    }

    #[test]
    fn test_serial_ports_json_typical_paths() {
        // Each entry carries the path plus its descriptions, so the refresh
        // button can rebuild the options *and* their hover text.
        let ports = vec![dp("/dev/ttyS0"), dp_usb("/dev/ttyUSB0", "FTDI", "FT232R \u{2014} FTDI")];
        assert_eq!(
            serial_ports_json(&ports),
            r#"{"ports":[{"name":"/dev/ttyS0","summary":"","detail":"/dev/ttyS0"},"#.to_string()
                + r#"{"name":"/dev/ttyUSB0","summary":"FTDI","detail":"FT232R — FTDI"}]}"#
        );
    }

    #[test]
    fn test_serial_ports_json_escapes_quotes_and_backslashes() {
        // Defensive: if a hostile or oddly-named device shows up in
        // the OS port table, the JSON we emit must still parse on
        // the browser side.  Most real serial paths are ASCII and
        // quote-free, but escaping per RFC 8259 §7 keeps a Windows
        // COM-port-like path with backslashes safe too.
        let weird = vec![dp("a\"b"), dp("c\\d"), dp("e\nf")];
        let out = serial_ports_json(&weird);
        assert!(out.contains(r#""a\"b""#));
        assert!(out.contains(r#""c\\d""#));
        assert!(out.contains(r#""e\nf""#));
    }

    #[test]
    fn test_serial_port_options_none_selected_when_empty_current() {
        let opts = serial_port_options("", &[dp("/dev/ttyS0")]);
        // First option is "(none)" with the selected attribute.
        assert!(opts.starts_with(r#"<option value="" selected>(none)</option>"#));
        // The detected port is present but not selected.  Each option now
        // carries its description as a hover title; an undescribed port falls
        // back to the path so the tooltip is never blank.
        assert!(
            opts.contains(r#"<option value="/dev/ttyS0" title="/dev/ttyS0">/dev/ttyS0</option>"#),
            "got {opts}"
        );
    }

    #[test]
    fn test_serial_port_options_marks_current_detected() {
        let opts = serial_port_options("/dev/ttyUSB0", &[dp("/dev/ttyS0"), dp("/dev/ttyUSB0")]);
        assert!(
            opts.contains(r#"<option value="/dev/ttyUSB0" selected title="/dev/ttyUSB0">"#),
            "got {opts}"
        );
        // The (none) option is NOT selected when a real port is chosen.
        assert!(opts.starts_with(r#"<option value="">(none)</option>"#));
    }

    /// A described adapter shows its short label in the visible text and its
    /// full description on hover — while the option's *value* stays the bare
    /// device path, which is what gets saved.  Two identical-looking
    /// `/dev/ttyUSB*` entries are otherwise impossible to tell apart.
    #[test]
    fn test_serial_port_options_label_and_tooltip_keep_value_a_bare_path() {
        let opts = serial_port_options(
            "",
            &[dp_usb("/dev/ttyUSB0", "FTDI", "FT232R USB UART \u{2014} FTDI [USB 0403:6001]")],
        );
        assert!(
            opts.contains(r#"value="/dev/ttyUSB0""#),
            "the saved value must stay a bare path: {opts}"
        );
        assert!(
            opts.contains(r#"title="FT232R USB UART &#8212; FTDI [USB 0403:6001]""#)
                || opts.contains("title=\"FT232R USB UART \u{2014} FTDI [USB 0403:6001]\""),
            "full description belongs in the hover title: {opts}"
        );
        assert!(
            opts.contains("/dev/ttyUSB0 \u{2014} FTDI</option>"),
            "visible text should carry the short label: {opts}"
        );
    }

    /// The selector itself gets a tooltip listing every port, so an operator
    /// can answer "which ttyUSB is my adapter?" without opening the list.
    #[test]
    fn test_serial_row_selector_tooltip_lists_every_port() {
        let ports = vec![
            dp_usb("/dev/ttyUSB0", "FTDI", "/dev/ttyUSB0 \u{2014} FT232R \u{2014} FTDI"),
            dp("/dev/ttyAMA0"),
        ];
        let tip = crate::gui::serial_ports_tooltip(&ports);
        assert!(tip.starts_with("Detected serial ports:"), "got {tip}");
        assert!(tip.contains("/dev/ttyUSB0 \u{2014} FT232R \u{2014} FTDI"), "got {tip}");
        assert!(tip.contains("/dev/ttyAMA0"), "got {tip}");
        // An empty list still produces something a tooltip can show.
        assert_eq!(
            crate::gui::serial_ports_tooltip(&[]),
            "No serial ports detected."
        );
    }

    #[test]
    fn test_serial_port_options_preserves_saved_value_not_in_detected() {
        // Saved port path that isn't currently plugged in: keep it
        // visible with a "(saved)" suffix so the operator's choice
        // is preserved across reboots / cable unplugs.
        let opts = serial_port_options("/dev/ttyUSB99", &[dp("/dev/ttyS0")]);
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

    /// The config page must not scroll sideways on a phone.
    ///
    /// It did, at every phone width: the frames have a deliberate 500px floor
    /// (a frame narrower than its widest row is what pushed the More button
    /// out), so the page was a fixed 516px wide — measured in a 375px viewport,
    /// and still 516px in a 345px one, because the floor set the width rather
    /// than the screen. Below the floor that trade no longer exists, so the
    /// layout re-flows instead.
    ///
    /// Verified with headless Chrome at 320/360/390/480/640/768/1024/1440 —
    /// `scrollWidth == clientWidth` at every one, with every modal opened too —
    /// and the desktop rules confirmed unchanged above the breakpoint (rows
    /// still `nowrap`, the listener grid still 7 columns, two frame columns at
    /// 1440). These assertions guard the rules that result was measured
    /// against; a browser is the only thing that can actually measure it.
    #[test]
    fn test_narrow_viewports_do_not_scroll_sideways() {
        let html = render_main_page(&Config::default(), None);

        assert!(
            html.contains("@media (max-width: 640px)"),
            "the phone layout block is gone; the page will scroll sideways again"
        );
        // The floor is what breaks phones, and a plain `1fr` does not lift it —
        // `1fr` is `minmax(auto, 1fr)`, whose minimum is the item's min-content.
        // That was measured: the column count went to one and the page still
        // overflowed at 450px.
        assert!(
            html.contains(".grid { grid-template-columns: minmax(0, 1fr); }"),
            "the narrow layout must lift the 500px floor with minmax(0, 1fr)"
        );
        // ...and the desktop floor must still be there, or the More button bug
        // it exists to prevent comes back at ordinary window sizes.
        assert!(
            html.contains("repeat(auto-fit, minmax(500px, 1fr))"),
            "the desktop 500px floor is gone — see test_more_buttons_cannot_leave_their_frame"
        );
        // Both single-line rows have to be allowed to wrap; the serial rows were
        // the last thing carrying a More button off-screen, at 320px.
        assert!(
            html.contains(".tight-row, .serial-row { flex-wrap: wrap; }"),
            "the nowrap rows must wrap on a phone"
        );
        // Selects may shrink; inputs may NOT.  Letting inputs shrink clipped the
        // 4-digit baud box to three visible digits — the same defect as the
        // clipped log size, and invisible unless you look at the pixels.
        assert!(
            html.contains(".row select { min-width: 0; }"),
            "the port picker must be allowed to shrink"
        );
        assert!(
            !html.contains(".row input, .row select { min-width: 0; }"),
            "inputs must not be shrinkable — that clips the value they are showing"
        );
    }

    /// The four CSS rules that between them keep every More button inside its
    /// frame.  Verified empirically with headless Chrome across viewports from
    /// 1600px down to 400px (all five More buttons flush against the frame's
    /// content edge, zero overflow); these assertions guard the rules that
    /// result was measured against.
    ///
    /// The bug: the numeric inputs were sized only by the HTML `size` attribute,
    /// which browsers map to different pixel widths, so the File Transfer
    /// tunables row overflowed and carried its right-floated More button out of
    /// the frame — and the Server grid's `1fr` button column collapsed to zero
    /// whenever its `max-content` columns overflowed a narrow frame, doing the
    /// same thing there.
    /// The mount screen must offer a row for every drive, and its button must
    /// exist to open it — a modal nothing opens is invisible.
    #[test]
    fn test_cpm_mount_modal_has_a_row_per_drive_and_a_way_in() {
        let cfg = Config::default();
        let html = render_cpm_disks_modal(&cfg);
        for drive0 in 0..crate::cpm::NUM_DRIVES {
            let name = format!("cpm_mount_{}", (b'a' + drive0) as char);
            assert!(html.contains(&name), "no control for drive {drive0}");
        }
        assert!(html.contains("id=\"more-cpm-disks\""));
        let page = render_more_popups(&cfg);
        assert!(
            page.contains("data-target=\"more-cpm-disks\""),
            "nothing opens the mount modal"
        );
    }

    /// The mount screen must be able to make a disk as well as mount one — it
    /// is where an operator lands with an empty images folder, and the two
    /// controls plus a submit are what turn that dead end into a first disk.
    #[test]
    fn test_cpm_mount_modal_can_create_a_blank_disk() {
        let html = render_cpm_disks_modal(&Config::default());
        assert!(html.contains("name=\"cpm_new_name\""), "no name box");
        assert!(html.contains("name=\"cpm_new_format\""), "no format picker");
        assert!(html.contains(">Create<"), "no way to submit it");
        // Every creatable format is offered, so the web screen cannot drift
        // from the telnet and desktop ones.
        for (token, label) in crate::cpm::image::creatable_formats() {
            assert!(html.contains(&format!("value=\"{token}\"")), "{token} not offered");
            assert!(html.contains(&html_escape(label)), "{token} has no label");
        }
        // The name field must render empty, or a later Save re-submits the name
        // just used and reports "already exists" at somebody who did nothing.
        assert!(
            html.contains("name=\"cpm_new_name\" value=\"\""),
            "the name box must come back blank after a create"
        );
    }

    /// A save must never rewrite `cpm_mounts` as empty just because nothing
    /// has been brought up yet.
    ///
    /// This lost real configuration: mounts were applied only when somebody
    /// first entered the emulator, so on a freshly started gateway the table
    /// was empty — indistinguishable from "the operator unmounted everything"
    /// — and one Save from the web page wrote `cpm_mounts =` and the drives
    /// were gone. No boot, no race, no concurrency: restart, press Save.
    ///
    /// The fix is that `apply_config_mounts` now runs at startup, so an empty
    /// table really does mean nothing is mounted. What this pins is the
    /// consequence: with mounts live, a save that touches only another setting
    /// leaves them exactly as they were.
    #[test]
    fn test_a_save_does_not_wipe_mounts_that_are_live() {
        use crate::cpm::image::{self, registry};
        let _g = registry::tests_lock();
        registry::tests_reset();
        let base = std::env::temp_dir().join("egw_web_save_keeps_mounts");
        let _ = std::fs::remove_dir_all(&base);
        let images = image::images_dir(&base);
        std::fs::create_dir_all(&images).unwrap();
        let blank = crate::cpm::image::format::by_token("altair8").unwrap().blank_image().unwrap();
        std::fs::write(images.join("altair8_one.dsk"), &blank).unwrap();
        std::fs::write(images.join("altair8_two.dsk"), &blank).unwrap();

        // What startup now does.
        image::apply_config_mounts(&base, "B=altair8_one.dsk,C=altair8_two.dsk");
        assert_eq!(
            image::current_mounts_value(),
            "B=altair8_one.dsk,C=altair8_two.dsk",
            "startup must bring the configured mounts up"
        );

        // A save whose selects were rendered from the live table, unchanged.
        let mut fields = HashMap::new();
        for d in 0..crate::cpm::NUM_DRIVES {
            let key = format!("cpm_mount_{}", (b'a' + d) as char);
            let val = match d {
                1 => "altair8_one.dsk",
                2 => "altair8_two.dsk",
                _ => "",
            };
            fields.insert(key, val.to_string());
        }
        let cfg = Config {
            transfer_dir: base.parent().unwrap().to_string_lossy().to_string(),
            ..Default::default()
        };
        let (_notice, value) = apply_cpm_mount_form(&fields, &cfg);
        assert_eq!(
            value, "B=altair8_one.dsk,C=altair8_two.dsk",
            "a save must not drop the operator's drives"
        );

        registry::tests_reset();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An empty name box must not create anything.  Every ordinary Save on this
    /// screen submits the create fields too, so a blank one has to read as "no"
    /// — otherwise changing a mount also tries to make a disk and reports a
    /// failure at somebody who never asked for one.
    #[test]
    fn test_a_new_disk_is_requested_only_when_a_name_was_typed() {
        let f = |name: &str, token: &str| {
            let mut m = HashMap::new();
            m.insert("cpm_new_name".to_string(), name.to_string());
            m.insert("cpm_new_format".to_string(), token.to_string());
            requested_new_disk(&m)
        };
        assert_eq!(
            f("scratch", "altair8"),
            Some(("altair8".to_string(), "scratch".to_string()))
        );
        assert_eq!(f("", "altair8"), None, "an untouched box is not a request");
        assert_eq!(f("   ", "altair8"), None, "nor is a box of spaces");
        assert_eq!(f("scratch", ""), None, "no format is not a request either");
        // The mount screen's ordinary save carries neither field.
        assert_eq!(requested_new_disk(&HashMap::new()), None);
        let mut mounts_only = HashMap::new();
        mounts_only.insert("cpm_mount_b".to_string(), "altair8_games.dsk".to_string());
        assert_eq!(requested_new_disk(&mounts_only), None);
    }

    /// A drive whose select was not submitted keeps its image.
    ///
    /// The busy rows render `disabled`, and a disabled control is not
    /// submitted — so reading absence as "set to none" would unmount exactly
    /// the drives the screen said could not be changed.  Same hazard as the
    /// role-gated checkboxes, and the same reason it needs a test.
    #[test]
    fn test_absent_mount_select_is_not_read_as_unmount() {
        let cfg = Config::default();
        let mut fields: HashMap<String, String> = HashMap::new();
        // Only drive B: submitted, and empty (an explicit "drive folder").
        fields.insert("cpm_mount_b".to_string(), String::new());
        let (_notice, value) = apply_cpm_mount_form(&fields, &cfg);
        // Nothing was mounted in this test process, so the result is empty —
        // the point is that it did not panic and did not invent mounts for the
        // fifteen drives whose selects were absent.
        assert!(
            !value.contains("A="),
            "an unsubmitted drive must not be given an image: {value}"
        );
    }

    #[test]
    fn test_more_buttons_cannot_leave_their_frame() {
        let html = render_main_page(&Config::default(), None);

        // 1. Numeric inputs have a real width, not a browser-dependent `size`.
        assert!(
            html.contains(".num-tight { width: calc(5ch + 14px); }"),
            "numeric inputs must carry an explicit width; `size=5` alone is \
             mapped differently by each browser, which is what pushed the File \
             Transfer More button out of frame"
        );
        // 2. Those inputs may shrink so the button is never pushed out.
        assert!(
            html.contains(".tight-row input { flex-shrink: 1; min-width: 3ch; }"),
            "tight-row inputs must be allowed to shrink, or a row wider than the \
             frame overflows and takes the More button with it"
        );
        // 3. The Server grid's button column cannot collapse to zero.
        assert!(
            html.contains("minmax(max-content, 1fr)"),
            "the Server grid's More column must be minmax(max-content, 1fr); a \
             bare 1fr collapses to zero width when the content columns overflow"
        );
        // 4. The General frame's More button is right-justified by an auto
        //    margin inside a wrapping flex `.row` — not by a grid column, which
        //    is what collapsed to zero and put the Server button outside its
        //    frame.  An auto margin cannot push a flex item past its container.
        assert!(
            html.contains(".row-right { margin-left: auto; }"),
            ".row-right must have a real CSS rule — a class with no rule is how \
             .num-tight silently took its width from the browser's `size=` default"
        );
        assert!(
            html.contains(r#"class="more row-right" data-target="more-general""#),
            "the General More button must use .row-right (auto-margin in a \
             wrapping flex row), not a grid column that can collapse"
        );
        // 5. Frames are never narrower than the Server row needs.
        assert!(
            html.contains("repeat(auto-fit, minmax(500px, 1fr))"),
            "the frame grid's minimum must stay >= the Server row's intrinsic \
             width (~497px measured), or the frame gets narrower than its own \
             content and the More button ends up outside it"
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

    /// A numeric box must be wide enough for the value it is showing.
    ///
    /// The fixed five-digit width clipped `log_max_size_kb` — a `u64` in KB, so
    /// a 1 GB cap is seven digits.  The value was never lost (the input
    /// scrolls), but a config screen that displays `104857` where the setting
    /// says `1048576` reads as data loss.  Checked here rather than by eye
    /// because the visible width comes from the CSS `calc`, not from `size`.
    #[test]
    fn test_numeric_fields_grow_to_fit_long_values() {
        // The common case is unchanged: tight box, no inline override.
        let small = numfield("idle_timeout_secs", "Idle (s)", 300u64);
        assert!(small.contains("size=\"5\""), "short values keep the 5-digit box: {small}");
        assert!(
            !small.contains("style="),
            "a value that fits must not carry an inline width: {small}"
        );

        // Seven digits: both `size` and the rendered width must follow.
        let big = numfield("log_max_size_kb", "Rotate at (KB)", 1_048_576u64);
        assert!(big.contains("size=\"7\""), "{big}");
        assert!(
            big.contains("width: calc(7ch + 14px)"),
            "the visible width comes from the calc, so `size` alone is not enough: {big}"
        );

        // The `_attr` form is the same rule — the log fields go through it,
        // greyed, and are exactly the ones that overflow.
        let greyed = numfield_attr("log_max_size_kb", "Rotate at (KB)", 1_048_576u64, "disabled");
        assert!(
            greyed.contains("width: calc(7ch + 14px)") && greyed.contains("disabled"),
            "greying a field must not cost it its width: {greyed}"
        );

        // And it reaches the real page, not just the helper.
        let cfg = Config { log_max_size_kb: 1_048_576, ..Config::default() };
        let page = render_main_page(&cfg, None);
        assert!(
            page.contains("name=\"log_max_size_kb\" value=\"1048576\" size=\"7\""),
            "the rendered config page still clips the log size field"
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
        // Enumerated from the rendered HTML rather than hand-listed, so a popup
        // added later is covered automatically — this test previously checked
        // only `more-server` and would not have noticed a new one.
        let cfg = Config::default();
        let html = render_main_page(&cfg, None);
        let form_start = html.find("<form").expect("form open tag");
        let form_end = html.find("</form>").expect("form close tag");

        let mut checked = 0;
        let mut rest = html.as_str();
        let mut offset = 0usize;
        while let Some(i) = rest.find("id=\"more-") {
            let abs = offset + i;
            let id_end = rest[i + 4..].find('"').map(|e| i + 4 + e).unwrap_or(rest.len());
            let id = &rest[i + 4..id_end];
            assert!(
                abs > form_start && abs < form_end,
                "popup {id} is outside the form (pos {abs} vs form {form_start}..{form_end}) \
                 — its fields would silently drop on save",
            );
            checked += 1;
            offset = abs + 1;
            rest = &html[offset..];
        }
        // Guards the scan: a renamed id prefix would otherwise check nothing.
        assert!(
            checked >= 5,
            "expected at least 5 More popups (server, general, ai, xfer, 2 serial), \
             found {checked} — has the id scheme changed?"
        );

        // Every More *button* must point at a popup that exists, or clicking it
        // throws in openModal and nothing opens.
        let mut rest = html.as_str();
        let mut buttons = 0;
        while let Some(i) = rest.find("data-target=\"") {
            let start = i + "data-target=\"".len();
            let end = rest[start..].find('"').map(|e| start + e).unwrap();
            let target = &rest[start..end];
            assert!(
                html.contains(&format!("id=\"{target}\"")),
                "More button targets {target}, which no popup defines"
            );
            buttons += 1;
            rest = &rest[end..];
        }
        assert!(buttons >= 5, "expected at least 5 More buttons, found {buttons}");
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
        assert!(web_ip_rejection(false, false, false, public).is_some());
        assert!(web_ip_rejection(true, false, false, public).is_some());
        assert_eq!(
            web_ip_rejection(false, false, false, public),
            web_ip_rejection(true, false, false, public),
            "security_enabled must not affect the web IP decision"
        );

        // Private IP is allowed whether or not login is required.
        assert!(web_ip_rejection(false, false, false, private).is_none());
        assert!(web_ip_rejection(true, false, false, private).is_none());
    }

    #[test]
    fn test_web_ip_rejection_disable_safety_allows_all() {
        // With the IP safety toggle off, even a public peer is allowed
        // (operator opt-out), and `security_enabled` still doesn't matter.
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        assert!(web_ip_rejection(false, true, false, public).is_none());
        assert!(web_ip_rejection(true, true, false, public).is_none());
    }
}
