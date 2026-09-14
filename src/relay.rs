//! Master/slave serial-extender relay — transport-agnostic plumbing
//! (Phase 1 of the Gateway Master/Slave design note).
//!
//! A **slave** gateway runs the Hayes modem emulator on its own blocking
//! UART (see `serial::bridge_uart_to_relay`).  When a local device
//! connects, the slave does *not* run the menu locally; instead it
//! bridges that device's data phase outward, over a relay channel, to a
//! **master** gateway.  The master accepts the relay stream here and runs
//! the full session machinery — menu, file transfer, dial-out — exactly as
//! if the device were attached to the master directly.  Files always land
//! on the master.
//!
//! This module is the **master-side intake**: given an already-connected
//! relay stream (an SSH channel in Phase 2, an in-process socket in the
//! loopback test), it wraps the stream in a relay [`TelnetSession`] and
//! runs it to completion.  It is deliberately transport-agnostic — it
//! knows nothing about SSH or TCP, only `AsyncRead`/`AsyncWrite` — so the
//! Phase 2 SSH `exec`/`subsystem` handler and the Phase 1 loopback test
//! are the same code path.
//!
//! The relay carries **raw serial semantics** end to end: no telnet IAC
//! escaping, no CR-NUL stuffing.  `TelnetSession::new_relay` sets the
//! session up for that (see its doc comment).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use crate::logger::glog;
use crate::telnet::{LockoutMap, SessionWriters, SharedWriter, TelnetSession};

/// Run a master-side session over an accepted relay stream.
///
/// `reader` / `write_half` are the two halves of the relay transport
/// (e.g. `tokio::io::split` of an SSH channel or a TCP/duplex socket).
/// `peer_addr` is the slave's IP, used for lockout accounting and logging.
/// `shutdown` / `restart` are the gateway's global flags so a relay
/// session tears down on server shutdown like every other session.
/// `session_writers` is the shared shutdown-broadcast list: the relay's
/// write half is registered for the lifetime of the session (and removed
/// on exit) exactly as `ssh.rs` `shell_request` does for an interactive
/// session, so the server-shutdown broadcast writes the "Goodbye" toward
/// the slave/device on this write half.
///
/// Note on teardown: unlike a *telnet* TCP session (where the broadcast's
/// `shutdown()` on the registered TCP write half makes the peer's read
/// EOF), the relay's registered half is the gateway side of a split
/// in-process duplex — shutting it does NOT directly unstick a relay
/// session parked reading the *other* half.  What actually tears a parked
/// relay session down promptly is the SSH server shutdown dropping the
/// connection handler: that drops the handler-side writer, which EOFs our
/// reader and ends the session.  Registration here is therefore for the
/// goodbye-toward-the-device, not a read-EOF guarantee.
///
/// Returns when the session ends (device disconnect, menu exit, relay
/// EOF, or shutdown).  The write half is flushed and shut down on the way
/// out so the slave sees a clean close.
pub async fn run_master_relay_session(
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    write_half: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    peer_addr: Option<IpAddr>,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    session_writers: SessionWriters,
    lockouts: LockoutMap,
) {
    use tokio::io::AsyncWriteExt;

    let writer: SharedWriter =
        Arc::new(tokio::sync::Mutex::new(write_half));

    // Register with the shutdown-broadcast list so the server-shutdown
    // goodbye is written toward the slave/device on this write half (the
    // prompt teardown of a parked read comes from the SSH handler dropping
    // on shutdown — see the fn doc).
    session_writers.lock().await.push(writer.clone());

    let mut session = TelnetSession::new_relay(
        reader,
        writer.clone(),
        shutdown,
        restart,
        peer_addr,
        lockouts,
    );

    if let Err(e) = session.run().await {
        glog!("Relay: master session error: {}", e);
    }

    // Flush and close the relay's write half so the slave's bridge sees
    // EOF and drops carrier to its device, then drop our entry from the
    // broadcast list.
    {
        let mut w = writer.lock().await;
        let _ = w.shutdown().await;
    }
    session_writers
        .lock()
        .await
        .retain(|w| !Arc::ptr_eq(w, &writer));
}

/// Master-side **onward dial** (Model B, §3): a slave relayed a device
/// that asked to dial an external `host:port`.  The slave resolved the
/// number against its *local* phonebook and asked the master to dial it;
/// the master opens the TCP connection on *its* network and pipes the
/// relay channel straight through (`device ↔ slave ↔ master ↔ BBS`).  No
/// menu, no IAC — transparent bytes both ways, like the modem emulator's
/// own `dial_tcp` online phase.
pub async fn run_master_relay_dial<S>(mut relay: S, host: String, port: u16)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    // Gate onward-dial behind `allow_peer_dial` (M-7).  Without this, any
    // holder of the shared gateway credentials could make the master open
    // outbound TCP to *any* reachable host:port — an SSRF/pivot/port-scan
    // primitive — gated only by master + master_accept_relays.  Onward-dial
    // to an arbitrary external host is at least as sensitive as peer-dial to
    // a gateway's own ports (which already checks this flag, see
    // `run_master_relay_peer`), so it shares the same operator opt-in.
    if !crate::config::get_config().allow_peer_dial {
        glog!("Relay: onward dial to {}:{} refused (allow_peer_dial=false)", host, port);
        let _ = relay.shutdown().await;
        return;
    }

    // Bound the onward connect like the local modem's `dial_tcp` does: the
    // device is sitting at a dial with an SSH session-cap slot held, so an
    // unbounded `connect()` to a down/firewalled host would pin both for the
    // full OS SYN-retry window (~2 min on Linux).  Cap it at
    // RELAY_PEER_ANSWER_WAIT and drop the relay on timeout, which the slave
    // reads as NO CARRIER.
    //
    // This comment used to say the device "sits at CONNECT (the slave reports
    // success as soon as the relay hello arrives)" -- true at the time, and the
    // defect: the hello was the *acceptance*, so every dial failure reached the
    // device as CONNECT then NO CARRIER.  The hello is now withheld until the
    // call is up (see `RELAY_HELLO`), so the wait below is a device waiting for
    // a dial result, which is what a modem does.
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    let mut tcp = match tokio::time::timeout(RELAY_PEER_ANSWER_WAIT, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            glog!("Relay: onward dial to {}:{} failed: {}", host, port, e);
            let _ = relay.shutdown().await;
            return;
        }
        Err(_) => {
            glog!(
                "Relay: onward dial to {}:{} timed out after {}s",
                host, port, RELAY_PEER_ANSWER_WAIT.as_secs()
            );
            let _ = relay.shutdown().await;
            return;
        }
    };
    glog!("Relay: onward dial connected to {}:{}", host, port);
    // The hello goes out HERE, not at accept: it is what the slave turns into
    // CONNECT, and until this line there was no call.

    // `copy_bidirectional` pipes both directions and handles half-close
    // correctly: when one side hits EOF it shuts down the other's write
    // and keeps draining until both ends close — so the final burst from
    // a BBS (or device) that closes its send side isn't dropped (the
    // earlier `select!` cancelled the losing direction mid-copy and could
    // truncate the last bytes of a relayed transfer).
    answer_and_bridge(&mut relay, &mut tcp).await;
    let _ = relay.shutdown().await;
}

/// How long to wait for a modem-mode peer-dial target to answer when the
/// caller has no local `S7` to bound it — the master bridging a relayed peer
/// call, and the slave modem-port announcer ringing its own port.  Matches
/// the telnet Serial Gateway picker's peer-call wait.
pub const RELAY_PEER_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Master-side **Kermit server** for a slave's Kermit-server-mode port.
///
/// The slave pipes its UART to this channel and serves nothing itself, so the
/// device on that wire is talking to *this* machine's Kermit server: `remote
/// dir` lists the master's transfer directory, an upload lands in it, and a
/// download is read from it.  Nothing has to synchronise two directories,
/// because only one of them was ever involved.
///
/// Gated by `allow_relay_kermit`, off by default.  The Kermit server has no
/// authentication of its own — that is inherent to the protocol's server mode,
/// and the same reason `allow_atdt_kermit` and the standalone listener are
/// opt-in.  This path is the better-placed of the three (the peer is a slave
/// that authenticated to this master over SSH, and `master_accept_relays` is
/// already required), but it still hands a remote wire unauthenticated read and
/// write access to the transfer directory, so the operator opts in.
/// May this master serve its Kermit server to a slave port?
///
/// **One predicate, because the answer is needed in two places and the
/// *order* is what went wrong.** The refusal has to be decided in `ssh.rs`
/// *before* [`RELAY_HELLO`] goes out -- a hello is the master saying "accepted",
/// and a gate evaluated after it turns a refusal into a slave that reports
/// success and reconnects once a second for ever. [`run_master_relay_kermit`]
/// keeps asking too, as a backstop for any caller that is not the SSH exec
/// path; both read this, so the two cannot drift apart on what the rule is.
pub fn kermit_relay_allowed(cfg: &crate::config::Config) -> bool {
    cfg.allow_relay_kermit
}

pub async fn run_master_relay_kermit<S>(mut relay: S, port_label: String, peer: Option<IpAddr>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let cfg = crate::config::get_config();
    // Backstop only: the SSH exec path refuses this before the hello, which is
    // the refusal an operator actually meets.  See `kermit_relay_allowed`.
    if !kermit_relay_allowed(&cfg) {
        glog!(
            "Relay: Kermit server for slave port {} refused (allow_relay_kermit=false)",
            port_label
        );
        let _ = relay.shutdown().await;
        return;
    }

    // Name the far end the way the operator sees it in the Serial Gateway
    // list, so a log line ties the transfer to a machine and a port.
    let who = match peer {
        Some(ip) => format!("{}@{}", port_label, ip),
        None => port_label.clone(),
    };
    let target_dir = std::path::PathBuf::from(&cfg.transfer_dir);
    if let Err(e) = tokio::fs::create_dir_all(&target_dir).await {
        glog!(
            "Relay: Kermit server ({}) cannot create transfer dir {:?}: {}",
            who, target_dir, e
        );
        let _ = relay.shutdown().await;
        return;
    }
    glog!("Relay: Kermit server serving slave port {} from {:?}", who, target_dir);

    let (mut read_half, mut write_half) = tokio::io::split(relay);
    let result = crate::kermit::kermit_server_with_outcome(
        &mut read_half,
        &mut write_half,
        false, // raw relay channel — no telnet IAC escaping
        false, // not PETSCII
        cfg.verbose,
        // The same disk-commit hook the serial Kermit paths use: validated
        // filename, safe subdir, collision-safe rename rather than clobber.
        |rx| crate::serial::commit_kermit_upload(&who, &target_dir, rx),
    )
    .await;
    let _ = write_half.shutdown().await;
    match result {
        Ok(_) => glog!("Relay: Kermit server ({}) session ended", who),
        Err(e) => glog!("Relay: Kermit server ({}) session error: {}", who, e),
    }
}

