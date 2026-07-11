//! SSH server interface for the Ethernet Gateway.
//!
//! Provides encrypted access to the same menus and features available over
//! telnet.  Uses russh's server implementation with an Ed25519 host key
//! that is generated on first run and persisted to `ethernet_ssh_host_key`.
//! Authentication is password-based with credentials configured independently
//! of the telnet credentials in `egateway.conf`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use russh::server::Server as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config;
use crate::logger::glog;
use crate::telnet;

const SSH_HOST_KEY_FILE: &str = "ethernet_ssh_host_key";
/// Client keypair used by the outgoing SSH gateway to authenticate
/// against remote servers via public-key authentication.  Generated on
/// first use and persisted for the lifetime of the deployment so that
/// the operator can add the same public key to remote `authorized_keys`
/// files once and reuse it across sessions.
pub(crate) const GATEWAY_CLIENT_KEY_FILE: &str = "ethernet_gateway_ssh_key";

// ─── Public API ────────────────────────────────────────────

/// Start the SSH server if enabled in config.
pub fn start_ssh_server(
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    session_writers: telnet::SessionWriters,
    lockouts: telnet::LockoutMap,
) {
    let cfg = config::get_config();
    if !cfg.ssh_enabled {
        return;
    }

    let port = cfg.ssh_port;

    tokio::spawn(async move {
        let key = match load_or_generate_host_key() {
            Ok(k) => k,
            Err(e) => {
                glog!("SSH server: failed to load/generate host key: {}", e);
                return;
            }
        };

        let config = russh::server::Config {
            keys: vec![key],
            auth_rejection_time: std::time::Duration::from_secs(1),
            // Keepalive (§9 #15): detect and reap dead clients — most
            // importantly a slave whose relay/registration link died
            // silently, so its master-side session slot and remote-port
            // registry entry are released promptly (SshHandler::drop) instead
            // of lingering until a write happens to fail.  Benefits ordinary
            // SSH sessions too (frees slots from half-open connections).  No
            // `inactivity_timeout` — an idle console registration is alive.
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            keepalive_max: 3,
            ..Default::default()
        };
        let config = Arc::new(config);

        let mut server = SshServer {
            shutdown: shutdown.clone(),
            restart: restart.clone(),
            session_count: Arc::new(AtomicUsize::new(0)),
            max_sessions: cfg.max_sessions,
            session_writers: session_writers.clone(),
            lockouts: lockouts.clone(),
        };

        let addr = format!("0.0.0.0:{}", port);
        glog!("SSH server listening on port {}", port);

        tokio::select! {
            result = server.run_on_address(config, &*addr) => {
                if let Err(e) = result {
                    glog!("SSH server error: {}", e);
                }
            }
            _ = shutdown_notify.notified() => {
                glog!("SSH server: shutting down");
            }
        }
    });
}

// ─── Host key management ───────────────────────────────────

/// Write a private-key PEM atomically with owner-only permissions
/// from the moment of creation.  On Unix we open the tmp file with
/// `O_CREAT|O_EXCL` and mode `0o600` in a single syscall — the file
/// is never visible at default-umask permissions, even briefly, so
/// a concurrent reader on a multi-user host cannot race the
/// post-write `chmod` window that `fs::write` + `set_permissions`
/// would leave.  On non-Unix targets we fall back to plain
/// `fs::write` since file modes don't apply.
///
/// The per-process atomic counter in the tmp filename prevents two
/// threads in the same process from clobbering each other's tmp
/// file (e.g. host key + client key both generated on first run);
/// the PID component prevents two instances in the same working
/// directory from doing the same.
fn atomic_write_private_key(path: &str, contents: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let tmp = format!("{}.{}.{}.tmp", path, std::process::id(), seq);

    #[cfg(unix)]
    let write_result = {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(contents))
    };
    #[cfg(not(unix))]
    let write_result = std::fs::write(&tmp, contents);

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// On Unix, warn (but do not refuse) if a *pre-existing* private-key file is
/// group- or world-accessible.  New keys are written `0o600` by
/// `atomic_write_private_key`, but a key restored from a backup or created by
/// an older build could be more permissive.  `sshd` refuses such keys
/// outright; we only warn, because the gateway's threat model is a trusted
/// LAN/operator and refusing would strand an existing deployment that still
/// works.  No-op off Unix (file modes don't apply).
#[cfg(unix)]
fn warn_if_key_perms_insecure(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            glog!(
                "SSH: warning: private key {} is group/world accessible (mode {:o}); recommend chmod 600",
                path,
                mode & 0o777
            );
        }
    }
}
#[cfg(not(unix))]
fn warn_if_key_perms_insecure(_path: &str) {}

