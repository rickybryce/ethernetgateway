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
const LOGO_PNG: &[u8] = include_bytes!("../eglogobrightsmall.png");

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
    /// Persist config and test the bound ports.  Its own action because the
    /// page that comes back shows the result in a modal, and only this one
    /// should.
    PortCheck,
}

impl SaveAction {
    fn from_form(value: Option<&str>) -> Self {
        match value {
            Some("save_and_restart") => SaveAction::SaveAndRestart,
            Some("save_and_restart_serial") => SaveAction::SaveAndRestartSerial,
            Some("portcheck") => SaveAction::PortCheck,
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
            // Off the async runtime, for the same reason the Save POST below
            // is: rendering asks `boot_choices` whether each image in the
            // folder would boot, and on a cold cache that reads every one of
            // them — tens of megabytes if the operator took the sample disks.
            // Doing it on the connection's own task would stall every other
            // session's timers, which is the trap this file already names.
            // Only the redirect a port check makes carries this, so the modal
            // belongs to the page that asked for it rather than to whoever
            // loads the page next.
            let show_check = parse_form(&request.query).contains_key("portcheck");
            let render_cfg = cfg.clone();
            // A join failure means the render panicked.  Answer 500, not 200
            // with an error page in it: a monitor polling `/` would read a 200
            // as healthy.  The payload is not interpolated -- a panic message
            // can carry anything, including markup, and this page is served
            // behind the operator's own credentials but is still a page.
            let (code, reason, body) =
                match tokio::task::spawn_blocking(move || render_main_page(&render_cfg, notice, show_check))
                    .await
                {
                    Ok(html) => (200, "OK", html),
                    Err(e) => {
                        glog!("Web: rendering the configuration page failed: {e}");
                        (500, "Internal Server Error", "<h1>500</h1><p>The configuration page \
                         could not be rendered. See the gateway log.</p>".to_string())
                    }
                };
            write_response(
                &mut stream,
                code,
                reason,
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
        ("GET", "/vdm") => {
            let cfg = config::get_config();
            let body = render_vdm_page(&cfg);
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
        ("GET", "/vdm/list") => {
            let body = vdm_list_json(&crate::cpm::screen::list());
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
        ("GET", "/vdm/frame") => {
            // An id that does not parse is not an error condition — a page left
            // open across a gateway restart will ask for a session that no
            // longer exists, and the honest answer to both is the same one.
            let id = parse_form(&request.query)
                .get("id")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let body = vdm_frame_json(id, &crate::cpm::screen::look(id));
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
        ("POST", "/vdm/joy") => {
            // Holding a direction, which is a different act from typing and so
            // a different route: a keystroke is delivered once and a stick is a
            // position that persists. Gated on its own key, read live, for the
            // same reason `/vdm/key` reads `cpm_screen_input` live.
            //
            // The whole mask arrives every time rather than a change, which is
            // what makes a dropped request harmless -- the next one restates
            // the truth. A mask is a plain integer, so unlike a keystroke it
            // needs no percent-encoding.
            let text = String::from_utf8_lossy(&request.body).to_string();
            let form = parse_form(&text);
            let id = form.get("id").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let mask = form.get("m").and_then(|s| s.parse::<u16>().ok());
            let body = if !config::get_config().cpm_joystick {
                // A refusal the page can act on: it unticks its own switch and
                // hands the ten letters back to the keyboard.
                "{\"held\":false,\"why\":\"off\"}".to_string()
            } else {
                match mask {
                    // `0` is a real and important report -- it is the release,
                    // and dropping it as "nothing" would leave the stick over.
                    Some(m) if crate::cpm::screen::set_joystick(id, m) => {
                        "{\"held\":true}".to_string()
                    }
                    Some(_) => "{\"held\":false,\"why\":\"gone\"}".to_string(),
                    None => "{\"held\":false,\"why\":\"nothing\"}".to_string(),
                }
            };
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
        ("POST", "/vdm/key") => {
            // Typing at a booted guest.  Gated on `cpm_screen_input`, read
            // live rather than captured at start-up so turning it off takes
            // effect on the next keystroke rather than on the next restart.
            //
            // The bytes arrive percent-encoded in the body, because a keystroke
            // is any byte at all — a control character in a query string is not
            // a thing we should be asking a browser to arrange.
            // Not valid UTF-8 is not an error to report: a keystroke that
            // arrived mangled is a keystroke to drop, and a browser cannot
            // usefully act on the difference.
            let text = String::from_utf8_lossy(&request.body).to_string();
            let form = parse_form(&text);
            let id = form.get("id").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let bytes: Vec<u8> = form.get("k").map(|s| s.as_bytes().to_vec()).unwrap_or_default();
            let body = if !config::get_config().cpm_screen_input {
                // A refusal the page can act on rather than a silent drop: it
                // stops offering a keyboard when it sees this.
                "{\"typed\":false,\"why\":\"off\"}".to_string()
            } else if bytes.is_empty() {
                "{\"typed\":false,\"why\":\"nothing\"}".to_string()
            } else if crate::cpm::screen::push_keys(id, &bytes) {
                "{\"typed\":true}".to_string()
            } else {
                "{\"typed\":false,\"why\":\"gone\"}".to_string()
            };
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
            // A port check also asks the page to show its result, because the
            // banner scrolls past and the console window is not where anybody
            // is looking.  A query flag rather than server-side state: the
            // modal belongs to the page that was just loaded, not to the next
            // person who happens to open one.
            let location = if action == SaveAction::PortCheck {
                format!("/?portcheck=1&notice={}", encode_query(&notice))
            } else {
                format!("/?notice={}", encode_query(&notice))
            };
            write_redirect(&mut stream, &location).await?;

            // Response has been flushed and the connection shut down —
            // safe to fire the restart now.  Doing it any earlier risks
            // the runtime tearing down mid-write so the operator never
            // sees the confirmation banner on the redirected GET.
            match action {
                // Nothing to do here: the check already ran inside
                // `apply_form_post`, and the page it redirects to shows the
                // result.  Listed rather than folded into `Save` so a reader
                // sees that a port check restarts nothing.
                SaveAction::Save | SaveAction::PortCheck => {}
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
    // EGT8080 defaults to, which is the point: the pair works together again.
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

    // Fetching the sample disks.  Its own action rather than a field, because
    // it is not a setting: nothing is saved, files arrive in the images folder,
    // and the operator finds out what happened in the banner.
    //
    // Synchronous, and that is a real cost — this holds the request for as long
    // as the download takes, which is about a minute on a cold folder.  It is
    // the honest arrangement for a page with no job queue: the alternative is
    // returning immediately and leaving the operator refreshing to guess
    // whether it worked.  The connection cap means a stuck download occupies
    // one of sixteen, not the server.
    if fields.get("action").map(String::as_str) == Some("getdisks") {
        let base = crate::cpm::layout::cpm_dir(&old_cfg.transfer_dir);
        let msg = match crate::cpm::fetch::download_missing(&base, |_, _, _| {}) {
            Ok(r) => {
                let mut m = format!("Sample disks: {}.", r.summary());
                // Named, not just counted: "3 failed" with no names leaves the
                // operator unable to retry or report anything.  Grouped by
                // reason, because "no internet" is one fact repeated once per
                // disk, not thirty-four separate problems.
                for line in r.failure_lines(3) {
                    m.push_str(&format!(" {line}."));
                }
                m
            }
            Err(e) => format!("Sample disks: {e}"),
        };
        notice = if notice.is_empty() { msg } else { format!("{notice} {msg}") };
    }

    // Fetching the monitor ROM the CP/M settings name.  An action rather than a
    // field for the same reason as the disks above: nothing is saved, a file
    // arrives in the ROMs folder, and the banner says what happened.  It takes
    // the ROM from the *submitted* form rather than the saved config, so
    // choosing one and pressing Fetch in the same visit does what it looks like.
    if fields.get("action").map(String::as_str) == Some("getrom") {
        let base = crate::cpm::layout::cpm_dir(&old_cfg.transfer_dir);
        let want = fields
            .get("cpm_boot_rom")
            .filter(|v| crate::cpm::rom::is_valid_rom_key(v))
            .cloned()
            .unwrap_or_else(|| old_cfg.cpm_boot_rom.clone());
        let msg = match crate::cpm::rom::download(&base, &want) {
            Ok(note) => format!("Monitor ROM: {note}."),
            Err(e) => format!("Monitor ROM: {e}"),
        };
        notice = if notice.is_empty() { msg } else { format!("{notice} {msg}") };
    }

    // Resolving a reported problem.  An action rather than a setting: it changes
    // a file (`gateway_hosts`), not the config, and it must be an explicit press
    // -- the gateway will not re-pin a changed host key on its own, because a
    // reinstalled master and a man-in-the-middle look identical from here.
    if let Some(id) = fields.get("resolve_id").filter(|v| !v.is_empty()) {
        let msg = match crate::resolve::resolve(id) {
            Ok(note) => note,
            Err(e) => e,
        };
        notice = if notice.is_empty() { msg } else { format!("{notice} {msg}") };
    }

    // The port check, on the same synchronous footing as the download above and
    // for the same reason: there is no job queue, and returning immediately
    // would leave the operator refreshing to guess whether it ran.  Four
    // connect timeouts is the worst case, which is well inside a page load.
    if fields.get("action").map(String::as_str) == Some("portcheck") {
        let blocked = crate::portcheck::run_check();
        let msg = if blocked == 0 {
            // Never "all ports are open".  A self-connection skips the firewall
            // on Windows and macOS, so a pass is not evidence -- and this page
            // is read on all three.
            "Port check: every bound listener answered on this machine. That rules out a local block on Linux; on Windows and macOS a connection to your own address skips the firewall, and nothing here can see a router that is not forwarding a port."
                .to_string()
        } else {
            format!(
                "Port check: {blocked} bound port{} did not answer on this machine — marked below.",
                if blocked == 1 { "" } else { "s" }
            )
        };
        notice = if notice.is_empty() { msg } else { format!("{notice} {msg}") };
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
        "groq_api_key", "ai_model", "browser_homepage", "weather_location", "weather_units",
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
        "cpm_emu_max_minstr", "cpm_emu_uart", "cpm_boot_image", "cpm_boot_machine",
        "cpm_boot_rom", "cpm_boot_backspace", "cpm_cpu", "cpm_boot_speed",
        "cpm_printer", "cpm_printer_port", "cpm_printer_autolf",
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
        "gateway_petscii_translate",
        "cpm_emu_enabled",
        "cpm_screen_input",
        "cpm_joystick",
        "cpm_boot_writable",
        "place_bundled_terminals",
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
    //
    // **`backspace` belongs here and spent 0.9.5 in `bool_keys`.** It is a
    // three-way choice, and the boolean loop wrote `is_truthy("rubout")` --
    // `false` -- for it: the page could not set the erase key at all, and worse,
    // ANY save from this page silently cleared one set from telnet or the
    // desktop, because an absent field also became `false` and
    // `backspace_target("false")` is `None`. A setting that quietly stops
    // working, in the one surface an operator is most likely to save from.
    let serial_keys: &[&str] = &[
        "mode", "port", "baud", "databits", "parity", "stopbits",
        "flowcontrol", "s_regs", "x_code", "dtr_mode", "flow_mode",
        "dcd_mode", "backspace", "gateway_petscii",
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

// ─── The VDM-1 screen ───────────────────────────────────────────────
//
// The Processor Technology VDM-1 was a video *card*: no serial line, no
// keyboard, no data port.  A booted guest paints by storing bytes into memory
// at CC00, and the card scans that window.  So does this page — the session
// task publishes a snapshot, and the browser paints it.  See `cpm::vdm` for the
// device and why sampling it cannot disturb the guest.
//
// It lives on this listener rather than one of its own quite deliberately: a
// second port would need its own auth, lockout, IP-safety and bindwatch, plus a
// port key on three config screens, to show a picture that this one can already
// serve behind the credentials it already checks.
//
// One caveat is worth stating rather than discovering: this page authenticates
// the *administrator*, while the person typing at the guest is on telnet or
// SSH.  Anyone who can open the config page can watch a booted session's
// screen.  On a gateway whose web UI already renders the password and the API
// key that is not a new privilege, but it is a different sentence from "the
// operator can see their own screen".

/// The live screen list, for the picker.
fn vdm_list_json(screens: &[crate::cpm::screen::Listing]) -> String {
    let mut out = String::from("{\"screens\":[");
    for (i, s) in screens.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"id\":{},\"label\":\"{}\",\"vdm\":{},\"dazzler\":{},\"frame\":{}}}",
            s.id,
            json_escape(&s.label),
            s.vdm_active,
            s.dazzler_on,
            s.has_frame,
        ));
    }
    out.push_str("]}");
    out
}

/// One screen's answer: gone, waiting for its first frame, or a picture.
///
/// The three states are carried through to the browser rather than collapsed,
/// because a session that has ended and one that has not painted yet both look
/// like a blank 64x16 grid — the one pair of states a viewer cannot tell apart
/// by looking, so the page has to be told.
///
/// The Dazzler travels as its *cells* — one 4-bit value each — rather than as
/// pixels or a colour string, because the palette belongs to whoever is
/// painting and because 16,384 of them at 128x128 is still a modest string.
/// The `colour` flag says whether those four bits are red/green/blue/intensity
/// or one of sixteen greys; the same nibble means both, and only the format
/// register separates them.
fn vdm_frame_json(id: u64, look: &crate::cpm::screen::Look) -> String {
    use crate::cpm::screen::Look;
    match look {
        Look::Gone => format!("{{\"id\":{id},\"state\":\"gone\"}}"),
        Look::Waiting { label } => format!(
            "{{\"id\":{id},\"state\":\"waiting\",\"label\":\"{}\"}}",
            json_escape(label)
        ),
        Look::Frame(snap) => {
            let grid = crate::cpm::vdm::frame(&snap.vdm.window, snap.vdm.scroll);
            let rows = crate::cpm::vdm::frame_text(&grid);
            let inv = crate::cpm::vdm::frame_inverse(&grid);
            let join = |v: &[String]| {
                v.iter()
                    .map(|s| format!("\"{}\"", json_escape(s)))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let dazzler = match &snap.dazzler {
                None => String::from("null"),
                Some(d) => {
                    let fmt = crate::cpm::dazzler::Format::from_byte(d.format);
                    let pic = crate::cpm::dazzler::frame(&d.bytes, fmt);
                    // One hex digit per cell: a nibble is exactly a hex digit,
                    // so the wire form is the data rather than an encoding of
                    // it, and 128x128 costs 16 KB of text.
                    let cells: String =
                        pic.cells.iter().map(|c| char::from_digit(*c as u32, 16).unwrap_or('0'))
                            .collect();
                    format!(
                        "{{\"w\":{w},\"h\":{h},\"colour\":{colour},\"base\":{base},\
                         \"format\":{format},\"cells\":\"{cells}\"}}",
                        w = pic.width,
                        h = pic.height,
                        colour = pic.colour,
                        base = crate::cpm::dazzler::base(d.address),
                        format = d.format,
                    )
                }
            };
            format!(
                "{{\"id\":{id},\"state\":\"live\",\"label\":\"{label}\",\"gen\":{generation},\
                 \"scroll\":{scroll},\"active\":{active},\"rows\":[{rows}],\"inv\":[{inv}],\
                 \"joy\":{joy},\"dazzler\":{dazzler}}}",
                label = json_escape(&snap.label),
                generation = snap.generation,
                scroll = snap.vdm.scroll,
                active = snap.vdm.active,
                rows = join(&rows),
                inv = join(&inv),
                joy = snap.joystick_seen,
            )
        }
    }
}

/// The VDM-1 viewer page.
///
/// Static: everything on it arrives from `/vdm/list` and `/vdm/frame`, so a
/// screen opened before a guest booted starts working when it does, and one
/// left open when a session ends says so instead of freezing on its last frame.
fn render_vdm_page(cfg: &Config) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str("<!doctype html><html lang=\"en\"><head>");
    out.push_str("<meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    out.push_str("<title>Ethernet Gateway — VDM / Dazzler</title>");
    out.push_str(STYLE);
    out.push_str(VDM_STYLE);
    out.push_str("</head><body>");
    out.push_str(&render_header(cfg));
    out.push_str(
        "<div class=\"hint\">What a booted disk is displaying, sampled from its own memory \
         &mdash; a Processor Technology <strong>VDM-1</strong> at <code>CC00</code>, \
         a Cromemco <strong>Dazzler</strong> wherever the guest put it, or both</div>",
    );
    out.push_str("<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">VDM / Dazzler</span>\
         <span class=\"head-right\"><a class=\"backlink\" href=\"/\">&larr; Configuration</a></span>\
         </div>");
    out.push_str(
        "<div class=\"vdm-pick\"><label for=\"vdm-id\">Session:</label>\
         <select id=\"vdm-id\"></select>\
         <span id=\"vdm-note\" class=\"vdm-note\"></span></div>",
    );
    // 64 columns of monospace, and the aspect ratio is left alone: this is the
    // card's memory laid out, not a photograph of a CRT.
    // One focusable stage around both displays.  Real focus rather than a
    // "typing mode" flag of our own: the browser already has the concept, and
    // an operator who clicks away expects the keys to stop going to the guest.
    out.push_str("<div id=\"vdm-stage\" tabindex=\"0\">");
    out.push_str("<div id=\"vdm-screen\" class=\"vdm-screen\"></div>");
    out.push_str("<div id=\"vdm-status\" class=\"vdm-status\"></div>");
    // The colour card, hidden until a guest switches one on — a machine with
    // no Dazzler should not show an empty black square that looks like one
    // that is broken.
    out.push_str(
        "<div id=\"dz-wrap\" class=\"dz-wrap\" hidden>\
         <div class=\"title\">Cromemco Dazzler</div>\
         <canvas id=\"dz\" class=\"dz\" width=\"64\" height=\"64\"></canvas>\
         <div id=\"dz-status\" class=\"vdm-status\"></div></div>",
    );
    out.push_str("</div>");
    out.push_str("<div id=\"vdm-kb\" class=\"vdm-status\"></div>");
    // **The joystick panel names every key.** A control you cannot see is a
    // control nobody uses: these games read a board with no console, so there
    // is nothing on the guest's own screen to say a joystick exists, let alone
    // which keys are it. The mapping is rendered from the same table the
    // script keys off, so the legend cannot drift from what the page sends.
    if cfg.cpm_joystick {
        out.push_str(&render_joystick_panel());
    }
    out.push_str("</section>");
    // The two intervals are Rust constants, so the page cannot drift from the
    // rate this file documents.
    out.push_str(&format!(
        "<script>var VDM_POLL_MS={VDM_POLL_MS};var VDM_LIST_MS={VDM_LIST_MS};\
         var VDM_INPUT={input};var VDM_JOY={joy};\
         var VDM_JOY_KEYS={keys};var VDM_JOY_IDLE_MS={idle};</script>",
        input = cfg.cpm_screen_input,
        joy = cfg.cpm_joystick,
        keys = joystick_keys_json(),
        idle = crate::cpm::screen::JOYSTICK_IDLE_MS,
    ));
    out.push_str(VDM_SCRIPT);
    out.push_str("</body></html>");
    out
}

use crate::cpm::d7a;

/// The joystick keys, in one place: the legend the page prints and the table
/// the script matches on are both built from this.
///
/// **One table and not two.** The mapping is the whole interface — a player who
/// reads `S` for right and presses `D` gets nothing, and a legend that drifts
/// from the handler is worse than no legend, because it is believed. The keys
/// are the ones the operator asked for: `W/A/S/Z` around a diamond with `X` to
/// fire, and `I/J/K/M` with `N`, which is how two people share one keyboard
/// without their hands colliding.
///
/// `(key, the bit it sets, stick, what it does)`.
///
/// The bit is the value from [`crate::cpm::d7a::bit`], not a name to be looked
/// up: an earlier draft carried the name and matched it at render time, which
/// put an `unreachable!()` on the path that draws a page. A typo in this table
/// would have been a panic serving a request rather than a compile error.
const JOYSTICK_KEYS: &[(&str, u16, u8, &str)] = &[
    ("W", d7a::bit::P1_UP, 1, "up"),
    ("A", d7a::bit::P1_LEFT, 1, "left"),
    ("S", d7a::bit::P1_RIGHT, 1, "right"),
    ("Z", d7a::bit::P1_DOWN, 1, "down"),
    ("X", d7a::bit::P1_FIRE, 1, "fire"),
    ("I", d7a::bit::P2_UP, 2, "up"),
    ("J", d7a::bit::P2_LEFT, 2, "left"),
    ("K", d7a::bit::P2_RIGHT, 2, "right"),
    ("M", d7a::bit::P2_DOWN, 2, "down"),
    ("N", d7a::bit::P2_FIRE, 2, "fire"),
];

/// The bit each key sets, as the script's lookup table.
///
/// Built from [`JOYSTICK_KEYS`] and [`crate::cpm::d7a::bit`] together, so the
/// page and the board agree about which bit is which by construction rather
/// than by two lists being kept in step.
fn joystick_keys_json() -> String {
    let mut out = String::from("{");
    for (i, (key, mask, _, _)) in JOYSTICK_KEYS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{}\":{}", key.to_ascii_lowercase(), mask));
    }
    out.push('}');
    out
}

/// The visible legend: every key, said plainly, beside a switch.
fn render_joystick_panel() -> String {
    let mut out = String::from(
        "<div class=\"joy\"><div class=\"joy-head\">         <label><input type=\"checkbox\" id=\"joy-on\" checked>          <strong>Joystick</strong> &mdash; Cromemco D+7A</label>         <span id=\"joy-note\" class=\"vdm-status\"></span></div>",
    );
    // Two columns, one per stick, each naming its five keys.
    for stick in [1u8, 2u8] {
        out.push_str(&format!("<div class=\"joy-p\"><span class=\"joy-who\">Player {stick}</span>"));
        for (key, _, s, what) in JOYSTICK_KEYS.iter().filter(|(_, _, s, _)| *s == stick) {
            let _ = s;
            out.push_str(&format!(
                "<span class=\"joy-key\"><kbd>{key}</kbd> {what}</span>"
            ));
        }
        out.push_str("</div>");
    }
    out.push_str(
        "<div class=\"joy-hint\">Hold a direction and it <strong>swings</strong> &mdash;          centred when you press, full deflection half a second later, because these are          analogue sticks and a key has no halfway. While the joystick is on, those ten          letters drive it instead of typing at the guest &mdash; it starts on because this          gateway has the board enabled, so untick it to type them.</div></div>",
    );
    out
}

const VDM_STYLE: &str = "<style>
.joy { margin-top: 12px; padding: 10px 12px; border: 1px solid #3a3a3a; border-radius: 6px; }
.joy-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; margin-bottom: 6px; }
.joy-p { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; margin: 4px 0; }
.joy-who { min-width: 72px; font-weight: bold; }
.joy-key { white-space: nowrap; }
.joy-key kbd { display: inline-block; min-width: 1.4em; padding: 1px 5px; text-align: center;
  border: 1px solid #666; border-bottom-width: 2px; border-radius: 4px; font-family: monospace; }
.joy-hint { margin-top: 6px; font-size: 0.9em; opacity: 0.85; }
.joy-live kbd { border-color: #7fd07f; }
.vdm-pick { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }
.vdm-pick select { min-width: 260px; }
.vdm-note { color: var(--amber-dim); font-style: italic; }
.vdm-screen {
  background: #000;
  color: var(--console-text);
  font-family: 'DejaVu Sans Mono', 'Consolas', monospace;
  font-size: 16px;
  line-height: 1.15;
  padding: 10px;
  border: 1px solid var(--border);
  border-radius: 4px;
  /* 64 columns must not wrap: a wrapped VDM-1 screen is unreadable and looks
     like a fault in the guest rather than in the window it is being shown in. */
  white-space: pre;
  overflow-x: auto;
}
/* Bit 7. The card inverts the cell; so do we, and the cursor comes along for
   free because on this hardware the cursor IS an inverse-video cell. */
.vdm-screen i { background: var(--console-text); color: #000; font-style: normal; }
.vdm-status { color: var(--amber-dim); font-size: 13px; margin-top: 8px; }
.dz-wrap { margin-top: 14px; }
/* The stage takes focus, so the browser's own idea of where the keys go
   decides it, rather than a mode of ours.  The outline is the whole feedback:
   an operator has to be able to tell at a glance whether what they type is
   reaching a guest or their own browser. */
#vdm-stage { outline: none; border-radius: 4px; }
#vdm-stage:focus { box-shadow: 0 0 0 2px var(--amber); }
/* The picture is 32x32 to 128x128 elements and every one of them is a square
   the size of a TV's, so it is scaled up on display and *not* smoothed: this
   is a memory map made visible, not a photograph of a CRT. */
.dz {
  image-rendering: pixelated;
  width: 512px;
  max-width: 100%;
  background: #000;
  border: 1px solid var(--border);
  border-radius: 4px;
}
</style>";

/// Poll interval for a frame, in milliseconds.
///
/// The session publishes one snapshot per request and nothing on its own, so
/// this number *is* the sampling rate — and the cost when the page is shut is
/// zero rather than small.  Fast enough that a scrolling guest reads as
/// scrolling; slow enough that seven connections a second is the whole expense.
const VDM_POLL_MS: u32 = 150;
/// How often the session list is rebuilt.  Sessions come and go at human speed.
const VDM_LIST_MS: u32 = 2000;

const VDM_SCRIPT: &str = "<script>
var vdmCurrent = null;
function vdmEsc(s) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
/* Paint one line, grouping runs of equal inverse-video attribute so a full
   line of normal text is one text node rather than sixty-four spans. */
function vdmLine(text, inv) {
  var out = '', i = 0;
  while (i < text.length) {
    var on = inv.charAt(i) === '1', j = i;
    while (j < text.length && (inv.charAt(j) === '1') === on) { j++; }
    var chunk = vdmEsc(text.substring(i, j));
    out += on ? '<i>' + chunk + '</i>' : chunk;
    i = j;
  }
  return out;
}
function vdmPaint(d) {
  var screen = document.getElementById('vdm-screen');
  var status = document.getElementById('vdm-status');
  if (d.state === 'gone') {
    status.textContent = 'That session has ended.';
    dzPaint(null);
    return;
  }
  if (d.state === 'waiting') {
    status.textContent = d.label + ' — waiting for the first frame\\u2026';
    dzPaint(null);
    return;
  }
  var html = [];
  for (var r = 0; r < d.rows.length; r++) {
    html.push(vdmLine(d.rows[r], d.inv[r]));
  }
  /* A guest that has never driven the VDM-1 and *is* driving a Dazzler is not
     using the character screen at all, and what sits at CC00 is its ordinary
     memory.  Giving that equal space to the picture it is really painting reads
     as a fault in the picture.  Collapsed rather than removed, because the
     bytes are still honestly there and the note says so. */
  /* Whether the guest is reading the joystick board, so the panel can say so
     rather than leaving a player guessing at silence. */
  if (JOY_SEEN !== !!d.joy) {
    JOY_SEEN = !!d.joy;
    joyNote();
  }
  var idle = !d.active && d.dazzler;
  screen.hidden = idle;
  screen.innerHTML = idle ? '' : html.join('\\n');
  dzPaint(d.dazzler);
  status.textContent = d.label + ' — frame ' + d.gen
    + (d.active
       ? ', scroll ' + d.scroll + ' — this guest is driving the VDM-1.'
       : idle
         ? ' — no VDM-1 here: this guest has never written the scroll register.'
         : ', scroll ' + d.scroll
           + ' — this guest has not written the VDM-1 scroll register; what you'
           + ' see is whatever is in its memory at CC00.');
}
/* The Dazzler's sixteen colours.  In colour mode a cell is
   red|green|blue|intensity from bit 0 up, so the palette is generated rather
   than typed — a 16-entry table would be four transcription mistakes waiting
   to happen.  In black-and-white the same four bits are one of sixteen greys,
   which the manual is explicit about, so the palette depends on the format. */
function dzColour(v, colour) {
  if (!colour) { var g = Math.round(v * 255 / 15); return [g, g, g]; }
  var hi = (v & 8) ? 255 : 128;
  return [(v & 1) ? hi : 0, (v & 2) ? hi : 0, (v & 4) ? hi : 0];
}
function dzPaint(d) {
  var wrap = document.getElementById('dz-wrap');
  if (!d) { wrap.hidden = true; return; }
  wrap.hidden = false;
  var cv = document.getElementById('dz');
  if (cv.width !== d.w || cv.height !== d.h) { cv.width = d.w; cv.height = d.h; }
  var ctx = cv.getContext('2d');
  var img = ctx.createImageData(d.w, d.h);
  for (var i = 0; i < d.w * d.h; i++) {
    var v = parseInt(d.cells.charAt(i), 16);
    if (isNaN(v)) { v = 0; }
    var rgb = dzColour(v, d.colour);
    img.data[i * 4] = rgb[0];
    img.data[i * 4 + 1] = rgb[1];
    img.data[i * 4 + 2] = rgb[2];
    img.data[i * 4 + 3] = 255;
  }
  ctx.putImageData(img, 0, 0);
  document.getElementById('dz-status').textContent =
    d.w + '\\u00d7' + d.h + (d.colour ? ' colour' : ' black-and-white')
    + ' \\u2014 picture at ' + d.base.toString(16).toUpperCase()
    + 'h, format ' + d.format.toString(16).toUpperCase() + 'h';
}
function vdmPoll() {
  if (vdmCurrent === null) { return; }
  fetch('/vdm/frame?id=' + vdmCurrent)
    .then(function(r) { return r.json(); })
    .then(vdmPaint)
    .catch(function() {});
}
function vdmRefreshList() {
  fetch('/vdm/list').then(function(r) { return r.json(); }).then(function(data) {
    var sel = document.getElementById('vdm-id');
    var note = document.getElementById('vdm-note');
    var want = sel.value;
    var built = '';
    for (var i = 0; i < data.screens.length; i++) {
      var s = data.screens[i];
      built += '<option value=\"' + s.id + '\">' + vdmEsc(s.label)
             + (s.vdm ? ' (VDM-1)' : '') + (s.dazzler ? ' (Dazzler)' : '') + '</option>';
    }
    /* Rebuilt only when it changed, so the list refresh cannot steal a
       selection the operator just made. */
    if (sel.innerHTML !== built) {
      sel.innerHTML = built;
      if (want) { sel.value = want; }
      if (!sel.value && data.screens.length > 0) { sel.selectedIndex = 0; }
      vdmCurrent = sel.value ? parseInt(sel.value, 10) : null;
    }
    if (data.screens.length === 0) {
      vdmCurrent = null;
      note.textContent = 'No disk is booted right now.';
      document.getElementById('vdm-status').textContent = '';
    } else {
      note.textContent = '';
    }
  }).catch(function() {});
}
/* What one key press is, in bytes.
   A browser sends ASCII whoever is watching, so this is the ASCII a terminal
   would put on the wire and nothing more clever: the gateway then applies the
   SAME translation it applies to the session's own bytes, which is how the
   operator's backspace choice reaches the guest identically from either
   keyboard.  DEL for Backspace for exactly that reason - it is what a terminal
   sends, and `cpm_boot_backspace` is what decides where it lands. */
function vdmKeyBytes(e) {
  if (e.altKey || e.metaKey) { return null; }
  if (e.ctrlKey) {
    if (e.key.length !== 1) { return null; }
    var c = e.key.toUpperCase().charCodeAt(0);
    /* Ctrl-A..Ctrl-_ , which is how a guest is sent Ctrl-C to break out and
       Ctrl-S to pause - GDEMO asks for that one by name. */
    if (c >= 64 && c <= 95) { return [c - 64]; }
    return null;
  }
  switch (e.key) {
    case 'Enter': return [13];
    case 'Backspace': return [127];
    case 'Tab': return [9];
    case 'Escape': return [27];
  }
  if (e.key.length === 1) {
    var b = e.key.charCodeAt(0);
    if (b >= 32 && b < 127) { return [b]; }
  }
  return null;
}
function vdmSendKeys(bytes) {
  var enc = '';
  for (var i = 0; i < bytes.length; i++) {
    enc += '%' + ('0' + bytes[i].toString(16)).slice(-2);
  }
  fetch('/vdm/key', {
    method: 'POST',
    headers: {'Content-Type': 'application/x-www-form-urlencoded'},
    body: 'id=' + vdmCurrent + '&k=' + enc
  }).then(function(r) { return r.json(); }).then(function(d) {
    if (!d.typed && d.why === 'off') {
      /* The operator turned typing off while this page was open.  Say so once
         rather than swallowing every keystroke silently. */
      VDM_INPUT = false;
      vdmKbNote();
    }
  }).catch(function() {});
}
/* ---- Joystick -------------------------------------------------------------
   The board is a LEVEL, not a stream of presses, so the page reports the whole
   set of held keys and lets the gateway time the swing.  Two consequences that
   are the point of the design:

   * every report carries every key, so one dropped request is corrected by the
     next rather than leaving a direction stuck;
   * the ramp is not computed here.  The guest reads its ports tens of thousands
     of times a second and this page can only speak on its own poll, so a level
     computed in the browser would arrive in visible steps.  We say what is
     held; the swing is the board's arithmetic.

   A repeat heartbeat exists because the gateway centres everything if it has
   not heard for VDM_JOY_IDLE_MS -- which is what makes a closed tab let go of
   the helm. */
var JOY_MASK = 0;
/* Whether the stick is live.  Seeded from the checkbox rather than written here,
   which is what keeps the two from disagreeing: the panel renders pre-ticked
   when the gateway has the board enabled -- an operator who switched the board
   on in the configuration meant to play, not to find another switch off -- and a
   browser restoring a soft-reloaded page hands back the state the player last
   chose, which a constant here would silently overrule. */
var JOY_ON = false;
var JOY_SEEN = false;
var joyBeat = null;

function joyBit(e) {
  if (!JOY_ON || e.ctrlKey || e.altKey || e.metaKey) { return 0; }
  var k = (e.key || '').toLowerCase();
  return VDM_JOY_KEYS[k] || 0;
}

function joySend() {
  if (vdmCurrent === null) { return; }
  fetch('/vdm/joy', {
    method: 'POST',
    headers: {'Content-Type': 'application/x-www-form-urlencoded'},
    body: 'id=' + vdmCurrent + '&m=' + JOY_MASK
  }).then(function(r) { return r.json(); }).then(function(d) {
    if (!d.held && d.why === 'off') {
      /* The operator turned the board off while this page was open. */
      JOY_ON = false;
      var box = document.getElementById('joy-on');
      if (box) { box.checked = false; }
      joyStopBeat();
      joyNote();
    }
  }).catch(function() {});
}

/* While anything is held, keep saying so: the gateway lets go on silence, and
   silence is how a closed tab is told apart from a steady hand. */
function joyStartBeat() {
  if (joyBeat === null) {
    joyBeat = setInterval(function() {
      if (JOY_MASK !== 0) { joySend(); } else { joyStopBeat(); }
    }, Math.max(100, Math.floor(VDM_JOY_IDLE_MS / 3)));
  }
}
function joyStopBeat() {
  if (joyBeat !== null) { clearInterval(joyBeat); joyBeat = null; }
}

function joyKeydown(e) {
  var bit = joyBit(e);
  if (!bit) { return false; }
  e.preventDefault();
  if ((JOY_MASK & bit) === 0) {
    JOY_MASK |= bit;
    joySend();
    joyPaint();
  }
  joyStartBeat();
  return true; /* handled: this letter is a control, not a character */
}

function joyKeyup(e) {
  var bit = joyBit(e);
  if (!bit) { return; }
  e.preventDefault();
  if ((JOY_MASK & bit) !== 0) {
    JOY_MASK &= ~bit;
    joySend();
    joyPaint();
  }
}

function joyRelease() {
  if (JOY_MASK !== 0) {
    JOY_MASK = 0;
    joySend();
    joyPaint();
  }
  joyStopBeat();
}

/* Light the keys that are down, so a player can see the page is hearing them
   -- a stick with no visible position is indistinguishable from a broken one. */
function joyPaint() {
  var keys = document.querySelectorAll('.joy-key');
  for (var i = 0; i < keys.length; i++) {
    var kbd = keys[i].querySelector('kbd');
    if (!kbd) { continue; }
    var bit = VDM_JOY_KEYS[kbd.textContent.toLowerCase()] || 0;
    if (bit && (JOY_MASK & bit) !== 0) {
      keys[i].classList.add('joy-live');
    } else {
      keys[i].classList.remove('joy-live');
    }
  }
}

function joyNote() {
  var el = document.getElementById('joy-note');
  if (!el) { return; }
  if (!JOY_ON) {
    el.textContent = 'Off — those ten letters type at the guest.';
    return;
  }
  if (vdmCurrent === null) {
    el.textContent = 'On — choose a session above.';
    return;
  }
  /* Whether the guest has actually READ the board, which is the one thing the
     picture cannot tell a player: a program that wants no joystick looks
     exactly like a joystick that is not working. */
  el.textContent = JOY_SEEN
    ? 'On — this program is reading the joystick. Click the screen and play.'
    : 'On — click the screen and play. This program has not read the joystick yet.';
}

(function() {
  var box = document.getElementById('joy-on');
  if (!box) { return; }
  JOY_ON = box.checked;
  box.addEventListener('change', function() {
    JOY_ON = this.checked;
    if (!JOY_ON) { joyRelease(); }
    joyNote();
    vdmKbNote();
    var stage = document.getElementById('vdm-stage');
    if (JOY_ON && stage) { stage.focus(); }
  });
})();

function vdmKbNote() {
  var el = document.getElementById('vdm-kb');
  if (!VDM_INPUT) {
    el.textContent = 'This gateway does not accept typing from the browser'
      + ' (cpm_screen_input is off).';
    return;
  }
  var here = document.activeElement === document.getElementById('vdm-stage');
  if (!here) {
    el.textContent = 'Click the screen to type at this guest.';
    return;
  }
  /* The joystick's ten letters are the exception, so say so while it is on
     rather than promising typing this page is about to intercept. */
  el.textContent = JOY_ON
    ? 'Typing goes to the guest, except the ten joystick letters.'
      + ' The terminal that started the session can type too.'
    : 'Typing goes to the guest. The terminal that started the session can type too.';
}
(function() {
  var stage = document.getElementById('vdm-stage');
  stage.addEventListener('focus', vdmKbNote);
  stage.addEventListener('blur', vdmKbNote);
  stage.addEventListener('keydown', function(e) {
    /* The joystick first, and only while it is switched on: its ten letters
       are ordinary printable characters, so a player holding W must not also
       type a W at the guest.  When it is off they type, exactly as before. */
    if (joyKeydown(e)) { return; }
    if (!VDM_INPUT || vdmCurrent === null) { return; }
    var bytes = vdmKeyBytes(e);
    if (!bytes) { return; }
    /* Taken, so Backspace does not navigate and Tab does not move focus out of
       a screen somebody is typing at. */
    e.preventDefault();
    vdmSendKeys(bytes);
  });
  stage.addEventListener('keyup', joyKeyup);
  /* **A stick must not stay pushed when the page stops being played.**  A
     key-up that never arrives is the one failure a level-based control has and
     a keystroke queue does not: blur, a tab switch, or the window going away
     all leave a finger down for ever otherwise. */
  stage.addEventListener('blur', joyRelease);
  window.addEventListener('blur', joyRelease);
  document.addEventListener('visibilitychange', function() {
    if (document.hidden) { joyRelease(); }
  });
  vdmKbNote();
  joyNote();
})();
document.getElementById('vdm-id').addEventListener('change', function() {
  /* Let go of the stick before leaving: the session being left would otherwise
     hold whatever was pushed until its idle release, and a guest left with the
     helm over because somebody changed sessions is nobody's intention. */
  joyRelease();
  vdmCurrent = this.value ? parseInt(this.value, 10) : null;
  JOY_SEEN = false;
  joyNote();
  vdmPoll();
});
vdmRefreshList();
setInterval(vdmRefreshList, VDM_LIST_MS);
setInterval(vdmPoll, VDM_POLL_MS);
</script>";

// ─── HTML rendering ─────────────────────────────────────────────────

/// Build the full configuration page.  `notice` is an optional banner
/// shown above the form (used to confirm a save).
/// `show_port_check` opens the result modal on load -- set only by the redirect
/// a port check makes, so the modal belongs to the page that asked for it.
/// The resolvable-problems panel, empty when there is nothing to resolve.
///
/// Each problem states what it means *before* its button, and for a changed host
/// key that includes saying it is indistinguishable from a man-in-the-middle --
/// the operator is about to discard the evidence, so the page says so rather
/// than offering a friendly one-click fix. The button carries the problem's id
/// in a hidden field, so two problems cannot be confused by position.
fn render_resolve_panel() -> String {
    let problems = crate::resolve::list();
    if problems.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("<div class=\"notice\" style=\"border-color:#c33\">");
    out.push_str("<strong>Problems needing your decision</strong>");
    for p in &problems {
        out.push_str(&format!("<div style=\"margin-top:.6em\"><strong>{}</strong><br>", html_escape(&p.title())));
        // The explanation is written as narrow lines for the C64 screens; joined
        // with spaces it reads as prose here, and the blank lines become breaks.
        let mut para = String::new();
        for line in p.explain() {
            if line.is_empty() {
                para.push_str("<br><br>");
            } else {
                if !para.is_empty() && !para.ends_with("<br><br>") {
                    para.push(' ');
                }
                para.push_str(&html_escape(&line));
            }
        }
        out.push_str(&para);
        out.push_str(&format!(
            "<br><button type=\"submit\" name=\"resolve_id\" value=\"{}\" class=\"secondary\">{}</button>",
            html_escape(&p.id()),
            html_escape(&p.action()),
        ));
        out.push_str("</div>");
    }
    out.push_str("</div>");
    out
}

fn render_main_page(cfg: &Config, notice: Option<String>, show_port_check: bool) -> String {
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
    // Problems an operator can fix: first inside the form, and *inside* is the
    // point -- its buttons are `submit`s carrying the problem's id, so a panel
    // drawn before the form opens would render perfectly and do nothing.  Only
    // while there are any, because a panel that is always there is a panel
    // nobody reads.  See `crate::resolve`.
    out.push_str(&render_resolve_panel());
    out.push_str(&render_grid(cfg));
    out.push_str(&render_more_popups(cfg));
    if show_port_check {
        out.push_str(&render_port_check_modal());
    }
    out.push_str(&render_warning_popups());
    out.push_str(&render_scripture_and_logo());
    out.push_str("</form>");
    out.push_str(&render_console());
    out.push_str(SCRIPT);
    out.push_str("</body></html>");
    out
}

/// Where "User Manual" goes, on this page and in the desktop GUI.
///
/// One constant rather than two literals: the GUI opens this same URL from its
/// own button, and a manual link that disagreed between the two surfaces would
/// send two operators to two different documents.
pub(crate) const MANUAL_URL: &str =
    "https://github.com/rickybryce/ethernetgateway/blob/master/usermanual.pdf";

fn render_header(cfg: &Config) -> String {
    let ip = local_ip();
    format!(
        // The manual link is the same destination the desktop GUI's "User
        // Manual" button opens, so the two surfaces send an operator to the
        // same document.  `target=_blank` because this page is a form the
        // operator may be part-way through filling in -- navigating away from
        // it in place would discard their edits.
        "<header><h1>Ethernet Gateway v{ver}</h1>\
         <div class=\"server-ip\">Server IP: <code>{ip}</code> \
         <a class=\"linkbtn\" href=\"{manual}\" target=\"_blank\" \
         rel=\"noopener\">User Manual</a></div>\
         </header>\
         <div class=\"hint\">Telnet: {tport} &middot; SSH: {sport} &middot; Kermit: {kport} &middot; Web: {wport}</div>",
        ver = env!("CARGO_PKG_VERSION"),
        ip = html_escape(&ip),
        manual = MANUAL_URL,
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
         {telnet_chk}{telnet_label}{telnet_port}\
         {web_chk}{web_label}{web_port}\
         <button type=\"button\" class=\"more{more_alert}\" data-target=\"more-server\" title=\"{more_hint}\">More\u{2026}</button>\
         {ssh_chk}{ssh_label}{ssh_port}\
         {kermit_chk}{kermit_label}{kermit_port}\
         <span class=\"grid-blank\"></span>\
         </div></section>",
        save = save_button("save_and_restart", "Save and Restart", "primary"),
        more_alert = if crate::portcheck::results().iter().any(|(_, _, r)| r.is_blocked()) {
            " alert"
        } else {
            ""
        },
        more_hint = {
            let n = crate::portcheck::results().iter().filter(|(_, _, r)| r.is_blocked()).count();
            if n > 0 {
                format!("{n} bound port(s) did not answer when this machine connected to them at its own network address. Open More… to test again.")
            } else {
                "More server settings, and the port check.".to_string()
            }
        },
        telnet_label = port_label("telnet"),
        web_label = port_label("web"),
        ssh_label = port_label("SSH"),
        kermit_label = port_label("Kermit"),
        telnet_chk = checkbox("telnet_enabled", "Telnet", cfg.telnet_enabled),
        telnet_port = port_input_for("telnet_port", cfg.telnet_port, None),
        ssh_chk = checkbox("ssh_enabled", "SSH", cfg.ssh_enabled),
        ssh_port = port_input_for("ssh_port", cfg.ssh_port, None),
        web_chk = checkbox_with_attr(
            "web_enabled",
            "Web Server",
            cfg.web_enabled,
            "onchange=\"warnIfDisablingWeb(this)\"",
        ),
        web_port = port_input_for(
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
        kermit_port = port_input_for("kermit_server_port", cfg.kermit_server_port, None),
    )
}

/// Six characters is enough for any valid TCP port (65535 = 5 digits) plus a
/// touch of breathing room.  When `extra_attr` is provided the attribute string
/// is appended verbatim (used for the web-port onchange warning) and a
/// `data-orig` carries the current value so the warning JS can detect changes.
/// The `Port:` label for one listener, red when the last check found it blocked.
///
/// **The label carries the signal, not a tag beside it.** The Server frame is a
/// fixed seven-column grid laid out by position; an extra item per row would
/// shear every row after it. Colour changes no metrics — which is why the CSS
/// sets a colour and deliberately not a weight: the columns are `max-content`,
/// so bolding this word would widen its column and de-align the colons the grid
/// exists to line up.
///
/// Only a blocked port is coloured — a pass is not evidence, see
/// [`crate::portcheck`].
fn port_label(listener: &str) -> String {
    match crate::portcheck::result_of(listener) {
        Some((port, reach)) if reach.is_blocked() => format!(
            "<span class=\"port-label port-blocked\" title=\"{}\">Port:</span>",
            html_escape(&reach.hover(port).unwrap_or_default())
        ),
        _ => "<span class=\"port-label\">Port:</span>".to_string(),
    }
}

/// A port-number `<input>` for the Server-frame grid.
///
/// The input and its marker are wrapped in one cell rather than added as a
/// second grid child: the Server frame is a fixed seven-column grid, and an
/// eighth item per row would shear every row after it.
///
/// **Only a blocked port draws anything.** There is deliberately no green
/// "open" marker to balance it — a self-connection does not meet the firewall
/// at all on Windows or macOS, so a pass is not evidence. See
/// [`crate::portcheck`].
fn port_input_for(name: &str, value: u16, extra_attr: Option<&str>) -> String {
    let attr = extra_attr.unwrap_or("");
    let input = format!(
        "<input type=\"text\" inputmode=\"numeric\" name=\"{name}\" value=\"{value}\" size=\"6\" class=\"port-num\" data-orig=\"{value}\" {attr}>",
        name = name,
        value = value,
        attr = attr,
    );
    input
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
    // Three rows: title+Save, the weather location, and Home with a
    // right-aligned "More…" button.  The Groq key and the weather units live in
    // the `more-ai` modal (render_more_popups), mirroring the GUI.
    //
    // **The API key used to be this row and was moved out deliberately.** It is
    // optional — AI chat is the only thing that wants one, and everything else
    // here works without it — but a key field at the top of a frame reads as
    // something you must fill in before the product works. The weather location
    // is the opposite: a field with no consequences that shows what the frame is
    // for.
    //
    // The booted disk's screen sits at the right edge of the middle row,
    // between this frame's Save and its More…, so the three right-hand
    // controls line up.  It is in *this* frame because it belongs to the CP/M
    // half of it, and it was tried in the header's ports line first, where it
    // read as one more small italic note rather than something to click.  An
    // anchor rather than a button: it navigates away, and a bare `<button>`
    // inside this form would submit it.
    //
    // **Named for the disk, not for a card.**  It said "VDM-1 Screen" while
    // that was the only card there was; a machine with a Dazzler and no VDM-1
    // — which `DISK10` is — would have sent the operator looking for a button
    // named after hardware their guest does not have.
    format!(
        "<section class=\"frame\"><div class=\"frame-head\">\
         <span class=\"title\">AI Chat, Browser, Weather &amp; CP/M</span>\
         <span class=\"head-right\">{save}</span></div>\
         <div class=\"row\"><span class=\"label\">Weather location:</span>\
         <input type=\"text\" name=\"weather_location\" value=\"{loc}\" \
         placeholder=\"city or postal code\">\
         <a class=\"row-right linkbtn\" href=\"/vdm\">VDM / Dazzler</a></div>\
         <div class=\"row\"><span class=\"label\">Home:</span>\
         <input type=\"text\" name=\"browser_homepage\" value=\"{home}\">\
         <button type=\"button\" class=\"more\" data-target=\"more-ai\">More\u{2026}</button></div>\
         </section>",
        save = save_button("save", "Save", "secondary"),
        loc = html_escape(&cfg.weather_location),
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
    let mounts = crate::cpm::image::registry::all();
    let usage = crate::cpm::image::registry::usage();

    // Resolved, not read off the key — a `cpm_boot_image` naming a disk that is
    // no longer there runs the emulator, and this page has to name the slots
    // the machine that starts will have.  Once per request, beside the folder
    // listing above.  The same `MountContext` the telnet and desktop screens
    // use, so the three cannot disagree about what a drive is called or which
    // images may go in one.
    let ctx = crate::cpm::boot::MountContext::resolve(
        &cfg.transfer_dir,
        &cfg.cpm_boot_image,
        &cfg.cpm_boot_machine,
    );
    let naming = ctx.naming.clone();
    let booting = ctx.booting();
    // Only images the machine that will run could reach: with a disk booting,
    // the board is chosen by size, so an image on another board mounts
    // perfectly and is invisible to the guest.
    let dir = crate::cpm::image::images_dir(&base);
    // Only files we could read, and only counted as hidden when a disk is
    // actually booting: a file that vanished between the listing and the stat is
    // not "on the wrong board", and with the emulator running nothing is.
    let mut hidden_images = 0usize;
    let mut images: Vec<String> = Vec::new();
    for n in crate::cpm::image::available_images(&base) {
        match std::fs::metadata(dir.join(&n)) {
            Ok(m) if ctx.accepts(m.len()) => images.push(n),
            Ok(_) => hidden_images += 1,
            Err(_) => {}
        }
    }
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
        // **Slot 0 is reserved while a disk boots.** Empty, it shows the disk
        // holding it and is not selectable; with something mounted it stays
        // editable, because a mount left behind the boot disk must be removable
        // without first clearing `cpm_boot_image`.  A disabled `select` submits
        // nothing, which `apply_cpm_mount_form` reads as "keep whatever is
        // there" -- and there is nothing there, so the two agree.
        let reserved_for_boot = drive0 == 0 && booting && mounted.is_none();
        let disabled = if busy.is_some() || reserved_for_boot { " disabled" } else { "" };

        // A reserved slot shows what reserved it, selected, so the control is not
        // an empty box beside a note saying it is occupied.
        let mut opts = if reserved_for_boot {
            format!(
                "<option value=\"\" selected>{}</option>",
                html_escape(ctx.boot_disk_name().unwrap_or("(booted disk)"))
            )
        } else {
            String::from("<option value=\"\">(drive folder)</option>")
        };
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
                // Two different reasons a mounted image is not in the list, and
                // they are not interchangeable: the file may be gone, or it may
                // be sitting in the folder and filtered out because the booted
                // disk's board cannot reach it.  Saying "missing from folder"
                // for the second sends the operator hunting for a file that is
                // right where they left it.
                let present = std::fs::metadata(dir.join(&m.filename)).is_ok();
                opts.push_str(&format!(
                    "<option value=\"{}\" selected>{} ({})</option>",
                    html_escape(&m.filename),
                    html_escape(&m.filename),
                    if present { "not on the booted disk's board" } else { "missing from folder" },
                ));
            }
        }

        let mut note = String::new();
        if let Some(m) = mounted {
            if crate::cpm::boot::mount_refuses_writes(&naming, m) {
                // The reason is our BDOS's and only fits its own verdict; under
                // a booted disk the one cause left is the host's own refusal.
                note.push_str(&format!(
                    " <span class=\"sub\">read-only: {}</span>",
                    html_escape(if booting {
                        "the image file is read-only on the host"
                    } else {
                        &m.read_only_reason
                    })
                ));
            }
        }
        if let Some(b) = &busy {
            note.push_str(&format!(" <span class=\"sub\">{}</span>", html_escape(b)));
        }
        // Under a booted disk the slot is a number on a board, not one of our
        // drive letters, and whether the guest reaches it is its own BIOS's
        // business.  Same `cpm_mounts` underneath; only the name differs.
        if booting {
            let len = mounted
                .and_then(|m| std::fs::metadata(&m.path).ok())
                .map(|md| md.len());
            // The board is named here and not in the telnet rows: a web page has
            // room for it, a 40-column PETSCII screen does not.
            // Both halves from the same place.  The board used to come from
            // *this row's* image while the slot name came from the booted disk,
            // so a mount left over from a different boot setting could render
            // `unit 0.1 on the MITS 88-DCDD` — the very mixture this is for.
            let board = ctx
                .board()
                .map(|b| format!(" on the {b}"))
                .unwrap_or_default();
            let _ = len;
            note.push_str(&format!(
                " <span class=\"sub\">{}{}</span>",
                html_escape(&ctx.slot(drive0)),
                html_escape(&board),
            ));
        }
        if drive0 == 0 {
            // One text for three surfaces, and it names the disk (see
            // `MountContext::boot_slot_note`).
            match ctx.boot_slot_note() {
                Some(n) => {
                    note.push_str(&format!(" <span class=\"sub\">{}</span>", html_escape(&n)));
                    // A mount underneath the boot disk is kept but unreachable.
                    // Said here, where it can still be changed, rather than only
                    // at boot time on another screen.
                    if mounted.is_some() {
                        note.push_str(&format!(
                            " <span class=\"warn\">{}</span>",
                            html_escape(crate::cpm::boot::BEHIND_BOOT_DISK)
                        ));
                    }
                }
                None => note.push_str(
                    " <span class=\"sub\">A: hides the terminals while mounted</span>",
                ),
            }
        }
        rows.push_str(&format!(
            "<div class=\"row\"><span class=\"label drive\">{letter} :</span>\
             <select name=\"cpm_mount_{}\"{}>{}</select>{}</div>",
            letter.to_ascii_lowercase(),
            disabled,
            opts,
            note
        ));
    }
    // **What is actually running, which no mount row can show.** A booted image
    // is not on one of our drives at all -- it is its board's slot 0, and the
    // guest's own operating system decides what to call it -- so it belongs in
    // its own list rather than folded into the drive letters above. The telnet
    // screen has carried this since 0.9.2; the web page and the desktop showed
    // nothing, so an image could be offered here, refused on Save as "being run
    // by a booted session", and accounted for nowhere (reported 2026-08-21).
    let booted = crate::cpm::image::registry::booted_to_report();
    if !booted.is_empty() {
        rows.push_str("<div class=\"row\"><span class=\"label drive\">Booted:</span><span>");
        for name in &booted {
            rows.push_str(&format!(
                "{} <span class=\"sub\">(running)</span><br>",
                html_escape(name)
            ));
        }
        rows.push_str(
            "<span class=\"sub\">Running its own operating system — not on a drive of \
             ours, and not mountable while it runs.</span></span></div>",
        );
    }

    let intro = if images.is_empty() && hidden_images > 0 {
        format!(
            "<div class=\"row\"><span class=\"sub\">None of the {hidden_images} image{} in {}/images \
             {} on the booted disk's board, so its operating system could not read {}. Change what \
             boots, or add a disk of the right kind.</span></div>",
            if hidden_images == 1 { "" } else { "s" },
            html_escape(&base.display().to_string()),
            if hidden_images == 1 { "is" } else { "are" },
            if hidden_images == 1 { "it" } else { "them" },
        )
    } else if images.is_empty() {
        format!(
            "<div class=\"row\"><span class=\"sub\">No images found. Put .dsk files in {}/images — readme.txt there explains the naming — or make an empty one below.</span></div>",
            html_escape(&base.display().to_string())
        )
    } else {
        String::from(
            "<div class=\"row\"><span class=\"sub\">A mounted drive uses the files inside the image instead of the files in its folder. The folder's files are not touched and return when you unmount.</span></div>",
        )
    };
    // Never let an image disappear from the list without saying so.  A folder
    // the operator can see the contents of, offering fewer disks than it holds,
    // is a mystery; naming the reason makes it something they can act on --
    // change what boots, or fetch a disk of the right kind.
    // Not beside "No images found": that pair reads as a contradiction, and the
    // empty case has already been given the real reason by the branch above --
    // which is what the telnet screen does.  Only appended when there is a list
    // for it to qualify.
    let intro = if hidden_images > 0 && !images.is_empty() {
        format!(
            "{intro}<div class=\"row\"><span class=\"sub\">{hidden_images} more image{} in the \
             folder {} not offered: with a disk set to boot, an image only reaches the guest if it \
             lands on the same board, and the board is chosen by the image's size. A floppy beside \
             a booted hard disk mounts perfectly and is invisible to it.</span></div>",
            if hidden_images == 1 { "" } else { "s" },
            if hidden_images == 1 { "is" } else { "are" },
        )
    } else {
        intro
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
         {intro}{get}{rows}{create}\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        save = save_button("save", "Save", "secondary"),
        get = get_disks_row(&images),
    )
}

/// The offer to fetch the sample disks.
///
/// Above the drive rows, because a fresh install has nothing to mount and
/// "where do I get a disk" is the question this screen otherwise leaves the
/// operator holding.  Says the count, the size and **where they come from**
/// before they agree: the disks are not ours, and an operator who would rather
/// fetch them by hand should be able to see that and decline.
fn get_disks_row(images: &[String]) -> String {
    let all = crate::cpm::fetch::catalogue();
    let here: std::collections::HashSet<&str> = images.iter().map(|s| s.as_str()).collect();
    let wanted: Vec<_> = all.iter().filter(|d| !here.contains(d.name.as_str())).collect();
    if wanted.is_empty() {
        return String::from(
            "<div class=\"row\"><span class=\"sub\">Every sample disk this gateway is \
             known to run is already in the images folder.</span></div>",
        );
    }
    let mb = wanted.iter().map(|d| d.bytes).sum::<u64>() as f64 / (1024.0 * 1024.0);
    format!(
        "<div class=\"row\">{button}\
         <span class=\"sub\">{n} disks, {mb:.0} MB, from {src} \u{2014} only the ones known to \
         run here. They are not ours; this fetches them for you, and anything already in the \
         folder is left alone.</span></div>",
        button = save_button("getdisks", "Download sample disks", "secondary"),
        n = wanted.len(),
        src = html_escape(&crate::cpm::fetch::source_repos().join(" and ")),
    )
}

/// The port-check result modal, open on load.
///
/// **A modal, because the banner scrolls past and nobody reads the console.**
/// The red `Port:` labels say *which* listener, but only to somebody already
/// looking at the Server frame; whoever just pressed Test ports is owed the
/// answer where they are.
///
/// It says "answered", never "open". A pass is not evidence — on Windows and
/// macOS a connection to your own address skips the firewall entirely — and
/// reporting it as open is the one mistake an operator would act on, going to
/// look at their router while something local drops every connection.
fn render_port_check_modal() -> String {
    let results = crate::portcheck::results();
    let blocked = results.iter().filter(|(_, _, r)| r.is_blocked()).count();
    let mut rows = String::new();
    if results.is_empty() {
        rows.push_str(
            "<div class=\"row\"><span class=\"sub\">No listener is bound, so there was \
             nothing to test.</span></div>",
        );
    }
    for (name, port, reach) in &results {
        rows.push_str(&format!(
            "<div class=\"row\"><span class=\"label\">{name} {port}</span>{verdict}</div>",
            name = html_escape(name),
            port = port,
            // Three states, not two: a probe that never reached a
            // connection attempt is not an answer, and reporting it as one
            // would be an all-clear this check did not earn.
            verdict = if reach.is_blocked() {
                format!(
                    "<span class=\"port-blocked\" style=\"font-weight:700\">{}</span>",
                    html_escape(&reach.verdict_phrase())
                )
            } else {
                format!(
                    "<span class=\"sub\">{}</span>",
                    html_escape(&reach.verdict_phrase())
                )
            },
        ));
    }
    format!(
        "<div class=\"modal open\" id=\"port-check\"><div class=\"modal-body warn\">\
         <div class=\"modal-head\"><span class=\"title\">{title}</span>\
         <button type=\"button\" class=\"close\" data-close=\"port-check\">&times;</button></div>\
         {rows}\
         {blocked_note}\
         <div class=\"row\"><span class=\"hint\">&ldquo;Answered&rdquo; is not the same as \
         reachable, and what this test can prove depends on the platform:</span></div>\
         {platforms}\
         <div class=\"row\"><span class=\"hint\">{closing}</span></div>\
         </div></div>",
        title = if blocked > 0 { "Port test — something is blocking" } else { "Port test" },
        // The same table the desktop popup and the manual render, from the one
        // source in `portcheck` -- a capability claim that drifted between
        // surfaces would be worse than not making it.
        platforms = {
            let here = crate::portcheck::this_platform();
            let head = ["Linux", "Windows", "macOS"]
                .iter()
                .map(|n| {
                    format!(
                        "<th{}>{n}</th>",
                        if here == Some(n) { " class=\"pc-here\"" } else { "" }
                    )
                })
                .collect::<String>();
            let rows = crate::portcheck::WHAT_THE_TEST_PROVES
                .iter()
                .map(|f| {
                    let cells = [("Linux", f.linux), ("Windows", f.windows), ("macOS", f.macos)]
                        .iter()
                        .map(|(n, v)| {
                            let cls = if here == Some(n) {
                                if *v == "yes" { " class=\"pc-here pc-yes\"" } else { " class=\"pc-here pc-no\"" }
                            } else {
                                ""
                            };
                            format!("<td{cls}>{v}</td>")
                        })
                        .collect::<String>();
                    format!("<tr><th scope=\"row\">{}</th>{cells}</tr>", html_escape(f.question))
                })
                .collect::<String>();
            format!(
                "<table class=\"pc-table\"><thead><tr><th></th>{head}</tr></thead>\
                 <tbody>{rows}</tbody></table>"
            )
        },
        // From the table's own first row rather than by naming a platform here.
        closing = if crate::portcheck::WHAT_THE_TEST_PROVES
            .first()
            .and_then(|f| f.here())
            != Some("yes")
        {
            "So on this platform a pass means very little: a connection to your own address \
             does not meet the firewall at all. Open the ports on your firewall and test from \
             another machine."
        } else {
            "Nothing here can see past this machine, so a router that is not forwarding a port \
             looks fine from in here. Open these ports on your firewall."
        },
        rows = rows,
        blocked_note = if blocked > 0 {
            "<div class=\"row\"><span class=\"hint\">A port that did not answer is being \
             blocked by something on this machine &mdash; a host firewall, or security \
             software.</span></div>"
        } else {
            ""
        },
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
         <div class=\"row\">{portcheck} <span class=\"sub\">{portcheck_note}</span></div>\
         <div class=\"row\"><span class=\"hint\">Remember to open these ports on your \
         firewall &mdash; a check from this machine cannot see a router that is not \
         forwarding them, and on Windows and macOS a connection to your own address \
         skips the firewall entirely.</span></div>\
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
         <h3>Telnet Gateway</h3>\
         <div class=\"row\">{tneg} {traw}</div>\
         <h3>SSH Gateway</h3>\
         <div class=\"row\"><span class=\"label\">Auth:</span>\
         <select name=\"ssh_gateway_auth\">\
         <option value=\"key\" {key_sel}>Key</option>\
         <option value=\"password\" {pwd_sel}>Password</option>\
         </select></div>\
         {gwpubkey}\
         <h3>Commodore (PETSCII) terminals</h3>\
         <div class=\"row\">{tpet}</div>\
         <h3>Terminal size reported to remote</h3>\
         <div class=\"row\">{gwcols} {gwrows}</div>\
         <div class=\"row\"><span class=\"hint\">{gwgeom_hint}</span></div>\
         {master_slave}\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        portcheck = save_button("portcheck", "Test ports", "secondary"),
        portcheck_note = {
            let blocked = crate::portcheck::results().iter().filter(|(_, _, r)| r.is_blocked()).count();
            if blocked > 0 {
                format!(
                    "{blocked} bound port{} did not answer &mdash; reddened below",
                    if blocked == 1 { "" } else { "s" }
                )
            } else if crate::portcheck::has_run() {
                "every bound port answered on this machine".to_string()
            } else {
                String::new()
            }
        },
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
            "Negotiate TTYPE / NAWS with remote (Telnet mode only)",
            cfg.telnet_gateway_negotiate,
            if cfg.telnet_gateway_raw { "disabled" } else { "" },
        ),
        // Its own heading, and deliberately NOT under "Telnet Gateway": it
        // governs the SSH Gateway too, and an SSH-only board is the case that
        // made the setting necessary at all, so filing it under Telnet would
        // hide it from the person who needs it most.  The desktop puts it in
        // the same place for the same reason.
        //
        // PETSCII only, and the label has to say so: on a screen an ANSI
        // operator also reads, an unqualified "translate" invites them to
        // turn off something that has never applied to them.  The hover text
        // carries the rest -- the row itself must stay short enough for the
        // column.
        tpet = checkbox_with_attr(
            "gateway_petscii_translate",
            "PETSCII: translate a remote's ANSI for the Commodore",
            cfg.gateway_petscii_translate,
            &format!("title=\"{}\"", crate::config::GATEWAY_PETSCII_TRANSLATE_HINT),
        ),
        traw = checkbox_with_attr(
            "telnet_gateway_raw",
            "Raw TCP mode (no telnet IAC layer)",
            cfg.telnet_gateway_raw,
            "onchange=\"updateGatewayFields()\"",
        ),
        key_sel = if cfg.ssh_gateway_auth == "key" { "selected" } else { "" },
        pwd_sel = if cfg.ssh_gateway_auth == "password" { "selected" } else { "" },
        // The gateway's own public key, for pasting into a remote's
        // `authorized_keys`.  Same condition and same source as the GUI
        // (`ssh::client_public_key_openssh`), so an operator reads the identical
        // string on either surface -- a key that differed between the two would
        // be the worst possible kind of disagreement here.
        //
        // Deliberately **unnamed**: a named control is submitted back on save,
        // and there is no key to write.  Read-only, and selected on click
        // because the whole point is to copy it.
        gwpubkey = if cfg.ssh_gateway_auth == "password" {
            String::new()
        } else {
            let key = match crate::ssh::client_public_key_openssh() {
                Ok(s) => s,
                Err(e) => format!("<could not load key: {e}>"),
            };
            format!(
                "<div class=\"row\"><span class=\"sub\">Gateway public key (paste into \
                 remote ~/.ssh/authorized_keys):</span></div>\
                 <div class=\"row\"><textarea class=\"pubkey\" readonly rows=\"2\" \
                 onclick=\"this.select()\">{}</textarea></div>",
                html_escape(&key)
            )
        },
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
         <h3>Log File</h3>\
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
            // The marker is `boot_setting_label`'s, not this page's.  It used
            // to be spelled here, which made this the only surface that said
            // anything — the desktop and telnet rows went on claiming a disk
            // was booting while the emulator was what started.
            let target = crate::cpm::boot::boot_target(&cfg.transfer_dir, &cfg.cpm_boot_image);
            choices.push((
                cfg.cpm_boot_image.clone(),
                crate::cpm::boot::boot_setting_label(&target, &cfg.cpm_boot_image),
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
    let cpm_machine_options: String = std::iter::once((
        crate::cpm::console::AUTO_MACHINE,
        crate::cpm::console::machine_label(crate::cpm::console::AUTO_MACHINE),
    ))
    .chain(crate::cpm::console::MACHINE_CHOICES.iter().map(|c| (c.key, c.description)))
    .map(|(key, label)| {
        format!(
            "<option value=\"{}\"{}>{}</option>",
            key,
            if key == cfg.cpm_boot_machine { " selected" } else { "" },
            html_escape(label),
        )
    })
    .collect();
    // The monitor ROM, with the state of its *file* in the option text. A
    // select that only names the setting would let an operator choose a ROM,
    // save, and find out at boot time that the file was never fetched -- so the
    // row that needs a download says so where the choice is made.
    let cpm_rom_options: String = crate::cpm::rom::ROM_CHOICES
        .iter()
        .map(|c| {
            let absent = c.rom.is_some()
                && crate::cpm::rom::missing(
                    &crate::cpm::layout::cpm_dir(&cfg.transfer_dir),
                    c.key,
                );
            format!(
                "<option value=\"{}\"{}>{}{}</option>",
                c.key,
                if c.key == cfg.cpm_boot_rom { " selected" } else { "" },
                html_escape(c.description),
                if absent { " \u{2014} file not here yet" } else { "" },
            )
        })
        .collect();
    let cpm_backspace_options: String = crate::cpm::boot::BACKSPACE_CHOICES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                value,
                // Compared through `backspace_erases`, not by string equality:
                // a hand-edited value neither choice matches must show as the
                // behaviour the gateway is really giving it, or saving this form
                // would silently change a setting nobody touched.
                if crate::cpm::boot::backspace_erases(&cfg.cpm_boot_backspace)
                    == crate::cpm::boot::backspace_erases(value)
                {
                    " selected"
                } else {
                    ""
                },
                html_escape(label),
            )
        })
        .collect();
    let cpm_printer_options: String = crate::cpm::printer::PRINTER_CHOICES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                value,
                if *value == cfg.cpm_printer.trim() { " selected" } else { "" },
                html_escape(label),
            )
        })
        .collect();
    // `off` first, then each board -- the same order the telnet screen cycles
    // in, so the two cannot present different lists.
    let cpm_printer_port_options: String = std::iter::once((
        crate::cpm::printer::PORT_OFF,
        crate::cpm::printer::PORT_OFF_LABEL,
    ))
    .chain(crate::cpm::printer::PORT_CHOICES.iter().map(|p| (p.key, p.label)))
    .map(|(value, label)| {
        format!(
            "<option value=\"{}\"{}>{}</option>",
            value,
            if value == cfg.cpm_printer_port.trim() { " selected" } else { "" },
            html_escape(label),
        )
    })
    .collect();
    let cpm_autolf_options: String = crate::cpm::printer::AUTOLF_CHOICES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                value,
                if *value == cfg.cpm_printer_autolf.trim() { " selected" } else { "" },
                html_escape(label),
            )
        })
        .collect();
    let cpm_cpu_options: String = crate::cpm::cpu::CPU_CHOICES
        .iter()
        .map(|(value, label)| {
            format!(
                "<option value=\"{}\"{}>{}</option>",
                value,
                // Compared through `is_8080`, not by string equality, for the
                // same reason as the backspace select above: a hand-edited
                // value neither choice matches must show as the processor the
                // gateway is really running, or saving this form would change
                // a setting nobody touched — and this one takes EGT8080 down.
                if crate::cpm::cpu::is_8080(&cfg.cpm_cpu) == crate::cpm::cpu::is_8080(value) {
                    " selected"
                } else {
                    ""
                },
                html_escape(label),
            )
        })
        .collect();
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
         <div class=\"row\"><span class=\"label\">Groq API Key (optional):</span>\
         <input type=\"password\" name=\"groq_api_key\" value=\"{key}\">\
         <span class=\"label\">AI model:</span>\
         <input type=\"text\" name=\"ai_model\" value=\"{aimodel}\">\
         <span class=\"hint\">Which Groq model AI Chat asks; blank uses the \
         shipped default. Groq <em>retires</em> models &mdash; the previous \
         default stopped existing and took AI Chat with it &mdash; so this is \
         how you move on without a new build. See \
         console.groq.com/docs/models for what is served now; if a model is \
         gone, the chat screen shows Groq&rsquo;s own error.</span>\
         <span class=\"hint\">AI Chat only. Everything else on this gateway \
         works without one; free at console.groq.com.</span></div>\
         <div class=\"row\"><span class=\"label\">Weather units:</span>\
         <select name=\"weather_units\">\
         <option value=\"auto\" {u_auto}>Auto</option>\
         <option value=\"us\" {u_us}>US (F/mph)</option>\
         <option value=\"metric\" {u_metric}>Metric (C/km/h)</option>\
         </select></div>\
         <div class=\"row\">{cpm}</div>\
         <div class=\"row\">{cpmscreen}</div>\
         <div class=\"row\">{cpmjoy}</div>\
         <div class=\"row\">{cpmspeed}</div>\
         <div class=\"row\">{cpmwrite}</div>\
         <div class=\"row\">{cpmmax}\
             <span class=\"hint\">Runaway ceiling for one CP/M emulator program, \
             in millions of instructions (2000 = 2 billion). A compute-bound \
             .COM that never reads the console is stopped at this count so the \
             A&gt; prompt always comes back. Minimum 1; anything above 1000000 \
             is capped at it rather than refused, so a value meant as \
             &quot;no limit&quot; is kept as far as it goes \u{2014} which at \
             emulated speed is over three months of continuous running. Bounds \
             one transient in the emulator only: a booted disk is the session \
             and has no ceiling.</span></div>\
         <div class=\"row\">{cpmprof}</div>\
         <div class=\"row\">{cpmecho}{cpmverb}{cpmquiet}</div>\
         <div class=\"row\">{cpmx}{cpmdcd}</div>\
         <div class=\"row\">{cpmsregs}</div>\
         <div class=\"row\">{cpmuart}</div>\
         <div class=\"row\">{cpmboot}</div>\
         <div class=\"row\">{cpmdisks}</div>\
         <div class=\"modal-foot\">{save}</div>\
         </div></div>",
        key = html_escape(&cfg.groq_api_key),
        aimodel = html_escape(&cfg.ai_model),
        u_auto = if cfg.weather_units == "auto" { "selected" } else { "" },
        u_us = if cfg.weather_units == "us" { "selected" } else { "" },
        u_metric = if cfg.weather_units == "metric" { "selected" } else { "" },
        cpm = checkbox(
            "cpm_emu_enabled",
            "CP/M Emulator (main menu; be sure you trust the CP/M files you run)",
            cfg.cpm_emu_enabled,
        ),
        cpmscreen = checkbox(
            "cpm_screen_input",
            "VDM / Dazzler screen may type at a booted disk (it is readable either way)",
            cfg.cpm_screen_input,
        ),
        cpmspeed = {
            let mut o = String::from(
                "<label>Booted-disk speed <select name=\"cpm_boot_speed\">",
            );
            for (value, label) in crate::cpm::speed::SPEED_CHOICES {
                let sel = if cfg.cpm_boot_speed.trim().eq_ignore_ascii_case(value) {
                    " selected"
                } else {
                    ""
                };
                let shown =
                    crate::cpm::speed::choice_label(value, label, &cfg.cpm_cpu);
                o.push_str(&format!("<option value=\"{value}\"{sel}>{shown}</option>"));
            }
            o.push_str("</select></label>");
            o
        },
        cpmjoy = checkbox(
            "cpm_joystick",
            "Joystick for a booted disk, played from the VDM / Dazzler screen (W A S Z X, I J K M N)",
            cfg.cpm_joystick,
        ),
        cpmwrite = checkbox(
            "cpm_boot_writable",
            "A booted disk may WRITE to its images (untick to discard its writes)",
            cfg.cpm_boot_writable,
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
             <select name=\"cpm_boot_image\">{cpm_boot_options}</select>\
             <span class=\"label\">Booted disk's machine:</span>\
             <select name=\"cpm_boot_machine\">{cpm_machine_options}</select>\
             <span class=\"hint\">Where a booted disk finds its console. \
             Ignored by the emulator, which has no console to place. A disk that \
             loads and then goes quiet is usually looking for a console that is \
             not there.</span>\
             <span class=\"label\">Booted disk's monitor ROM:</span>\
             <select name=\"cpm_boot_rom\">{cpm_rom_options}</select> {romget}\
             <span class=\"hint\">Some disks print through a routine that was \
             never on the disk, because on the machine they were built for it was \
             already in memory \u{2014} they boot into silence without it. \
             DISK11.DSK is one: it checks for CUTER at 0xC000 and says so. The \
             ROM files are not shipped; put one in CPM/roms/ or press \
             <em>Fetch monitor ROM</em>, which takes it from {cpm_rom_source} \
             pinned to one commit and checked against a recorded SHA-256. \
             Ignored by the emulator.</span>\
             <span class=\"label\">Booted disk's backspace key:</span>\
             <select name=\"cpm_boot_backspace\">{cpm_backspace_options}</select>\
             <span class=\"hint\">Most of these operating systems erase on BS and \
             read your terminal's DEL as a Teletype rubout \u{2014} deleting the \
             character and then printing the character they deleted. CP/M 1.x is \
             the opposite: the rubout is its editing key and BS prints a literal \
             ^H. This is the whole answer: set it to match the disk you \
             this.</span>\
             <span class=\"label\">Printer output:</span>\
             <select name=\"cpm_printer\">{cpm_printer_options}</select>\
             <span class=\"hint\">Where CP/M printer output goes. Reaches both \
             CP/M machines: in the emulator the printer is an OS service (BDOS 5 \
             and the BIOS LIST vector), and a booted disk drives the board below. \
             A document lands in a &quot;printer&quot; folder inside the transfer \
             directory \u{2014} its own folder so a printer left on does not \
             scatter files through yours \u{2014} named \
             PRINT-YYYYMMDD-HHMMSS, five seconds after the last character printed \
             \u{2014} and in the emulator also the moment the program returns to \
             A&gt;, which is exact. Off means printer output appears on the \
             terminal, as it always has. Bold and underline survive into an \
             .odt: period software does not ask for them, it OVERSTRIKES \
             \u{2014} WordStar prints the line, sends a bare CR and reprints just \
             the emphasised run at the same columns \u{2014} and that becomes real \
             styling.</span>\
             <span class=\"label\">Booted disk's printer board:</span>\
             <select name=\"cpm_printer_port\">{cpm_printer_port_options}</select>\
             <span class=\"label\">Bare carriage return:</span>\
             <select name=\"cpm_printer_autolf\">{cpm_autolf_options}</select>\
             <span class=\"hint\">Does a bare CR advance the paper? The DIP \
             switch a real printer interface carried, and it carried one because \
             the bytes cannot say. Both meanings are in use by period software on \
             the same board, and both were measured here: Altair Hard Disk \
             BASIC's LPRINT sends ALPHA&lt;CR&gt;BETA&lt;CR&gt; with no line feed \
             at all, so a bare CR is its line ending; WordStar 3.0, installed for \
             a &quot;Teletype-like printer&quot;, emphasises by OVERSTRIKING \
             \u{2014} it prints the line, sends a bare CR and reprints just the \
             bold run at the same columns. Turn it on and WordStar's emphasis \
             lands on lines of its own; turn it off and BASIC prints its whole \
             report on one line. Auto keeps whatever was measured for the printer \
             in question.</span>\
             <span class=\"hint\">Ignored by the emulator, whose printer is a \
             BDOS service with no port at all, and ignored entirely when printer \
             output is off. Measured against real software: Altair Hard Disk \
             BASIC answering LINEPRINTER? C sends one character per byte to data \
             register 03h.</span>\
             <span class=\"label\">CPU:</span>\
             <select name=\"cpm_cpu\">{cpm_cpu_options}</select>\
             <span class=\"hint\">The one setting here that applies to the \
             emulator as well as to a booted disk. The Z80 runs the 8080 \
             software these disks are made of; the 8080 is the processor the \
             Altair shipped with, and is what period diagnostics that identify \
             the CPU from DCR A expect. <b>EGT8080.COM</b> is placed on drive \
             A:: built to the 8080's instruction set, so it runs on either \
             setting.</span>",
            // Offered beside the select rather than only where the disks are
            // fetched, because a ROM is chosen here: an operator who picks one
            // and saves would otherwise learn at boot time that the file was
            // never downloaded.  The action reads the ROM out of the submitted
            // form, so choosing and fetching in one visit works.
            romget = save_button("getrom", "Fetch monitor ROM", "secondary"),
            // Named rather than "its author's repository": an operator deciding
            // whether to trust a download needs the name, and it is derived from
            // the pinned URL so the page cannot name the wrong one.
            cpm_rom_source = crate::cpm::rom::ROM_CHOICES
                .iter()
                .filter_map(|c| c.rom.as_ref())
                .map(|f| f.source())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(" and "),
        ),
        cpmdisks = {
            // Before the mount button, and only while there is something to
            // fetch: an operator can choose a *boot* disk from this popup
            // without ever opening the mount screen, so this is the one place
            // they are certain to pass through.
            let base = crate::cpm::layout::cpm_dir(&cfg.transfer_dir);
            // Disks *and* monitor ROMs, from the downloader itself: gating on the
            // disks alone hid this button from everybody who already had the
            // collection, and the ROM would then never arrive by this route.
            let (disks, roms) = crate::cpm::fetch::outstanding(&base);
            let get = if disks.is_empty() && roms.is_empty() {
                String::new()
            } else {
                // Says what is really outstanding: "0 sample disks not here yet"
                // beside a button that is about to fetch a ROM reads as a bug.
                let what = match (disks.len(), roms.len()) {
                    (0, r) => format!("{r} monitor ROM(s) not here yet"),
                    (d, 0) => format!("{d} sample disks not here yet"),
                    (d, r) => format!("{d} sample disks and {r} monitor ROM(s) not here yet"),
                };
                format!(
                    "{} <span class=\"sub\">{what}</span> ",
                    save_button("getdisks", "Download sample disks", "secondary"),
                )
            };
            format!(
                "{get}<button type=\"button\" class=\"more\" \
                 data-target=\"more-cpm-disks\">Mount CP/M drives&hellip;</button>"
            )
        },

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
         <h3>Bundled CP/M Terminals</h3>\
         <div class=\"row\">{terms}</div>\
         <div class=\"row\"><span class=\"hint\">EGT8080.COM and EGT80.COM are \
         this gateway's own CP/M terminal, written into the transfer directory \
         (and onto CP/M drive A:) when missing, so you can send one to real \
         hardware without starting the emulator. A file already there is never \
         overwritten &mdash; it holds the settings you saved into it &mdash; so \
         this only decides whether a missing one is written back.</span></div>\
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
        terms = checkbox(
            "place_bundled_terminals",
            "Write EGT8080.COM and EGT80.COM when they are missing",
            cfg.place_bundled_terminals,
        ),
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
        atd = atdt_kermit_checkbox(cfg),
        apd = peer_dial_checkbox(cfg),
        pbs = numfield("punter_block_size", "Block size (8-255)", cfg.punter_block_size),
        pneg = numfield("punter_negotiation_timeout", "Neg (s)", cfg.punter_negotiation_timeout),
        pblk = numfield("punter_block_timeout", "Block (s)", cfg.punter_block_timeout),
        pret = numfield("punter_max_retries", "Retries", cfg.punter_max_retries),
        pbad = numfield("punter_max_bad_rounds", "Bad rounds", cfg.punter_max_bad_rounds),
        pint = numfield("punter_negotiation_retry_interval", "Poke (s)", cfg.punter_negotiation_retry_interval),
        phang = checkbox("punter_hangup_on_failure", "Hang up (drop carrier) on a failed transfer", cfg.punter_hangup_on_failure),
    ));