/// Master-side **peer-dial** (Phase 2): a slave relayed a device that dialed
/// `<Port>@<host>`.  The master resolves the address either to one of its
/// *own* ports (rings a modem port / connects a console port, reusing the
/// local peer-dial machinery) or, when it names another gateway, to a port a
/// slave **registered** with it — the crossbar, bridging the two relay legs
/// (`device ↔ slave-A ↔ master ↔ slave-B ↔ device`).  Refuses unless
/// `allow_peer_dial` is on.
pub async fn run_master_relay_peer<S>(mut relay: S, addr: String)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    if !crate::config::get_config().allow_peer_dial {
        glog!("Relay: peer-dial refused (allow_peer_dial=false)");
        let _ = relay.shutdown().await;
        return;
    }

    // The CP/M emulator endpoint on this master (`CPM@<masterip>`): ring the
    // local virtual modem and bridge, just like a local A/B port.  Additive —
    // sits ahead of the A/B resolution, which ignores the CPM label.
    if crate::serial::is_local_cpm_peer(&addr) {
        match crate::serial::request_cpm_call(RELAY_PEER_ANSWER_WAIT).await {
            Ok(mut b) => {
                glog!("Relay: peer-dial bridged to local CP/M endpoint");
                answer_and_bridge(&mut relay, &mut b).await;
            }
            Err(o) => glog!("Relay: peer-dial to CP/M endpoint failed: {:?}", o),
        }
        let _ = relay.shutdown().await;
        return;
    }

    // A LOCAL target — a port on this master (2a): ring (modem) or connect
    // (console) it and bridge.
    if let Some(target) = crate::serial::resolve_local_peer_target(&addr) {
        let cfg = crate::config::get_config();
        let tp = cfg.port(target);
        // A Kermit-server port only ever serves on its own wire — it does
        // not answer a peer-dial ring — so refuse fast instead of ringing
        // a port that never picks up (mirrors connect_local_peer and the
        // telnet Serial Gateway guard).
        if tp.mode == "kermit" {
            glog!(
                "Relay: peer-dial to Port {} refused (Kermit-server port, not dialable)",
                target.label()
            );
            let _ = relay.shutdown().await;
            return;
        }
        let bridge = if tp.mode == "console" {
            crate::serial::request_console_bridge(target).await.map_err(|e| e.to_string())
        } else {
            crate::serial::request_peer_call(target, RELAY_PEER_ANSWER_WAIT)
                .await
                .map_err(|o| format!("{:?}", o))
        };
        match bridge {
            Ok(mut b) => {
                glog!("Relay: peer-dial bridged to local Port {}", target.label());
                answer_and_bridge(&mut relay, &mut b).await;
            }
            Err(why) => glog!("Relay: peer-dial to Port {} failed: {}", target.label(), why),
        }
        let _ = relay.shutdown().await;
        return;
    }

    // A REMOTE target (2b crossbar): a port a slave registered with us —
    // claim its registration channel, activate it, and bridge the two
    // relay legs (device ↔ slave ↔ master ↔ other-slave port).
    if let Some((ip, label)) = parse_remote_peer_addr(&addr) {
        // The hello now waits for the far device here too, which is what
        // `PeerClaim` is for: claiming the slave's registration channel is a
        // map removal and one activate byte, and the slave rings its *own*
        // device afterwards.  Until the answer byte existed this path answered
        // `CONNECT` on the claim and `NO CARRIER` moments later -- the one
        // route the master cannot observe for itself, and so the last one left
        // over from the deferred-hello work.
        match claim_remote_peer(ip, &label).await {
            PeerClaim::Answered(mut remote) => {
                glog!("Relay: peer-dial crossbar to {}@{}", label, ip);
                answer_and_bridge(&mut relay, &mut remote).await;
            }
            // No hello: the slave turns its absence into `NO CARRIER`, which
            // is what the caller's modem should hear for a device that did not
            // pick up.  The outcome is logged rather than signalled onward --
            // this leg carries a relayed *session*, and its only vocabulary
            // for "no call" is the absence of the hello.  The caller's own
            // gateway is where `BUSY` and `NO ANSWER` are spoken.
            PeerClaim::Failed(why) => {
                glog!("Relay: peer-dial to {}@{} did not connect: {:?}", label, ip, why)
            }
            PeerClaim::NotRegistered => {
                glog!("Relay: peer-dial target {}@{} not registered", label, ip)
            }
        }
        let _ = relay.shutdown().await;
        return;
    }

    glog!("Relay: peer-dial address {} not resolvable; refusing", addr);
    let _ = relay.shutdown().await;
}

// ─── Slave side — outbound SSH relay client ────────────────

/// What a relayed call connects to on the master (Model B, §3): either
/// the master's own menu/services, or an external `host:port` the slave
/// resolved from its local phonebook for the master to dial onward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayTarget {
    /// Bridge to the master's menu / services.
    Menu,
    /// Ask the master to dial this external address and bridge through.
    Dial { host: String, port: u16 },
    /// Peer-dial (§ Phase 2): connect to a specific port addressed as
    /// `<Port>@<host>` — the master resolves the address against its own
    /// ports and bridges (ringing a modem port, or connecting a console
    /// port).  `addr` is the raw address the device dialed.
    Peer { addr: String },
    /// A **Kermit-server-mode** port: the master runs its Kermit server on
    /// this channel, so the device on the slave's wire is talking to the
    /// *master's* server and every file operation — `remote dir`, uploads,
    /// downloads — resolves against the master's transfer directory.
    ///
    /// The slave is a pipe here: it never serves Kermit itself in this mode,
    /// which is what makes "files always land on the master" true by
    /// construction rather than by synchronising two directories.
    Kermit,
}

impl RelayTarget {
    /// Build the `exec` command the slave sends on its relay channel.
    /// `port_label` is the slave's logical port ("A"/"B") so the master
    /// knows which device this is.  Grammar:
    ///   `serial-relay <port> menu`
    ///   `serial-relay <port> dial <host>:<port>`
    ///   `serial-relay <port> peer <Port>@<host>`
    pub fn exec_command(&self, port_label: &str) -> String {
        match self {
            RelayTarget::Menu => format!("serial-relay {} menu", port_label),
            RelayTarget::Dial { host, port } => {
                // Bracket an IPv6 literal so the master's `split_dial_host_port`
                // can tell host from port (F1); IPv4/hostnames pass through bare.
                if host.contains(':') {
                    format!("serial-relay {} dial [{}]:{}", port_label, host, port)
                } else {
                    format!("serial-relay {} dial {}:{}", port_label, host, port)
                }
            }
            RelayTarget::Peer { addr } => {
                format!("serial-relay {} peer {}", port_label, addr)
            }
            RelayTarget::Kermit => format!("serial-relay {} kermit", port_label),
        }
    }
}

/// The master's parse of a relay `exec` command (the counterpart to
/// [`RelayTarget::exec_command`]).  Shared by `ssh.rs`'s `exec_request`
/// and the contract tests so the two halves can't drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRelay {
    /// The slave's logical port ("A"/"B", or "?" if absent).
    pub port_label: String,
    /// `None` ⇒ bridge to the master's menu; `Some((host, port))` ⇒
    /// onward-dial that external address (Model B).
    pub dial: Option<(String, u16)>,
    /// `Some(addr)` ⇒ peer-dial the master's port addressed as
    /// `<Port>@<host>` (Phase 2).  Mutually exclusive with `dial`.
    pub peer: Option<String>,
    /// `true` ⇒ the slave's port is in Kermit-server mode and wants the
    /// master's Kermit server on this channel.  Mutually exclusive with
    /// `dial` and `peer`.
    pub kermit: bool,
}

/// Parse a `serial-relay …` exec command.  Returns `None` for anything
/// that isn't a well-formed relay command (the master refuses it — this
/// is not a general command-exec shell).  Grammar:
///   `serial-relay <port> menu`
///   `serial-relay <port> dial <host>:<port>`
///   `serial-relay <port> peer <Port>@<host>`
/// Split a dial target into `(host, port)`, accepting both `host:port` and
/// the bracketed IPv6 form `[2001:db8::1]:6400`, and returning the host as a
/// *bare* literal (brackets stripped) that `TcpStream::connect((host, port))`
/// accepts.  An unbracketed IPv6 literal is rejected as ambiguous — callers
/// must bracket it — as is a missing/zero/invalid port.  Used by both the
/// slave-side resolve and the master-side parse so the two halves agree on
/// IPv6 handling (F1 — the onward-dial path previously used a bare
/// `rsplit_once(':')` that left brackets on the host and broke `connect`).
pub fn split_dial_host_port(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6: [host]:port
        let (host, after) = rest.split_once(']')?;
        let port: u16 = after.strip_prefix(':')?.parse().ok()?;
        if port == 0 {
            return None;
        }
        return Some((host.to_string(), port));
    }
    let (host, port_str) = s.rsplit_once(':')?;
    // A leftover ':' in the host means an unbracketed IPv6 literal — ambiguous
    // (can't tell host from port), so require the bracketed form instead.
    if host.contains(':') {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host.to_string(), port))
}

pub fn parse_relay_command(command: &str) -> Option<ParsedRelay> {
    let mut toks = command.split_whitespace();
    if toks.next()? != "serial-relay" {
        return None;
    }
    let port_label = toks.next().unwrap_or("?").to_string();
    let mut dial = None;
    let mut peer = None;
    let mut kermit = false;
    match toks.next().unwrap_or("menu") {
        "menu" => {}
        "dial" => {
            dial = Some(split_dial_host_port(toks.next()?)?);
        }
        "peer" => {
            let addr = toks.next()?;
            if addr.is_empty() {
                return None;
            }
            peer = Some(addr.to_string());
        }
        "kermit" => kermit = true,
        _ => return None,
    }
    Some(ParsedRelay { port_label, dial, peer, kermit })
}

/// SSH client handler for the slave→master relay connection.  The
/// master's host key is verified against the shared `gateway_hosts`
/// known-hosts file (TOFU — pinned on first contact, rejected on change)
/// by `connect_master_relay`; this handler only captures the presented
/// key for that post-handshake check, mirroring the SSH-gateway proxy.
pub struct SlaveRelayHandler {
    /// Captures the master's presented host key so `connect_master_relay`
    /// can verify it against known-hosts *after* the transport handshake
    /// (the same pattern the SSH-gateway proxy uses — the accept/reject
    /// decision can't be made inside `check_server_key` because it has no
    /// host:port context).
    server_key: std::sync::Arc<std::sync::Mutex<Option<russh::keys::PublicKey>>>,
}

impl russh::client::Handler for SlaveRelayHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        if let Ok(mut slot) = self.server_key.lock() {
            *slot = Some(server_public_key.clone());
        }
        Ok(true)
    }
}