fn load_or_generate_host_key() -> Result<russh::keys::PrivateKey, String> {
    use russh::keys::ssh_key::LineEnding;

    // Try to load existing key.  If the file exists but won't parse, REFUSE
    // rather than overwriting it with a fresh key: silently minting a new
    // identity would change the server's host key and trip every client's
    // "REMOTE HOST IDENTIFICATION HAS CHANGED" MITM warning (and could clobber
    // a key that was merely truncated by a full disk and is otherwise
    // recoverable).  sshd refuses to start on a bad host key for the same
    // reason.  The caller logs this and simply doesn't start the SSH server,
    // leaving the file untouched for the operator to fix or remove.  Only a
    // *missing* file falls through to generation below.
    if std::path::Path::new(SSH_HOST_KEY_FILE).exists() {
        warn_if_key_perms_insecure(SSH_HOST_KEY_FILE);
        match russh::keys::load_secret_key(SSH_HOST_KEY_FILE, None) {
            Ok(key) => {
                glog!("SSH server: loaded host key from {}", SSH_HOST_KEY_FILE);
                return Ok(key);
            }
            Err(e) => {
                return Err(format!(
                    "host key {} exists but could not be read: {}. \
                     Refusing to overwrite it with a new key (that would change \
                     the server identity). Remove or restore the file, then restart.",
                    SSH_HOST_KEY_FILE, e
                ));
            }
        }
    }

    // Generate new Ed25519 key
    let key = russh::keys::PrivateKey::random(
        &mut rand::rng(),
        russh::keys::Algorithm::Ed25519,
    )
    .map_err(|e| format!("key generation failed: {}", e))?;

    // Save to file in OpenSSH format
    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("key encoding failed: {}", e))?;
    if let Err(e) = atomic_write_private_key(SSH_HOST_KEY_FILE, pem.as_bytes()) {
        glog!(
            "SSH server: warning: could not save host key to {}: {}",
            SSH_HOST_KEY_FILE, e
        );
    } else {
        glog!(
            "SSH server: generated new host key, saved to {}",
            SSH_HOST_KEY_FILE
        );
    }

    Ok(key)
}

/// Load or (on first use) generate the gateway's outgoing-SSH client
/// keypair used for public-key authentication against remote servers.
///
/// Mirrors `load_or_generate_host_key`: Ed25519, OpenSSH-format PEM at
/// `GATEWAY_CLIENT_KEY_FILE`, chmod 0o600 on Unix.  The file mode is
/// the only at-rest protection; the private key itself has no
/// passphrase because the gateway process needs to use it without user
/// interaction.
pub(crate) fn load_or_generate_client_key() -> Result<russh::keys::PrivateKey, String> {
    use russh::keys::ssh_key::LineEnding;

    if std::path::Path::new(GATEWAY_CLIENT_KEY_FILE).exists() {
        warn_if_key_perms_insecure(GATEWAY_CLIENT_KEY_FILE);
        match russh::keys::load_secret_key(GATEWAY_CLIENT_KEY_FILE, None) {
            Ok(key) => {
                return Ok(key);
            }
            Err(e) => {
                glog!(
                    "SSH gateway: could not read {}: {} (generating new key)",
                    GATEWAY_CLIENT_KEY_FILE, e
                );
            }
        }
    }

    let key = russh::keys::PrivateKey::random(
        &mut rand::rng(),
        russh::keys::Algorithm::Ed25519,
    )
    .map_err(|e| format!("client key generation failed: {}", e))?;

    let pem = key
        .to_openssh(LineEnding::LF)
        .map_err(|e| format!("client key encoding failed: {}", e))?;
    if let Err(e) = atomic_write_private_key(GATEWAY_CLIENT_KEY_FILE, pem.as_bytes()) {
        glog!(
            "SSH gateway: warning: could not save client key to {}: {}",
            GATEWAY_CLIENT_KEY_FILE, e,
        );
    } else {
        glog!(
            "SSH gateway: generated new client key, saved to {}",
            GATEWAY_CLIENT_KEY_FILE,
        );
    }

    Ok(key)
}

/// Return the gateway client's public key in OpenSSH one-line format
/// (`<algorithm> <base64>`), suitable for pasting into a remote's
/// `~/.ssh/authorized_keys`.  Generates the keypair on first call.
pub(crate) fn client_public_key_openssh() -> Result<String, String> {
    let key = load_or_generate_client_key()?;
    let public = key.public_key();
    let line = public.to_string();
    // `PublicKey::to_string` produces `<algo> <b64> [comment]`.  We
    // return just `<algo> <b64>` so operators don't paste a stray
    // comment they didn't provide.
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() >= 2 {
        Ok(format!("{} {}", parts[0], parts[1]))
    } else {
        Ok(line)
    }
}

// ─── Server (connection factory) ───────────────────────────

struct SshServer {
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    session_count: Arc<AtomicUsize>,
    max_sessions: usize,
    session_writers: telnet::SessionWriters,
    /// Brute-force lockout map shared with the telnet server.
    lockouts: telnet::LockoutMap,
}

impl russh::server::Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> SshHandler {
        let cfg = config::get_config();
        // Do NOT consume a session slot here.  new_client fires for every
        // inbound TCP connection, before any authentication, so counting at
        // connect time let an unauthenticated peer that opens many transport
        // handshakes and stalls exhaust `max_sessions` and lock out real
        // users.  The slot is claimed in auth_password on a successful login
        // (atomic fetch_add + rollback, the same pattern the telnet accept
        // loop uses).
        if let Some(addr) = peer_addr {
            glog!("SSH: connection from {}", addr);
        }
        SshHandler {
            shutdown: self.shutdown.clone(),
            restart: self.restart.clone(),
            session_count: self.session_count.clone(),
            max_sessions: self.max_sessions,
            // SSH, telnet, and the web UI all authenticate against the
            // same unified `username` / `password` pair.  The earlier
            // ssh_username / ssh_password config fields were dropped
            // — a single credential pair is simpler to manage and
            // matches operator expectations.  Snapshot at connect
            // time so a mid-session config save can't invalidate an
            // already-authenticated connection.
            username: cfg.username.clone(),
            password: cfg.password.clone(),
            peer_addr: peer_addr.map(|a| a.ip()),
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: self.session_writers.clone(),
            lockouts: self.lockouts.clone(),
            counted: false,
        }
    }
}

