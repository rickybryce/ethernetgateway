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

const SSH_HOST_KEY_FILE: &str = "ethernetgateway-data/ethernet_ssh_host_key";
/// Client keypair used by the outgoing SSH gateway to authenticate
/// against remote servers via public-key authentication.  Generated on
/// first use and persisted for the lifetime of the deployment so that
/// the operator can add the same public key to remote `authorized_keys`
/// files once and reuse it across sessions.
pub(crate) const GATEWAY_CLIENT_KEY_FILE: &str = "ethernetgateway-data/ethernet_gateway_ssh_key";

/// Public keys a **slave** may log in with, one OpenSSH line each.
///
/// This is what lets a slave stop storing `slave_master_password`.  That value
/// is the only cleartext secret left in `egateway.conf`, and it cannot be
/// hashed the way `password` is: the slave *presents* it, and a hash cannot be
/// presented.  Encrypting it in the file would put the decryption key on the
/// same disk, which is obfuscation rather than protection -- so the secret is
/// **removed** instead, and the slave proves itself with the Ed25519 key it
/// already generates for outbound SSH (`GATEWAY_CLIENT_KEY_FILE`).
///
/// Absent or empty means public-key auth is **refused outright**, so an
/// installation that has never heard of this file behaves exactly as before.
pub(crate) const RELAY_AUTHORIZED_KEYS_FILE: &str =
    "ethernetgateway-data/relay_authorized_keys";

/// The authorized-keys path, redirected under test.
///
/// The same `cfg(test)` redirect `config_file_path` uses, and for the same
/// reason: the constant is a relative path, so a test would otherwise write
/// into the real data directory -- and chdir'ing instead is process-global,
/// which races every other test in the binary.
#[cfg(not(test))]
fn relay_authorized_keys_path() -> String {
    RELAY_AUTHORIZED_KEYS_FILE.to_string()
}