/// Connect/auth timeout for the slave→master relay.  Without this the
/// blocking serial thread would park indefinitely if the master accepts
/// TCP but stalls in the SSH handshake/auth — the attached vintage device
/// would hang at the modem with no result code (mirrors the SSH-gateway
/// proxy's `GATEWAY_CONNECT_TIMEOUT`).
const RELAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A master password typed at a screen, held **in memory only**.
///
/// There is no reason to write it down. It is needed for exactly one login --
/// the one that enrols this slave's key -- and after that the key does the
/// work, so persisting it would create on disk the very thing this whole
/// feature exists to remove, for the sake of a few seconds.
///
/// The config's `slave_master_password` is still read (a wizard or a
/// hand-edited file may carry one, and those are erased once the key works),
/// but a password an operator types at a screen never reaches the file at all.
///
/// Lost on restart, deliberately: if the gateway is restarted before the key is
/// enrolled, the screen asks again. That is a fair price for a secret that was
/// never stored, and the ask is one line.
static PENDING_MASTER_PASSWORD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Bumped whenever something happens that makes an immediate retry worthwhile.
///
/// A relay that has just been refused backs off for minutes, which is right for
/// a master that is down and wrong the instant an operator types the missing
/// password: without this they would enter it, see nothing happen, and
/// reasonably conclude it had not worked.  The backoff waits poll this, so a
/// change cuts the wait short.
static RETRY_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ask the relay loops to stop waiting and try again now.
pub fn request_retry_now() {
    RETRY_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// The current generation, for a waiter to compare against.
pub fn retry_generation() -> u64 {
    RETRY_GENERATION.load(std::sync::atomic::Ordering::SeqCst)
}

/// Hold a password an operator just typed, for the next relay connection.
pub fn set_pending_master_password(password: &str) {
    {
        let mut g = PENDING_MASTER_PASSWORD.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some(password.to_string());
    }
    // Try it now rather than in six minutes: an operator who types a password
    // and watches nothing happen has no way to tell "waiting" from "wrong".
    request_retry_now();
}

/// Forget it -- the key works now, or the operator cleared it.
pub fn clear_pending_master_password() {
    let mut g = PENDING_MASTER_PASSWORD.lock().unwrap_or_else(|e| e.into_inner());
    *g = None;
}

/// The password to try: the one typed at a screen, else whatever is configured.
fn master_password_to_try(configured: &str) -> String {
    PENDING_MASTER_PASSWORD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| configured.to_string())
}

/// The master this slave cannot authenticate to, if that is where it stands.
///
/// Set when a registration is refused **and there is no password left to try**,
/// which is the one relay failure an operator can fix in ten seconds and the one
/// they will otherwise never see: a slave is headless, so the log line naming it
/// is read by nobody. Cleared the moment any relay connection authenticates.
///
/// Read by all three configuration surfaces and by the telnet/serial session
/// start, so whoever reaches this gateway first is told, wherever they arrive.
static MASTER_CREDENTIAL_NEEDED: std::sync::Mutex<Option<(String, u16)>> =
    std::sync::Mutex::new(None);

/// Note that this slave has no usable credential for its master.
pub fn note_master_credential_needed(host: &str, port: u16) {
    let mut g = MASTER_CREDENTIAL_NEEDED.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        glog!(
            "Relay: no usable credential for master {}:{} — the next person to reach \
             this gateway will be asked for the master's password.",
            host, port
        );
    }
    *g = Some((host.to_string(), port));
}

/// Withdraw it because a relay connection authenticated.
///
/// The half that makes the prompt trustworthy: a screen that keeps asking after
/// the problem is gone teaches an operator to dismiss it.
pub fn clear_master_credential_needed() {
    let mut g = MASTER_CREDENTIAL_NEEDED.lock().unwrap_or_else(|e| e.into_inner());
    *g = None;
}

/// The master answered and refused the password, so this slave has no way in
/// again: drop the credential it refused and put the ask back on every screen.
///
/// Its own function because the rule is easy to state and was easy to miss --
/// [`note_master_credential_needed`] is reachable from exactly one other place,
/// the "no password at all" branch, which a pending password stops us reaching.
pub fn note_password_refused(host: &str, port: u16) {
    clear_pending_master_password();
    note_master_credential_needed(host, port);
}

/// The master to ask about, if this slave currently has no way in.
pub fn master_credential_needed() -> Option<(String, u16)> {
    MASTER_CREDENTIAL_NEEDED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Whether the last relay authentication this slave completed used its key.
///
/// **A slave logging in by key has an empty `slave_master_password`, which is
/// the whole point of the feature -- and every configuration surface rendered
/// that empty value as a dim `(not set)`, the same thing a genuinely broken
/// slave shows.** So the state the feature is trying to reach read as a fault,
/// and the operator had no way to tell one from the other.
///
/// Published here rather than derived at the surfaces because only the connect
/// path knows it: the config cannot say whether a key is enrolled on the
/// *master*, and a slave that has never connected honestly does not know
/// either. Hence "authenticated by key", an outcome, rather than "a key
/// exists", which is true on every gateway and would claim something unproven.
static AUTHENTICATED_BY_KEY: AtomicBool = AtomicBool::new(false);

/// Record how the connection that just authenticated got in.
///
/// Called on both outcomes, never only on success: a key that stops working
/// (revoked on the master, or the authorized-keys file lost) must take the
/// claim down with it, or the surfaces would go on saying "using key" about a
/// slave that is back on its password -- a stale reassurance being worse than
/// the dim `(not set)` this replaced.
pub fn note_relay_key_auth(used_key: bool) {
    AUTHENTICATED_BY_KEY.store(used_key, Ordering::Relaxed);
}

/// Whether this slave is currently getting in with its key.
pub fn relay_authenticated_by_key() -> bool {
    AUTHENTICATED_BY_KEY.load(Ordering::Relaxed)
}

/// Serializes the tests that drive [`AUTHENTICATED_BY_KEY`] and
/// [`PENDING_MASTER_PASSWORD`].
///
/// Both are process-wide, and the tests that exercise them live in two
/// modules -- `relay::tests` reads the state those statics produce, and
/// `resolve::tests` drives the remedy that refuses on the strength of one.
/// Run in parallel they would set the flag out from under each other, which is
/// a flake that reports as a real refusal defect.
#[cfg(test)]
pub(crate) static KEY_AUTH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take that lock, surviving a previous test's panic.
#[cfg(test)]
pub(crate) fn key_auth_test_lock() -> std::sync::MutexGuard<'static, ()> {
    KEY_AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a configuration surface should say about a slave's master password.
///
/// One answer for all three surfaces, so telnet, the web editor and the
/// desktop cannot describe one state three ways -- the same reason
/// `master_password_screen_lines` is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterPasswordState {
    /// The key is doing the work, so there is no password to show and none to
    /// ask for: the field says `Auth OK` and holds its place in the layout.
    UsingKey,
    /// A password is stored in `egateway.conf`.
    Stored,
    /// One was typed at a screen and is held in memory, not on disk.
    Entered,
    /// Nothing to log in with.
    Missing,
}

impl MasterPasswordState {
    /// The short label, fitted to the narrowest surface (40 columns with the
    /// menus' two-space indent and the `Pass:` column) -- see
    /// `test_every_master_password_label_fits_a_c64`.
    pub fn label(self) -> &'static str {
        match self {
            MasterPasswordState::UsingKey => "Auth OK",
            MasterPasswordState::Stored => "(set)",
            MasterPasswordState::Entered => "(entered)",
            MasterPasswordState::Missing => "(not set)",
        }
    }
}

/// How this slave is getting in, given whatever the config holds.
///
/// **`UsingKey` outranks `Stored` deliberately.** The field answers "how does
/// this slave log in?", and once the key works that is the honest answer even
/// if a password is still on disk -- the wipe is best-effort and a master that
/// refuses enrolment leaves one behind for ever. What is still *stored* is a
/// separate question with a separate remedy, and it is reported on the Resolve
/// Errors screen rather than squeezed into this one word.
pub fn master_password_state(configured: &str) -> MasterPasswordState {
    if relay_authenticated_by_key() {
        return MasterPasswordState::UsingKey;
    }
    if !configured.is_empty() {
        return MasterPasswordState::Stored;
    }
    if PENDING_MASTER_PASSWORD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        // Typed at a screen and not yet spent.  Saying `(not set)` here is what
        // an operator sees immediately after typing it, which reads as the box
        // having swallowed their input.
        return MasterPasswordState::Entered;
    }
    MasterPasswordState::Missing
}

/// Enrolment happens **once per process**, not once per connection: a slave
/// opens several relay connections (port A, port B, the CP/M endpoint) and they
/// would otherwise each offer the same key.
///
/// The wipe deliberately has no such latch -- see [`forget_master_password`],
/// where one was a bug.  The difference is that an offer has nothing to test
/// itself against, while the wipe can simply ask whether the password is still
/// there.
static KEY_OFFERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// How long to wait for the master's answer to a key offer.
///
/// Short on purpose: this runs inside the relay connect's own budget, and a
/// master that does not answer is one the slave should stop waiting on and go
/// register with, password in hand.
const ENROL_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Hand the master this slave's public key, so the next connection can use it.
///
/// Deliberately fire-and-forget.  Whether it worked is answered by the *next*
/// connection authenticating with the key -- which is also the only evidence
/// good enough to act on (see `forget_master_password`) -- so nothing here
/// waits on, or trusts, a reply.  A master too old to know the command answers
/// `channel_failure`, which is exactly the "not supported" signal we want and
/// needs no protocol version bump.
async fn offer_key_for_enrolment_once(session: &russh::client::Handle<SlaveRelayHandler>) {
    use std::sync::atomic::Ordering;
    // **Latched only on a definite answer.**  Setting it here, before the
    // attempt, meant one bad moment -- a master briefly not accepting relays, a
    // channel that would not open, a lost race with another slave -- retired
    // enrolment for the life of the process, on a headless daemon, silently.
    if KEY_OFFERED.load(Ordering::SeqCst) {
        return;
    }
    let line = match crate::ssh::client_public_key_line() {
        Ok(l) => l,
        Err(e) => {
            glog!("Relay: cannot offer a key for enrolment: {}", e);
            return;
        }
    };
    // The label is this machine's name, for the comment the master writes
    // beside the key.  Advisory only -- the master sanitises it, and identity
    // is the key itself.
    let label = hostname_label();
    let channel = match session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => {
            glog!("Relay: could not open a channel to offer the key ({}); will retry", e);
            return;
        }
    };
    let cmd = format!("enroll-key {} {}", line.trim(), label);
    let mut channel = channel;
    if let Err(e) = channel.exec(true, cmd.as_bytes()).await {
        glog!("Relay: could not send the key for enrolment ({}); will retry", e);
        return;
    }
    // **`exec` only queues the request**, so its `Ok` says nothing about what
    // the master decided -- reporting success there told an operator the key
    // was enrolled while the master was refusing it.  The answer is a channel
    // Success or Failure, and it is worth waiting briefly for: it is the
    // difference between "keep the password because this master is old" and
    // "keep the password because something went wrong", and between retrying
    // and not.
    let answer = tokio::time::timeout(ENROL_REPLY_WAIT, async {
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Success => return Some(true),
                russh::ChannelMsg::Failure => return Some(false),
                _ => continue,
            }
        }
        None
    })
    .await;
    match answer {
        Ok(Some(true)) => {
            KEY_OFFERED.store(true, Ordering::SeqCst);
            glog!("Relay: the master accepted this slave's key for enrolment ({})", label);
        }
        Ok(Some(false)) => {
            // A master that does not know the command, is not a master, or has
            // relays off answers exactly this.  Not retried: the answer will be
            // the same next time, and the password keeps working.
            KEY_OFFERED.store(true, Ordering::SeqCst);
            glog!(
                "Relay: this master did not accept key enrolment; keeping the stored password"
            );
        }
        _ => glog!("Relay: no answer to the key offer; will retry on the next connection"),
    }
}