// ─── Handler (per-connection) ──────────────────────────────

struct SshHandler {
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    session_count: Arc<AtomicUsize>,
    max_sessions: usize,
    /// Snapshot of `cfg.username` taken at connect time (telnet, SSH,
    /// and the web UI share one credential pair).
    username: String,
    /// Snapshot of `cfg.password` taken at connect time.
    password: String,
    peer_addr: Option<std::net::IpAddr>,
    /// Write half of the duplex bridge to the TelnetSession.
    /// Set once a shell is opened; prevents duplicate shell requests.
    duplex_writer:
        Option<Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>>,
    /// Per-channel write halves for master/slave **relay** channels
    /// (`exec "serial-relay <port>"`).  Keyed by channel so one SSH
    /// connection from a slave can carry several relay channels (Ports A
    /// and B) concurrently — `data()`/`channel_eof()` route by channel.
    /// Separate from `duplex_writer` (the single interactive shell).
    relay_writers: std::collections::HashMap<
        russh::ChannelId,
        Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
    >,
    /// Console-mode **registration** channels (`exec "serial-register
    /// <port>"`): channel -> `(port label, registration generation)`.  Lets
    /// channel teardown remove the matching entry from the global
    /// remote-port registry — but only if it is still *this* registration
    /// (the generation guards a re-register race; see
    /// `relay::remove_remote_port_gen`) — and release its session-cap slot
    /// (§9 #12).
    registered_ports: std::collections::HashMap<russh::ChannelId, (String, u64)>,
    session_writers: telnet::SessionWriters,
    /// Shared brute-force lockout map (telnet + SSH).
    lockouts: telnet::LockoutMap,
    /// Whether this connection claimed a session slot (set once auth
    /// succeeds).  Gates the Drop decrement so an unauthenticated
    /// connection that never counted can't underflow the shared counter.
    counted: bool,
}

impl Drop for SshHandler {
    fn drop(&mut self) {
        // Release a slot only if we actually claimed one (auth succeeded);
        // unauthenticated connections never incremented the counter.
        if self.counted {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
        }
        // Backstop for an abrupt connection drop (e.g. TCP RST) that never
        // delivered channel_eof/channel_close: release any console
        // registration channels still held — each consumes a per-channel
        // session slot and a global remote-port registry entry that
        // teardown_channel would otherwise have drained on a graceful
        // close.  Generation-matched removal can't evict a newer
        // re-registration from another connection.  (register_console_port
        // requires a peer addr, so any entry here has one.)
        for (_channel, (label, generation)) in self.registered_ports.drain() {
            if let Some(addr) = self.peer_addr {
                let _ = crate::relay::remove_remote_port_gen(addr, &label, generation);
            }
            self.session_count.fetch_sub(1, Ordering::SeqCst);
        }
        if let Some(addr) = self.peer_addr {
            glog!("SSH: {} disconnected", addr);
        }
    }
}

