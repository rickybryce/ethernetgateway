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

    // Bound the onward connect like the local modem's `dial_tcp` does: a
    // relayed device sits at CONNECT (the slave reports success as soon as
    // the relay hello arrives) while an SSH session-cap slot stays held, so
    // an unbounded `connect()` to a down/firewalled host would pin both for
    // the full OS SYN-retry window (~2 min on Linux).  Cap it at
    // RELAY_PEER_ANSWER_WAIT and report NO CARRIER (drop the relay) on timeout.
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

    // `copy_bidirectional` pipes both directions and handles half-close
    // correctly: when one side hits EOF it shuts down the other's write
    // and keeps draining until both ends close — so the final burst from
    // a BBS (or device) that closes its send side isn't dropped (the
    // earlier `select!` cancelled the losing direction mid-copy and could
    // truncate the last bytes of a relayed transfer).
    let _ = tokio::io::copy_bidirectional(&mut relay, &mut tcp).await;
    let _ = relay.shutdown().await;
}

/// How long to wait for a modem-mode peer-dial target to answer when the
/// caller has no local `S7` to bound it — the master bridging a relayed peer
/// call, and the slave modem-port announcer ringing its own port.  Matches
/// the telnet Serial Gateway picker's peer-call wait.
pub const RELAY_PEER_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

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
                let _ = tokio::io::copy_bidirectional(&mut relay, &mut b).await;
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
                let _ = tokio::io::copy_bidirectional(&mut relay, &mut b).await;
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
        match claim_remote_peer(ip, &label).await {
            Some(mut remote) => {
                glog!("Relay: peer-dial crossbar to {}@{}", label, ip);
                let _ = tokio::io::copy_bidirectional(&mut relay, &mut remote).await;
            }
            None => glog!("Relay: peer-dial target {}@{} not registered", label, ip),
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
        _ => return None,
    }
    Some(ParsedRelay { port_label, dial, peer })
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

/// Relay wire-protocol version.  Bump on any incompatible change to the
/// master↔slave relay framing so a version-skewed pair fails cleanly with a
/// clear message instead of desyncing (§9).
pub const RELAY_PROTOCOL_VERSION: u8 = 1;

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
pub const RELAY_HELLO: [u8; 4] = [b'E', b'G', b'R', RELAY_PROTOCOL_VERSION];

/// How long the slave waits for the master's [`RELAY_HELLO`] after the exec
/// before concluding the master refused the channel.  Short — a real master
/// writes the hello immediately on accept; only a refusing/incompatible
/// master leaves the channel silent.
const RELAY_HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    connect_relay_exec(host, port, username, password, &target.exec_command(port_label)).await
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
) -> Result<MasterRelay, RelayConnectError> {
    connect_relay_exec(
        host,
        port,
        username,
        password,
        &format!("serial-register {}", port_label),
    )
    .await
}

/// Shared connect+auth+channel+exec, bounded by `RELAY_CONNECT_TIMEOUT`
/// so a wedged master can't freeze the serial thread.
async fn connect_relay_exec(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    exec_command: &str,
) -> Result<MasterRelay, RelayConnectError> {
    match tokio::time::timeout(
        RELAY_CONNECT_TIMEOUT,
        connect_master_relay_inner(host, port, username, password, exec_command),
    )
    .await
    {
        Ok(result) => result,
        // A handshake/auth stall is a transport problem, not a credential
        // one — classify as Network so the slave retries briskly.
        Err(_) => Err(RelayConnectError::Network(format!(
            "timed out after {}s connecting to master {}:{}",
            RELAY_CONNECT_TIMEOUT.as_secs(),
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
            crate::telnet::HostKeyStatus::Changed => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "host key changed", "")
                    .await;
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

    match session.authenticate_password(username, password).await {
        Ok(russh::client::AuthResult::Success) => {}
        Ok(_) => {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                .await;
            return Err(RelayConnectError::Auth(
                "authentication rejected by master".to_string(),
            ));
        }
        // A transport error mid-auth is network, not a credential rejection.
        Err(e) => return Err(RelayConnectError::Network(format!("auth error: {}", e))),
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

    let mut stream = channel.into_stream();
    // §9 handshake: read the master's relay hello before handing the
    // channel to the caller.  This is what distinguishes an ACCEPTED relay
    // from a refused-but-open channel (russh `exec()` returns Ok even on
    // the master's `channel_failure`) and catches a protocol-version skew.
    read_relay_hello(&mut stream).await?;
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
async fn read_relay_hello<R>(stream: &mut R) -> Result<(), RelayConnectError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut hello = [0u8; RELAY_HELLO.len()];
    match tokio::time::timeout(RELAY_HELLO_TIMEOUT, stream.read_exact(&mut hello)).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => {
            return Err(RelayConnectError::Refused(
                "master accepted the channel but sent no relay hello — it is \
                 refusing relays (not a master, master_accept_relays off, at \
                 capacity) or is an incompatible build"
                    .to_string(),
            ));
        }
        Err(_) => {
            return Err(RelayConnectError::Refused(format!(
                "timed out after {}s waiting for the master's relay hello — \
                 relays disabled / standalone / incompatible master?",
                RELAY_HELLO_TIMEOUT.as_secs()
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

/// A registered remote console port: the master's end of the idle SSH
/// registration channel, paired with the generation stamped when it was
/// registered (see [`REMOTE_PORTS`] for why the generation matters).
type RegisteredPort = (tokio::io::DuplexStream, u64);

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
pub fn register_remote_port(slave_ip: IpAddr, label: String, stream: tokio::io::DuplexStream) -> u64 {
    let generation = REMOTE_PORT_GEN.fetch_add(1, Ordering::Relaxed);
    let mut g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(HashMap::new)
        .insert((slave_ip, label), (stream, generation));
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
        .map(|(stream, _gen)| stream)
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
        Some((_, stored)) if *stored == generation => map.remove(&key).map(|(stream, _)| stream),
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

/// Claim a registered remote port for a peer-dial and signal the slave to
/// start bridging (`RELAY_ACTIVATE_BYTE`), returning the master's channel
/// stream to pump.  `None` if no such port is registered or it went away
/// before the activate byte landed.  Shared by the peer-dial crossbar and
/// mirrors the Serial Gateway picker's claim+activate.
pub async fn claim_remote_peer(ip: IpAddr, label: &str) -> Option<tokio::io::DuplexStream> {
    use tokio::io::AsyncWriteExt;
    let mut stream = remove_remote_port(ip, label)?;
    if stream.write_all(&[RELAY_ACTIVATE_BYTE]).await.is_err() || stream.flush().await.is_err() {
        return None;
    }
    Some(stream)
}

/// List the currently-registered remote console ports, sorted stably so
/// the picker order doesn't jump around between redraws.
pub fn list_remote_ports() -> Vec<(IpAddr, String)> {
    let g = REMOTE_PORTS.lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<(IpAddr, String)> = g
        .as_ref()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
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

/// Read a slave port's current link state.
pub fn slave_link_state(port_index: usize) -> SlaveLinkState {
    SLAVE_LINK
        .get(port_index)
        .map(|c| SlaveLinkState::from_u8(c.load(Ordering::Relaxed)))
        .unwrap_or(SlaveLinkState::Down)
}

#[cfg(test)]
mod tests;