/// Remove `slave_master_password` from this slave's config, once.
fn forget_master_password() {
    // **The emptiness check is the idempotence, and a latch on top of it was
    // a bug.**  This used to `swap` a once-per-process flag true on the way
    // in, before asking whether there was anything to erase -- so the first
    // key login of a slave whose config was already empty spent the turn on a
    // no-op, and a password appearing afterwards (a hand-edited file, an
    // upgrade landing mid-session) was never erased for the life of the
    // process.  Moving the latch below the check does not fix it either: the
    // reappearing password is exactly the case a latch refuses.
    //
    // So there is no latch.  After the first successful wipe the config is
    // empty and every later call returns here, which is the repeat-write
    // guard the flag was meant to be; the only thing lost is that several
    // relay connections authenticating at the same instant may each write the
    // same empty value, which `update_config_values` serializes and which
    // costs nothing.  The wipe now heals itself rather than needing a human
    // to notice it had stopped.
    if crate::config::get_config().slave_master_password.is_empty() {
        return;
    }
    crate::config::update_config_values(&[("slave_master_password", "")]);
    glog!(
        "Relay: this slave now logs in with its key, so the master's password has been \
         removed from {} — it is no longer stored anywhere on this machine.",
        crate::config::CONFIG_FILE
    );
}

/// This machine's name, reduced to something worth writing in a file.
///
/// Shared with the telnet MORE page, which names the computer its restart and
/// shutdown keys act on -- one rule for "what is this machine called", because
/// two would disagree.
pub(crate) fn hostname_label() -> String {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .filter(|s| !s.trim().is_empty())
        // Windows.
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        // **macOS has neither**, and nor do a good many containers: there is
        // no `/etc/hostname` on a Mac and `COMPUTERNAME` is a Windows
        // variable, so this returned the empty string on the one Unix where
        // double-clicking is the normal launch.  The telnet MORE page names
        // the computer it is about to shut down and simply omitted the row,
        // which is the one sentence that page exists to say.  `hostname` is on
        // every Unix; it is asked only when the file was not there, so the
        // common path still costs one read.
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        })
        .unwrap_or_default();
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(32)
        .collect()
}

/// Relay wire-protocol version.  Bump on any incompatible change to the
/// master↔slave relay framing so a version-skewed pair fails cleanly with a
/// clear message instead of desyncing (§9).
///
/// **v2 added the slave's answer byte** ([`RELAY_ANSWERED_BYTE`]), which is a
/// real framing change in both directions: a v1 slave never sends it, so a v2
/// master would wait out its answer timeout on every call; and a v2 slave
/// sends it to a v1 master that is not reading it, putting a stray `01` at the
/// head of the device's data.  Either way the pair must upgrade together,
/// which is exactly what the version check turns into one clear message
/// instead of a silent malfunction.
///
/// Taken deliberately **before 1.0.0 final**: the cost of this break is that
/// every deployed master/slave pair upgrades together, and that cost only
/// grows once there are releases people are holding at.
pub const RELAY_PROTOCOL_VERSION: u8 = 2;

/// Master→slave **relay hello**: the master writes these bytes as the very
/// first data on an accepted relay/registration channel, ahead of any
/// session or bridge bytes — magic `"EGR"` (Ethernet Gateway Relay) plus a
/// protocol-version byte.  The slave reads and validates it (see
/// [`read_relay_hello`]) before using the channel.  Its purpose is twofold:
///  1. **Accepted vs refused.** The russh client `exec()` future resolves
///     `Ok` even when the master answered the exec with `channel_failure`
///     (a refusing master — wrong role / `master_accept_relays=false` /
///     capacity), so a refused channel stays open and the slave used to
///     mistake it for a live registration and idle forever.  A refusing
///     master never writes the hello, so its absence (EOF/timeout) now
///     reliably signals refusal.
///  2. **Version skew.** A mismatched version byte fails with a clear
///     "upgrade the older gateway" message rather than a garbled session.
///
/// **When it is sent depends on the target, and that is the third purpose.**
/// For [`RelayTarget::Menu`] and [`RelayTarget::Kermit`] the master is itself
/// the far end, so accepting the channel *is* the answer and the hello goes out
/// at accept. For [`RelayTarget::Dial`] and [`RelayTarget::Peer`] the master
/// still has to place a call, and the slave turns this byte sequence straight
/// into a modem `CONNECT` with carrier asserted — so sending it at accept told
/// the device a call was up before anything had been dialled. Measured
/// 2026-08-21: `ATDT` through a master to an unreachable host answered
/// `CONNECT 19200` and then `NO CARRIER`, and did the same when the master
/// refused the dial outright on `allow_peer_dial`. `CONNECT` to a modem means
/// carrier; vintage terminal software and BBS scripts act on it. For those two
/// targets the hello is now written only once the call is up (see
/// [`answer_and_bridge`]), so its absence means exactly what the slave needs:
/// no call, answer `NO CARRIER`.
///
/// **That change moved no version**, because it changed only *when* these four
/// bytes are written and not what is on the wire; the skew it could cause was
/// an old slave against a new master on a dial that succeeds slowly, where the
/// old slave gives up at its fixed 5 s.  ([`RELAY_PROTOCOL_VERSION`] has since
/// moved to 2, for the separate reason recorded there -- the slave's answer
/// byte.  These bytes are still the same four.)
pub const RELAY_HELLO: [u8; 4] = [b'E', b'G', b'R', RELAY_PROTOCOL_VERSION];

/// How long the slave waits for the master's [`RELAY_HELLO`] when the master
/// answers at accept — [`RelayTarget::Menu`], [`RelayTarget::Kermit`] and a
/// `serial-register`.  Short: a real master writes the hello immediately, and
/// only a refusing or incompatible one leaves the channel silent.
const RELAY_HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the slave waits for the hello when it asked the master to **place
/// a call** — the master withholds it until the far end answers, so this has
/// to cover the longest the master might wait, with slack for the round trip.
/// Too short here does not merely mis-report: the slave would say `NO CARRIER`
/// while the master went on to connect, leaving a live call nobody is holding.
///
/// **Sized off [`RELAY_ANSWER_WAIT`], not off [`RELAY_PEER_ANSWER_WAIT`].**
/// Those were the same number until the crossbar grew an answer byte, and the
/// difference is a layer: on a crossbar dial the master does not ring anything
/// itself, it waits `RELAY_ANSWER_WAIT` for a *second* slave to report whether
/// its device picked up, and that slave's own ring is the
/// `RELAY_PEER_ANSWER_WAIT` inside it.  Left at the inner value the two
/// deadlines were equal while this one starts earlier -- before the master has
/// accepted the exec and parsed the target -- so a far device answering near
/// the end of its ring produced exactly the abandoned leg described above.
const RELAY_HELLO_TIMEOUT_DIALING: std::time::Duration =
    RELAY_ANSWER_WAIT.saturating_add(std::time::Duration::from_secs(5));

/// The hello wait for a given target — see [`RELAY_HELLO`] for why they differ.
fn hello_wait(target: &RelayTarget) -> std::time::Duration {
    match target {
        RelayTarget::Dial { .. } | RelayTarget::Peer { .. } => RELAY_HELLO_TIMEOUT_DIALING,
        RelayTarget::Menu | RelayTarget::Kermit => RELAY_HELLO_TIMEOUT,
    }
}

/// Tell the slave the call is up, then bridge the two halves.
///
/// **One function, because the hello and the bridge must not come apart.** The
/// dialing targets have four success sites between them (an onward TCP dial, a
/// local port, the CP/M endpoint, and the crossbar to another slave) and a
/// great many failure sites. Writing the hello at each success site by hand is
/// the arrangement where a fifth one added later silently forgets it and hangs
/// the slave until its timeout — so a caller that bridges gets the hello by
/// construction, and a caller that refuses simply never calls this.
async fn answer_and_bridge<S, B>(relay: &mut S, other: &mut B)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    // Flushed before any payload: the slave reads exactly these four bytes
    // before handing the stream to the bridge, so they must not sit in a
    // buffer behind the far end's banner.
    if relay.write_all(&RELAY_HELLO).await.is_err() || relay.flush().await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(relay, other).await;
}

/// A live slave→master relay: the SSH client session (kept alive for the
/// duration of the call — dropping it tears the connection down) and the
/// channel stream the modem bridge pumps through.
pub struct MasterRelay {
    /// Held only to keep the connection open; not otherwise used.
    pub _session: russh::client::Handle<SlaveRelayHandler>,
    pub stream: russh::ChannelStream<russh::client::Msg>,
}

/// The SSH client session handle for a relay call — held alive to keep
/// the connection open across a `+++` escape so ATO can resume it (see
/// `serial::ActiveConnection::Relay`).
pub type RelaySession = russh::client::Handle<SlaveRelayHandler>;
/// Read half of a preserved relay channel stream.
pub type RelayReadHalf = tokio::io::ReadHalf<russh::ChannelStream<russh::client::Msg>>;
/// Write half of a preserved relay channel stream.
pub type RelayWriteHalf = tokio::io::WriteHalf<russh::ChannelStream<russh::client::Msg>>;

/// Why a slave→master relay connect attempt failed.  The slave's reconnect
/// loop (§9 #14) backs off differently per class: a transient `Network`
/// error retries briskly (capped), while `Auth` / `Refused` back off hard —
/// hammering bad credentials trips the master's shared per-IP lockout
/// (3 failures → 5-minute ban) and would lock the slave's *own* IP out of
/// telnet/SSH/web, and hammering a master that is declining relays is
/// pointless until its config changes.
#[derive(Debug)]
pub enum RelayConnectError {
    /// Transport/network problem — master unreachable, link dropped, or the
    /// connect/handshake timed out.  Retry briskly with a capped backoff.
    Network(String),
    /// The master rejected our identity: wrong `slave_master_username` /
    /// `slave_master_password`, or a host-key problem (the *changed* /
    /// *missing* case — an unknown key is pinned and is not an error).
    /// Back off hard.
    Auth(String),
    /// Authenticated, but the master declined the relay channel — it is
    /// `standalone`, `master_accept_relays` is off, or it is an older build
    /// with no relay handler.  Back off hard; surface as a config issue.
    Refused(String),
}

impl RelayConnectError {
    /// The human-readable detail message.
    pub fn message(&self) -> &str {
        match self {
            RelayConnectError::Network(m)
            | RelayConnectError::Auth(m)
            | RelayConnectError::Refused(m) => m,
        }
    }
}

