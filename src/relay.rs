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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    let mut tcp = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(e) => {
            glog!("Relay: onward dial to {}:{} failed: {}", host, port, e);
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
}

impl RelayTarget {
    /// Build the `exec` command the slave sends on its relay channel.
    /// `port_label` is the slave's logical port ("A"/"B") so the master
    /// knows which device this is.  Grammar:
    ///   `serial-relay <port> menu`
    ///   `serial-relay <port> dial <host>:<port>`
    pub fn exec_command(&self, port_label: &str) -> String {
        match self {
            RelayTarget::Menu => format!("serial-relay {} menu", port_label),
            RelayTarget::Dial { host, port } => {
                format!("serial-relay {} dial {}:{}", port_label, host, port)
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
}

/// Parse a `serial-relay …` exec command.  Returns `None` for anything
/// that isn't a well-formed relay command (the master refuses it — this
/// is not a general command-exec shell).  Grammar:
///   `serial-relay <port> menu`
///   `serial-relay <port> dial <host>:<port>`
pub fn parse_relay_command(command: &str) -> Option<ParsedRelay> {
    let mut toks = command.split_whitespace();
    if toks.next()? != "serial-relay" {
        return None;
    }
    let port_label = toks.next().unwrap_or("?").to_string();
    let dial = match toks.next().unwrap_or("menu") {
        "menu" => None,
        "dial" => {
            let (h, p) = toks.next()?.rsplit_once(':')?;
            let port: u16 = p.parse().ok()?;
            if port == 0 {
                return None;
            }
            Some((h.to_string(), port))
        }
        _ => return None,
    };
    Some(ParsedRelay { port_label, dial })
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
) -> Result<MasterRelay, String> {
    connect_relay_exec(host, port, username, password, &target.exec_command(port_label)).await
}

/// Connect to the master and register a **console-mode** port as
/// available (§9 #12).  The master holds the channel idle in its
/// remote-port registry until a master user picks it; the returned
/// [`MasterRelay`] is then driven by the slave's console-registration
/// loop (read one activate byte, then bridge the UART).
pub async fn connect_master_register(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    port_label: &str,
) -> Result<MasterRelay, String> {
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
) -> Result<MasterRelay, String> {
    match tokio::time::timeout(
        RELAY_CONNECT_TIMEOUT,
        connect_master_relay_inner(host, port, username, password, exec_command),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "timed out after {}s connecting to master {}:{}",
            RELAY_CONNECT_TIMEOUT.as_secs(),
            host,
            port
        )),
    }
}

async fn connect_master_relay_inner(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    exec_command: &str,
) -> Result<MasterRelay, String> {
    let config = Arc::new(russh::client::Config::default());
    let server_key = std::sync::Arc::new(std::sync::Mutex::new(None));
    let handler = SlaveRelayHandler {
        server_key: server_key.clone(),
    };
    let mut session = russh::client::connect(config, (host, port), handler)
        .await
        .map_err(|e| format!("connect failed: {}", e))?;

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
                return Err(format!(
                    "master {}:{} host key CHANGED — refusing (possible MITM); \
                     remove the stale gateway_hosts entry if the master was reinstalled",
                    host, port
                ));
            }
        },
        None => {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "no host key", "")
                .await;
            return Err("master presented no host key".to_string());
        }
    }

    match session.authenticate_password(username, password).await {
        Ok(russh::client::AuthResult::Success) => {}
        Ok(_) => {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                .await;
            return Err("authentication rejected by master".to_string());
        }
        Err(e) => return Err(format!("auth error: {}", e)),
    }

    let channel = session
        .channel_open_session()
        .await
        .map_err(|e| format!("channel open failed: {}", e))?;
    channel
        .exec(true, exec_command.as_bytes())
        .await
        .map_err(|e| format!("relay exec failed: {}", e))?;

    let stream = channel.into_stream();
    Ok(MasterRelay {
        _session: session,
        stream,
    })
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

/// Console-mode slave ports currently registered with this master, keyed
/// by `(slave IP, port label)`.  Each value pairs the master's end of the
/// idle SSH registration channel with a monotonic **generation** stamped
/// at registration time.  The Serial Gateway picker lists the keys and
/// `claim`s (removes) an entry to bridge a master user to the slave's
/// console device.  Populated by `ssh.rs` `exec_request`
/// (`serial-register`), drained by the picker or by channel teardown.
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

#[cfg(test)]
mod tests;
