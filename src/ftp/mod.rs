//! The FTP server: RFC 959 and its extensions (1123, 775, 2389, 2428, 2577,
//! 2640, 3659, 7151), serving the transfer directory.
//!
//! Hand-written, with libunftp read as a reference -- see
//! `.claude/design/ftp.md` for why (its PORT has no bounce protection, and it
//! can't be extended past SITE).
//!
//! - `proto`: the pure protocol pieces (Telnet-aware line reading, argument
//!   forms, virtual paths, listing formats).
//! - `store`: every file operation, behind the jail and the CP/M guard.
//! - `session`: one control connection.
//!
//! **Security is the gateway's, not FTP's own.**  The accept gate is telnet's,
//! in telnet's order: the per-IP connection rate first (shared with telnet and
//! SSH), then the session cap, then -- with security off and IP safety on --
//! the private-address allowlist with the block-gateway setting.  Logins use
//! the shared lockout map.  Everything is read per connection, so a change in
//! any UI applies to the next one without a restart.

pub(crate) mod backend;
pub(crate) mod proto;
pub(crate) mod remote;
pub(crate) mod session;
pub(crate) mod store;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::net::TcpListener;

use crate::config;
use crate::glog;
use crate::telnet::{ConnRateMap, LockoutMap};

/// Why an accepted connection is turned away before its banner, or `None`
/// to serve it.  A pure function of the settings and the counts, so every
/// combination is tested without a socket.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Over the per-IP connection rate: dropped without a word, as telnet
    /// does (a scanner isn't reading, and a reply per refusal is the
    /// amplifier the limit exists to avoid).
    RateLimited,
    TooManySessions,
    /// The private-address allowlist, with its reason text.
    Address(String),
}

pub(crate) struct Gate {
    pub security: bool,
    pub disable_ip_safety: bool,
    pub block_gateway: bool,
    pub rate_count: u32,
    pub rate_max: u32,
    pub sessions_before: usize,
    pub max_sessions: usize,
}

pub(crate) fn refusal(gate: &Gate, peer: std::net::IpAddr, routers: &[std::net::IpAddr]) -> Option<Refusal> {
    if gate.rate_max > 0 && gate.rate_count > gate.rate_max {
        return Some(Refusal::RateLimited);
    }
    if gate.sessions_before >= gate.max_sessions {
        return Some(Refusal::TooManySessions);
    }
    // Telnet's coupling, deliberately: with security on, the login is the
    // gate and any address may try it (the lockout covers guessing).  With it
    // off, logins are anonymous, so only private addresses may connect.
    if !gate.security && !gate.disable_ip_safety {
        if let Some(reason) = crate::telnet::reject_insecure_ip_with(peer, gate.block_gateway, routers) {
            return Some(Refusal::Address(reason));
        }
    }
    None
}

/// Send a one-line FTP refusal and close, bounded so a peer that won't read
/// can't hold the task.
async fn refuse(mut stream: tokio::net::TcpStream, code: u16, text: String) {
    use tokio::io::AsyncWriteExt;
    let msg = proto::reply(code, &[&text]);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = stream.write_all(&msg).await;
        let _ = stream.shutdown().await;
    })
    .await;
}

/// Bind the FTP listener when `ftp_enabled` is on.
pub fn start_ftp_server(
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    lockouts: LockoutMap,
    conn_rates: ConnRateMap,
) {
    let cfg = config::get_config();
    if !cfg.ftp_enabled {
        return;
    }
    let port = cfg.ftp_port;
    crate::bindwatch::expect("FTP", port);
    tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{port}")).await {
            Ok(l) => l,
            Err(e) => {
                glog!("FTP server: failed to bind port {}: {}", port, e);
                crate::bindwatch::failed("FTP", &e);
                return;
            }
        };
        crate::bindwatch::bound("FTP");
        glog!("FTP server listening on port {}", port);
        let sessions = Arc::new(AtomicUsize::new(0));
        loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((stream, addr)) => accept(stream, addr, &sessions, &lockouts, &conn_rates, &shutdown, &shutdown_notify),
                    Err(e) => glog!("FTP: accept error: {}", e),
                },
                _ = shutdown_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
    });
}

/// Releases a session slot however the session ends.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    sessions: &Arc<AtomicUsize>,
    lockouts: &LockoutMap,
    conn_rates: &ConnRateMap,
    shutdown: &Arc<AtomicBool>,
    notify: &Arc<tokio::sync::Notify>,
) {
    let (rate_max, rate_window) = config::get_conn_rate();
    let (rate_count, say_so) = if rate_max > 0 {
        crate::telnet::note_connection(conn_rates, addr.ip(), rate_max, rate_window)
    } else {
        (0, false)
    };
    let (security, disable_ip_safety, block_gateway) = config::get_security_flags();
    let max_sessions = config::get_config().max_sessions;
    // Claimed before the decision, so two accepts can't both see room for one.
    let before = sessions.fetch_add(1, Ordering::SeqCst);
    let slot = Slot(sessions.clone());
    let gate = Gate {
        security,
        disable_ip_safety,
        block_gateway,
        rate_count,
        rate_max,
        sessions_before: before,
        max_sessions,
    };
    match refusal(&gate, addr.ip(), &crate::router::cached_addrs()) {
        Some(Refusal::RateLimited) => {
            if say_so {
                glog!(
                    "FTP: rejected {} (more than {} connections in {}s); further refusals \
                     from this address are not logged until it is under the limit again",
                    addr, rate_max, rate_window.as_secs()
                );
            }
            drop(stream);
        }
        Some(Refusal::TooManySessions) => {
            glog!("FTP: rejected {} (max {} sessions)", addr, max_sessions);
            drop(slot);
            tokio::spawn(refuse(stream, 421, "Too many connections; try again later.".into()));
        }
        Some(Refusal::Address(reason)) => {
            glog!("FTP: rejected {} ({})", addr, reason);
            drop(slot);
            tokio::spawn(refuse(stream, 421, reason));
        }
        None => {
            glog!("FTP: connection from {} ({}/{})", addr, before + 1, max_sessions);
            let (lockouts, shutdown, notify) = (lockouts.clone(), shutdown.clone(), notify.clone());
            tokio::spawn(async move {
                let _slot = slot;
                // Counted for Save and Restart's "someone is connected" warning.
                let _live = crate::telnet::LiveConnection::new();
                match session::Session::new(stream, addr, lockouts, shutdown, notify) {
                    Ok(s) => s.run().await,
                    Err(e) => glog!("FTP: {} could not start: {}", addr, e),
                }
                glog!("FTP: {} disconnected", addr);
            });
        }
    }
}