    // Per-port serial popups.
    out.push_str(&serial_more_popup("serial_a", "Port A", &cfg.serial_a, cfg));
    out.push_str(&serial_more_popup("serial_b", "Port B", &cfg.serial_b, cfg));
    out
}

/// One serial port's "More" popup.
///
/// Takes the whole `cfg` as well as the port, because the Modem Dial Targets
/// section at the foot shows two **global** settings rather than per-port ones.
/// That is deliberate and it mirrors the desktop GUI, where the same two
/// checkboxes appear under Port A and Port B: either port's modem can reach
/// either dial target, so tucking them under one port would be arbitrary.
fn serial_more_popup(
    prefix: &str,
    label: &str,
    port: &config::SerialPortConfig,
    cfg: &Config,
) -> String {
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
         <div class=\"row\"><span class=\"label\">Erase key:</span>{erase}</div>\
         <div class=\"row\"><span class=\"label\">Gateway PETSCII:</span>{gwpet}</div>\
         <div class=\"row\">{bits} {stop}\
         <span class=\"label\">Parity:</span><select name=\"{prefix}_parity\">{po}</select>\
         <span class=\"label\">Flow:</span><select name=\"{prefix}_flowcontrol\">{fo}</select>\
         </div>\
         <h3>Hayes AT Saved State</h3>\
         <div class=\"row\">{echo} {verb} {quiet} {petscii}</div>\
         <div class=\"row\">{xc} {dtr} {flw} {dcd} {carrier}</div>\
         <h3>S-Registers</h3>\
         <div class=\"row\"><span class=\"label\">S-registers:</span>\
         <input type=\"text\" name=\"{prefix}_s_regs\" value=\"{sregs}\" size=\"40\"></div>\
         <div class=\"row\"><span class=\"sub\">Stored Phone Numbers \
         (AT&amp;Zn=s / ATDSn)</span></div>\
         <div class=\"row\">{n0} {n1}</div>\
         <div class=\"row\">{n2} {n3}</div>\
         <h3>Modem Dial Targets</h3>\
         <div class=\"row\"><span class=\"sub\">Both are single global settings, \
         shown on each port because either port's modem can reach them.</span></div>\
         <div class=\"row\">{atd}</div>\
         <div class=\"row\">{apd}</div>\
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
        // The **second** PETSCII question this port answers, and the two must
        // not be confused: `AT+PETSCII` above describes the far end of *this
        // wire*, this one the far end of the hop a Telnet or SSH Gateway opens
        // afterwards.  Per port because that is where the operator is already
        // thinking about the machine plugged in; `Default` defers to the
        // server-wide key, which is what a Commodore arriving over telnet on a
        // WiFi modem uses, having no port to speak for it.
        gwpet = {
            let opts: String = crate::serial::GW_PETSCII_CHOICES
                .iter()
                .map(|(value, label)| {
                    format!(
                        "<option value=\"{}\"{}>{}</option>",
                        value,
                        if *value == port.gateway_petscii.trim().to_ascii_lowercase() {
                            " selected"
                        } else {
                            ""
                        },
                        html_escape(label),
                    )
                })
                .collect();
            format!(
                "<select name=\"{}_gateway_petscii\" title=\"{}\">{}</select>\
                 <span class=\"hint\">PETSCII clients on this port only, and about the \
                 <em>gateway's</em> onward connection &mdash; not this wire, which \
                 <code>AT+PETSCII</code> above governs. <em>Translate</em>: the gateway \
                 converts a remote's ANSI colour and clear-screen to PETSCII and \
                 case-swaps its text. <em>Pass through</em>: the far end does its own \
                 terminal detection, so it is sent the C64's real 0x14, recognises the \
                 Commodore and serves native PETSCII in 40 columns &mdash; better where \
                 it works, and wrong for a board that cannot, which then gives you no \
                 backspace. <em>Default</em> uses the server-wide setting under \
                 Server &rarr; More.</span>",
                prefix,
                html_escape(crate::config::GATEWAY_PETSCII_TRANSLATE_HINT),
                opts,
            )
        },
        erase = {
            // **Console mode only, and greyed rather than merely explained.**
            // The hint used to carry that alone, which left a live control that
            // did nothing on two of the three modes -- an operator could set it,
            // see it stored, and get no change on the wire. Disabled here from
            // the *stored* mode and toggled live by `updateSerialFields()` when
            // the Mode select changes, the same two halves the raw-TCP gate on
            // `telnet_gateway_negotiate` uses.
            //
            // A disabled control is not submitted, and that is safe **because
            // `serial_keys` skips an absent field** rather than reading it as a
            // cleared one -- the stored value survives, so switching a port to
            // modem mode and back does not lose the erase key. That is the
            // opposite of the boolean loop, where absent means `false` and
            // `bool_checkbox_gated_off` has to skip explicitly; the pair is
            // pinned by `test_a_greyed_erase_key_is_not_cleared_by_a_save`.
            let gated = port.mode != "console";
            let opts: String = crate::serial::BACKSPACE_CHOICES
                .iter()
                .map(|(value, label)| {
                    format!(
                        "<option value=\"{}\"{}>{}</option>",
                        value,
                        if *value == port.backspace.trim().to_ascii_lowercase() {
                            " selected"
                        } else {
                            ""
                        },
                        html_escape(label),
                    )
                })
                .collect();
            format!(
                "<select name=\"{}_backspace\"{}>{}</select>\
                 <span class=\"hint\">Which byte the device is handed when you press \
                 Backspace or Delete on a <em>console-mode</em> bridge. Your terminal picks \
                 one and cannot be asked to change it, and a lot of period hardware edits \
                 with 0x08 while a modern client sends 0x7F &mdash; neither end is wrong. \
                 <strong>Console mode only</strong>, so it is greyed out in Modem and \
                 Kermit Server mode: a modem port passes these bytes through, and on a \
                 Kermit-server port they are packet data whose rewriting would corrupt \
                 transfers. It rewrites what you type, so switch it back to \
                 <em>pass through</em> before a file transfer over the same bridge \
                 &mdash; <code>PCGET</code> and <code>PCPUT</code> run XMODEM over the \
                 console line.</span>",
                prefix,
                if gated { " disabled" } else { "" },
                opts,
            )
        },
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
        // The two global dial-target opt-ins, rendered from the one definition
        // the Kermit section also uses -- see `atdt_kermit_checkbox`.  They are
        // NOT per-port, hence no `prefix`: this popup shows them the way the
        // desktop GUI does, once per port popup, and `syncShared` keeps the
        // copies from disagreeing inside the single form.
        atd = atdt_kermit_checkbox(cfg),
        apd = peer_dial_checkbox(cfg),
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

/// The `allow_atdt_kermit` checkbox, wherever it appears.
///
/// One definition because this control is rendered in more than one place, and
/// two hand-written copies is how the *same* setting came to be described two
/// different ways: this page said "(modem emulator)" where the desktop GUI said
/// "(bypasses security)".  One named a subsystem, the other named the cost --
/// and the cost is the thing an operator scanning the page needs to see.  The
/// GUI's wording won.
///
/// `syncShared` runs from inside `warnOnEnable`, so every copy tracks both the
/// click and a cancelled warning.
fn atdt_kermit_checkbox(cfg: &Config) -> String {
    checkbox_with_attr(
        "allow_atdt_kermit",
        "Allow ATDT KERMIT (bypasses security)",
        cfg.allow_atdt_kermit,
        "onchange=\"warnOnEnable(this, 'warn-atdt-kermit')\"",
    )
}

/// The `allow_peer_dial` checkbox, wherever it appears.
///
/// Same reasoning as [`atdt_kermit_checkbox`].  No warning modal on this one:
/// it dials another of your own serial ports rather than bypassing an auth
/// gate, so it is an ordinary opt-in -- but it still has to stay in step with
/// its copies, hence `syncShared`.
fn peer_dial_checkbox(cfg: &Config) -> String {
    checkbox_with_attr(
        "allow_peer_dial",
        "Allow peer-dial (ATD Port@IP / ring modem ports)",
        cfg.allow_peer_dial,
        "onchange=\"syncShared(this)\"",
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
  /* Sampled from the logo's own border, so it has no visible edge against
     the page.  Kept in step with `BG_DARKEST` in gui.rs. */
  --bg-darkest: #00040e;
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
/* Links between the two pages this server serves (config and the VDM-1
   screen).  Amber like everything else here; underlined only on hover, so a
   line of them does not read as a row of buttons. */
.backlink { color: var(--amber); text-decoration: none; }
.backlink:hover { text-decoration: underline; }
/* A link wearing the small button's clothes — the VDM / Dazzler control, which
   lines up under this frame's Save and above its More… and so has to match
   them.  Deliberately an anchor: it navigates away, and a `<button>` in this
   form would want a `type` to avoid submitting it. */
a.linkbtn {
  background: var(--bg-mid);
  color: var(--amber);
  border: 1px solid var(--border);
  border-radius: 3px;
  padding: 2px 8px;
  font-size: 13px;
  font-weight: bold;
  text-decoration: none;
}
a.linkbtn:hover { background: #22365a; }
/* Right-justify an item on its own `.row` (the General frame's More button).
   `.row` is flex with wrap, so auto-margin pushes the button to the frame's
   right edge and it stays inside on every width — unlike the Server frame's
   CSS Grid, where a `1fr` button column collapsed to zero and put the button
   outside the frame.  Guarded by test_more_buttons_cannot_leave_their_frame. */
.row-right { margin-left: auto; }
/* A drive letter is a *column*, not a word.  The page font is proportional, so
   `I:` and `M:` are different widths and sixteen rows of them left every select
   starting at a slightly different place — a jitter of a few pixels down a list
   of sixteen, which reads as sloppiness rather than as a font.  Fixed width and
   right-aligned, so the colons line up and every control below starts at the
   same x. */
.label.drive { display: inline-block; min-width: 30px; text-align: right; }
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
/* Only a blocked port is ever marked; there is no open-port counterpart,
   because a self-connection does not meet the firewall on Windows or macOS. */
/* Specific enough to win.  `.server-grid .port-label` sets the colour at two
   classes, so a one-class `.port-blocked` lost the colour and kept only the
   weight -- the label came out bold and unchanged, which reads as emphasis
   rather than as a warning.  Three classes takes it back. */
/* Colour only, deliberately no `font-weight`.  The grid's content columns are
   `max-content`, so bolding `Port:` widens the column and shifts the row --
   de-aligning the very colons the grid exists to line up, and pushing the row
   past the width the frame's minimum was computed for.  Colour changes no
   metrics. */
.server-grid .port-label.port-blocked { color: #ff5a4a; cursor: help; }
/* The frame says there is something to look at; the popup says what.  A row of
   its own carrying a button and an advisory cost the frame a line it does not
   have to spare, and sat there whether or not a check had ever run. */
button.more.alert { color: #ff5a4a; border-color: #ff5a4a; }
/* What a port test proves, per platform.  The running platform's column is the
   one the operator is in; the others are there to show why it differs. */
.pc-table { border-collapse: collapse; margin: 4px 0 8px; font-size: 0.82rem; }
.pc-table th, .pc-table td { padding: 3px 10px; text-align: center; }
.pc-table th[scope=row] { text-align: left; font-weight: 400; }
.pc-table .pc-here { font-weight: 700; }
.pc-table .pc-yes { color: #33cc33; }
.pc-table .pc-no { color: #ff5a4a; }
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
/* The SSH gateway's public key.  Monospaced and wrapped rather than scrolled:
   an operator is copying the whole thing, so a horizontal scrollbar hiding the
   tail is exactly the wrong behaviour. */
textarea.pubkey {
  width: 100%; resize: vertical;
  background: var(--popup-input); color: var(--text-input);
  border: 1px solid var(--border); border-radius: 3px;
  font-family: inherit; font-size: 12px; padding: 4px 6px;
  word-break: break-all;
}
.modal {
  display: none;
  position: fixed; top: 0; left: 0; right: 0; bottom: 0;
  background: rgba(0, 4, 14, 0.85);
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
// Keep every copy of a shared setting in step.
//
// `allow_atdt_kermit` and `allow_peer_dial` are single global settings shown in
// more than one place (the Kermit section and both serial popups), the way the
// desktop GUI shows them in both port popups.  There the two widgets bind to one
// `&mut bool` and cannot disagree; here they are separate <input>s in ONE form
// -- every popup is a modal div inside `cfg-form`, not a form of its own.
//
// Letting them disagree is a real defect, not a cosmetic one: `parse_form` keeps
// the LAST value for a repeated name, and an unchecked checkbox submits nothing
// at all.  So unticking one copy while another stayed ticked would submit `on`
// and silently discard the operator's change -- on `allow_atdt_kermit`, the one
// setting here that bypasses the auth gate.
function syncShared(el) {
  document.querySelectorAll('input[name=' + el.name + ']').forEach(function(o) {
    if (o !== el) o.checked = el.checked;
  });
}
function warnOnEnable(cb, id) {
  // Sync first, so every copy reflects the click even if the warning is
  // cancelled a moment later -- and again on cancel, because `showWarn`'s
  // revert only knows about the box that was clicked.
  syncShared(cb);
  if (cb.checked) {
    showWarn(id, function() { cb.checked = false; syncShared(cb); });
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
// The erase key applies to CONSOLE mode only: a modem port passes 0x08/0x7F/0x14
// through, and on a Kermit-server port they are packet data whose rewriting
// would corrupt a transfer.  Grey it in the other two modes, matching the
// desktop editor -- telnet goes further and hides the row, because a 40-column
// screen has no way to show it greyed.  The server renders the initial state from
// the stored mode (correct with no JS at all); this keeps it in sync while the
// Mode select is changed.  Unlike the checkbox gates this needs no save-side
// skip: `serial_keys` ignores an absent field instead of reading it as cleared,
// so a port switched to modem mode and back keeps its erase key.
// Names are spelled out rather than built from the port prefix so
// test_disabled_inputs_are_re_enabled_by_js can see them.
var ERASE_GATED = [
  ['serial_a_mode', 'serial_a_backspace'],
  ['serial_b_mode', 'serial_b_backspace'],
];
function updateSerialFields() {
  ERASE_GATED.forEach(function(pair) {
    var mode = document.querySelector('[name=' + pair[0] + ']');
    var erase = document.querySelector('[name=' + pair[1] + ']');
    if (mode && erase) erase.disabled = (mode.value !== 'console');
  });
}
updateSerialFields();
ERASE_GATED.forEach(function(pair) {
  var mode = document.querySelector('[name=' + pair[0] + ']');
  if (mode) mode.addEventListener('change', updateSerialFields);
});
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

        let html = render_main_page(&cfg, None, false);
        let script = html
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("page must have a <script> block");

        let mut checked = 0;
        for tag in html.split('<') {
            // **`select` as well as `input`.** This scanned inputs only, and the
            // first greyed `<select>` on the page -- the console-mode erase key
            // -- went straight past it. The rule is about a control the operator
            // cannot re-enable, which has nothing to do with which tag draws it.
            let is_input = tag.starts_with("input");
            let is_select = tag.starts_with("select");
            if (!is_input && !is_select) || !tag.contains("disabled") {
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
                "control {name:?} renders disabled but no JS mentions it, so nothing \
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
        let on = render_main_page(&Config { log_to_file: true, ..cfg.clone() }, None, false);
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
        let html = render_main_page(&cfg, None, false);

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
        let html = render_main_page(&cfg, None, false);
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
        let html = render_main_page(&cfg, None, false);
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

    /// The backspace setting needs all three UIs too, both choices offered, and
    /// the *behaviour* selected rather than the string.
    ///
    /// That last part is the one worth a test: `backspace_erases` treats
    /// anything it does not recognise as the default, so a hand-edited or
    /// stale value must render as the behaviour the gateway is really giving
    /// it. Matching on string equality instead would leave nothing selected,
    /// and the browser would then post the *first* option — silently changing a
    /// setting the operator never touched, by opening a page.
    #[test]
    fn test_render_main_page_offers_both_backspace_choices() {
        let mut cfg = Config::default();
        let html = render_main_page(&cfg, None, false);
        assert!(html.contains("name=\"cpm_boot_backspace\""), "the select must be on the page");
        assert!(html.contains("Booted disk's backspace key"), "and be labelled");
        for (value, label) in crate::cpm::boot::BACKSPACE_CHOICES {
            assert!(html.contains(&format!("value=\"{value}\"")), "{value} is missing");
            assert!(html.contains(&html_escape(label)), "{value} has no label");
        }
        assert!(
            html.contains(&format!(
                "value=\"{}\" selected",
                crate::cpm::boot::DEFAULT_BACKSPACE
            )),
            "a fresh config must select the default"
        );

        cfg.cpm_boot_backspace = crate::cpm::boot::BACKSPACE_RUBOUT.to_string();
        let html = render_main_page(&cfg, None, false);
        assert!(
            html.contains(&format!("value=\"{}\" selected", crate::cpm::boot::BACKSPACE_RUBOUT)),
            "the configured value must come back selected"
        );

        // The case the string comparison would get wrong.
        cfg.cpm_boot_backspace = "something nobody offers".to_string();
        let html = render_main_page(&cfg, None, false);
        assert!(
            html.contains(&format!(
                "value=\"{}\" selected",
                crate::cpm::boot::DEFAULT_BACKSPACE
            )),
            "an unrecognised value must render as the behaviour actually in force"
        );
    }

    /// **Every CP/M select must be rendered once AND collected by the save.**
    ///
    /// The tests above prove each select is *drawn* with the right options
    /// selected, which is the half that is easy to see. The other half is the
    /// bug class that hit `allow_relay_kermit` and is invisible: a field the
    /// save path does not collect renders perfectly, the operator changes it,
    /// and the save silently drops it. `cpm_cpu` arrived with the same shape —
    /// a select in the CP/M panel and one line in `plain_keys` — so the whole
    /// cluster is pinned here rather than only the new one.
    ///
    /// Exactly once, too: the page is a single form, so a name appearing twice
    /// submits twice and the last value wins.
    #[test]
    fn test_cpm_selects_are_rendered_and_saved() {
        let cfg = Config::default();
        let page = render_main_page(&cfg, None, false);
        let keys = [
            "cpm_boot_image",
            "cpm_boot_machine",
            "cpm_boot_backspace",
            "cpm_cpu",
            "cpm_boot_speed",
            "cpm_emu_uart",
        ];
        for name in keys {
            assert_eq!(
                page.matches(&format!("name=\"{name}\"")).count(),
                1,
                "{name} must appear exactly once in the form"
            );
        }

        // And a submitted value comes back out of the save path.  Values that
        // `apply_config_key` accepts, so a refusal downstream cannot be mistaken
        // for the field being collected.
        let mut form = empty_form();
        form.insert("cpm_boot_machine".into(), crate::cpm::console::AUTO_MACHINE.into());
        form.insert("cpm_boot_backspace".into(), crate::cpm::boot::BACKSPACE_RUBOUT.into());
        form.insert("cpm_cpu".into(), crate::cpm::cpu::CPU_8080.into());
        form.insert("cpm_emu_uart".into(), crate::cpm::uart::DEFAULT_UART.into());
        let (updates, _) = collect_form_updates(&form, &cfg);
        for (name, want) in [
            ("cpm_boot_machine", crate::cpm::console::AUTO_MACHINE),
            ("cpm_boot_backspace", crate::cpm::boot::BACKSPACE_RUBOUT),
            ("cpm_cpu", crate::cpm::cpu::CPU_8080),
            ("cpm_emu_uart", crate::cpm::uart::DEFAULT_UART),
        ] {
            assert_eq!(
                updates.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()),
                Some(want),
                "{name} is rendered but the save does not collect it"
            );
        }
    }

    /// The CPU needs all three UIs too, both processors offered, and — as with
    /// the backspace select above — the *behaviour* selected rather than the
    /// string, since `is_8080` reads anything it does not recognise as the Z80.
    ///
    /// The consequence here is sharper than a mis-drawn menu: if nothing
    /// renders as selected the browser posts the first option, so merely
    /// opening the page could change the processor under a running CP/M.
    #[test]
    fn test_render_main_page_offers_both_cpus() {
        let mut cfg = Config::default();
        let html = render_main_page(&cfg, None, false);
        assert!(html.contains("name=\"cpm_cpu\""), "the select must be on the page");
        for (value, label) in crate::cpm::cpu::CPU_CHOICES {
            assert!(html.contains(&format!("value=\"{value}\"")), "{value} is missing");
            assert!(html.contains(&html_escape(label)), "{value} has no label");
        }
        assert!(
            html.contains(&format!("value=\"{}\" selected", crate::cpm::cpu::DEFAULT_CPU)),
            "a fresh config must select the default"
        );

        cfg.cpm_cpu = crate::cpm::cpu::CPU_8080.to_string();
        let html = render_main_page(&cfg, None, false);
        assert!(
            html.contains(&format!("value=\"{}\" selected", crate::cpm::cpu::CPU_8080)),
            "the configured processor must come back selected"
        );

        // The case the string comparison would get wrong.
        cfg.cpm_cpu = "something nobody offers".to_string();
        let html = render_main_page(&cfg, None, false);
        assert!(
            html.contains(&format!("value=\"{}\" selected", crate::cpm::cpu::CPU_Z80)),
            "an unrecognised value must render as the processor actually in force"
        );
    }

    /// The machine setting needs all three UIs too, and every choice has to be
    /// offered — a select that renders only the current value looks fine and
    /// cannot be changed.
    #[test]
    fn test_render_main_page_offers_every_boot_machine() {
        let cfg = Config::default();
        let html = render_main_page(&cfg, None, false);
        assert!(html.contains("name=\"cpm_boot_machine\""), "the select must be on the page");
        assert!(html.contains("Booted disk's machine"), "and be labelled");
        // Iterated over the real list, so a machine added to `console.rs` cannot
        // quietly fail to reach the web page.
        for c in crate::cpm::console::MACHINE_CHOICES {
            assert!(
                html.contains(&format!("value=\"{}\"", c.key)),
                "{} is missing from the web select",
                c.key
            );
            assert!(html.contains(&html_escape(c.description)), "{} has no label", c.key);
        }
        // `auto` is offered and is what a fresh config selects.
        assert!(
            html.contains(&format!(
                "value=\"{}\" selected",
                crate::cpm::console::AUTO_MACHINE
            )),
            "detection must be the selected option on a fresh config"
        );
        assert!(
            html.contains(&html_escape(crate::cpm::console::machine_label(
                crate::cpm::console::AUTO_MACHINE
            ))),
            "and be labelled"
        );
    }

    /// A disk named in the config but no longer in the images folder must still
    /// be shown — otherwise the next save silently resets it and the operator
    /// never learns why their gateway is running the emulator.
    #[test]
    fn test_a_missing_boot_image_still_appears_in_the_web_list() {
        let cfg = Config { cpm_boot_image: "vanished.dsk".to_string(), ..Default::default() };
        let html = render_main_page(&cfg, None, false);
        assert!(html.contains("vanished.dsk"), "the setting must be visible");
        assert!(html.contains("(missing)"), "and marked as not being there");
    }

    #[test]
    fn test_render_main_page_includes_notice() {
        let cfg = Config::default();
        let html = render_main_page(&cfg, Some("Saved!".into()), false);
        assert!(html.contains("Saved!"));
    }

    #[test]
    fn test_render_page_html_escapes_user_input() {
        let cfg = Config {
            browser_homepage: "<script>alert(1)</script>".into(),
            ..Config::default()
        };
        let html = render_main_page(&cfg, None, false);
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

    /// **The erase key is a three-way choice, not a checkbox.** It sat in
    /// `bool_keys` through 0.9.5, where `is_truthy("rubout")` wrote `false`: the
    /// page could not set it, and any save from this page cleared one set from
    /// telnet or the desktop, because an absent field became `false` too and
    /// `backspace_target("false")` is `None` -- pass-through. Both halves are
    /// asserted; the clobber is the worse one and the one a "can it be set?"
    /// test misses.
    #[test]
    fn test_the_web_page_can_set_the_erase_key_and_never_clobbers_it() {
        let mut stored = Config::default();
        stored.port_mut(crate::config::SerialPortId::A).backspace = "rubout".into();

        for choice in ["passthrough", "backspace", "rubout"] {
            let mut form = empty_form();
            form.insert("serial_a_backspace".into(), choice.into());
            let (updates, _) = collect_form_updates(&form, &stored);
            let written: Vec<&String> = updates
                .iter()
                .filter(|(k, _)| k == "serial_a_backspace")
                .map(|(_, v)| v)
                .collect();
            assert_eq!(
                written,
                vec![choice],
                "submitting {choice:?} must store {choice:?}, not a boolean"
            );
        }

        // A save that never touched the select must leave the stored value alone.
        let (updates, _) = collect_form_updates(&empty_form(), &stored);
        assert!(
            !updates.iter().any(|(k, _)| k == "serial_a_backspace"),
            "an unrelated save wrote the erase key, silently reverting the \
             operator's setting to pass-through"
        );
    }

    /// The erase key is greyed outside console mode, and greying it is safe.
    ///
    /// Two halves that have to hold together, because either alone is a bug.
    /// **Greyed**: the setting does nothing on a modem or Kermit-server port --
    /// a modem port passes those bytes through, and on a Kermit wire they are
    /// packet data -- and a live control that silently does nothing is worse
    /// than no control. **Not cleared**: a disabled control is not submitted, so
    /// if the save read an absent field as an empty one, greying it would wipe
    /// the operator's setting the moment they saved from any other section.
    /// `serial_keys` skips an absent field, which is what makes this safe; the
    /// boolean loop does the opposite and needs `bool_checkbox_gated_off`.
    ///
    /// The console-mode case is the positive control, and it is the half that
    /// matters: without it, code that disabled the select unconditionally --
    /// making the setting unreachable on every surface but telnet -- passes.
    #[test]
    fn test_a_greyed_erase_key_is_not_cleared_by_a_save() {
        // The rendered `<select ...>` opening tag for port A, whatever attributes
        // it carries. Matched from the page rather than rebuilt, or this asserts
        // against a copy of itself.
        fn erase_tag(html: &str) -> String {
            let at = html
                .find("<select name=\"serial_a_backspace\"")
                .expect("the erase select must be on the page");
            let rest = &html[at..];
            rest[..rest.find('>').expect("unterminated tag")].to_string()
        }

        for (mode, expect_gated) in [("modem", true), ("kermit", true), ("console", false)] {
            let mut cfg = Config::default();
            {
                let p = cfg.port_mut(crate::config::SerialPortId::A);
                p.mode = mode.into();
                p.backspace = "rubout".into();
            }
            let tag = erase_tag(&render_main_page(&cfg, None, false));
            assert_eq!(
                tag.contains("disabled"),
                expect_gated,
                "in {mode:?} mode the erase select should{} be greyed; got {tag:?}",
                if expect_gated { "" } else { " not" }
            );

            // Whatever the mode, a browser save that did not submit the control
            // (which is exactly what a disabled one does) must not touch it.
            let (updates, _) = collect_form_updates(&empty_form(), &cfg);
            assert!(
                !updates.iter().any(|(k, _)| k == "serial_a_backspace"),
                "saving from a {mode:?}-mode page cleared the stored erase key; \
                 an operator switching to modem mode and back would lose it"
            );
        }
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
        let page = render_main_page(&cfg, None, false);
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
        let page = render_main_page(&cfg, None, false);
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
        let page_css = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&cfg, None, false);
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

    // ─── The VDM-1 screen ───────────────────────────────────────────

    /// A window with a few cells set, the rest blank.
    fn vdm_part(cells: &[(usize, u8)], active: bool) -> crate::cpm::screen::VdmPart {
        let mut window = Box::new([b' '; crate::cpm::vdm::WINDOW]);
        for (i, b) in cells {
            window[*i] = *b;
        }
        crate::cpm::screen::VdmPart { window, scroll: 0, active }
    }

    #[test]
    fn test_vdm_list_json_empty_is_a_list_not_an_error() {
        // The page asks for this every two seconds whether or not anything is
        // booted, so "nothing" has to be an ordinary answer.
        assert_eq!(vdm_list_json(&[]), r#"{"screens":[]}"#);
    }

    #[test]
    fn test_vdm_list_json_carries_what_the_picker_shows() {
        use crate::cpm::screen::Listing;
        let screens = vec![
            Listing { id: 1, label: "TDISK04.DSK — telnet 10.0.0.9".into(), vdm_active: true, dazzler_on: false, has_frame: true },
            Listing { id: 7, label: "CPM14.DSK — SSH 10.0.0.4".into(), vdm_active: false, dazzler_on: true, has_frame: false },
        ];
        assert_eq!(
            vdm_list_json(&screens),
            r#"{"screens":[{"id":1,"label":"TDISK04.DSK — telnet 10.0.0.9","vdm":true,"dazzler":false,"frame":true},"#
                .to_string()
                + r#"{"id":7,"label":"CPM14.DSK — SSH 10.0.0.4","vdm":false,"dazzler":true,"frame":false}]}"#
        );
    }

    /// Ended and not-yet-painted are different answers, because both look like
    /// a blank 64x16 grid — the one pair of states the viewer cannot tell apart
    /// by looking at the screen.
    #[test]
    fn test_vdm_frame_json_reports_the_three_states_apart() {
        use crate::cpm::screen::Look;
        assert_eq!(vdm_frame_json(4, &Look::Gone), r#"{"id":4,"state":"gone"}"#);
        assert_eq!(
            vdm_frame_json(4, &Look::Waiting { label: "TDISK04.DSK".into() }),
            r#"{"id":4,"state":"waiting","label":"TDISK04.DSK"}"#
        );
    }

    #[test]
    fn test_vdm_frame_json_renders_the_screen_and_its_inverse_mask() {
        use crate::cpm::{screen, vdm};
        let screen = screen::register("webserver unit test — frame json");
        // 'H', then 'I' with bit 7 set: the same letter, lit differently.
        screen.publish(vdm_part(&[(0, b'H'), (1, b'I' | 0x80)], true), None, false);
        let screen::Look::Frame(_) = screen::look(screen.id()) else { panic!("published") };
        let json = vdm_frame_json(9, &screen::look(screen.id()));

        assert!(json.contains(r#""state":"live""#), "got {json}");
        assert!(json.contains(r#""id":9"#), "the id is the caller's, not the registry's");
        assert!(json.contains(r#""active":true"#));
        // Sixteen rows and sixteen masks, every one the full width — a short
        // row would silently shift the rest of the line in the browser.
        let rows = json.matches(r#"","#).count();
        assert!(rows >= 30, "16 rows + 16 masks: got {json}");
        assert!(json.contains(&format!(r#""HI{}""#, " ".repeat(vdm::COLS - 2))));
        assert!(json.contains(&format!(r#""01{}""#, "0".repeat(vdm::COLS - 2))));
    }

    /// A guest can paint anything it likes on its own screen, including the two
    /// characters that would end the JSON string early.  This is not a
    /// hypothetical: a CP/M command line full of quotes is ordinary.
    #[test]
    fn test_vdm_frame_json_escapes_what_a_guest_can_paint() {
        use crate::cpm::screen;
        let screen = screen::register("webserver unit test — escaping");
        screen.publish(vdm_part(&[(0, b'"'), (1, b'\\')], false), None, false);
        let json = vdm_frame_json(1, &screen::look(screen.id()));
        assert!(json.contains(r#"\"\\"#), "got {json}");
    }

    /// **Sixteen drive letters are a column, not sixteen words.**
    ///
    /// The page font is proportional, so `I:` and `M:` are different widths and
    /// every select on the mount screen started at a slightly different x — a
    /// few pixels of jitter down a list of sixteen, which reads as sloppiness
    /// rather than as a font. Ricky spotted it on the real page.
    ///
    /// Guarded the way `.row-right` is: a class with no rule behind it is how
    /// this page has silently lost a layout before.
    #[test]
    fn test_the_mount_screen_drive_letters_share_one_column() {
        let html = render_main_page(&Config::default(), None, false);
        assert!(
            html.contains(".label.drive { display: inline-block; min-width: 30px; text-align: right; }"),
            "the drive-letter column needs a real CSS rule, not just a class"
        );
        // Every one of the sixteen uses it — a row that opted out would be the
        // one that jitters, and one crooked row is what the eye finds.
        for letter in 'A'..='P' {
            assert!(
                html.contains(&format!("<span class=\"label drive\">{letter} :</span>")),
                "drive {letter} is not in the shared column"
            );
        }
    }

    /// The Dazzler travels as one hex digit per element — a nibble *is* a hex
    /// digit, so the wire form is the data rather than an encoding of it.
    #[test]
    fn test_vdm_frame_json_carries_the_dazzler_picture() {
        use crate::cpm::screen;
        let s = screen::register("webserver unit test — dazzler");
        // KSCOPE's measured settings: on at 0200, 64x64 colour in 2K.
        let mut bytes = vec![0u8; crate::cpm::dazzler::LARGE];
        bytes[0] = 0x21;
        s.publish(
            vdm_part(&[], false),
            Some(screen::DazzlerPart { bytes, address: 0x81, format: 0x30 }),
            false,
        );
        let json = vdm_frame_json(3, &screen::look(s.id()));
        assert!(json.contains(r#""w":64,"h":64,"colour":true"#), "got {json}");
        assert!(json.contains(r#""base":512"#), "the address register decoded, not passed through");
        assert!(json.contains(r#""cells":"12"#), "low nibble first: 1 then 2");
        // 64x64 elements, one digit each.
        let cells = json.split(r#""cells":""#).nth(1).unwrap().split('"').next().unwrap();
        assert_eq!(cells.len(), 64 * 64);
    }

    /// A machine with no Dazzler says so with `null`, not with a black picture
    /// — the page hides the canvas on the strength of it.
    #[test]
    fn test_vdm_frame_json_says_null_when_there_is_no_dazzler() {
        use crate::cpm::screen;
        let s = screen::register("webserver unit test — no dazzler");
        s.publish(vdm_part(&[], false), None, false);
        assert!(vdm_frame_json(1, &screen::look(s.id())).contains(r#""dazzler":null"#));
    }

    /// The page only offers a keyboard when the operator has left one on, and
    /// it learns which from a constant rather than by guessing.
    #[test]
    fn test_the_screen_page_says_whether_typing_is_allowed() {
        let mut cfg = Config::default();
        assert!(cfg.cpm_screen_input, "on by default");
        assert!(render_vdm_page(&cfg).contains("var VDM_INPUT=true"));

        cfg.cpm_screen_input = false;
        let off = render_vdm_page(&cfg);
        assert!(off.contains("var VDM_INPUT=false"));
        // And the page still *renders*: turning typing off must not cost the
        // operator the screen, which is the whole feature.
        assert!(off.contains("/vdm/frame?id="));
    }

    /// The keyboard has to reach the route that exists, and take the keys back
    /// off the browser — a Backspace that navigates away mid-session is worse
    /// than one that does nothing.
    #[test]
    fn test_the_screen_page_posts_keys_and_keeps_them() {
        let page = render_vdm_page(&Config::default());
        assert!(page.contains("'/vdm/key'"), "the route the server answers");
        assert!(page.contains("e.preventDefault()"), "keys must not reach the browser");
        // DEL for Backspace, so `cpm_boot_backspace` decides where it lands —
        // the same byte a terminal sends, translated by the same code.
        assert!(page.contains("case 'Backspace': return [127];"));
        // Ctrl-letter, because GDEMO asks for Ctrl-S by name and Ctrl-C is how
        // a guest is interrupted.
        assert!(page.contains("e.ctrlKey"));
        // A real focus, not a mode of ours.
        assert!(page.contains("tabindex=\"0\""));
    }

    /// **The download offer names what it will do before doing it** — how many
    /// disks, how big, and whose they are. An operator who would rather fetch
    /// them by hand has to be able to see that and decline.
    #[test]
    fn test_the_disk_download_offer_says_what_it_is() {
        let dir = std::env::temp_dir().join(format!("egweb{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let row = get_disks_row(&[]);
        assert!(row.contains("Download sample disks"));
        assert!(row.contains("github.com/dhansel/Altair8800"), "whose disks: {row}");
        assert!(row.contains("known to"), "that they are the ones that work: {row}");
        assert!(row.contains("left alone"), "that nothing is overwritten: {row}");
        // The count is the catalogue's, not a number typed here.
        assert!(row.contains(&crate::cpm::fetch::catalogue().len().to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With every disk already there the offer becomes a statement, not a
    /// button that would do nothing.
    #[test]
    fn test_the_offer_disappears_once_the_disks_are_here() {
        let names: Vec<String> =
            crate::cpm::fetch::catalogue().into_iter().map(|d| d.name).collect();
        let row = get_disks_row(&names);
        assert!(!row.contains("<button"), "nothing left to fetch: {row}");
        assert!(row.contains("already in the images folder"));
    }

    /// The page is inert HTML plus two fetches; if the endpoints it names ever
    /// drift from the routes, it silently shows nothing at all.
    /// **The page states every key, because nothing else can.** These games
    /// read a board with no console, so the guest's own screen says nothing
    /// about a joystick existing, let alone which keys are it. A control you
    /// cannot see is a control nobody uses.
    #[test]
    fn test_the_page_names_every_joystick_key() {
        let cfg = Config { cpm_joystick: true, ..Default::default() };
        let page = render_vdm_page(&cfg);
        for (key, _, stick, what) in JOYSTICK_KEYS {
            assert!(
                page.contains(&format!("<kbd>{key}</kbd> {what}")),
                "the page must say that {key} is player {stick}'s {what}",
            );
        }
        assert!(page.contains("Player 1") && page.contains("Player 2"), "both sticks are named");
        // And the swing is explained, because a control that starts at centre
        // reads as an unresponsive one for its first fraction of a second.
        assert!(page.contains("swings"), "the ramp has to be described, not discovered");
        assert!(page.contains("Cromemco D+7A"), "the board is named");
    }

    /// **A setting that is on must not land as a switch that is off.** The
    /// panel only exists when `cpm_joystick` is enabled, so an operator
    /// reaching this page has already said they want the board -- finding an
    /// unticked box reads as the setting not having taken, which is the
    /// "a setting is not an outcome" trap from the other direction. The
    /// script seeds `JOY_ON` from the box rather than carrying its own
    /// constant, or the two answers could differ (and a browser restoring a
    /// soft-reloaded page would be overruled).
    #[test]
    fn test_the_switch_starts_on_when_the_board_is_enabled() {
        let cfg = Config { cpm_joystick: true, ..Default::default() };
        let page = render_vdm_page(&cfg);
        assert!(
            page.contains("id=\"joy-on\" checked"),
            "the joystick switch must render already ticked",
        );
        assert!(
            page.contains("JOY_ON = box.checked"),
            "and the script must take its state from that box, not from a constant",
        );
        assert!(
            !page.contains("var JOY_ON = true"),
            "a second answer about the same fact is how the two drift apart",
        );
        // And the panel says which way it starts, since ten ordinary letters
        // quietly not reaching the guest is otherwise a mystery.
        assert!(
            page.contains("it starts on because"),
            "the page has to say the stick is live before a player types a W",
        );
    }

    /// The legend and the handler are built from one table, and this is what
    /// makes that worth doing: a legend that has drifted from the keys the page
    /// actually sends is worse than none, because it is believed.
    #[test]
    fn test_the_legend_and_the_script_agree_about_every_bit() {
        use crate::cpm::d7a::bit;
        let json = joystick_keys_json();
        let expect = [
            ("w", bit::P1_UP),
            ("a", bit::P1_LEFT),
            ("s", bit::P1_RIGHT),
            ("z", bit::P1_DOWN),
            ("x", bit::P1_FIRE),
            ("i", bit::P2_UP),
            ("j", bit::P2_LEFT),
            ("k", bit::P2_RIGHT),
            ("m", bit::P2_DOWN),
            ("n", bit::P2_FIRE),
        ];
        for (key, mask) in expect {
            assert!(
                json.contains(&format!("\"{key}\":{mask}")),
                "the script must map {key} to {mask}; got {json}",
            );
        }
        // Ten distinct keys and ten distinct bits: a duplicate either way would
        // silently make one direction unreachable.
        let keys: std::collections::BTreeSet<&str> =
            JOYSTICK_KEYS.iter().map(|(k, ..)| *k).collect();
        assert_eq!(keys.len(), JOYSTICK_KEYS.len(), "no key does two jobs");
        let bits: std::collections::BTreeSet<u16> = expect.iter().map(|(_, m)| *m).collect();
        assert_eq!(bits.len(), expect.len(), "no bit is set by two keys");
        // Every bit the board knows about is reachable from the keyboard.
        let all: u16 = bits.iter().fold(0, |a, b| a | b);
        assert_eq!(all, bit::ALL, "every direction and button must have a key");
    }

    /// Off means off: no panel, and the ten letters stay ordinary characters.
    #[test]
    fn test_the_panel_is_absent_when_the_board_is_off() {
        let cfg = Config { cpm_joystick: false, ..Default::default() };
        let page = render_vdm_page(&cfg);
        assert!(!page.contains("Cromemco D+7A"), "no legend for a board that is not there");
        assert!(page.contains("var VDM_JOY=false"), "and the script is told");
    }

    #[test]
    fn test_the_vdm_page_polls_the_routes_that_exist() {
        let page = render_vdm_page(&Config::default());
        assert!(page.contains("/vdm/list"));
        assert!(page.contains("/vdm/frame?id="));
        assert!(page.contains(&format!("var VDM_POLL_MS={VDM_POLL_MS}")));
        assert!(page.contains(&format!("var VDM_LIST_MS={VDM_LIST_MS}")));
        // And a way back to the configuration page, since this one has no form.
        assert!(page.contains("href=\"/\""));
    }

    /// The screen has to be *findable*, not merely reachable. It sat in the
    /// header's ports line first, where it read as one more small italic note;
    /// it now sits at the right edge of the CP/M frame's middle row, lined up
    /// between that frame's Save and its More…. Pinned there, because "there is
    /// a link to it somewhere on the page" is exactly the assertion that let
    /// the first placement pass.
    #[test]
    fn test_the_config_page_links_to_the_screen_from_the_cpm_frame() {
        let page = render_main_page(&Config::default(), None, false);
        assert!(page.contains("href=\"/vdm\""), "the screen must be reachable without a URL");

        let frame = frame_ai_browser(&Config::default());
        assert!(frame.contains("href=\"/vdm\""), "it belongs to the frame that says CP/M");
        // Right-justified, by the same auto-margin the other frames use.
        assert!(frame.contains("class=\"row-right linkbtn\""), "got {frame}");
        // And on the *middle* row: after that row's input, before the Home row.
        // The middle row is the weather location — the Groq key used to be here
        // and moved to the popup, because an optional key field at the top of a
        // frame reads as a prerequisite.
        let link = frame.find("href=\"/vdm\"").expect("present");
        assert!(link > frame.find("weather_location").expect("present"));
        assert!(link < frame.find("browser_homepage").expect("present"));
        assert!(!frame.contains("groq_api_key"), "the key belongs in the popup now");
        // The header keeps its ports line and nothing else — a second copy
        // would be two things to keep in step for no gain.
        assert!(!render_header(&Config::default()).contains("/vdm"));
    }

    /// The manual link the desktop GUI has, and this page used to lack.
    ///
    /// Pinned to the shared [`MANUAL_URL`] rather than to a literal, because
    /// the point of the constant is that the two surfaces cannot drift to two
    /// different documents — a test carrying its own copy of the URL would let
    /// exactly that happen and still pass.
    #[test]
    fn test_header_offers_the_user_manual() {
        let head = render_header(&Config::default());
        assert!(head.contains(MANUAL_URL), "no manual link: {head}");
        assert!(head.contains(">User Manual</a>"), "not labelled: {head}");
        // A new tab: this page is a form, and navigating away in place would
        // discard whatever the operator had part-way typed into it.
        assert!(head.contains("target=\"_blank\""), "would lose form edits: {head}");
    }

    /// The SSH gateway's public key, shown on the same terms as in the GUI.
    ///
    /// Two assertions that matter beyond "it renders": it is **absent** under
    /// password auth (the GUI's `!= "password"` condition), and it carries no
    /// `name`, so the save cannot post it back as if it were a setting.
    #[test]
    fn test_ssh_gateway_public_key_is_shown_for_key_auth_only() {
        let mut cfg = Config { ssh_gateway_auth: "key".to_string(), ..Default::default() };

        let with_key = render_more_popups(&cfg);
        assert!(with_key.contains("class=\"pubkey\""), "no key box under key auth");
        assert!(with_key.contains("authorized_keys"), "no caption saying what to do with it");
        // Read-only and unnamed.  A named control is submitted on save, and
        // there is no key to write — it would arrive as an unknown field.
        assert!(with_key.contains("class=\"pubkey\" readonly"));
        assert!(!with_key.contains("name=\"pubkey\""), "it must not be submitted");

        cfg.ssh_gateway_auth = "password".to_string();
        let with_password = render_more_popups(&cfg);
        assert!(
            !with_password.contains("class=\"pubkey\""),
            "password auth does not use this key, so showing it invites a pointless paste"
        );
    }

    /// `place_bundled_terminals` reaches the web UI, and a save can store it.
    ///
    /// The allowlist half is the one that bites: a bool absent from
    /// `bool_keys` is silently dropped on save rather than rejected, so the
    /// box would tick and untick and never change anything.
    #[test]
    fn test_place_bundled_terminals_is_on_the_page_and_savable() {
        let html = render_main_page(&Config::default(), None, false);
        assert!(html.contains("name=\"place_bundled_terminals\""), "not on the page");
        assert!(html.contains("<h3>Bundled CP/M Terminals</h3>"), "no section heading");
        assert!(html.contains("never overwritten"), "the page must say it does not clobber");

        // A save carries it both ways.  An unchecked checkbox submits nothing
        // at all, so the "off" direction is the one a missing allowlist entry
        // breaks silently: the key never appears in the updates and the stored
        // value simply stays as it was.
        let old = Config::default();
        let lookup = |ups: &Vec<(String, String)>, k: &str| {
            ups.iter().find(|(uk, _)| uk == k).map(|(_, v)| v.clone())
        };

        let mut form = empty_form();
        form.insert("place_bundled_terminals".into(), "true".into());
        let (ticked, _) = collect_form_updates(&form, &old);
        assert_eq!(lookup(&ticked, "place_bundled_terminals"), Some("true".into()));

        let (unticked, _) = collect_form_updates(&empty_form(), &old);
        assert_eq!(
            lookup(&unticked, "place_bundled_terminals"),
            Some("false".into()),
            "an unticked box must save as false, not vanish from the update list",
        );
    }

    /// The two global dial-target opt-ins appear on every surface that can
    /// reach them, and every copy is wired to stay in step.
    ///
    /// This page is **one** `<form>` -- the popups are modal divs inside it --
    /// so repeating a checkbox name is not free.  `parse_form` keeps the last
    /// value for a repeated key and an unchecked box submits nothing, so two
    /// copies that disagree submit `on` and silently discard an operator's
    /// untick.  Every copy therefore carries an `onchange` that calls
    /// `syncShared` (directly, or via `warnOnEnable` which calls it too).
    ///
    /// The browser-side behaviour was verified by driving a real headless
    /// browser: ticking one copy checks all three, unticking a *different* copy
    /// clears all three, and cancelling the ATDT warning reverts all three.
    /// That last one matters most -- without it a cancelled security warning
    /// would still have submitted `on` from the copies it did not know about.
    #[test]
    fn test_shared_dial_target_checkboxes_are_kept_in_step() {
        let html = render_main_page(&Config::default(), None, false);
        for (name, handler) in
            [("allow_atdt_kermit", "warnOnEnable"), ("allow_peer_dial", "syncShared")]
        {
            let needle = format!("name=\"{name}\"");
            let copies = html.matches(&needle).count();
            assert!(copies > 1, "{name} should appear more than once, got {copies}");
            // Every occurrence must carry a handler, not just the first.
            let wired = html.matches(handler).count();
            assert!(
                wired >= copies,
                "{name} has {copies} copies but only {wired} {handler} handlers — \
                 an unwired copy can disagree with the others and eat a change",
            );
        }
        assert!(html.contains("function syncShared"), "the sync itself is missing");
        // The label names the cost, as the GUI's does.  "(modem emulator)" said
        // which subsystem it belonged to and not what it does to your security.
        assert!(html.contains("Allow ATDT KERMIT (bypasses security)"));
        // One heading per serial popup, and it covers BOTH checkboxes: the old
        // "Direct-to-Kermit Dial Target" named only the first of the two, and
        // peer-dial does not reach Kermit at all.
        assert_eq!(html.matches("<h3>Modem Dial Targets</h3>").count(), 2);
        assert!(!html.contains("Direct-to-Kermit"), "the misleading heading is gone");
    }

    /// The Server / General / serial popups group their controls under the
    /// same headings the desktop GUI uses.
    ///
    /// Grouping is the part of "looks like the GUI" that is structural rather
    /// than cosmetic: these popups were flat lists, so a reader had to know
    /// already which control belonged to which subsystem.
    #[test]
    fn test_more_popups_carry_the_guis_section_headings() {
        let html = render_more_popups(&Config::default());
        for heading in [
            "<h3>Telnet Gateway</h3>",
            "<h3>SSH Gateway</h3>",
            "<h3>Terminal size reported to remote</h3>",
            "<h3>Master/Slave</h3>",
            "<h3>Log File</h3>",
            "<h3>Hayes AT Saved State</h3>",
            "<h3>S-Registers</h3>",
        ] {
            assert!(html.contains(heading), "missing {heading}");
        }
    }

    /// A class with no CSS rule is how this page has silently lost styling
    /// before — see `.row-right`, which is guarded the same way.
    #[test]
    fn test_the_screen_link_has_a_real_style() {
        let page = render_main_page(&Config::default(), None, false);
        assert!(page.contains("a.linkbtn {"), "linkbtn must have a real rule, not just a class");
        assert!(page.contains("a.linkbtn:hover"), "and a hover state, like the buttons it sits with");
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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);
        assert!(html.contains(r#"class="server-grid""#));
        // Four port inputs, all with class="port-num" + size="6".
        let port_num_count = html.matches(r#"class="port-num""#).count();
        assert_eq!(port_num_count, 4, "expected 4 port-num inputs in Server frame, got {}", port_num_count);
        let size6_in_server = html.matches(r#" size="6" class="port-num""#).count();
        assert_eq!(size6_in_server, 4, "all 4 port inputs must be size=6");
        // Six port-label cells (one per port column in each row).
        // Prefix, not the whole attribute: a port the check found blocked carries
        // `class="port-label port-blocked"`, and an exact match would count 3
        // and fail for a reason that has nothing to do with the layout.
        let port_label_count = html.matches(r#"class="port-label"#).count();
        assert_eq!(port_label_count, 4, "expected 4 port-label cells (one per port input)");
    }

    #[test]
    fn test_server_frame_more_button_renders_on_row_one() {
        // More button must appear in the grid BETWEEN the row 1
        // listeners (Telnet/Web) and the row 2 listeners (SSH/Kermit).
        // In CSS-Grid auto-flow that position puts the button as the
        // last cell of row 1.  If a future refactor places More after
        // kermit_server_enabled instead, this test catches the regress.
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);

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

    /// **Slot 0 is reserved while a disk boots, and the row has to say by what.**
    ///
    /// Reported 2026-08-21: the first row showed an empty picker beside a note
    /// reading "the booted disk is here", naming nothing. Now the note names the
    /// disk, the picker shows it and is not selectable — and a mount left
    /// underneath keeps its picker (it has to be removable without clearing
    /// `cpm_boot_image` first) while gaining the warning that the guest cannot
    /// reach it.
    #[test]
    fn test_the_first_mount_row_names_the_disk_that_boots_there() {
        let dir = std::env::temp_dir().join(format!("egw_web_slot0_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let images = crate::cpm::image::images_dir(&crate::cpm::layout::cpm_dir(
            &dir.to_string_lossy(),
        ));
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("boots.dsk"), crate::cpm::boot::tests::bootable_image()).unwrap();

        // The emulator: an ordinary drive, and the old note stands.
        let emu = Config {
            transfer_dir: dir.to_string_lossy().to_string(),
            cpm_boot_image: String::new(),
            ..Config::default()
        };
        let html = render_cpm_disks_modal(&emu);
        assert!(html.contains("A: hides the terminals while mounted"), "the emulator note");
        assert!(!html.contains("boots here"), "nothing is booting");

        // A disk booting: the note names it and the picker is not selectable.
        let booting = Config { cpm_boot_image: "boots.dsk".into(), ..emu.clone() };
        let html = render_cpm_disks_modal(&booting);
        assert!(html.contains("boots.dsk boots here"), "slot 0 must name its occupant: {html:.0}");
        let row = html
            .split("<div class=\"row\">")
            .find(|r| r.contains("cpm_mount_a"))
            .expect("an A: row");
        assert!(
            row.contains("<select name=\"cpm_mount_a\" disabled>"),
            "the reserved picker must not be selectable: {row}"
        );
        assert!(
            row.contains(">boots.dsk</option>"),
            "the reserved picker must show what reserved it: {row}"
        );
        // The old contradiction must be gone.
        assert!(!row.contains("(drive folder)"), "a reserved slot is not a free drive: {row}");

        let _ = std::fs::remove_dir_all(&dir);
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
        // The transfer dir and the images must be the *same* tree, because the
        // code under test derives its base from `cfg.transfer_dir` rather than
        // being handed one. This used to set `transfer_dir` to the parent of
        // the images folder — the system temp dir — so the save resolved
        // `<temp>/CPM/images`, a directory every other test that uses
        // `temp_dir` also writes into, while the images sat somewhere else.
        // The assertion then held only while the save happened to be a no-op,
        // and the test failed intermittently in release, where the ordering
        // differs. Nothing about the behaviour it pins was wrong.
        let root = std::env::temp_dir().join("egw_web_save_keeps_mounts");
        let _ = std::fs::remove_dir_all(&root);
        let base = crate::cpm::layout::cpm_dir(&root.to_string_lossy());
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
            transfer_dir: root.to_string_lossy().to_string(),
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
    ///
    /// **It takes the registry lock, and that is not a formality.**
    /// `apply_cpm_mount_form` is not a pure function: a submitted-but-empty
    /// select means "drive folder", so this call really does unmount drive B:
    /// in the process-global table.  This test used to run without the lock on
    /// the strength of a comment saying "nothing was mounted in this test
    /// process" — true of the test in isolation, and false the moment it
    /// interleaves with one that mounts something.  That is the whole mechanism
    /// of the release-only flake in
    /// [`test_a_save_does_not_wipe_mounts_that_are_live`]: it mounts B: and C:,
    /// this test unmounts B: underneath it, and the assertion comes back
    /// `C=altair8_two.dsk` with no B — which reads as a save wiping a mount,
    /// i.e. as the very defect that test exists to catch.
    #[test]
    fn test_absent_mount_select_is_not_read_as_unmount() {
        use crate::cpm::image::registry;
        let _g = registry::tests_lock();
        registry::tests_reset();
        let cfg = Config::default();
        let mut fields: HashMap<String, String> = HashMap::new();
        // Only drive B: submitted, and empty (an explicit "drive folder").
        fields.insert("cpm_mount_b".to_string(), String::new());
        let (_notice, value) = apply_cpm_mount_form(&fields, &cfg);
        // Nothing is mounted — the reset above makes that true rather than
        // assumed — so the point is that it did not panic and did not invent
        // mounts for the fifteen drives whose selects were absent.
        assert!(
            !value.contains("A="),
            "an unsubmitted drive must not be given an image: {value}"
        );
        registry::tests_reset();
    }

    #[test]
    fn test_more_buttons_cannot_leave_their_frame() {
        let html = render_main_page(&Config::default(), None, false);

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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&Config::default(), None, false);
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
        let html = render_main_page(&cfg, None, false);
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
        let html = render_main_page(&Config::default(), Some("Configuration saved.".into()), false);
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
        let page = render_main_page(&cfg, None, false);
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
        let html = render_main_page(&cfg, None, false);
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

    /// Every value-bearing control on the page survives a save **verbatim**.
    ///
    /// This is the systemic form of the bug that shipped in 0.9.5: the three
    /// erase-key `<select>`s sat in `bool_keys`, so a save ran
    /// `is_truthy("rubout")` over a three-way choice and stored `false`.  The
    /// page could not set the key at all, and *any* save from the browser wrote
    /// `false` over whatever telnet or the desktop had set.
    ///
    /// **Reachability alone would not have caught it** -- the key still appeared
    /// in the update list, carrying a coerced `false`.  What separates a value
    /// control from a checkbox is that its value is its *content*, so this
    /// submits a distinctive string through every `<select>` and text/number
    /// `<input>` and requires that exact string to come back.  A control routed
    /// through the boolean loop fails, because that loop can only emit `true`
    /// or `false`.
    ///
    /// Checkboxes are deliberately out of scope: coercion is correct for them,
    /// and `test_collect_form_updates_absent_checkboxes_become_false` and
    /// `test_place_bundled_terminals_is_on_the_page_and_savable` cover that side.
    #[test]
    fn test_every_value_control_survives_a_save_verbatim() {
        // Master role, so the role-gated relay controls are live rather than
        // skipped -- a gate must not be able to hide a control from this scan.
        let cfg = Config { gateway_role: "master".to_string(), ..Config::default() };

        let mut html = render_main_page(&cfg, None, false);
        html.push_str(&render_more_popups(&cfg));

        // Controls whose values a *different* handler owns.  Named rather than
        // pattern-matched, and each is asserted to still be on the page below,
        // so an exclusion cannot outlive the control it excuses.
        let mut elsewhere: Vec<String> =
            vec!["cpm_new_format".to_string(), "cpm_new_name".to_string()];
        for d in 0..16u8 {
            elsewhere.push(format!("cpm_mount_{}", (b'a' + d) as char));
        }

        let controls = value_controls(&html);
        assert!(
            controls.len() > 40,
            "only {} value controls found -- the scan stopped matching the markup",
            controls.len(),
        );

        const PROBE: &str = "zzprobe";
        let mut form = empty_form();
        for name in &controls {
            form.insert(name.clone(), PROBE.to_string());
        }
        // A master with relays armed, so nothing is skipped as inert.
        form.insert("gateway_role".to_string(), "master".to_string());
        let (updates, _) = collect_form_updates(&form, &cfg);

        for name in &controls {
            if name == "gateway_role" {
                continue; // pinned to `master` above to open the role gate
            }
            if elsewhere.contains(name) {
                assert!(
                    html.contains(&format!("name=\"{name}\"")),
                    "{name} is excused as handled elsewhere but is no longer on the page",
                );
                continue;
            }
            let got = updates.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
            assert_eq!(
                got,
                Some(PROBE),
                "{name} is a value control, so a save must carry its value through \
                 unchanged. `None` means no list claims it and the save is silently \
                 dropped; `true`/`false` means it is in `bool_keys`, which coerces a \
                 choice to a boolean -- the 0.9.5 erase-key bug.",
            );
        }
    }

    /// Names of every `<select>` and text/number `<input>` in `html`.
    ///
    /// Reads the rendered page rather than the source, so a control added by a
    /// helper this test has never heard of is still scanned.  Checkboxes,
    /// buttons, hidden fields and `<meta name=...>` are all excluded by looking
    /// only at these two tags and at the `type` a control declares.
    fn value_controls(html: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for tag in html.split('<').skip(1) {
            let end = tag.find('>').unwrap_or(tag.len());
            let tag = &tag[..end];
            let (is_select, is_input) =
                (tag.starts_with("select "), tag.starts_with("input "));
            if !is_select && !is_input {
                continue;
            }
            let attr = |a: &str| -> Option<String> {
                let pat = format!("{a}=\"");
                let i = tag.find(&pat)? + pat.len();
                let rest = &tag[i..];
                Some(rest[..rest.find('"')?].to_string())
            };
            if is_input
                && !matches!(attr("type").as_deref(), Some("text" | "number" | "password"))
            {
                continue;
            }
            if let Some(name) = attr("name")
                && !out.contains(&name)
            {
                out.push(name);
            }
        }
        out
    }

}