impl std::fmt::Display for RelayConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// Connect to the master's SSH server, authenticate with the slave's
/// stored master credentials, open a channel, and request the relay
/// `exec`.  On success the returned [`MasterRelay`] carries the channel
/// stream the caller bridges the UART to (and the session handle that
/// must stay alive for the call).  Caller runs on the blocking serial
/// thread and drives this via `Handle::block_on`.
pub async fn connect_master_relay(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    target: &RelayTarget,
    port_label: &str,
) -> Result<MasterRelay, RelayConnectError> {
    connect_relay_exec(
        host,
        port,
        username,
        password,
        &target.exec_command(port_label),
        hello_wait(target),
    )
    .await
}

/// Connect to the master and register a port as available (§9 #12).  The
/// master holds the channel idle in its remote-port registry
/// (`REMOTE_PORTS`, keyed by IP+label, mode-agnostic) until it is claimed —
/// by a Serial Gateway menu pick or a peer-dial — then signals with the
/// activate byte.  Used by both the console-registration loop (which then
/// bridges the UART) and the modem-port peer-dial announcer (which then rings
/// the local modem port); the master treats them uniformly.
pub async fn connect_master_register(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    port_label: &str,
    mode: &str,
    erase: &str,
) -> Result<MasterRelay, RelayConnectError> {
    connect_relay_exec(
        host,
        port,
        username,
        password,
        // **The mode is a second token, and the label is still the first.** A
        // master older than this addition takes the whole remainder as the label
        // and will register `"B console"`, so a mixed pair shows an odd address
        // in the picker and dial-by-name (`ATDT B@ip`) stops resolving until the
        // master is upgraded -- the menu pick still works, because it uses the
        // label it was given. That degradation is the reason the mode goes after
        // the label rather than before it.
        // **The erase key is a THIRD token, added the same way and for the same
        // reason the mode was.** The slave folds it in its own process, so a
        // master had no way to know a remote console port was rewriting bytes --
        // and that is the case this setting was first reported for. A master too
        // old to expect it simply ignores the extra token (it splits on
        // whitespace and takes what it knows), so a new slave against an old
        // master degrades to exactly today's behaviour rather than breaking.
        &format!("serial-register {} {} {}", port_label, mode, erase),
        RELAY_HELLO_TIMEOUT,
    )
    .await
}

/// Parse the arguments of a `serial-register` exec, the other half of the
/// command [`connect_master_register`] builds.
///
/// Here rather than inline in the SSH handler so the two halves of the grammar
/// sit together: a builder and a parser that live apart drift apart, and this
/// one has now grown a token twice.
///
/// **Tokens, never "the remainder".** The label came first and alone; the mode
/// was added as a second token and the erase key as a third. A master that took
/// everything after the space as the label registered `"B console"` the moment a
/// newer slave appeared, so each addition has to be a token a reader can ignore.
/// Anything beyond the third is ignored for the same reason -- a fourth is how
/// this grammar has grown twice already.
///
/// A missing token is `None` rather than a default: "we were not told" and "it
/// is set to pass-through" are different answers, and only the first should keep
/// a screen quiet about something it cannot see.
pub fn parse_register_args(rest: &str) -> (String, RemotePortFacts) {
    let mut toks = rest.split_whitespace();
    let label = toks.next().unwrap_or("").to_string();
    (
        label,
        RemotePortFacts {
            mode: toks.next().map(str::to_string),
            erase: toks.next().map(str::to_string),
        },
    )
}

/// Shared connect+auth+channel+exec, bounded so a wedged master can't freeze
/// the serial thread.
///
/// **The bound is `RELAY_CONNECT_TIMEOUT` plus whatever the target's hello wait
/// exceeds the default by** -- 15s for the targets a master answers at accept,
/// 50s for a Dial or Peer, which waits on a call actually being placed.  Said
/// here because the summary named the constant alone while the body already
/// said otherwise, which is the same wrong number this function used to print
/// in its own timeout message.
async fn connect_relay_exec(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    exec_command: &str,
    hello_wait: std::time::Duration,
) -> Result<MasterRelay, RelayConnectError> {
    // The outer budget has to cover the hello wait, or a dialing target would
    // be cut off by this timeout before its own wait expired -- and reported as
    // a *network* failure (brisk retry) rather than the refusal it is.  Written
    // as connect + whatever the hello wait exceeds the default by, so the
    // paths that answer at accept keep exactly the budget they had.
    let budget = RELAY_CONNECT_TIMEOUT
        .saturating_add(hello_wait.saturating_sub(RELAY_HELLO_TIMEOUT));
    match tokio::time::timeout(
        budget,
        connect_master_relay_inner(host, port, username, password, exec_command, hello_wait),
    )
    .await
    {
        Ok(result) => result,
        // A handshake/auth stall is a transport problem, not a credential
        // one — classify as Network so the slave retries briskly.
        // `budget`, not `RELAY_CONNECT_TIMEOUT`: the two are equal only for the
        // targets the master answers at accept.  A Dial/Peer attempt is allowed
        // the longer hello wait as well, so naming the constant reported 15s
        // after a stall of up to 50 -- a number that sends whoever reads the
        // log looking for the wrong thing.
        Err(_) => Err(RelayConnectError::Network(format!(
            "timed out after {}s connecting to master {}:{}",
            budget.as_secs(),
            host,
            port
        ))),
    }
}

async fn connect_master_relay_inner(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    exec_command: &str,
    hello_wait: std::time::Duration,
) -> Result<MasterRelay, RelayConnectError> {
    // Keepalive (§9 #15): without it a silently-dropped relay link (master
    // powered off, cable pulled, NAT idle-eviction) isn't noticed until the
    // next write fails — leaving an idle console registration wedged.  Ping
    // every 30s and give up after 3 unanswered, so a dead link is detected
    // in ~2 min and the slave's reconnect loop (#14) re-establishes.  No
    // `inactivity_timeout` — an idle-but-alive registration must stay up.
    let config = Arc::new(russh::client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    });
    let server_key = std::sync::Arc::new(std::sync::Mutex::new(None));
    let handler = SlaveRelayHandler {
        server_key: server_key.clone(),
    };
    let mut session = russh::client::connect(config, (host, port), handler)
        .await
        .map_err(|e| RelayConnectError::Network(format!("connect failed: {}", e)))?;

    // Verify the master's host key against known-hosts (TOFU): pin on
    // first contact, reject on a changed key (the slave is about to send
    // the master's *unified* credentials, so a MITM would harvest a full
    // login — see review finding).
    let presented = server_key.lock().ok().and_then(|mut s| s.take());
    match presented {
        Some(key) => match crate::telnet::check_known_host(host, port, &key) {
            crate::telnet::HostKeyStatus::Known => {}
            crate::telnet::HostKeyStatus::Unknown => {
                crate::telnet::save_known_host(host, port, &key);
                glog!(
                    "Relay (slave): pinned master {}:{} host key {} (first contact)",
                    host,
                    port,
                    key.fingerprint(russh::keys::HashAlg::Sha256)
                );
            }
            crate::telnet::HostKeyStatus::Unreadable(e) => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "known-hosts unreadable", "")
                    .await;
                // This path auto-pins without a prompt, so an unreadable
                // known-hosts file must be a hard failure: falling through to
                // the Unknown arm would re-pin whatever key is presented and
                // then send the master's unified credentials to it.  Auth
                // class so we back off rather than hammer — a permissions
                // problem won't fix itself on a retry.
                return Err(RelayConnectError::Auth(format!(
                    "cannot verify master {}:{} host key — the known-hosts file \
                     exists but could not be read ({}); refusing rather than \
                     re-pinning. Fix its permissions and reconnect.",
                    host, port, e
                )));
            }
            crate::telnet::HostKeyStatus::Changed => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "host key changed", "")
                    .await;
                // **Offer the fix, do not take it.** A changed key is either a
                // reinstalled master or a man in the middle and nothing here can
                // tell which, so it goes on the resolvable-problems list where an
                // operator can act on it from any of the three configuration
                // surfaces. Before this, the only remedy was a log line telling
                // somebody to hand-edit `gateway_hosts` -- which is no remedy at
                // all on a headless slave reached from a C64.
                crate::resolve::report(crate::resolve::Problem::MasterHostKeyChanged {
                    host: host.to_string(),
                    port,
                });
                // A changed key won't fix itself and may be a MITM — back
                // off hard (Auth class) rather than hammer.
                return Err(RelayConnectError::Auth(format!(
                    "master {}:{} host key CHANGED — refusing (possible MITM); \
                     remove the stale gateway_hosts entry if the master was reinstalled",
                    host, port
                )));
            }
        },
        None => {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "no host key", "")
                .await;
            return Err(RelayConnectError::Auth(
                "master presented no host key".to_string(),
            ));
        }
    }

    // **Offer the key before the password.**  `slave_master_password` is the
    // only cleartext secret left in `egateway.conf`, and it cannot be hashed
    // the way `password` is: the slave *presents* it, and a hash cannot be
    // presented.  So the secret is removed rather than disguised -- the slave
    // proves itself with the Ed25519 key it already generates for outbound SSH,
    // once the master has that key in its `relay_authorized_keys`.
    //
    // The password remains the fallback, so an installation that has enrolled
    // nothing is unchanged and an upgrade needs no coordination.  A master too
    // old to know about keys simply refuses the method and the password runs.
    let mut authed = false;
    let mut by_key = false;
    match crate::ssh::load_or_generate_client_key() {
        Ok(key) => {
            let hash_alg = session.best_supported_rsa_hash().await.ok().flatten().flatten();
            match session
                .authenticate_publickey(
                    username,
                    russh::keys::PrivateKeyWithHashAlg::new(std::sync::Arc::new(key), hash_alg),
                )
                .await
            {
                Ok(russh::client::AuthResult::Success) => {
                    authed = true;
                    by_key = true;
                    note_relay_key_auth(true);
                    glog!("Relay: authenticated to master {}:{} by public key", host, port);
                }
                // Not enrolled (or this master predates key auth).  That is an
                // ordinary state, not a fault: fall through to the password.
                //
                // It is also the moment a *previously* enrolled key stopped
                // working, so the claim comes down here as well as going up
                // above -- the configuration screens must not go on saying
                // "using key" about a slave that is back on its password.
                Ok(_) => note_relay_key_auth(false),
                Err(e) => {
                    return Err(RelayConnectError::Network(format!("key auth error: {}", e)))
                }
            }
        }
        // No usable key is not fatal while a password exists -- say so once and
        // carry on, rather than failing a slave that was working.
        Err(e) => {
            note_relay_key_auth(false);
            glog!("Relay: no client key ({}); falling back to the password", e)
        }
    }

    // A password typed at a screen wins over the configured one, and never
    // touched the disk to get here.
    let password = &master_password_to_try(password);

    if !authed {
        // **An empty password after a refused key is a diagnosis, not a
        // rejection.**  It means the operator has moved to keys and the
        // enrolment has not been done (or was undone), and saying so here is
        // the difference between a five-minute fix and reading a log for an
        // hour.
        if password.is_empty() {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                .await;
            note_master_credential_needed(host, port);
            return Err(RelayConnectError::Auth(format!(
                "master {}:{} refused this slave's public key and no \
                 slave_master_password is set — enter the master's password on \
                 any configuration screen, or add this slave's key to {} on the \
                 master (the slave logs it at startup)",
                host, port, crate::ssh::RELAY_AUTHORIZED_KEYS_FILE
            )));
        }
        match session.authenticate_password(username, password).await {
            Ok(russh::client::AuthResult::Success) => {}
            Ok(_) => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                    .await;
                // **A refused password puts the ask back.**  All three surfaces
                // clear the "master password needed" flag the moment somebody
                // types one -- rightly, because a screen still demanding a
                // password just entered reads as "it did not take".  But the
                // only place that flag was ever *raised* is the empty-password
                // branch above, which a pending password stops us reaching --
                // so one wrong answer silenced the prompt for good, and a
                // headless slave went on failing with nothing on any screen.
                // Measured live on 141: the screen was gone on the very next
                // session while the relay was still being refused.
                //
                // The wrong one is dropped rather than kept: the master
                // *answered*, so this is a definite refusal and not a
                // transport blip (that is the `Err` arm below).  Retrying a
                // credential the master has already rejected only walks the
                // slave's IP toward the per-IP lockout it shares with telnet.
                note_password_refused(host, port);
                return Err(RelayConnectError::Auth(
                    "authentication rejected by master".to_string(),
                ));
            }
            // A transport error mid-auth is network, not a credential rejection.
            Err(e) => return Err(RelayConnectError::Network(format!("auth error: {}", e))),
        }
    }

    clear_master_credential_needed();
    if by_key {
        // **The key works, so the password is no longer needed -- and only now
        // is that safe to act on.**  Wiping it on the strength of the master
        // saying "stored" would strand this slave if the enrolment were lost
        // between then and the next connect; waiting for a key login to
        // actually succeed proves the whole path before discarding the fallback.
        // The typed one first: it is the copy that exists right now, and the
        // config may never have had one at all.
        clear_pending_master_password();
        forget_master_password();
    } else {
        // Authenticated by password: offer the key, so the next connection can
        // use it and this one's credential can go.  Best-effort and fire-and-
        // forget -- whether it worked is answered by the next connect, not by a
        // reply, and a master too old to know the command simply refuses the
        // channel.
        offer_key_for_enrolment_once(&session).await;
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| RelayConnectError::Network(format!("channel open failed: {}", e)))?;
    // The master sends channel_failure to a non-master / relays-off / older
    // build, surfacing here as an exec error — that's a *refusal* (config),
    // not a transient network fault, so back off hard rather than hammer.
    channel
        .exec(true, exec_command.as_bytes())
        .await
        .map_err(|e| RelayConnectError::Refused(format!("relay declined by master: {}", e)))?;

    // The key verified and the channel opened, so any pinned-key problem for
    // this master no longer applies. Withdrawing it here rather than leaving it
    // for the operator to dismiss is what stops the list becoming a place stale
    // warnings accumulate.
    crate::resolve::clear(&crate::resolve::Problem::MasterHostKeyChanged {
        host: host.to_string(),
        port,
    }
    .id());

    let mut stream = channel.into_stream();
    // §9 handshake: read the master's relay hello before handing the
    // channel to the caller.  This is what distinguishes an ACCEPTED relay
    // from a refused-but-open channel (russh `exec()` returns Ok even on
    // the master's `channel_failure`) and catches a protocol-version skew.
    read_relay_hello(&mut stream, hello_wait).await?;
    Ok(MasterRelay {
        _session: session,
        stream,
    })
}