impl SshHandler {
    /// Register a console-mode remote port (`serial-register <port>`,
    /// §9 #12).  Gated like the relay path (master + accept_relays + a
    /// known peer IP) and counted against the session cap (a registered
    /// idle port holds a slot until it disconnects).  The channel is held
    /// idle in the global registry; the Serial Gateway picker claims it.
    async fn register_console_port(
        &mut self,
        channel: russh::ChannelId,
        label: &str,
        session: &mut russh::server::Session,
    ) -> Result<(), russh::Error> {
        let cfg = config::get_config();
        if cfg.gateway_role != "master" || !cfg.master_accept_relays {
            glog!(
                "SSH: refused serial-register from {:?} (role={}, accept_relays={})",
                self.peer_addr,
                cfg.gateway_role,
                cfg.master_accept_relays
            );
            session.channel_failure(channel)?;
            return Ok(());
        }
        let Some(slave_ip) = self.peer_addr else {
            glog!("SSH: serial-register with no peer address; refusing");
            session.channel_failure(channel)?;
            return Ok(());
        };
        if label.is_empty() {
            session.channel_failure(channel)?;
            return Ok(());
        }

        // A registered idle port consumes a session slot for its lifetime.
        // Like the relay path (see exec_request), this is on TOP of the auth
        // slot — an accepted over-count (M-11, fails safe); see that note.
        let prev = self.session_count.fetch_add(1, Ordering::SeqCst);
        if prev >= self.max_sessions {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            glog!(
                "SSH: serial-register from {} rejected (server at capacity {})",
                slave_ip,
                self.max_sessions
            );
            session.channel_failure(channel)?;
            return Ok(());
        }

        // Acknowledge the channel.  If that errors (transport already
        // dying), release the slot we just claimed before propagating —
        // teardown_channel never runs for a channel we failed to record.
        if let Err(e) = session.channel_success(channel) {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
        // §9 handshake: write the relay hello as the first bytes on the
        // accepted channel so the slave can tell this ACCEPTED registration
        // from a refused-but-open channel and check the protocol version.
        if let Err(e) = session.data(
            channel,
            bytes::Bytes::copy_from_slice(&crate::relay::RELAY_HELLO),
        ) {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
        glog!(
            "SSH: registered remote console port {} from {}",
            label,
            slave_ip
        );

        // gateway_stream (kept whole, stored in the registry) IS the
        // master's end of the channel: writing to it reaches the slave,
        // reading from it yields the slave's bytes.
        let (gateway_stream, handler_stream) = tokio::io::duplex(65536);
        let (handler_read, handler_write) = tokio::io::split(handler_stream);
        self.relay_writers.insert(
            channel,
            Arc::new(tokio::sync::Mutex::new(handler_write)),
        );
        spawn_channel_reader(session.handle(), channel, handler_read);

        let label = label.to_string();
        let generation =
            crate::relay::register_remote_port(slave_ip, label.clone(), gateway_stream);
        self.registered_ports.insert(channel, (label, generation));
        Ok(())
    }

    /// Shut down the bridge for a closed channel.  A relay channel closes
    /// only that channel's session; any non-relay channel falls back to the
    /// single interactive shell bridge.  Shared by channel_eof and
    /// channel_close (a peer may send either, or both — idempotent).
    async fn teardown_channel(&mut self, channel: russh::ChannelId) {
        // A registration channel: drop its registry entry (only if it is
        // still *this* registration — a slave that re-registered the same
        // port on a newer channel must not be evicted by this old channel's
        // teardown) and release its session-cap slot.
        if let Some((label, generation)) = self.registered_ports.remove(&channel) {
            if let Some(addr) = self.peer_addr {
                let _ = crate::relay::remove_remote_port_gen(addr, &label, generation);
            }
            self.session_count.fetch_sub(1, Ordering::SeqCst);
        }
        if let Some(writer) = self.relay_writers.remove(&channel) {
            let mut w = writer.lock().await;
            let _ = w.shutdown().await;
        } else if let Some(writer) = self.duplex_writer.take() {
            let mut w = writer.lock().await;
            let _ = w.shutdown().await;
        }
    }
}

/// Pump a duplex bridge's gateway-output half back to the SSH client
/// channel, closing the channel when the session ends.  Shared by
/// `shell_request` (interactive) and `exec_request` (relay) so the two
/// can't drift (review finding: this loop was duplicated).
fn spawn_channel_reader(
    handle: russh::server::Handle,
    channel: russh::ChannelId,
    mut reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
) {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if handle
                        .data(channel, bytes::Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = handle.close(channel).await;
    });
}

impl russh::server::Handler for SshHandler {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<russh::server::Auth, Self::Error> {
        // SSH intentionally does NOT honor `security_enabled` or call
        // `reject_insecure_ip` the way telnet/web do.  Those guards exist
        // only to protect telnet/web's *optional-auth* mode: when
        // `security_enabled = false` they accept connections with no
        // credentials at all, so the insecure-IP check stops an
        // accidentally-public gateway from being wide open.  SSH has no
        // unauthenticated mode — `auth_password` is the only auth method we
        // implement (russh defaults the rest to reject), the sole
        // `Auth::Accept` below requires correct constant-time-compared
        // credentials, and the transport is encrypted.  So the IP guard
        // would be redundant here, not missing.
        //
        // Capacity is enforced below — atomically, and only once auth
        // succeeds (see the fetch_add + rollback in the accept branch) — so
        // an unauthenticated/stalled peer can't occupy a slot.
        //
        // Reject immediately if this IP is locked out from too many
        // failures (map is shared with the telnet server so bouncing
        // protocols doesn't help an attacker).
        if let Some(ip) = self.peer_addr
            && telnet::is_locked_out(&self.lockouts, ip)
        {
            glog!("SSH: auth from {} rejected (locked out)", ip);
            return Ok(russh::server::Auth::reject());
        }
        // Constant-time comparison to prevent timing attacks.
        let user_ok =
            telnet::constant_time_eq(user.as_bytes(), self.username.as_bytes());
        let pass_ok =
            telnet::constant_time_eq(password.as_bytes(), self.password.as_bytes());
        if user_ok && pass_ok {
            // Valid credentials reset any failure lockout for this IP.
            if let Some(ip) = self.peer_addr {
                telnet::clear_lockout(&self.lockouts, ip);
            }
            // If this connection already claimed a slot on a prior
            // successful auth, accept again without re-counting — otherwise
            // a second auth_password call would fetch_add a slot that Drop
            // (which subtracts once) could never release.
            if self.counted {
                return Ok(russh::server::Auth::Accept);
            }
            // Now claim a session slot.  Atomic fetch_add + rollback (the
            // same pattern as the telnet accept loop) enforces the cap
            // exactly here, where a connection becomes a real authenticated
            // session — accepting sessions 0..max_sessions-1 and rejecting
            // the rest.
            let prev = self.session_count.fetch_add(1, Ordering::SeqCst);
            if prev >= self.max_sessions {
                self.session_count.fetch_sub(1, Ordering::SeqCst);
                if let Some(ip) = self.peer_addr {
                    glog!(
                        "SSH: {} authenticated but server at capacity ({}); rejecting",
                        ip,
                        self.max_sessions,
                    );
                }
                return Ok(russh::server::Auth::reject());
            }
            self.counted = true;
            Ok(russh::server::Auth::Accept)
        } else {
            if let Some(ip) = self.peer_addr {
                let count = telnet::record_auth_failure(&self.lockouts, ip);
                if count >= telnet::AUTH_MAX_ATTEMPTS {
                    glog!(
                        "SSH: {} exceeded {} failed attempts; locked out",
                        ip,
                        telnet::AUTH_MAX_ATTEMPTS,
                    );
                }
            }
            Ok(russh::server::Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        _session: &mut russh::server::Session,
    ) -> Result<bool, Self::Error> {
        let _ = channel;
        Ok(true)
    }

    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // Only allow one shell per connection.
        if self.duplex_writer.is_some() {
            session.channel_failure(channel)?;
            return Ok(());
        }

        session.channel_success(channel)?;

        // Create a duplex bridge between the SSH channel and a TelnetSession.
        let (gateway_stream, handler_stream) = tokio::io::duplex(4096);
        let (gateway_read, gateway_write) = tokio::io::split(gateway_stream);
        let (handler_read, handler_write) = tokio::io::split(handler_stream);

        // Store the handler-side writer so data() can forward SSH input.
        self.duplex_writer =
            Some(Arc::new(tokio::sync::Mutex::new(handler_write)));

        // Wrap the gateway write half as a SharedWriter for TelnetSession.
        let writer_box: Box<dyn tokio::io::AsyncWrite + Unpin + Send> =
            Box::new(gateway_write);
        let writer_arc: telnet::SharedWriter =
            Arc::new(tokio::sync::Mutex::new(writer_box));

        let shutdown = self.shutdown.clone();
        let restart = self.restart.clone();
        let peer_addr = self.peer_addr;
        let session_writers = self.session_writers.clone();

        // Add this SSH session's writer to the shared list so the
        // shutdown broadcast reaches SSH clients too.
        session_writers.lock().await.push(writer_arc.clone());

        // Spawn the TelnetSession on the gateway side of the duplex.
        let writer_for_task = writer_arc.clone();
        let lockouts_for_task = self.lockouts.clone();
        tokio::spawn(async move {
            let mut sess = telnet::TelnetSession::new_ssh(
                Box::new(gateway_read),
                writer_for_task.clone(),
                shutdown,
                restart,
                peer_addr,
                lockouts_for_task,
            );
            if let Err(e) = sess.run().await {
                glog!("SSH: session error: {}", e);
            }
            let mut w = writer_for_task.lock().await;
            let _ = w.shutdown().await;
            drop(w);
            session_writers.lock().await.retain(|w| !Arc::ptr_eq(w, &writer_for_task));
        });

        // Reader task: forward TelnetSession output back to the SSH client
        // (shared with exec_request's relay path).
        spawn_channel_reader(session.handle(), channel, handler_read);

        Ok(())
    }