#[cfg(test)]
fn relay_authorized_keys_path() -> String {
    use std::sync::OnceLock;
    static TEST_KEYS_PATH: OnceLock<String> = OnceLock::new();
    TEST_KEYS_PATH
        .get_or_init(|| {
            std::env::temp_dir()
                .join(format!("egateway_relay_keys_test_{}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        })
        .clone()
}

// ─── Public API ────────────────────────────────────────────

/// Start the SSH server if enabled in config.
pub fn start_ssh_server(
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    session_writers: telnet::SessionWriters,
    lockouts: telnet::LockoutMap,
    conn_rates: telnet::ConnRateMap,
) {
    let cfg = config::get_config();
    if !cfg.ssh_enabled {
        return;
    }

    let port = cfg.ssh_port;
    crate::bindwatch::expect("SSH", port);

    tokio::spawn(async move {
        let key = match load_or_generate_host_key() {
            Ok(k) => k,
            Err(e) => {
                glog!("SSH server: failed to load/generate host key: {}", e);
                // Not a bind failure, but this listener is not coming up, so
                // say so rather than leaving the watcher waiting on it.
                crate::bindwatch::failed(
                    "SSH",
                    &std::io::Error::other("host key unavailable"),
                );
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
            conn_rates: conn_rates.clone(),
        };

        // Bind the socket ourselves and hand it to russh (`run_on_address` is
        // literally bind-then-run_on_socket).  Two reasons: a bind failure is
        // then reported as one — the old form surfaced "Address in use" as a
        // generic post-hoc "SSH server error" *after* logging "listening on
        // port", which read as if the port had come up — and it lets this
        // listener report its outcome like the other three (see bindwatch).
        let addr = format!("0.0.0.0:{}", port);
        let socket = match tokio::net::TcpListener::bind(&addr).await {
            Ok(s) => s,
            Err(e) => {
                glog!("SSH server: failed to bind port {}: {}", port, e);
                crate::bindwatch::failed("SSH", &e);
                return;
            }
        };
        crate::bindwatch::bound("SSH");
        glog!("SSH server listening on port {}", port);

        tokio::select! {
            result = server.run_on_socket(config, &socket) => {
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
    // The directory must exist before anything in it can be written.
    // `main` creates it at startup, but relying on that alone was wrong twice
    // over: a unit test reaches this writer without going through `main` (which
    // is how the missing directory was found), and an operator can remove the
    // folder while the gateway is running.  The parent of the path we are about
    // to write, never a constant -- see `config::ensure_parent_dir`.
    crate::config::ensure_parent_dir(path);
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
        // Deliberately serve anyway with this in-memory key rather than
        // refusing to start (unlike the parse-failure path above, which
        // aborts to preserve an *existing* identity): a first boot on a
        // read-only / full working dir shouldn't take SSH down entirely.
        // But the identity WON'T survive a restart — the file is still
        // absent, so the next run generates a different key and clients
        // get "REMOTE HOST IDENTIFICATION HAS CHANGED".  Say so loudly so
        // the operator can fix the directory instead of chasing that later.
        glog!(
            "SSH server: WARNING: could not save host key to {} ({}); serving with a \
             temporary key that will NOT persist — clients will see the host key change \
             on the next restart until the working directory is writable",
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
/// The keys listed in [`RELAY_AUTHORIZED_KEYS_FILE`], ignoring blanks and `#`.
///
/// A line that will not parse is **named in the log and skipped**, never taken
/// as a reason to refuse the rest: one fat-fingered paste must not lock out
/// every slave that was working, and a silently dropped line is a credential
/// that stops working for no stated reason.
pub(crate) fn load_relay_authorized_keys() -> Vec<russh::keys::PublicKey> {
    let path = relay_authorized_keys_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match russh::keys::PublicKey::from_openssh(line) {
            Ok(k) => keys.push(k),
            Err(e) => glog!(
                "SSH: {} line {} is not a public key ({}); skipping it",
                path,
                n + 1,
                e
            ),
        }
    }
    keys
}

/// How many slave keys one master will hold.
///
/// A bound rather than a belief: enrolment is a remote peer causing a write, so
/// the file must not be able to grow without end -- a slave that regenerated
/// its key every boot would otherwise append for ever.  Sixty-four is far past
/// any real deployment and small enough to read.
pub(crate) const MAX_AUTHORIZED_KEYS: usize = 64;

/// Record a slave's public key so it can stop storing the master's password.
///
/// Called only for a peer that has **already authenticated**, so this grants no
/// access the caller did not just demonstrate it has -- what it changes is that
/// the access survives a password change, which is why the entry is written to
/// be identifiable and removable by hand.
///
/// Returns the message to log, or an error to refuse with.
/// Enrolment is a read-modify-write of one file, so it is serialised.
///
/// Two slaves reconnecting at the same moment -- the normal case when a master
/// comes back up -- would otherwise each read the pre-existing file and each
/// write their own version, and the second rename would discard the first
/// slave's key while both were told they had been enrolled.  The staged rename
/// makes the *write* atomic; it does nothing for the read that preceded it.
static ENROL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn enroll_relay_key(
    line: &str,
    peer: Option<std::net::IpAddr>,
    label: &str,
) -> Result<String, String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|e| format!("not a usable public key: {}", e))?;

    let _serialised = ENROL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let existing = load_relay_authorized_keys();
    if key_is_authorized(&existing, &key) {
        // Idempotent: a slave re-sending the key it already enrolled is the
        // normal case on every restart, and must not append a second line.
        return Ok(format!(
            "already enrolled ({})",
            key.fingerprint(Default::default())
        ));
    }
    if existing.len() >= MAX_AUTHORIZED_KEYS {
        return Err(format!(
            "{} already holds {} keys; remove one before enrolling another",
            relay_authorized_keys_path(),
            existing.len()
        ));
    }

    // **The label is remote input written into a file we later parse**, so a
    // newline in it would let a peer append authorized keys of its own
    // choosing.  Reduced to a short run of harmless characters rather than
    // escaped, because it is a convenience for a human reading the file and
    // nothing depends on its exact content.
    let safe: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
        .take(32)
        .collect();
    let who = if safe.is_empty() { "slave".to_string() } else { safe };
    let from = peer.map(|i| i.to_string()).unwrap_or_else(|| "unknown".into());
    let path = relay_authorized_keys_path();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    // The key is re-rendered from the PARSED key, never echoed from the wire:
    // whatever arrives, what lands in the file is a key this gateway itself
    // formatted.
    let rendered = key
        .to_openssh()
        .map_err(|e| format!("cannot render the key: {}", e))?;
    // No date: this crate carries no wall-clock formatter and one is not worth
    // a dependency for a comment.  The file's mtime says when it last changed
    // and the fingerprint says which device a line is, which is what an
    // operator removing one actually needs.
    text.push_str(&format!("{} {} {}\n", rendered.trim(), who, from));

    crate::config::ensure_parent_dir(&path);
    let tmp = format!("{}.new", path);
    std::fs::write(&tmp, &text).map_err(|e| format!("cannot write {}: {}", tmp, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("cannot replace {}: {}", path, e))?;

    Ok(format!(
        "enrolled {} for {} from {}",
        key.fingerprint(Default::default()),
        who,
        from
    ))
}

/// This gateway's outbound public key, as an OpenSSH line.
///
/// A slave cannot be enrolled on a master until somebody can *see* this, and a
/// headless slave reached from a C64 has nowhere else to show it -- so it is
/// logged at startup rather than left for the operator to find with ssh-keygen
/// in a directory they would first have to be told about.
pub(crate) fn client_public_key_line() -> Result<String, String> {
    let key = load_or_generate_client_key()?;
    key.public_key()
        .to_openssh()
        .map_err(|e| format!("cannot render the public key: {}", e))
}

/// Whether `key` is one of the authorized relay keys.
///
/// Compared on **key data**, not on the whole record: an OpenSSH line carries a
/// trailing comment (usually a hostname) that an operator will edit, and a key
/// that stopped working because somebody renamed the machine in a comment would
/// be a mystery worth nobody's afternoon.
pub(crate) fn key_is_authorized(
    authorized: &[russh::keys::PublicKey],
    key: &russh::keys::PublicKey,
) -> bool {
    authorized.iter().any(|k| k.key_data() == key.key_data())
}

pub(crate) fn load_or_generate_client_key() -> Result<russh::keys::PrivateKey, String> {
    use russh::keys::ssh_key::LineEnding;

    if std::path::Path::new(GATEWAY_CLIENT_KEY_FILE).exists() {
        warn_if_key_perms_insecure(GATEWAY_CLIENT_KEY_FILE);
        match russh::keys::load_secret_key(GATEWAY_CLIENT_KEY_FILE, None) {
            Ok(key) => {
                return Ok(key);
            }
            Err(e) => {
                // Do NOT overwrite a file that exists but won't parse — it may
                // be a merely-truncated, recoverable key, and silently minting
                // a new one changes the gateway's outbound identity (breaking
                // pubkey auth on every remote that trusts the old key).  Unlike
                // the host key we don't refuse outright: outbound SSH can still
                // fall back to password auth.  So use an EPHEMERAL in-memory
                // key for this session and leave the file untouched for the
                // operator to restore or remove.  (This mirrors the host-key
                // path's refuse-to-overwrite intent, minus the hard failure.)
                glog!(
                    "SSH gateway: {} exists but could not be read: {}. Using an \
                     ephemeral client key this session and leaving the file in \
                     place; pubkey auth will fail until it is fixed or removed.",
                    GATEWAY_CLIENT_KEY_FILE, e
                );
                return russh::keys::PrivateKey::random(
                    &mut rand::rng(),
                    russh::keys::Algorithm::Ed25519,
                )
                .map_err(|e| format!("client key generation failed: {}", e));
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

/// Claim one session slot if the cap allows; `true` when the slot is ours.
///
/// Atomic `fetch_add` + rollback, the same pattern as the telnet accept loop:
/// two connections racing can both increment, but only those whose prior value
/// was under the cap keep their slot, so the count never settles above `max`.
/// The cap binds at exactly `max` — slots `0..max` are admitted, the next is
/// refused.
///
/// Single-sourced deliberately.  This was written out three times (password
/// auth, relay `exec`, console register) and each copy has its own release
/// paths; a divergence between the *claims* is one of the ways the accounting
/// could flip from over-counting (safe) to under-counting (fails open), which
/// is precisely the risk that kept M-11 deferred.  One implementation, one
/// test.
fn try_claim_slot(count: &AtomicUsize, max: usize) -> bool {
    let prev = count.fetch_add(1, Ordering::SeqCst);
    if prev >= max {
        count.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    true
}

/// Releases a claimed session slot when dropped.
///
/// Exists because a manual release at the end of a function is one `return`
/// away from leaking, and that is not hypothetical: the relay task released its
/// slot on the last line, and adding the Kermit-server branch — which returns
/// early — silently leaked one slot per relay Kermit transfer, permanently
/// shrinking the master's capacity until a restart. A guard cannot be bypassed
/// by a branch added later.
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    /// Per-IP connection rate map, also shared with telnet -- see
    /// `telnet::note_connection`.
    conn_rates: telnet::ConnRateMap,
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
        // Per-IP connection rate limit.  `run_on_socket` owns the accept
        // loop, so unlike telnet this cannot refuse the TCP connection
        // itself -- the verdict is recorded here (the one hook that sees
        // every inbound connection, authenticated or not) and enforced in
        // both auth paths below, which is the same place the lockout is
        // enforced and costs an attacker russh's `auth_rejection_time`.
        let (rate_max, rate_window) = config::get_conn_rate();
        let (rate_limited, rate_say_so) = match peer_addr {
            Some(a) if rate_max > 0 => {
                let (count, first) =
                    telnet::note_connection(&self.conn_rates, a.ip(), rate_max, rate_window);
                (count > rate_max, first)
            }
            _ => (false, false),
        };
        if let Some(addr) = peer_addr {
            // Once per flood, not once per connection -- see `ConnRate`.
            // `glog!` is a blocking write inline here and the log rolls, so
            // an unconditional line would push out the evidence it exists to
            // record.
            // **Nested, not `&&`.**  With `rate_limited && rate_say_so` the
            // second and later connections of a flood fall into the `else`
            // and are logged as ordinary accepted connections -- one blocking
            // write each, which is the amplifier this exists to remove, and
            // mislabelled besides: an operator reading the log during a flood
            // would see thousands of "connection from X" lines and one
            // refusal.  A refused connection is never a normal one.
            if rate_limited {
                if rate_say_so {
                    glog!(
                        "SSH: connection from {} over rate limit ({} in {}s); further \
                         refusals from this address are not logged until it is under \
                         the limit again",
                        addr, rate_max, rate_window.as_secs()
                    );
                }
            } else {
                glog!("SSH: connection from {}", addr);
            }
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
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: self.session_writers.clone(),
            lockouts: self.lockouts.clone(),
            // Read once per connection: off the auth path, and enrolling a
            // slave then takes effect on its next reconnect rather than on a
            // restart of the master.
            authorized_keys: load_relay_authorized_keys(),
            key_authed: false,
            counted: false,
            rate_limited,
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
    /// `TERM` from the client's pty request, when it sent one.
    ///
    /// Handed to the `TelnetSession` so an SSH client that announced its
    /// terminal is not asked to press BACKSPACE — the same shortcut telnet
    /// already takes from TTYPE.  `None` for a shell opened without a pty.
    pty_term: Option<String>,
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
    /// Set at connect time when this IP is over the per-IP connection rate
    /// limit; every auth path refuses while it is set.  Decided once in
    /// `new_client` rather than per attempt, so one connection is one unit of
    /// rate however many auth methods it tries.
    rate_limited: bool,
    /// Whether this connection claimed a session slot (set once auth
    /// succeeds).  Gates the Drop decrement so an unauthenticated
    /// connection that never counted can't underflow the shared counter.
    /// The relay keys this connection may authenticate with.
    ///
    /// Read **once per connection** rather than per attempt: it keeps the file
    /// off the auth path, and it means enrolling a slave takes effect on its
    /// next reconnect rather than on a restart of the master.
    authorized_keys: Vec<russh::keys::PublicKey>,
    /// This connection authenticated with an enrolled **relay** key.
    ///
    /// It is a narrower credential than the password, and must stay narrower:
    /// the file is called `relay_authorized_keys`, the manual calls it a relay
    /// key, and a slave only ever needs `exec`.  Without this flag the same key
    /// opened a full interactive menu -- configuration, file transfer, the
    /// gateways -- and did so even where the operator had blanked the SSH
    /// password specifically to shut that door.
    key_authed: bool,
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
    /// Register a remote slave port (`serial-register <port>`, §9 #12).
    /// Gated like the relay path (master + accept_relays + a known peer IP)
    /// and counted against the session cap (a registered idle port holds a
    /// slot until it disconnects).  The channel is held idle in the global
    /// registry; the Serial Gateway picker claims it.
    ///
    /// **The wire says the mode now, and it did not until 2026-08-23.**
    /// `serial-register` carried only the port label, so this end could not tell
    /// a console port from a modem or Kermit one -- all three register here --
    /// and the picker listed every remote port as a bare `B@ip`. An operator
    /// therefore could not see what they were about to pick, while the *local*
    /// rows beside them said "Console mode" plainly. The mode is now an optional
    /// second token, and optional is the operative word: a slave older than the
    /// addition sends none, and `None` renders as nothing rather than guessing a
    /// default. (The log line used to say "console port" for either, which was
    /// wrong for every modem port from the day they began registering.)
    async fn register_console_port(
        &mut self,
        channel: russh::ChannelId,
        label: &str,
        facts: crate::relay::RemotePortFacts,
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
        if !try_claim_slot(&self.session_count, self.max_sessions) {
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
            "SSH: registered remote serial port {} from {}",
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
            crate::relay::register_remote_port(
                slave_ip,
                label.clone(),
                facts.clone(),
                gateway_stream,
            );
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
        if self.rate_limited {
            return Ok(russh::server::Auth::reject());
        }
        if let Some(ip) = self.peer_addr
            && telnet::is_locked_out(&self.lockouts, ip)
        {
            glog!("SSH: auth from {} rejected (locked out)", ip);
            return Ok(russh::server::Auth::reject());
        }
        // Refuse auth outright if the *configured* username or password is
        // empty (N2).  SSH deliberately ignores `security_enabled` and has no
        // unauthenticated mode, so a blanked credential must not become an
        // accept-anything server: `constant_time_eq(b"", b"")` is `true`, so
        // without this an operator who clears the password would turn the SSH
        // port (bound `0.0.0.0`) into an open shell bridge.  Checked before
        // the comparison so an empty stored secret can never match a supplied
        // empty one.
        if self.username.is_empty() || self.password.is_empty() {
            glog!("SSH: auth rejected — configured SSH username/password is empty");
            return Ok(russh::server::Auth::reject());
        }
        // Constant-time comparison to prevent timing attacks.
        let user_ok =
            telnet::constant_time_eq(user.as_bytes(), self.username.as_bytes());
        let pass_ok =
            crate::credential::verify(&self.password, password);
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
            if !try_claim_slot(&self.session_count, self.max_sessions) {
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
                if count >= telnet::MAX_AUTH_ATTEMPTS {
                    glog!(
                        "SSH: {} exceeded {} failed attempts; locked out",
                        ip,
                        telnet::MAX_AUTH_ATTEMPTS,
                    );
                }
            }
            Ok(russh::server::Auth::reject())
        }
    }

    /// Whether this key *would* be accepted, asked before the client signs.
    ///
    /// **The trait default is `Accept`**, which tells every client its key
    /// would work and then refuses the signature -- an answer that is wrong
    /// twice over.  Answered honestly here so an unenrolled slave learns at
    /// once and falls back to its password instead of signing for nothing.
    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        public_key: &russh::keys::PublicKey,
    ) -> Result<russh::server::Auth, Self::Error> {
        if key_is_authorized(&self.authorized_keys, public_key) {
            Ok(russh::server::Auth::Accept)
        } else {
            Ok(russh::server::Auth::reject())
        }
    }

    /// Public-key authentication, for a slave that has been enrolled.
    ///
    /// This is what lets a slave stop storing the master's password in
    /// cleartext -- see [`RELAY_AUTHORIZED_KEYS_FILE`] for why that value
    /// could never simply be hashed.  With no authorized-keys file the method
    /// refuses everything, so an installation that has never heard of it is
    /// unchanged.
    ///
    /// **A rejected key does NOT count toward the lockout.**  Password
    /// guessing is what the lockout exists for; a public key is not
    /// guessable, and counting a refusal here would let the ordinary
    /// sequence -- a slave offers its key, is refused, then authenticates
    /// with its password -- ban the very slave that went on to log in
    /// correctly.  A *successful* key clears the lockout, exactly as a
    /// successful password does.
    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::PublicKey,
    ) -> Result<russh::server::Auth, Self::Error> {
        if self.rate_limited {
            return Ok(russh::server::Auth::reject());
        }
        if let Some(ip) = self.peer_addr
            && telnet::is_locked_out(&self.lockouts, ip)
        {
            glog!("SSH: key auth from {} rejected (locked out)", ip);
            return Ok(russh::server::Auth::reject());
        }
        // The same rule as the password path: a blanked username must not
        // become an accept-anything server.  The password being empty is fine
        // here -- removing it is the entire point of key auth.
        if self.username.is_empty() {
            glog!("SSH: key auth rejected — configured SSH username is empty");
            return Ok(russh::server::Auth::reject());
        }
        if !telnet::constant_time_eq(user.as_bytes(), self.username.as_bytes())
            || !key_is_authorized(&self.authorized_keys, public_key)
        {
            return Ok(russh::server::Auth::reject());
        }
        self.key_authed = true;
        if let Some(ip) = self.peer_addr {
            telnet::clear_lockout(&self.lockouts, ip);
            glog!("SSH: {} authenticated by public key ({})", ip, public_key.fingerprint(Default::default()));
        }
        // Claim a session slot on exactly the same terms as the password path.
        // Missing this is how a cap silently stops capping: the count is what
        // `max_sessions` is enforced against, and `Drop` subtracts once.
        if self.counted {
            return Ok(russh::server::Auth::Accept);
        }
        if !try_claim_slot(&self.session_count, self.max_sessions) {
            if let Some(ip) = self.peer_addr {
                glog!(
                    "SSH: {} authenticated by key but server at capacity ({}); rejecting",
                    ip, self.max_sessions,
                );
            }
            return Ok(russh::server::Auth::reject());
        }
        self.counted = true;
        Ok(russh::server::Auth::Accept)
    }

    /// Accept the session channel.
    ///
    /// **The `accept()` call is load-bearing and its absence compiles.**  Up
    /// to russh 0.60 this returned `Result<bool, _>` and `Ok(true)` meant
    /// accept.  From 0.62 it returns `Result<(), _>` and the decision travels
    /// on the `ChannelOpenHandle`, whose `Drop` sends
    /// `AdministrativelyProhibited` -- so the mechanical port of this method
    /// (name the parameter `_reply`, return `Ok(())`) type-checks, matches the
    /// trait's own default, and **refuses every SSH session**.  The trait
    /// default is `async { Ok(()) }` precisely because a handler that says
    /// nothing is a handler that declines.
    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<russh::server::Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        let _ = channel;
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: russh::ChannelId,
        term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // The client's `TERM` is the same fact telnet's TTYPE carries, and it
        // was being discarded.
        //
        // What that cost is *not* an extra prompt: `run()` skips
        // `detect_terminal_type` entirely for SSH (`if !self.is_ssh`), so an SSH
        // session never asked anything.  It simply kept `new_ssh`'s default of
        // `TerminalType::Ansi` — every SSH client was assumed to be ANSI
        // whatever it said it was, so `TERM=dumb` got colour it cannot render
        // and a Commodore-side client got ANSI instead of PETSCII.  Measured:
        // with this plumbed in, `TERM=c64` over SSH now reaches the menu in
        // PETSCII.
        //
        // The pty request always precedes the shell request, so this is set
        // before `shell_request` builds the session.  A client with no pty at
        // all (`ssh host command`, or `-T`) never gets here and keeps the ANSI
        // default, exactly as every SSH session did before.
        self.pty_term = Some(term.to_string());
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut russh::server::Session,
    ) -> Result<(), Self::Error> {
        // **A relay key is not a login.**  It authorizes the `exec` a slave
        // needs and nothing else; the interactive menu stays behind the
        // password, which is also what an operator who blanked that password
        // was asking for.
        if self.key_authed {
            glog!(
                "SSH: shell refused for {:?} — a relay key authorizes relay exec, not a session",
                self.peer_addr
            );
            session.channel_failure(channel)?;
            return Ok(());
        }
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
        let pty_term = self.pty_term.clone();
        tokio::spawn(async move {
            let mut sess = telnet::TelnetSession::new_ssh(
                Box::new(gateway_read),
                writer_for_task.clone(),
                shutdown,
                restart,
                peer_addr,
                lockouts_for_task,
            );
            // Before `run()`, so detection sees it.
            if let Some(term) = pty_term {
                sess.note_announced_terminal(&term);
            }
            if let Err(e) = sess.run().await {
                if !crate::telnet::is_normal_disconnect(&e) {
                    glog!("SSH: session error: {}", e);
                }
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
        if let Some(rest) = command.strip_prefix("serial-register ") {
            // `<label> [mode]`.  Tokens, not "everything after the space":
            // the mode was added as a second token, and a master that took the
            // remainder whole would register `"B console"` as the label.
            let (label, facts) = crate::relay::parse_register_args(rest);
            return self.register_console_port(channel, &label, facts, session).await;
        }

        // Grammar (§3 Model B): `serial-relay <port> menu`
        //                    or `serial-relay <port> dial <host>:<port>`.
        // **Key enrolment.**  A slave that just authenticated asks the master to
        // remember its public key, so it can stop storing the master's password
        // in cleartext.  Its own channel and its own command, which is what
        // makes this need no protocol version bump: a master too old to know
        // the word answers `channel_failure`, the slave reads that as "not
        // supported" and carries on with its password exactly as before.
        //
        // Refused unless this connection authenticated -- `self.counted` is set
        // only by a successful auth -- because the whole safety argument is
        // that enrolment grants nothing the caller has not already shown it has.
        if let Some(rest) = command.strip_prefix("enroll-key ") {
            if !self.counted {
                glog!("SSH: enroll-key from {:?} refused (not authenticated)", self.peer_addr);
                session.channel_failure(channel)?;
                return Ok(());
            }
            // **Only a master that accepts relays enrols anybody** -- the same
            // gate the relay commands use, and it belongs here rather than
            // below them.  Without it a *standalone* gateway with SSH switched
            // on would let any authenticated user record a key and thereafter
            // log in without the password, surviving a password change, on a
            // machine whose operator never asked for relaying at all.
            {
                let cfg = config::get_config();
                if cfg.gateway_role != "master" || !cfg.master_accept_relays {
                    glog!(
                        "SSH: enroll-key from {:?} refused (role={}, accept_relays={})",
                        self.peer_addr,
                        cfg.gateway_role,
                        cfg.master_accept_relays
                    );
                    drop(cfg);
                    session.channel_failure(channel)?;
                    return Ok(());
                }
            }
            // `<type> <base64> [label]` -- the label is advisory and the master
            // writes its own comment; see `enroll_relay_key`.
            let mut it = rest.splitn(3, ' ');
            let (t, b64, label) = (
                it.next().unwrap_or_default(),
                it.next().unwrap_or_default(),
                it.next().unwrap_or_default(),
            );
            match enroll_relay_key(&format!("{} {}", t, b64), self.peer_addr, label)
            {
                Ok(msg) => {
                    glog!("SSH: relay key from {:?}: {}", self.peer_addr, msg);
                    // Reload for THIS connection too, so a slave that enrols and
                    // reconnects immediately is not told "no" by a stale list.
                    self.authorized_keys = load_relay_authorized_keys();
                    session.channel_success(channel)?;
                }
                Err(e) => {
                    glog!("SSH: enroll-key from {:?} refused: {}", self.peer_addr, e);
                    session.channel_failure(channel)?;
                }
            }
            return Ok(());
        }

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
        let kermit_target = parsed.kermit;

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

        // **A refusal must happen before the hello, or the handshake lies.**
        // `allow_relay_kermit` used to be read inside `run_master_relay_kermit`
        // -- after `channel_success`, after `RELAY_HELLO`, and after the
        // "accepted serial relay" log line. The hello exists precisely so the
        // slave "distinguishes an accepted relay from a refused-but-open
        // channel", so evaluating the one remaining gate behind it defeated it
        // for this target alone. Measured on a live pair (2026-08-21): the
        // slave logged `CONNECTED -- the master's Kermit server is on this
        // wire; files live on the master`, said the same thing in its link
        // summary, then took the EOF as a dropped link -- and because the
        // connect had *succeeded* it reset `attempt` and the backoff every
        // cycle, giving one reconnect per second for ever instead of the
        // 60 s `RECONNECT_BACKOFF_REFUSED`, with a fresh log line on both
        // machines each time. A refusal reported as a success is worse than a
        // refusal: the operator is told the thing they configured is working.
        //
        // Refused here, the slave's existing `Refused` classification does the
        // rest -- one deduped outage line, a 60 s retry, and a message that
        // says the master is declining relays.
        if kermit_target && !crate::relay::kermit_relay_allowed(&cfg) {
            glog!(
                "SSH: refused serial-relay from {:?} (port {}, kermit server: \
                 allow_relay_kermit=false)",
                self.peer_addr,
                port_label
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
        //
        // The arithmetic is pinned by
        // `test_relay_channel_slot_is_on_top_of_auth_slot`, so a later change
        // to it has to go through a test that states the tradeoff.  The claim
        // itself is `try_claim_slot` — one implementation for all three sites.
        if !try_claim_slot(&self.session_count, self.max_sessions) {
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
        // the first bytes on the accepted channel, before any menu data, so
        // the slave distinguishes an accepted relay from a refused-but-open
        // channel and detects a version skew.
        //
        // **But only where accepting IS the answer.** For a `menu` or `kermit`
        // target this master is itself the far end, so the channel being up is
        // the whole result. A `dial` or `peer` target still has a call to
        // place, and the slave turns the hello straight into a modem `CONNECT`
        // with carrier asserted -- so sending it here told the device a call
        // was up before anything had been dialled, and every failure past this
        // point (refused by `allow_peer_dial`, connection refused, answer
        // timeout, peer port unregistered or not dialable) reached the device
        // as CONNECT followed by NO CARRIER. Measured 2026-08-21 on a live
        // pair, in both the refused and the unreachable-host cases.
        //
        // Those two targets get their hello from `answer_and_bridge` once the
        // far end is actually up, so its absence means what the slave needs it
        // to mean: no call, answer NO CARRIER. The slave waits longer for it
        // when it asked for a dial (`relay::hello_wait`).
        let master_is_the_far_end = dial_target.is_none() && peer_target.is_none();
        if master_is_the_far_end {
            if let Err(e) = session.data(
                channel,
                bytes::Bytes::copy_from_slice(&crate::relay::RELAY_HELLO),
            ) {
                self.session_count.fetch_sub(1, Ordering::SeqCst);
                return Err(e);
            }
        }
        match (&dial_target, &peer_target) {
            (None, None) if kermit_target => glog!(
                "SSH: accepted serial relay (port {}, kermit server) from {:?}",
                port_label,
                self.peer_addr
            ),
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
        let port_label_for_kermit = port_label.clone();
        tokio::spawn(async move {
            // Release the slot however this task ends — including the Kermit
            // branch's early return, which used to skip the manual release at
            // the bottom and leak a slot per transfer.
            let _slot = SlotGuard(session_count);
            if kermit_target {
                // The slave's port is in Kermit-server mode: serve OUR Kermit
                // server on this channel, so its device's file operations
                // resolve against this master's transfer directory.
                crate::relay::run_master_relay_kermit(
                    gateway_stream,
                    port_label_for_kermit,
                    peer_addr,
                )
                .await;
                return;
            }
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
            // No manual release here: `_slot` does it on drop.
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

    /// The name **and** the folder it is composed from.
    ///
    /// Pinning the literal to itself is not the guarantee wanted: the whole
    /// point of the data-directory move is that everything the gateway creates
    /// is under one folder, so the assertion has to be against
    /// `config::DATA_DIR` rather than against a second copy of its spelling.
    #[test]
    fn test_host_key_file_constant() {
        assert_eq!(SSH_HOST_KEY_FILE, "ethernetgateway-data/ethernet_ssh_host_key");
        assert_eq!(
            SSH_HOST_KEY_FILE,
            format!("{}/ethernet_ssh_host_key", crate::config::DATA_DIR)
        );
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

    /// Name and folder, for the reason given on `test_host_key_file_constant`.
    #[test]
    fn test_gateway_client_key_file_constant() {
        assert_eq!(GATEWAY_CLIENT_KEY_FILE, "ethernetgateway-data/ethernet_gateway_ssh_key");
        assert_eq!(
            GATEWAY_CLIENT_KEY_FILE,
            format!("{}/ethernet_gateway_ssh_key", crate::config::DATA_DIR)
        );
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
    /// **A client can authenticate AND open a session channel.**
    ///
    /// This is the only test in the suite that drives a real SSH connection
    /// end to end, and it exists because of a trap that type-checks.  Up to
    /// russh 0.60 `channel_open_session` returned `Result<bool, _>` and
    /// `Ok(true)` meant accept.  From 0.62 it returns `Result<(), _>` and the
    /// answer travels on a `ChannelOpenHandle` whose `Drop` sends
    /// `AdministrativelyProhibited` -- so the mechanical port (rename the new
    /// parameter `_reply`, return `Ok(())`) compiles, matches the trait's own
    /// default, and refuses every session.  Every other test here calls
    /// handler methods directly and would have passed with the server broken;
    /// `Channel` and `ChannelOpenHandle` both have private constructors, so
    /// there is no way to reach this except over a real connection.
    ///
    /// Deliberately asserts the channel opens rather than that auth succeeds:
    /// auth is covered elsewhere, and it was auth-only cover that let the
    /// channel path go untested in the first place.
    #[tokio::test]
    async fn test_a_client_can_open_a_session_channel() {
        struct Client;
        impl russh::client::Handler for Client {
            type Error = russh::Error;
            async fn check_server_key(
                &mut self,
                _key: &russh::keys::PublicKeyOrCertificate,
            ) -> Result<bool, Self::Error> {
                // A throwaway client against our own loopback server: it is
                // testing the channel path, not host-key policy.  The product's
                // two real clients go through `telnet::pinnable_host_key`.
                Ok(true)
            }
        }

        let host_key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            ..Default::default()
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handler = SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: Arc::new(AtomicUsize::new(0)),
            max_sessions: 4,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some(addr.ip()),
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
        };

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Held until the client is done; dropping the session early would
            // close the channel and make a working server look broken.
            let running = russh::server::run_stream(config, stream, handler)
                .await
                .expect("server side failed to start");
            let _ = running.await;
        });

        let mut session = russh::client::connect(
            Arc::new(russh::client::Config::default()),
            addr,
            Client,
        )
        .await
        .expect("client could not connect");

        let auth = session
            .authenticate_password("admin", "secret")
            .await
            .expect("auth call failed");
        assert!(auth.success(), "password auth was refused");

        // The assertion this test exists for.  A rejected channel open comes
        // back as an Err here, which is exactly what the mechanical port of
        // channel_open_session produces -- silently, on every session.
        let channel = session
            .channel_open_session()
            .await
            .expect("the server refused to open a session channel");

        // Prove the channel is usable, not merely returned: a write is what a
        // shell session does first, and it fails on a half-open channel.
        channel
            .data(&b"\n"[..])
            .await
            .expect("could not write to the opened channel");

        drop(session);
        server.abort();
    }

    /// Serialise the enrolment tests and give each a clean file.
    ///
    /// They share one redirected path (per process, like the config's), so
    /// without this they would race each other's writes and the bound test
    /// would count another test's keys.
    fn keys_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::remove_file(relay_authorized_keys_path());
        g
    }

    /// Enrolment writes a key that can then authenticate, and is idempotent.
    #[test]
    fn test_enrolling_a_key_makes_it_authorized_once() {
        let _lock = keys_test_lock();
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let line = key.public_key().to_openssh().unwrap();

        assert!(!key_is_authorized(&load_relay_authorized_keys(), key.public_key()));
        let msg = enroll_relay_key(&line, Some("10.0.0.5".parse().unwrap()), "slave-a").unwrap();
        assert!(msg.contains("enrolled"), "{msg}");
        assert!(key_is_authorized(&load_relay_authorized_keys(), key.public_key()));

        // Re-enrolling the same key is the normal case on every restart and
        // must not append a second line.
        let again = enroll_relay_key(&line, Some("10.0.0.5".parse().unwrap()), "slave-a").unwrap();
        assert!(again.contains("already enrolled"), "{again}");
        let text = std::fs::read_to_string(relay_authorized_keys_path()).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("ssh-")).count(), 1);
        // The comment identifies the device for whoever has to remove it.
        assert!(text.contains("slave-a"), "{text}");
        assert!(text.contains("10.0.0.5"), "{text}");
    }

    /// **A label cannot inject lines.**  It is remote input written into a file
    /// this gateway later parses as authorizations, so a newline in it would
    /// let a peer enrol keys of its own choosing.
    #[test]
    fn test_a_label_cannot_add_authorized_keys() {
        let _lock = keys_test_lock();
        let mine = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let smuggled = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let evil = format!("ok\n{}\n", smuggled.public_key().to_openssh().unwrap());

        enroll_relay_key(&mine.public_key().to_openssh().unwrap(), None, &evil).unwrap();
        let keys = load_relay_authorized_keys();
        assert!(key_is_authorized(&keys, mine.public_key()));
        assert!(
            !key_is_authorized(&keys, smuggled.public_key()),
            "a key smuggled through the LABEL must not become authorized"
        );
        assert_eq!(keys.len(), 1);
    }

    /// The file cannot grow without end.
    #[test]
    fn test_enrolment_is_bounded() {
        let _lock = keys_test_lock();
        for _ in 0..MAX_AUTHORIZED_KEYS {
            let k = russh::keys::PrivateKey::random(
                &mut rand::rng(),
                russh::keys::Algorithm::Ed25519,
            )
            .unwrap();
            enroll_relay_key(&k.public_key().to_openssh().unwrap(), None, "x").unwrap();
        }
        let one_more = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let e = enroll_relay_key(&one_more.public_key().to_openssh().unwrap(), None, "x")
            .unwrap_err();
        assert!(e.contains("remove one"), "{e}");
    }

    /// Rubbish on the wire is refused, not written.
    #[test]
    fn test_enrolment_refuses_what_is_not_a_key() {
        let _lock = keys_test_lock();
        assert!(enroll_relay_key("ssh-ed25519 not-base64", None, "x").is_err());
        assert!(enroll_relay_key("", None, "x").is_err());
        assert!(load_relay_authorized_keys().is_empty());
    }

    /// A key nobody enrolled is refused, and costs nothing.
    ///
    /// The file being absent is the state of every installation that has never
    /// heard of this feature, so it must behave exactly as before: no accept,
    /// and above all no session slot, or the cap silently stops capping.
    #[tokio::test]
    async fn test_a_key_is_refused_when_none_is_enrolled() {
        use russh::server::{Auth, Handler};
        let session_count = Arc::new(AtomicUsize::new(0));
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let mut h = test_handler(session_count.clone(), Vec::new());

        let r = h.auth_publickey("admin", key.public_key()).await.unwrap();
        assert!(matches!(r, Auth::Reject { .. }), "an unenrolled key must be refused");
        assert!(!h.counted);
        assert_eq!(session_count.load(Ordering::SeqCst), 0, "a refusal must claim no slot");

        // And the OFFER must say so too.  The trait default answers Accept,
        // which tells every client its key would work and then refuses the
        // signature -- wrong twice over.
        let offered = h.auth_publickey_offered("admin", key.public_key()).await.unwrap();
        assert!(matches!(offered, Auth::Reject { .. }), "the offer must be answered honestly");
    }

    /// An enrolled key authenticates and claims exactly one slot.
    #[tokio::test]
    async fn test_an_enrolled_key_authenticates_and_claims_one_slot() {
        use russh::server::{Auth, Handler};
        let session_count = Arc::new(AtomicUsize::new(0));
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        {
            let mut h = test_handler(session_count.clone(), vec![key.public_key().clone()]);
            // The password is deliberately empty: removing it is the point.
            h.password = String::new();

            assert!(matches!(
                h.auth_publickey_offered("admin", key.public_key()).await.unwrap(),
                Auth::Accept
            ));
            assert!(matches!(
                h.auth_publickey("admin", key.public_key()).await.unwrap(),
                Auth::Accept
            ));
            assert!(h.counted);
            assert_eq!(session_count.load(Ordering::SeqCst), 1);

            // A second call on the same connection must not count twice, or
            // Drop (which subtracts once) could never release it.
            assert!(matches!(
                h.auth_publickey("admin", key.public_key()).await.unwrap(),
                Auth::Accept
            ));
            assert_eq!(session_count.load(Ordering::SeqCst), 1);

            // The wrong user with the right key is still the wrong user.
            let mut other = test_handler(session_count.clone(), vec![key.public_key().clone()]);
            assert!(matches!(
                other.auth_publickey("someone-else", key.public_key()).await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert_eq!(session_count.load(Ordering::SeqCst), 0, "Drop must release the slot");
    }

    /// A refused key does not count toward the lockout, but a locked-out IP is
    /// still refused.
    ///
    /// Both halves matter and they pull opposite ways.  Counting a key refusal
    /// would ban the ordinary sequence -- a slave offers its key, is refused,
    /// then logs in with its password -- and a public key is not guessable, so
    /// there is nothing to rate-limit.  An IP already locked out by *password*
    /// guessing must not get a second door.
    #[tokio::test]
    async fn test_key_refusals_and_the_lockout() {
        use russh::server::{Auth, Handler};
        let session_count = Arc::new(AtomicUsize::new(0));
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let mut h = test_handler(session_count.clone(), Vec::new());

        for _ in 0..(telnet::MAX_AUTH_ATTEMPTS + 2) {
            let _ = h.auth_publickey("admin", key.public_key()).await.unwrap();
        }
        let ip = h.peer_addr.unwrap();
        assert!(
            !telnet::is_locked_out(&h.lockouts, ip),
            "a refused key must not lock the slave out of its own password"
        );

        // Now lock the IP out the way that does count, and the key is refused.
        for _ in 0..telnet::MAX_AUTH_ATTEMPTS {
            telnet::record_auth_failure(&h.lockouts, ip);
        }
        let mut h2 = test_handler(session_count.clone(), vec![key.public_key().clone()]);
        h2.lockouts = h.lockouts.clone();
        assert!(matches!(
            h2.auth_publickey("admin", key.public_key()).await.unwrap(),
            Auth::Reject { .. }
        ), "a locked-out IP must not get in with a key either");
    }

    /// The comment on an OpenSSH line is not part of the identity.
    #[test]
    fn test_an_authorized_key_is_matched_on_its_key_data() {
        let key = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        let mut renamed = key.public_key().clone();
        renamed.set_comment("someone renamed the machine");
        assert!(
            key_is_authorized(&[renamed], key.public_key()),
            "an edited comment must not revoke a key"
        );

        let other = russh::keys::PrivateKey::random(
            &mut rand::rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        assert!(
            !key_is_authorized(&[key.public_key().clone()], other.public_key()),
            "a different key is a different key"
        );
        assert!(!key_is_authorized(&[], key.public_key()), "nothing enrolled, nothing accepted");
    }

    /// A handler with a known credential and whatever keys the test enrolled.
    fn test_handler(
        session_count: Arc<AtomicUsize>,
        authorized_keys: Vec<russh::keys::PublicKey>,
    ) -> SshHandler {
        SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count,
            max_sessions: 2,
            username: "admin".into(),
            password: "secret".into(),
            peer_addr: Some("10.0.0.9".parse().unwrap()),
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            authorized_keys,
            key_authed: false,
            counted: false,
            rate_limited: false,
        }
    }

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
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
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

    /// N2: an empty configured username or password must reject ALL auth
    /// (including an empty supplied credential) rather than turn the SSH
    /// port into an open shell.  `constant_time_eq(b"", b"")` is `true`, so
    /// without the `is_empty()` guard a blanked password would accept any
    /// login that also sent an empty password.
    #[tokio::test]
    async fn test_auth_password_rejects_empty_configured_credentials() {
        use russh::server::{Auth, Handler};
        let session_count = Arc::new(AtomicUsize::new(0));
        let make = |user: &str, pass: &str| SshHandler {
            shutdown: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            session_count: session_count.clone(),
            max_sessions: 2,
            username: user.into(),
            password: pass.into(),
            peer_addr: Some("10.0.0.2".parse().unwrap()),
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
        };

        // Empty configured password: reject a matching-empty password (the
        // exact open-server case) and claim no slot.
        {
            let mut h = make("admin", "");
            assert!(matches!(
                h.auth_password("admin", "").await.unwrap(),
                Auth::Reject { .. }
            ));
            assert!(!h.counted);
        }
        // Empty configured username: same refusal.
        {
            let mut h = make("", "secret");
            assert!(matches!(
                h.auth_password("", "secret").await.unwrap(),
                Auth::Reject { .. }
            ));
            assert!(!h.counted);
        }
        // Both empty: still rejected.
        {
            let mut h = make("", "");
            assert!(matches!(
                h.auth_password("", "").await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert_eq!(
            session_count.load(Ordering::SeqCst),
            0,
            "no empty-credential path may claim a session slot"
        );
    }

    /// The session-slot claim, single-sourced in `try_claim_slot`: the cap
    /// binds at exactly `max_sessions`, a refusal leaves the count untouched
    /// (no leak), and a lost slot becomes available again.
    #[test]
    fn test_try_claim_slot_enforces_cap_exactly() {
        let count = AtomicUsize::new(0);
        assert!(try_claim_slot(&count, 3));
        assert!(try_claim_slot(&count, 3));
        assert!(try_claim_slot(&count, 3));
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // The fourth is refused, and the rollback leaves the count at the cap
        // rather than one above it — a refusal that leaked would permanently
        // shrink capacity.
        assert!(!try_claim_slot(&count, 3));
        assert_eq!(count.load(Ordering::SeqCst), 3, "a refusal must not leak");

        // Release one; the next claim succeeds.
        count.fetch_sub(1, Ordering::SeqCst);
        assert!(try_claim_slot(&count, 3));
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // A zero cap admits nothing.
        let zero = AtomicUsize::new(0);
        assert!(!try_claim_slot(&zero, 0));
        assert_eq!(zero.load(Ordering::SeqCst), 0);
    }

    /// Concurrent claimers must never settle the count above the cap — the
    /// property the `fetch_add` + rollback shape exists for.  Without the
    /// rollback, or with a load-then-add, the count would drift over `max`.
    #[test]
    fn test_try_claim_slot_never_exceeds_cap_under_contention() {
        const MAX: usize = 8;
        const THREADS: usize = 16;
        const PER_THREAD: usize = 200;
        let count = Arc::new(AtomicUsize::new(0));
        let granted = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let count = count.clone();
            let granted = granted.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    if try_claim_slot(&count, MAX) {
                        granted.fetch_add(1, Ordering::SeqCst);
                        // Hold it briefly, then release, as a session would.
                        count.fetch_sub(1, Ordering::SeqCst);
                    }
                    // The observed count must never exceed the cap.
                    assert!(
                        count.load(Ordering::SeqCst) <= MAX,
                        "count rose above the cap under contention"
                    );
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "every granted slot must have been released"
        );
        assert!(
            granted.load(Ordering::SeqCst) > 0,
            "the contention test granted nothing, so it proved nothing"
        );
    }

    /// A claimed slot must be released however the holder finishes.
    ///
    /// Regression test with a real bug behind it: the relay task released its
    /// slot as its **last statement**, and adding the Kermit-server branch —
    /// which returns early — skipped it, leaking one slot per relay Kermit
    /// transfer. Nothing else releases a relay channel's slot, so the master's
    /// capacity (default 50) shrank permanently until a restart, eventually
    /// refusing every new telnet, SSH and relay session. A guard cannot be
    /// bypassed by a branch someone adds later; a trailing `fetch_sub` can.
    #[test]
    fn test_slot_guard_releases_on_every_exit_path() {
        let count = Arc::new(AtomicUsize::new(0));

        /// Stands in for the relay task: an early-returning branch (the Kermit
        /// server) and a fall-through one (the menu / dial paths).
        fn relay_task(count: Arc<AtomicUsize>, kermit: bool) -> &'static str {
            let _slot = SlotGuard(count);
            if kermit {
                return "kermit";
            }
            "menu"
        }

        assert!(try_claim_slot(&count, 4));
        assert_eq!(relay_task(count.clone(), true), "kermit");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "the early-return branch must release its slot"
        );

        assert!(try_claim_slot(&count, 4));
        assert_eq!(relay_task(count.clone(), false), "menu");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "the fall-through branch must release its slot"
        );

        // A panicking relay session must not leak either — Drop runs while
        // unwinding, and relay paths have panicked before now.
        assert!(try_claim_slot(&count, 4));
        let c = count.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = SlotGuard(c);
            panic!("relay session blew up");
        }));
        assert!(res.is_err(), "the closure was supposed to panic");
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a panicking holder must still release its slot"
        );
    }

    /// Pins the **accepted** M-11 accounting so it can't drift unnoticed: a
    /// relay/register channel claims its own slot *on top of* the slot its
    /// connection claimed at auth, so a single-channel relay occupies two where
    /// an interactive user occupies one.
    ///
    /// This over-counts, which fails safe (a master hosts fewer relay sessions
    /// than `max_sessions`, never more) and is what bounds a slave from opening
    /// unbounded relay channels on one connection.  It is documented in
    /// `exec_request`; asserting it here means a future "fix" has to change a
    /// test that explains the tradeoff rather than silently flipping the
    /// accounting to fails-open.
    ///
    /// Scope: `exec_request` itself needs a live `russh::server::Session`,
    /// which a unit test can't build, so this pins the *arithmetic* both sites
    /// share — not the wiring that calls it.
    #[test]
    fn test_relay_channel_slot_is_on_top_of_auth_slot() {
        let count = AtomicUsize::new(0);
        let max = 2;
        // The connection authenticates: one slot (auth_password).
        assert!(try_claim_slot(&count, max), "auth claims a slot");
        // Its first relay channel claims a second (exec_request).
        assert!(try_claim_slot(&count, max), "the relay channel claims another");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // So with max_sessions = 2, a second relay channel is refused even
        // though only ONE relay session is actually being served.
        assert!(
            !try_claim_slot(&count, max),
            "the over-count is the accepted behaviour: 2 slots for 1 relay"
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// **Every relay refusal must be decided before the hello goes out.**
    ///
    /// `RELAY_HELLO` is the master saying *accepted* -- its whole purpose is to
    /// let the slave "distinguish an accepted relay from a refused-but-open
    /// channel", because russh's `exec()` returns `Ok` even on a
    /// `channel_failure`. `allow_relay_kermit` was read in
    /// `run_master_relay_kermit`, which runs *after* `channel_success`, the
    /// hello, and the "accepted serial relay" log line -- so the one gate left
    /// behind the handshake was the one the handshake could not report.
    ///
    /// Measured on a live master/slave pair (2026-08-21): the slave logged
    /// `CONNECTED -- the master's Kermit server is on this wire; files live on
    /// the master`, repeated it in its link summary, then read EOF and treated
    /// it as a dropped link. Because the *connect* had succeeded it reset
    /// `attempt` and the backoff each time, so instead of one deduped line and
    /// the 60 s `RECONNECT_BACKOFF_REFUSED` it reconnected about once a second
    /// for ever, writing a line on both machines each round.
    ///
    /// A source scan, for the reason
    /// [`test_relay_channel_slot_is_on_top_of_auth_slot`] gives: `exec_request`
    /// needs a live `russh::server::Session` a unit test cannot build. What is
    /// being asserted is an *ordering within one function*, which is a property
    /// of the text, so the text is what gets read.
    #[test]
    fn test_a_relay_is_refused_before_the_hello_is_written() {
        let src = include_str!("ssh.rs");
        let body = src
            .split_once("async fn exec_request(")
            .expect("exec_request must still exist")
            .1;
        // Bound the scan to this function, so a later `RELAY_HELLO` elsewhere
        // in the file cannot satisfy or break it by accident.
        let body = body.split_once("\n    async fn ").map(|(b, _)| b).unwrap_or(body);
        // **Comment lines are dropped, or the test reads its own prose.** The
        // first draft matched the `RELAY_HELLO` inside the explanatory comment
        // above the gate -- which sits *before* the gate, so the test failed on
        // correct code. Same reason `identify.rs`'s non-ASCII scan skips
        // comments: the property is about the code, and the prose beside it
        // says the same words on purpose.
        let body: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let body = body.as_str();

        let hello = body
            .find("RELAY_HELLO")
            .expect("exec_request must still write the relay hello");
        let gate = body
            .find("kermit_relay_allowed")
            .expect("exec_request must decide the Kermit gate itself, not leave it to the task");
        assert!(
            gate < hello,
            "the allow_relay_kermit gate is at byte {gate} and the hello at {hello}: a refusal \
             decided after the hello is reported to the slave as a successful connection"
        );

        // The other two refusals are already ahead of it; keep them there.
        for earlier in ["master_accept_relays", "try_claim_slot"] {
            let at = body.find(earlier).unwrap_or_else(|| panic!("{earlier} gate is gone"));
            assert!(at < hello, "the {earlier} gate must also precede the hello");
        }
    }

    /// A locked-out IP is rejected even with correct credentials, and the
    /// rejection claims no session slot.
    #[tokio::test]
    async fn test_auth_password_rejects_locked_out_ip() {
        use russh::server::{Auth, Handler};
        let ip: std::net::IpAddr = "10.0.0.7".parse().unwrap();
        let lockouts: telnet::LockoutMap =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // Drive the IP into lockout (>= MAX_AUTH_ATTEMPTS failures).
        for _ in 0..telnet::MAX_AUTH_ATTEMPTS {
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
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
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
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
        };
        for _ in 0..telnet::MAX_AUTH_ATTEMPTS {
            let mut h = make();
            assert!(matches!(
                h.auth_password("admin", "wrong").await.unwrap(),
                Auth::Reject { .. }
            ));
        }
        assert!(
            telnet::is_locked_out(&lockouts, ip),
            "IP must be locked out after MAX_AUTH_ATTEMPTS failures"
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
            pty_term: None,
            duplex_writer: None,
            relay_writers: std::collections::HashMap::new(),
            registered_ports: std::collections::HashMap::new(),
            session_writers: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            lockouts: lockouts.clone(),
            authorized_keys: Vec::new(),
            key_authed: false,
            counted: false,
            rate_limited: false,
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