/// Read and validate the master's [`RELAY_HELLO`] from a freshly-accepted
/// relay channel.  Maps every failure mode to a [`RelayConnectError`] the
/// slave's reconnect loop (§9 #14) can classify:
/// - no hello (EOF or [`RELAY_HELLO_TIMEOUT`]) ⇒ `Refused` (the master is
///   declining relays / standalone / an older build with no relay handler);
/// - wrong magic ⇒ `Refused` (not our relay protocol on this channel);
/// - version mismatch ⇒ `Refused`, with an explicit upgrade message.
async fn read_relay_hello<R>(
    stream: &mut R,
    wait: std::time::Duration,
) -> Result<(), RelayConnectError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut hello = [0u8; RELAY_HELLO.len()];
    match tokio::time::timeout(wait, stream.read_exact(&mut hello)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => {
            return Err(RelayConnectError::Refused(
                "master accepted the channel but sent no relay hello — it is \
                 refusing relays (not a master, master_accept_relays off, \
                 allow_relay_kermit off for a Kermit port, at capacity) or is \
                 an incompatible build"
                    .to_string(),
            ));
        }
        Err(_) => {
            return Err(RelayConnectError::Refused(format!(
                "no answer after {}s — the master did not take the call \
                 (dial refused or unreachable), or relays are disabled / \
                 standalone / allow_relay_kermit off / incompatible master",
                wait.as_secs()
            )));
        }
    }
    if hello[..3] != RELAY_HELLO[..3] {
        return Err(RelayConnectError::Refused(format!(
            "master did not send a valid relay hello (got {:02x?}) — \
             incompatible or non-relay endpoint",
            hello
        )));
    }
    let master_version = hello[3];
    if master_version != RELAY_PROTOCOL_VERSION {
        return Err(RelayConnectError::Refused(format!(
            "relay protocol version mismatch: master v{}, slave v{} — \
             upgrade the older gateway",
            master_version, RELAY_PROTOCOL_VERSION
        )));
    }
    Ok(())
}

// ─── Master-side remote-port registry (console-mode, §9 #12) ──────

/// Master→slave control byte sent on a registration channel when a master
/// user picks that remote console port: "a user attached — start bridging
/// your UART".  The slave reads exactly one byte before entering its
/// transparent console bridge; the value is ignored (positional), so no
/// in-band escaping of the subsequent raw byte stream is needed.
pub const RELAY_ACTIVATE_BYTE: u8 = 0x01;

/// Slave→master **answer byte**: the one byte a slave writes back on a
/// registration channel after [`RELAY_ACTIVATE_BYTE`], saying what became of
/// the call.  [`RELAY_ANSWERED_BYTE`] means the bridge follows immediately;
/// [`RELAY_NO_ANSWER_BYTE`], [`RELAY_BUSY_BYTE`] and [`RELAY_ERROR_BYTE`] each
/// mean no call, and the channel is closing.  See
/// [`outcome_from_answer_byte`] for why the failures are told apart rather
/// than being one "no".
///
/// **This exists because claiming is not answering.**  Activating a
/// registration channel is a map removal and one byte; the far slave then
/// rings its *own* device and bridges only if that device picks up.  Until v2
/// it told the master nothing when it did not, so a crossbar peer-dial to an
/// unanswered device gave the caller `CONNECT` and then `NO CARRIER` -- the
/// same defect the deferred hello removed from every path the master can
/// observe for itself, surviving on the one path it cannot (measured
/// 2026-08-21, recorded then rather than half-fixed because closing it needs
/// this byte, and this byte needs a version bump).
///
/// **Every activated channel sends exactly one**, including a console port,
/// which in practice always answers: its UART is already open by the time it
/// registers, so nothing is left to fail between the activate byte and the
/// bridge.  It sends the byte anyway, because a rule with an exception is a
/// rule the reader has to hold two versions of -- and the master would
/// otherwise have to know which *kind* of port it had claimed before it knew
/// how to read the stream, which is a fact the registry does not always
/// carry (`RemotePort::mode` is an `Option`).
///
/// It is positional, like the activate byte it answers: one byte, before any
/// bridged data, so nothing downstream needs escaping.
pub const RELAY_ANSWERED_BYTE: u8 = 0x01;

/// Rang, nobody picked up — [`crate::serial::PeerCallOutcome::NoAnswer`].
pub const RELAY_NO_ANSWER_BYTE: u8 = 0x02;

/// Never started ringing: the port is in another call, or not idle at its
/// prompt — [`crate::serial::PeerCallOutcome::Busy`].
pub const RELAY_BUSY_BYTE: u8 = 0x03;

/// The port errored or its thread went away —
/// [`crate::serial::PeerCallOutcome::Error`].
pub const RELAY_ERROR_BYTE: u8 = 0x04;

/// **Why the answer is an outcome and not a yes/no.**
///
/// A peer-dial to a port on *this* gateway already distinguishes these: a busy
/// target answers `BUSY` and an unanswered ring answers `NO ANSWER`, which are
/// documented modem result codes 7 and 8 at `X3` and above.  A boolean here
/// would have collapsed all of them to `NO CARRIER`, so the same `ATD B@<ip>`
/// would answer one thing when the port is on this gateway and another when it
/// is on a slave -- a difference the caller has no way to account for and no
/// reason to expect.  The slave already computes the outcome; this carries it
/// the rest of the way.
///
/// Decoding is total: any byte that is not one of these is an
/// [`crate::serial::PeerCallOutcome::NoAnswer`], which is also what a v2 peer
/// sending a value added later would degrade to.  That keeps a future addition
/// here off the version byte -- it is the *presence* of the answer byte that
/// v2 introduced, not its vocabulary.
fn outcome_from_answer_byte(b: u8) -> Result<(), crate::serial::PeerCallOutcome> {
    use crate::serial::PeerCallOutcome as O;
    match b {
        RELAY_ANSWERED_BYTE => Ok(()),
        RELAY_BUSY_BYTE => Err(O::Busy),
        RELAY_ERROR_BYTE => Err(O::Error),
        // RELAY_NO_ANSWER_BYTE and anything unrecognised.
        _ => Err(O::NoAnswer),
    }
}

/// The wire byte for an outcome — the inverse of [`outcome_from_answer_byte`].
fn answer_byte_for_outcome(outcome: Result<(), crate::serial::PeerCallOutcome>) -> u8 {
    use crate::serial::PeerCallOutcome as O;
    match outcome {
        Ok(()) => RELAY_ANSWERED_BYTE,
        Err(O::Busy) => RELAY_BUSY_BYTE,
        Err(O::Error) => RELAY_ERROR_BYTE,
        Err(O::NoAnswer) | Err(O::Answered) => RELAY_NO_ANSWER_BYTE,
    }
}

/// Slave→master: report whether the local endpoint answered, as the one byte
/// the master is waiting for after its activate byte.
///
/// Returns whether the byte reached the wire.  A failure here is not worth
/// acting on -- it means the channel is already gone, which the caller is
/// about to discover anyway -- but it is worth *not* pretending succeeded, so
/// the result is returned rather than discarded inside.
///
/// Generic over the stream so the three slave paths (console, modem and the
/// CP/M endpoint) share one statement of the framing.  They had three copies
/// of the ring-and-bridge shape and none of them told the master anything,
/// which is precisely how the same omission ended up in all three.
pub async fn send_peer_answer<S>(
    stream: &mut S,
    outcome: Result<(), crate::serial::PeerCallOutcome>,
) -> bool
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let byte = answer_byte_for_outcome(outcome);
    stream.write_all(&[byte]).await.is_ok() && stream.flush().await.is_ok()
}