    /// Master/slave relay intake.  A slave opens a channel and runs
    /// `exec "serial-relay <port>"` instead of a shell; we route that
    /// channel into a master-side relay session (the full menu / transfer
    /// / dial-out machinery) rather than an interactive shell.
    ///
    /// Auth already happened in `auth_password` (the slave logs in with
    /// the master's unified credentials — review finding 2), so the only
    /// extra gates here are: the command must be `serial-relay`, this
    /// gateway must be a `master`, and `master_accept_relays` must be on.
    /// Any other `exec` is refused — this is not a general command shell.
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data);
        let command = command.trim();

        // Console-mode registration (§9 #12): `serial-register <port>` —
        // the slave offers a console port; we hold the channel idle in the
        // remote-port registry until a master user picks it in the Serial
        // Gateway picker.
        if let Some(label) = command.strip_prefix("serial-register ") {
            return self
                .register_console_port(channel, label.trim(), session)
                .await;
        }

        // Grammar (§3 Model B): `serial-relay <port> menu`
        //                    or `serial-relay <port> dial <host>:<port>`.
        let Some(parsed) = crate::relay::parse_relay_command(command) else {
            glog!(
                "SSH: refused exec {:?} from {:?} (only serial-relay is allowed)",
                command,
                self.peer_addr
            );
            session.channel_failure(channel)?;
            return Ok(());
        };
        let port_label = parsed.port_label;
        let dial_target = parsed.dial;
        let peer_target = parsed.peer;

        let cfg = config::get_config();
        if cfg.gateway_role != "master" || !cfg.master_accept_relays {
            glog!(
                "SSH: refused serial-relay from {:?} (role={}, accept_relays={})",
                self.peer_addr,
                cfg.gateway_role,
                cfg.master_accept_relays
            );
            session.channel_failure(channel)?;
            return Ok(());
        }

        // Count this relay channel against the session cap.  Each relay
        // channel spawns a full master session, so it must occupy a slot —
        // previously relay sessions bypassed max_sessions, letting one
        // authenticated slave spawn unbounded master sessions (review finding).
        //
        // NOTE (M-11, accepted): unlike the interactive shell — which rides
        // the slot claimed at auth (see auth_password) — each relay/register
        // channel claims its OWN slot on top of that auth slot.  So a
        // single-channel relay connection occupies two slots where an
        // interactive user occupies one.  This OVER-counts (fails safe: a
        // master hosts fewer relay sessions than max_sessions, never more),
        // and the per-channel count is what bounds a slave from opening
        // unbounded relay channels on one connection.  Left as-is on the
        // trusted-LAN master/slave threat model rather than converting the
        // auth slot per-channel, which risks a fails-open under-count in
        // this concurrency-critical path (three release sites).
        let prev = self.session_count.fetch_add(1, Ordering::SeqCst);
        if prev >= self.max_sessions {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            glog!(
                "SSH: relay from {:?} rejected (server at capacity {})",
                self.peer_addr,
                self.max_sessions
            );
            session.channel_failure(channel)?;
            return Ok(());
        }

        // Acknowledge the channel.  If that errors before we spawn the
        // relay task (the sole owner of the matching fetch_sub), release
        // the slot here so a transport error can't leak it.
        if let Err(e) = session.channel_success(channel) {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
        // §9 handshake: write the relay hello (magic + protocol version) as
        // the first bytes on the accepted channel, before any menu/onward-
        // dial data, so the slave distinguishes an accepted relay from a
        // refused-but-open channel and detects a version skew.
        if let Err(e) = session.data(
            channel,
            bytes::Bytes::copy_from_slice(&crate::relay::RELAY_HELLO),
        ) {
            self.session_count.fetch_sub(1, Ordering::SeqCst);
            return Err(e);
        }
        match (&dial_target, &peer_target) {
            (None, None) => glog!(
                "SSH: accepted serial relay (port {}, menu) from {:?}",
                port_label,
                self.peer_addr
            ),
            (Some((h, p)), _) => glog!(
                "SSH: accepted serial relay (port {}, dial {}:{}) from {:?}",
                port_label,
                h,
                p,
                self.peer_addr
            ),
            (None, Some(addr)) => glog!(
                "SSH: accepted serial relay (port {}, peer {}) from {:?}",
                port_label,
                addr,
                self.peer_addr
            ),
        }

        // Bridge the SSH channel to the gateway side via a duplex (same
        // pattern as shell_request).  The gateway-side consumer depends on
        // the target: the master's menu session, or a transparent onward
        // dial to an external host (Model B).
        let (gateway_stream, handler_stream) = tokio::io::duplex(65536);
        let (handler_read, handler_write) = tokio::io::split(handler_stream);

        // Route this channel's inbound data to the relay bridge.
        self.relay_writers.insert(
            channel,
            Arc::new(tokio::sync::Mutex::new(handler_write)),
        );

        let shutdown = self.shutdown.clone();
        let restart = self.restart.clone();
        let peer_addr = self.peer_addr;
        let session_writers = self.session_writers.clone();
        let lockouts = self.lockouts.clone();
        let session_count = self.session_count.clone();
        tokio::spawn(async move {
            match (dial_target, peer_target) {
                (Some((host, port)), _) => {
                    // Pass the WHOLE (unsplit) duplex so copy_bidirectional
                    // can half-close each direction without dropping the
                    // peer's final bytes.
                    crate::relay::run_master_relay_dial(gateway_stream, host, port).await;
                }
                (None, Some(addr)) => {
                    // Phase 2 peer-dial: bridge the channel to the master's
                    // own addressed port (ring modem / connect console).
                    crate::relay::run_master_relay_peer(gateway_stream, addr).await;
                }
                (None, None) => {
                    let (gateway_read, gateway_write) = tokio::io::split(gateway_stream);
                    crate::relay::run_master_relay_session(
                        Box::new(gateway_read),
                        Box::new(gateway_write),
                        peer_addr,
                        shutdown,
                        restart,
                        session_writers,
                        lockouts,
                    )
                    .await;
                }
            }
            // Release the slot when the relay session ends.
            session_count.fetch_sub(1, Ordering::SeqCst);
        });

        // Forward relay-session output back to the SSH channel (shared
        // with shell_request).
        spawn_channel_reader(session.handle(), channel, handler_read);

        Ok(())
    }

    async fn data(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // Route by channel: a relay channel's bytes go to its relay
        // session; otherwise to the single interactive shell bridge.
        //
        // NOTE (head-of-line blocking): `write_all().await` here holds the
        // per-connection handler callback while the duplex drains.  Today
        // a slave opens one channel per connection (connect-per-call), so
        // there is no contention.  If the deferred concurrent multi-channel
        // design lands (Ports A+B on one connection), a stalled channel
        // would block the others — the correct fix then is a per-channel
        // mpsc pump with the SSH window providing backpressure, NOT a
        // try_send (drops data) or unbounded buffer (grows without bound).
        // Left as-is deliberately rather than half-fixed.
        if let Some(writer) = self.relay_writers.get(&channel) {
            let mut w = writer.lock().await;
            let _ = w.write_all(data).await;
        } else if let Some(writer) = &self.duplex_writer {
            let mut w = writer.lock().await;
            let _ = w.write_all(data).await;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        self.teardown_channel(channel).await;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: russh::ChannelId,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // A peer may send CHANNEL_CLOSE without a preceding CHANNEL_EOF
        // (EOF is optional in the SSH protocol), which russh routes here,
        // not to channel_eof.  Without this handler a relay channel's
        // entry (and its held duplex write-half) would leak for the whole
        // connection lifetime on a long-lived slave that opens many
        // channels (review finding).  Idempotent with channel_eof.
        self.teardown_channel(channel).await;
        Ok(())
    }
}

// ─── Tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_key_file_constant() {
        assert_eq!(SSH_HOST_KEY_FILE, "ethernet_ssh_host_key");
    }