/// How long a master waits for the slave's answer byte.
///
/// It has to outlast the slave's own ring, which runs for
/// [`RELAY_PEER_ANSWER_WAIT`], plus a round trip -- the same shape and the same
/// reason as [`RELAY_HELLO_TIMEOUT_DIALING`] one layer out.  Too short does not
/// merely mis-report: the master would give up and tear the channel down while
/// the slave was still ringing a device that then answers into nothing.
pub const RELAY_ANSWER_WAIT: std::time::Duration =
    RELAY_PEER_ANSWER_WAIT.saturating_add(std::time::Duration::from_secs(5));

/// A registered remote console port: the master's end of the idle SSH
/// registration channel, paired with the generation stamped when it was
/// registered (see [`REMOTE_PORTS`] for why the generation matters).
/// A registered remote port: the master's end of the channel, the generation
/// stamp that guards a re-register race, and the mode the slave reported.
///
/// The mode is `None` for a slave too old to send one -- see
/// [`register_remote_port`].
type RegisteredPort = (tokio::io::DuplexStream, u64, RemotePortFacts);

/// What the wire told us about a registered port, beyond where it is.
///
/// A struct rather than two more tuple slots: both fields are
/// `Option<String>` and sit side by side, so a positional pair could be
/// swapped at any call site and still compile -- the master would then grey a
/// picker row by the erase key and warn about the mode.
///
/// **Every field is `Option` because each is a fact the wire may not carry.**
/// A slave older than the addition that introduced it sends nothing, and the
/// honest rendering of "we were not told" is to say nothing rather than guess a
/// default. Guessing would put "Modem mode" beside a console port, or -- worse
/// for `erase` -- stay silent about a port that really is rewriting bytes, or
/// warn about one that is not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemotePortFacts {
    /// The slave's `serial_*_mode` for this port, when it said.
    pub mode: Option<String>,
    /// The slave's `serial_*_backspace`, when it said. Only meaningful on a
    /// console port -- the slave folds in its own process, and this is the only
    /// way a master can know it is happening.
    pub erase: Option<String>,
}

/// Slave ports currently registered with this master, keyed by `(slave IP,
/// port label)`.  Each value pairs the master's end of the idle SSH
/// registration channel with a monotonic **generation** stamped at
/// registration time.  Mode-agnostic: a **console** port (bridged on claim)
/// and a **modem** port (the peer-dial announcer, which *rings* the slave's
/// local port on claim) both register through the same `serial-register`
/// path, so both appear here — and both are claimable by the Serial Gateway
/// picker and by a peer-dial (`claim_remote_peer`).  Populated by `ssh.rs`
/// `exec_request`, drained by a claim or by channel teardown.
///
/// The generation disambiguates a re-registration race: if a slave whose
/// link briefly dropped re-registers the same `(IP, label)` on a fresh
/// channel *before* the master observes the old channel close, the new
/// stream overwrites the old in the map.  Without the generation, the old
/// channel's teardown — which removes by `(IP, label)` — would evict the
/// new, live registration.  Teardown therefore removes only when the
/// stored generation matches the one it registered (`remove_remote_port_gen`),
/// while a picker claim (`remove_remote_port`) always takes whatever is
/// current.
static REMOTE_PORTS: StdMutex<Option<HashMap<(IpAddr, String), RegisteredPort>>> =
    StdMutex::new(None);

/// Monotonic source for the per-registration generation stamp.
static REMOTE_PORT_GEN: AtomicU64 = AtomicU64::new(0);

/// Register (or replace) a console-mode remote port as available.  Returns
/// the generation stamp the caller must keep so its later teardown can
/// remove the entry *only if it is still the same registration* (see
/// [`remove_remote_port_gen`]).  Marked `#[must_use]`: dropping the
/// generation silently reintroduces the re-register eviction race.
#[must_use]
pub fn register_remote_port(
    slave_ip: IpAddr,
    label: String,
    facts: RemotePortFacts,
    stream: tokio::io::DuplexStream,
) -> u64 {
    let generation = REMOTE_PORT_GEN.fetch_add(1, Ordering::Relaxed);
    let mut g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    let evicted = g
        .get_or_insert_with(HashMap::new)
        .insert((slave_ip, label.clone()), (stream, generation, facts));
    // **Say when a registration displaced a live one, without naming a cause.**
    // The key is `(peer IP, label)`, so an entry is replaced by either of two
    // things and this function cannot tell them apart.  The ordinary one is the
    // same slave reconnecting -- the master only drops an entry when it observes
    // the channel teardown, so a slave that noticed the dead link first
    // (keepalive, a NAT idle eviction, a restart whose RST we have not reaped)
    // re-registers over its own stale entry, which is correct and expected.
    // The other is two gateways sharing a source address -- a site behind NAT,
    // or two instances on one host -- each evicting the other for ever.
    //
    // An earlier version of this line asked "two slaves behind one address?",
    // which is the rarer of the two and would have fired on every ordinary
    // reconnect: a line that misnames a cause is a line an operator learns to
    // scroll past, and it would have been wrong most of the times it appeared.
    // It reports the fact and leaves the diagnosis to the reader, who can see
    // from the surrounding lines whether one slave is reconnecting or two are
    // trading the key.
    //
    // Logged, not refused: a re-register has to be allowed to win, so this
    // cannot arbitrate.  Only a stable per-slave instance id on the wire could,
    // and that is a grammar change.
    if evicted.is_some() {
        glog!(
            "Relay: {} registered port {} over a registration still held for \
             that address",
            slave_ip,
            label
        );
    }
    generation
}

/// **Claim** a registered remote port for bridging, returning the master's
/// channel end if present (the caller then owns the stream).  Takes
/// whatever is currently registered regardless of generation — the picker
/// always wants the live entry.
pub fn remove_remote_port(slave_ip: IpAddr, label: &str) -> Option<tokio::io::DuplexStream> {
    let mut g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    g.as_mut()
        .and_then(|m| m.remove(&(slave_ip, label.to_string())))
        .map(|(stream, _gen, _mode)| stream)
}

/// Drop a *specific* registration on channel teardown: removes the entry
/// only when its stored generation matches `gen`, so an old channel's
/// teardown can't evict a newer re-registration of the same `(IP, label)`.
/// Returns the stream if it was the matching registration (so the caller
/// can drop it deterministically), else `None`.
pub fn remove_remote_port_gen(
    slave_ip: IpAddr,
    label: &str,
    generation: u64,
) -> Option<tokio::io::DuplexStream> {
    let mut g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.as_mut()?;
    let key = (slave_ip, label.to_string());
    match map.get(&key) {
        Some((_, stored, _)) if *stored == generation => {
            map.remove(&key).map(|(stream, _, _)| stream)
        }
        _ => None,
    }
}

/// Parse a peer-dial address `<Port>@<host>` into `(ip, LABEL)` when the
/// host is an IP literal, for looking a *remote* registered port up in
/// [`REMOTE_PORTS`] (keyed by the slave's peer IP + uppercase label).
/// `None` if the label isn't `A`/`B`/`CPM` or the host isn't an IP.
pub fn parse_remote_peer_addr(addr: &str) -> Option<(IpAddr, String)> {
    let (label, host) = addr.split_once('@')?;
    let label = label.trim().to_ascii_uppercase();
    if label != "A" && label != "B" && label != "CPM" {
        return None;
    }
    let ip: IpAddr = host.trim().trim_start_matches('[').trim_end_matches(']').parse().ok()?;
    Some((ip, label))
}

/// What claiming a registered remote port came to.
///
/// **`Answered` and `Failed` are different facts, and so are `Failed` and
/// `NotRegistered`.**  Collapsing the first pair is the defect this type exists
/// to prevent -- describing a claimed channel as a connection is what put
/// `CONNECT` in front of `NO CARRIER`.  Collapsing the second is milder but
/// still wrong: they want different words, since "nothing is registered there"
/// is the operator's problem to fix and "it did not pick up" is the caller's to
/// retry.  `Failed` then carries *why*, so the caller can say `BUSY` where a
/// local dial would.
#[derive(Debug)]
pub enum PeerClaim {
    /// The far endpoint picked up.  The stream is the live bridge.
    Answered(tokio::io::DuplexStream),
    /// Registered and activated, but the call did not complete.
    ///
    /// Carries the slave's own [`crate::serial::PeerCallOutcome`] so a crossbar
    /// call site can answer the caller exactly as the local one does -- `BUSY`
    /// for a port already in a call, `NO ANSWER` for a ring nobody picked up.
    /// [`crate::serial::PeerCallOutcome::Answered`] never appears here; that
    /// case is [`PeerClaim::Answered`], which carries the stream instead.
    Failed(crate::serial::PeerCallOutcome),
    /// Nothing is registered under that address and label.
    NotRegistered,
}

/// Claim a registered remote port, signal the slave to start bridging
/// (`RELAY_ACTIVATE_BYTE`), and **wait for the slave to say whether its local
/// endpoint answered** ([`RELAY_ANSWERED_BYTE`]).
///
/// The wait is the whole point: activating is a map removal and one byte, so
/// before v2 this returned a "connected" stream for a device that was still
/// ringing and might never pick up.  Every master-side claim goes through here
/// -- the peer-dial crossbar, a local modem's `ATD`, the CP/M endpoint and the
/// Serial Gateway picker -- because the answer byte is now part of the framing
/// and a claimer that did not consume it would hand its user a stray `01` as
/// the first byte of the session.
pub async fn claim_remote_peer(ip: IpAddr, label: &str) -> PeerClaim {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(mut stream) = remove_remote_port(ip, label) else {
        return PeerClaim::NotRegistered;
    };
    if stream.write_all(&[RELAY_ACTIVATE_BYTE]).await.is_err() || stream.flush().await.is_err() {
        // The channel died between the registry read and the activate byte.
        // The port *was* registered, so this is not `NotRegistered`; the slave
        // going away mid-claim is its port erroring as far as the caller can
        // tell.
        return PeerClaim::Failed(crate::serial::PeerCallOutcome::Error);
    }
    let mut answer = [0u8; 1];
    match tokio::time::timeout(RELAY_ANSWER_WAIT, stream.read_exact(&mut answer)).await {
        Ok(Ok(_)) => match outcome_from_answer_byte(answer[0]) {
            Ok(()) => PeerClaim::Answered(stream),
            // Not bridged: an answer byte that is not `ANSWERED` is a refusal,
            // and an unrecognised one is treated as a refusal too rather than
            // being passed on -- an unknown control byte is not data, and
            // guessing would put it in front of the user's session.
            Err(why) => PeerClaim::Failed(why),
        },
        // EOF (the slave dropped the channel) or silence past the wait.  Both
        // are "no call", and neither is a busy signal we can honestly claim.
        _ => PeerClaim::Failed(crate::serial::PeerCallOutcome::NoAnswer),
    }
}