    // The key-permission warning is a warn-only helper; verify it runs without
    // panicking for a secure (0600) mode, an insecure (0644) mode, and a
    // nonexistent path. Unix-only (file modes don't apply elsewhere).
    #[cfg(unix)]
    #[test]
    fn test_warn_if_key_perms_insecure_no_panic() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("egw_ssh_perm_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let secure = dir.join("secure_key");
        std::fs::write(&secure, b"x").unwrap();
        std::fs::set_permissions(&secure, std::fs::Permissions::from_mode(0o600)).unwrap();
        warn_if_key_perms_insecure(secure.to_str().unwrap()); // no warning, no panic

        let insecure = dir.join("insecure_key");
        std::fs::write(&insecure, b"x").unwrap();
        std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o644)).unwrap();
        warn_if_key_perms_insecure(insecure.to_str().unwrap()); // warns, no panic

        // Nonexistent path: metadata() fails, helper silently returns.
        warn_if_key_perms_insecure(dir.join("missing").to_str().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_host_key() {
        // Verify key generation doesn't panic and produces an Ed25519 key.
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .expect("Ed25519 key generation should succeed");
        assert_eq!(key.algorithm(), russh::keys::Algorithm::Ed25519);
    }

    #[test]
    fn test_key_roundtrip() {
        use russh::keys::ssh_key::LineEnding;

        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();

        let pem = key.to_openssh(LineEnding::LF).unwrap();
        let decoded =
            russh::keys::decode_secret_key(&pem, None).expect("should decode generated key");
        assert_eq!(decoded.algorithm(), russh::keys::Algorithm::Ed25519);
    }

    #[test]
    fn test_key_save_and_load() {
        use russh::keys::ssh_key::LineEnding;

        let dir = std::env::temp_dir().join("xmodem_test_ssh_key");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_host_key");

        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();

        let pem = key.to_openssh(LineEnding::LF).unwrap();
        std::fs::write(&path, pem.as_bytes()).unwrap();

        let loaded = russh::keys::load_secret_key(&path, None)
            .expect("should load saved key");
        assert_eq!(loaded.algorithm(), russh::keys::Algorithm::Ed25519);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_gateway_client_key_file_constant() {
        assert_eq!(GATEWAY_CLIENT_KEY_FILE, "ethernet_gateway_ssh_key");
    }

    /// The generator is expected to produce an Ed25519 keypair whose
    /// OpenSSH PEM can be round-tripped through `load_secret_key`.
    /// This test exercises the pure generate→encode→decode path so it
    /// doesn't touch `GATEWAY_CLIENT_KEY_FILE` on disk.
    #[test]
    fn test_client_key_generation_shape() {
        use russh::keys::ssh_key::LineEnding;
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .expect("Ed25519 generation should succeed");
        assert_eq!(key.algorithm(), russh::keys::Algorithm::Ed25519);
        let pem = key.to_openssh(LineEnding::LF).unwrap();
        let decoded = russh::keys::decode_secret_key(&pem, None)
            .expect("generated key should round-trip through OpenSSH PEM");
        assert_eq!(decoded.algorithm(), russh::keys::Algorithm::Ed25519);
    }

    /// `client_public_key_openssh` should emit exactly `<algo> <b64>`
    /// with no trailing comment.  Tested via a synthesized key.
    #[test]
    fn test_client_public_key_openssh_format() {
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let line = key.public_key().to_string();
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        assert!(
            parts.len() >= 2,
            "public key string should be at least `<algo> <b64>`"
        );
        let trimmed = if parts.len() >= 2 {
            format!("{} {}", parts[0], parts[1])
        } else {
            line.clone()
        };
        // Should start with the Ed25519 algorithm name.
        assert!(
            trimmed.starts_with("ssh-ed25519 "),
            "expected ssh-ed25519 prefix, got {:?}",
            trimmed,
        );
        // Should not contain a third space-separated field (comment).
        assert_eq!(trimmed.split(' ').count(), 2);
    }

    /// Full `load_or_generate_client_key` loop: generate, save (via
    /// temp file path), reload, verify algorithm and on Unix the mode.
    /// We do NOT use `GATEWAY_CLIENT_KEY_FILE` itself because other
    /// tests and the running binary may share the CWD.
    #[test]
    fn test_client_key_persists_with_restrictive_mode() {
        use russh::keys::ssh_key::LineEnding;
        let dir = std::env::temp_dir().join("xmodem_test_gateway_client_key");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("client_key");
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let pem = key.to_openssh(LineEnding::LF).unwrap();
        std::fs::write(&path, pem.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        let loaded = russh::keys::load_secret_key(&path, None)
            .expect("should load client key back");
        assert_eq!(loaded.algorithm(), russh::keys::Algorithm::Ed25519);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── Session-slot accounting (claim on successful auth) ───

    /// A session slot is claimed only on a successful login, released only
    /// if it was claimed, and the cap is enforced at exactly `max_sessions`
    /// — so an unauthenticated/stalled connection can't exhaust the cap.
    #[tokio::test]
    async fn test_auth_password_slot_accounting_and_cap() {
        use russh::server::{Auth, Handler};
        let session_count = Arc::new(AtomicUsize::new(0));
        let make = || SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: session_count.clone(),
            max_sessions: 2,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some("10.0.0.1".parse().unwrap()),
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            counted: false,
        };

        // Failed auth must NOT claim a slot, and dropping an uncounted
        // handler must not change (or underflow) the counter.
        {
            let mut h = make();
            let r = h.auth_password("admin", "wrong").await.unwrap();
            assert!(matches!(r, Auth::Reject { .. }));
            assert!(!h.counted);
            assert_eq!(session_count.load(Ordering::SeqCst), 0);
        }
        assert_eq!(session_count.load(Ordering::SeqCst), 0);

        // Successful auth claims exactly one slot; Drop releases it.
        {
            let mut h = make();
            assert!(matches!(
                h.auth_password("admin", "secret").await.unwrap(),
                Auth::Accept
            ));
            assert!(h.counted);
            assert_eq!(session_count.load(Ordering::SeqCst), 1);
        }
        assert_eq!(session_count.load(Ordering::SeqCst), 0);

        // Cap: hold max_sessions (2) authenticated handlers, then a third
        // *valid* login is rejected and rolls its increment back.
        let mut h1 = make();
        assert!(matches!(
            h1.auth_password("admin", "secret").await.unwrap(),
            Auth::Accept
        ));
        let mut h2 = make();
        assert!(matches!(
            h2.auth_password("admin", "secret").await.unwrap(),
            Auth::Accept
        ));
        assert_eq!(session_count.load(Ordering::SeqCst), 2);

        let mut h3 = make();
        assert!(matches!(
            h3.auth_password("admin", "secret").await.unwrap(),
            Auth::Reject { .. }
        ));
        assert!(!h3.counted, "over-cap login must not be counted");
        assert_eq!(
            session_count.load(Ordering::SeqCst),
            2,
            "over-cap login must roll its increment back"
        );
        drop(h3); // uncounted → no change
        assert_eq!(session_count.load(Ordering::SeqCst), 2);
        drop(h2);
        drop(h1);
        assert_eq!(session_count.load(Ordering::SeqCst), 0);
    }

    /// A locked-out IP is rejected even with correct credentials, and the
    /// rejection claims no session slot.
    #[tokio::test]
    async fn test_auth_password_rejects_locked_out_ip() {
        use russh::server::{Auth, Handler};
        let ip: std::net::IpAddr = "10.0.0.7".parse().unwrap();
        let lockouts: telnet::LockoutMap =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // Drive the IP into lockout (>= AUTH_MAX_ATTEMPTS failures).
        for _ in 0..telnet::AUTH_MAX_ATTEMPTS {
            telnet::record_auth_failure(&lockouts, ip);
        }
        assert!(telnet::is_locked_out(&lockouts, ip));

        let session_count = Arc::new(AtomicUsize::new(0));
        let mut h = SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: session_count.clone(),
            max_sessions: 2,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some(ip),
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            counted: false,
        };
        // Correct credentials, but locked out → reject, no slot claimed.
        assert!(matches!(
            h.auth_password("admin", "secret").await.unwrap(),
            Auth::Reject { .. }
        ));
        assert!(!h.counted);
        assert_eq!(session_count.load(Ordering::SeqCst), 0);
    }

    /// Repeated wrong passwords lock the IP out and never claim a slot.
    #[tokio::test]
    async fn test_auth_password_failures_trigger_lockout() {
        use russh::server::{Auth, Handler};
        let ip: std::net::IpAddr = "10.0.0.8".parse().unwrap();
        let lockouts: telnet::LockoutMap =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let session_count = Arc::new(AtomicUsize::new(0));
        let make = || SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: session_count.clone(),
            max_sessions: 5,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some(ip),
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            counted: false,
        };
        for _ in 0..telnet::AUTH_MAX_ATTEMPTS {
            let mut h = make();
            assert!(matches!(
                h.auth_password("admin", "wrong").await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(
            telnet::is_locked_out(&lockouts, ip),
            "IP must be locked out after AUTH_MAX_ATTEMPTS failures"
        );
        assert_eq!(
            session_count.load(Ordering::SeqCst),
            0,
            "failed auth must never claim a session slot"
        );
    }

    /// A successful login clears a prior (sub-threshold) failure count, so a
    /// later single failure starts counting from one again.
    #[tokio::test]
    async fn test_auth_password_success_clears_failure_counter() {
        use russh::server::{Auth, Handler};
        let ip: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        let lockouts: telnet::LockoutMap =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // Some failures, but below the lockout threshold.
        telnet::record_auth_failure(&lockouts, ip);
        telnet::record_auth_failure(&lockouts, ip);
        assert!(!telnet::is_locked_out(&lockouts, ip));

        let session_count = Arc::new(AtomicUsize::new(0));
        let mut h = SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: session_count.clone(),
            max_sessions: 2,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some(ip),
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            counted: false,
        };
        assert!(matches!(
            h.auth_password("admin", "secret").await.unwrap(),
            Auth::Accept
        ));
        // Success cleared the counter: the next failure is counted as the first.
        assert_eq!(
            telnet::record_auth_failure(&lockouts, ip),
            1,
            "successful login must reset the failure counter"
        );
    }
}