/// List the currently-registered remote console ports, sorted stably so
/// the picker order doesn't jump around between redraws.
/// How many distinct **slaves** are registered with this master right now.
///
/// Counted by address, not by port: the registry is keyed by
/// `(slave IP, port label)` and one slave commonly registers both its serial
/// ports and its CP/M endpoint, so a port count would report one machine as
/// three and a master with two slaves as "six connected".
pub fn connected_slave_count() -> usize {
    let mut ips: Vec<IpAddr> = list_remote_ports().into_iter().map(|p| p.ip).collect();
    ips.sort();
    ips.dedup();
    ips.len()
}

pub fn list_remote_ports() -> Vec<RemotePort> {
    let g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<RemotePort> = g
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|((ip, label), (_, _, facts))| RemotePort {
                    ip: *ip,
                    label: label.clone(),
                    mode: facts.mode.clone(),
                    erase: facts.erase.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    // Sorted by the pair that identifies a port, as before -- the mode is
    // description, not identity, so it must not affect the order a picker draws.
    v.sort_by(|a, b| (a.ip, &a.label).cmp(&(b.ip, &b.label)));
    v
}

/// One registered remote port, as a picker sees it.
///
/// **The mode is `Option`, because it is a fact the wire may not carry.** A
/// slave older than this protocol addition sends only its label, and the honest
/// rendering of "we were not told" is to say nothing rather than to guess a
/// default -- guessing would put "Modem mode" beside a console port, which is
/// worse than a bare address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePort {
    pub ip: IpAddr,
    pub label: String,
    /// The slave's `serial_*_mode` for this port, when it said.
    pub mode: Option<String>,
    /// The slave's `serial_*_backspace`, when it said. `None` from a slave too
    /// old to send it, and the master then says nothing rather than guessing.
    pub erase: Option<String>,
}

impl RemotePort {
    /// `B@192.168.1.141`, the string an operator types to dial it.
    pub fn address(&self) -> String {
        format!("{}@{}", self.label, self.ip)
    }

    /// How the mode reads on a picker row, in the same words the local rows use
    /// (`Console mode`, `Modem mode`) -- two lists that described one thing
    /// differently would be worse than one list.
    ///
    /// All four arms were checked against a live master/slave pair: a console
    /// port reads `Console mode`, a modem port `Modem mode`, and the CP/M
    /// endpoint `CP/M emulator`. **`kermit` is unreachable from the picker
    /// today**, and deliberately so rather than by omission: a Kermit-mode slave
    /// port takes the *relay* path (`serial-relay <port> kermit`, "asking master
    /// to serve Kermit") instead of registering, because its wire is served by
    /// the master's Kermit server rather than picked by a user. The arm stays so
    /// that a Kermit port which ever did register would be named rather than
    /// bare.
    pub fn mode_label(&self) -> Option<&'static str> {
        match self.mode.as_deref() {
            Some("console") => Some("Console mode"),
            Some("modem") => Some("Modem mode"),
            Some("kermit") => Some("Kermit server"),
            Some("emulator") => Some("CP/M emulator"),
            _ => None,
        }
    }
}

// ─── Slave-side link status (observability, §9 #10) ──────────

/// Live state of a slave console port's registration link to its master,
/// surfaced read-only by the telnet Master/Slave status screen so an
/// operator can see whether a slave is actually reaching its master without
/// grepping logs.  Per port (A/B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaveLinkState {
    /// Not connected — idle, backing off after a failure, or unconfigured.
    Down = 0,
    /// Reaching / authenticating with the master (includes retry backoff).
    Connecting = 1,
    /// Registered with the master; idle, awaiting a pick.
    Registered = 2,
    /// A master user picked this port; actively bridging the console.
    Bridging = 3,
}

impl SlaveLinkState {
    /// Short human label for the status screen.
    pub fn label(self) -> &'static str {
        match self {
            SlaveLinkState::Down => "down",
            SlaveLinkState::Connecting => "connecting",
            SlaveLinkState::Registered => "registered",
            SlaveLinkState::Bridging => "bridging",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => SlaveLinkState::Connecting,
            2 => SlaveLinkState::Registered,
            3 => SlaveLinkState::Bridging,
            _ => SlaveLinkState::Down,
        }
    }
}

/// Per-port (index A=0, B=1) slave link state.  Written by the slave
/// console register loop (`serial::console_slave_register_tick`), read by
/// the telnet status screen.  `Relaxed` is fine — it is a single-value
/// status indicator with no ordering dependency on other state.
static SLAVE_LINK: [AtomicU8; 2] = [AtomicU8::new(0), AtomicU8::new(0)];

/// Record a slave port's current link state (no-op for an out-of-range
/// index, though only A/B exist).
pub fn set_slave_link(port_index: usize, state: SlaveLinkState) {
    if let Some(cell) = SLAVE_LINK.get(port_index) {
        cell.store(state as u8, Ordering::Relaxed);
    }
}

/// What a configuration surface should say about this slave's link to its
/// master, aggregated over its ports.
///
/// **One answer for all three surfaces**, the same reason
/// `master_password_state` is shared: telnet, the web editor and the desktop
/// must not describe one link three ways.
///
/// Aggregated rather than per-port because the question the credential boxes
/// answer is "can this gateway reach its master at all" -- a per-port list is
/// the Master/Slave screen's job, and it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlaveRelayStatus {
    /// At least one port is registered with the master (or bridging a call).
    Connected,
    /// Something is mid-attempt, including the retry backoff.
    Connecting,
    /// Nothing can be tried: there is no usable credential for the master.
    CredentialNeeded,
    /// Configured as a slave, but nothing is attempting anything -- typically
    /// no port is enabled, so no register loop exists to run.
    Idle,
}

impl SlaveRelayStatus {
    /// The word the credential boxes show. Short: it is drawn *inside* a text
    /// box on the narrowest of the three surfaces.
    pub fn label(self) -> &'static str {
        match self {
            SlaveRelayStatus::Connected => "Connected",
            SlaveRelayStatus::Connecting => "Connecting...",
            SlaveRelayStatus::CredentialNeeded => "Password needed",
            SlaveRelayStatus::Idle => "Not connected",
        }
    }

    /// Whether the credential fields should stand in for themselves rather
    /// than be editable: once the link is up there is nothing to type.
    pub fn stands_in_for_credentials(self) -> bool {
        matches!(self, SlaveRelayStatus::Connected)
    }
}

/// The current link status, over both ports.
///
/// `Connected` outranks everything: one working port means this gateway can
/// reach its master, whatever the other is doing. `CredentialNeeded` outranks
/// `Connecting` because a retry loop with no usable credential is going to
/// keep failing, and saying "connecting" about it would be an encouraging
/// untruth of exactly the kind this file keeps having to remove.
pub fn slave_relay_status() -> SlaveRelayStatus {
    let states = [slave_link_state(0), slave_link_state(1)];
    if states
        .iter()
        .any(|s| matches!(s, SlaveLinkState::Registered | SlaveLinkState::Bridging))
    {
        return SlaveRelayStatus::Connected;
    }
    if master_credential_needed().is_some() {
        return SlaveRelayStatus::CredentialNeeded;
    }
    if states.iter().any(|s| matches!(s, SlaveLinkState::Connecting)) {
        return SlaveRelayStatus::Connecting;
    }
    SlaveRelayStatus::Idle
}

/// Read a slave port's current link state.
/// True while this gateway is the one announcing its CP/M emulator to the
/// master as the dialable `CPM` endpoint.  Set by the announcer task; read for
/// the slave-link summary so the log shows the emulator alongside the ports.
static CPM_ANNOUNCED: AtomicBool = AtomicBool::new(false);

/// Record whether the CP/M endpoint is currently announced to the master.
pub fn set_cpm_announced(on: bool) {
    CPM_ANNOUNCED.store(on, Ordering::SeqCst);
}

/// Log one consolidated picture of the slave link.
///
/// A slave's log used to be a scatter of per-port lines, which answered "did
/// port B register?" but never "am I connected, and what does the master
/// actually see?".  This prints the whole state in one block whenever it
/// changes, naming each port, the mode it is in, and what the link is doing —
/// so an operator reading the log terminal can tell at a glance what the master
/// can reach.  Ports that are disabled or not in slave-relay mode are listed as
/// such rather than omitted, because "why is port B missing?" is exactly the
/// question a summary should answer.
pub fn log_slave_link_summary(host: &str, port: u16) {
    let cfg = crate::config::get_config();
    let mut lines: Vec<String> = Vec::new();
    for (idx, (label, pc)) in [("A", &cfg.serial_a), ("B", &cfg.serial_b)]
        .into_iter()
        .enumerate()
    {
        let state = slave_link_state(idx);
        // A Kermit-mode port is never "picked" by a user: the master's Kermit
        // server is simply on the wire whenever the link is up, and serves
        // nothing when it isn't.  Saying "a master user is attached" there
        // would describe the wrong feature.
        let kermit_mode = pc.mode == "kermit";
        let detail = match state {
            SlaveLinkState::Registered => "registered — awaiting a pick from the master",
            SlaveLinkState::Bridging if kermit_mode => {
                "the master's Kermit server is on this wire — files live on the master"
            }
            SlaveLinkState::Bridging => "bridging — a master user is attached",
            SlaveLinkState::Connecting => "connecting to the master",
            SlaveLinkState::Down if !pc.enabled => "port disabled",
            SlaveLinkState::Down if kermit_mode => {
                "down — serving nothing until the master is reachable"
            }
            SlaveLinkState::Down => "down — not connected to the master",
        };
        lines.push(format!("    Port {label}  mode={:<7} {detail}", pc.mode));
    }
    if CPM_ANNOUNCED.load(Ordering::SeqCst) {
        lines.push("    CPM       emulator  announced — dialable as CPM@this-host".to_string());
    } else if cfg.cpm_emu_enabled {
        // Say the *actual* reason.  This line used to read "needs
        // allow_peer_dial", which was true when the announcer was gated on that
        // flag and is now simply misleading: it sent an operator looking for a
        // setting to change when the answer is usually that nothing is running
        // to announce yet.  The endpoint exists only while a CP/M session with
        // its virtual modem is live — that is when something can answer a ring.
        let why = if matches!(
            crate::cpm::uart::resolve_access(&cfg.cpm_emu_uart),
            crate::cpm::uart::ModemAccess::Off
        ) {
            "virtual modem off — set cpm_emu_uart to a port/AUX/HBIOS profile"
        } else {
            // The endpoint is registered for the whole server lifetime now, so
            // reaching here means the registration itself is not up (master
            // unreachable, or it is still connecting) — not that nobody has the
            // emulator open.  An open session is only needed to *answer*.
            "registration not up — see the CP/M emulator lines above"
        };
        lines.push(format!("    CPM       emulator  not announced ({why})"));
    }
    glog!("Slave link to master {}:{} —", host, port);
    for l in lines {
        glog!("{}", l);
    }
}

pub fn slave_link_state(port_index: usize) -> SlaveLinkState {
    SLAVE_LINK
        .get(port_index)
        .map(|c| SlaveLinkState::from_u8(c.load(Ordering::Relaxed)))
        .unwrap_or(SlaveLinkState::Down)
}

#[cfg(test)]
mod tests;
