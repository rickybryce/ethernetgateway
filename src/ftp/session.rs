//! One FTP control connection: login, the command loop, data connections and
//! transfers.
//!
//! **One writer.**  Only the control loop writes to the control connection.  A
//! transfer runs as its own task (so ABOR, STAT and NOOP are answered while it
//! runs) and hands its final reply back through its `JoinHandle`, which the
//! loop polls beside the socket.  Commands other than ABOR/STAT/NOOP that
//! arrive mid-transfer wait in the queue until it ends -- RFC 959 lets a server
//! hold them, and no client pipelines past a transfer anyway.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use super::proto::{self, Eprt, Fed, LineReader};
use super::backend::{Sink, Source, Store};
use super::store::{LocalStore, StoreError, WriteMode};
use crate::config;
use crate::glog;
use crate::telnet::LockoutMap;

/// How long a passive listener waits for the client's data connection, and an
/// active connect for the client to answer.
const DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A data connection that moves no bytes for this long is given up.  Not the
/// idle timeout: that is for an idle *session*, and may be off.
pub(super) const DATA_STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// A reply the client won't read for this long ends the session.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// A connection must log in within this long of arriving (or of REIN, or of a
/// USER that ended its login).  Commands do not extend it: before login, NOOP
/// would otherwise hold a `max_sessions` slot for ever, and fifty such
/// connections would shut every real client out.  Long enough for someone
/// typing USER and PASS by hand into a telnet window.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Names that are anonymous logins.  With security on they are refused at
/// once and *not* counted against the address: browsers, curl and FileZilla
/// try them unprompted, and no credential was guessed.
fn is_anonymous_name(user: &str) -> bool {
    user.eq_ignore_ascii_case("anonymous") || user.eq_ignore_ascii_case("ftp")
}

/// Commands that work before login (RFC 959 §5.4 and the extensions).
fn allowed_before_login(verb: &str) -> bool {
    matches!(
        verb,
        "USER" | "PASS" | "ACCT" | "QUIT" | "NOOP" | "HELP" | "FEAT" | "SYST" | "OPTS" | "HOST"
            | "LANG" | "AUTH" | "ADAT" | "PBSZ" | "PROT" | "CCC" | "REIN" | "STAT"
    )
}

/// Queued in place of a line too long to take, so its `500` goes out in
/// command order rather than ahead of replies still owed (RFC 959 §4.2).
const TOO_LONG: &str = "\u{1}too-long";

/// Every verb we answer, for HELP.  Order is HELP's.
const HELP_VERBS: &[&str] = &[
    "ABOR ACCT ALLO APPE CDUP CWD  DELE EPRT EPSV FEAT HELP HOST",
    "LANG LIST MDTM MKD  MLSD MLST MODE NLST NOOP OPTS PASS PASV",
    "PORT PWD  QUIT REIN REST RETR RMD  RNFR RNTO SITE SIZE SMNT",
    "STAT STOR STOU STRU SYST TYPE USER XCUP XCWD XMKD XPWD XRMD",
];

/// Where the next data connection comes from.
enum DataPlan {
    Passive(TcpListener),
    Active(SocketAddr),
}

/// What a transfer does once its data connection is open.
enum Job {
    /// A listing, already built.
    Send(Vec<u8>),
    Retr(Source),
    Stor(Sink),
}

/// The control loop's view of a running transfer.
struct Transfer {
    handle: tokio::task::JoinHandle<Done>,
    bytes: Arc<AtomicU64>,
    what: String,
    /// An upload's data has all arrived and it is being saved.  ABOR then
    /// waits for the save instead of cancelling it: a rename (or, on a slave,
    /// the master's commit) already under way finishes regardless, and the
    /// client must not be told `426` about a file that was saved.
    committing: Arc<AtomicBool>,
}

/// A finished transfer's reply and log line.
struct Done {
    code: u16,
    text: String,
    log: Option<String>,
}

/// What the control loop does after a command.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

pub(super) struct Session {
    pub(super) ctrl_r: OwnedReadHalf,
    pub(super) ctrl_w: OwnedWriteHalf,
    pub(super) peer: SocketAddr,
    pub(super) local: SocketAddr,
    pub(super) lockouts: LockoutMap,
    pub(super) shutdown: Arc<AtomicBool>,
    pub(super) notify: Arc<tokio::sync::Notify>,
    pub(super) store: Store,
    reader: LineReader,
    user: Option<String>,
    logged_in: bool,
    /// When the current wait for a login began, and how long it may last:
    /// see [`LOGIN_TIMEOUT`] (shortened by tests).
    login_started: tokio::time::Instant,
    pub(super) login_timeout: Duration,
    cwd: String,
    rest: u64,
    rnfr: Option<String>,
    data: Option<DataPlan>,
    epsv_all: bool,
    /// TYPE A was asked for.  Files go unchanged either way; STAT says which.
    type_ascii: bool,
    transfer: Option<Transfer>,
}

impl Session {
    pub(super) fn new(
        stream: TcpStream,
        peer: SocketAddr,
        lockouts: LockoutMap,
        shutdown: Arc<AtomicBool>,
        notify: Arc<tokio::sync::Notify>,
    ) -> std::io::Result<Session> {
        let local = stream.local_addr()?;
        let _ = stream.set_nodelay(true);
        let (ctrl_r, ctrl_w) = stream.into_split();
        Ok(Session {
            ctrl_r,
            ctrl_w,
            peer,
            local,
            lockouts,
            shutdown,
            notify,
            store: Store::Local(LocalStore),
            reader: LineReader::default(),
            user: None,
            logged_in: false,
            login_started: tokio::time::Instant::now(),
            login_timeout: LOGIN_TIMEOUT,
            cwd: "/".into(),
            rest: 0,
            rnfr: None,
            data: None,
            epsv_all: false,
            type_ascii: false,
            transfer: None,
        })
    }

    async fn send_raw(&mut self, bytes: &[u8]) -> bool {
        matches!(
            tokio::time::timeout(REPLY_TIMEOUT, self.ctrl_w.write_all(bytes)).await,
            Ok(Ok(()))
        )
    }

    async fn reply(&mut self, code: u16, text: &str) -> bool {
        self.send_raw(&proto::reply(code, &[text])).await
    }

    async fn reply_lines(&mut self, code: u16, lines: &[&str]) -> bool {
        self.send_raw(&proto::reply(code, lines)).await
    }

    fn verbose() -> bool {
        config::get_config().verbose
    }

    /// Run the session to its end.
    pub(super) async fn run(mut self) {
        if !self.reply(220, "Ethernet Gateway FTP server ready.").await {
            return;
        }
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut buf = [0u8; 2048];
        let mut last_activity = tokio::time::Instant::now();
        loop {
            // ABOR is urgent (RFC 959 §4.1.3): during a transfer it is taken
            // from wherever it sits in the queue, not only from the front --
            // a pipelining client may have queued the next PASV ahead of it.
            // Only when something ahead of it is blocked: a NOOP or STAT in
            // front runs anyway, and jumping it would answer out of order.
            if self.transfer.is_some()
                && let Some(i) = queue.iter().position(|l| proto::split_command(l).0 == "ABOR")
                && queue.iter().take(i).any(|l| !matches!(proto::split_command(l).0.as_str(), "STAT" | "NOOP"))
            {
                let line = queue.remove(i).unwrap_or_default();
                if self.command(&line).await == Flow::Close {
                    return;
                }
                last_activity = tokio::time::Instant::now();
            }
            // Run what can run now.
            while let Some(line) = queue.front() {
                let (verb, _) = proto::split_command(line);
                if self.transfer.is_some() && !matches!(verb.as_str(), "ABOR" | "STAT" | "NOOP") {
                    break;
                }
                let line = queue.pop_front().unwrap_or_default();
                if self.command(&line).await == Flow::Close {
                    self.abort_transfer();
                    return;
                }
                last_activity = tokio::time::Instant::now();
            }
            if self.shutdown.load(Ordering::SeqCst) {
                self.abort_transfer();
                let _ = self.reply(421, "Service closing control connection.").await;
                return;
            }
            let idle = config::get_config().idle_timeout_secs;
            let idle_deadline = (idle > 0 && self.transfer.is_none())
                .then(|| last_activity + Duration::from_secs(idle));
            let login_deadline = (!self.logged_in).then(|| self.login_started + self.login_timeout);

            enum Event {
                Read(std::io::Result<usize>),
                TransferDone(Done),
                Idle,
                LoginTimeout,
                Tick,
            }
            let event = {
                let ctrl_r = &mut self.ctrl_r;
                let transfer = &mut self.transfer;
                let notify = self.notify.clone();
                tokio::select! {
                    r = ctrl_r.read(&mut buf) => Event::Read(r),
                    done = async {
                        match transfer {
                            Some(t) => (&mut t.handle).await.unwrap_or(Done {
                                code: 451,
                                text: "Transfer failed.".into(),
                                log: None,
                            }),
                            None => std::future::pending().await,
                        }
                    } => Event::TransferDone(done),
                    _ = async {
                        match idle_deadline {
                            Some(d) => tokio::time::sleep_until(d).await,
                            None => std::future::pending().await,
                        }
                    } => Event::Idle,
                    _ = async {
                        match login_deadline {
                            Some(d) => tokio::time::sleep_until(d).await,
                            None => std::future::pending().await,
                        }
                    } => Event::LoginTimeout,
                    _ = notify.notified() => Event::Tick,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => Event::Tick,
                }
            };
            match event {
                Event::Read(Ok(0)) | Event::Read(Err(_)) => {
                    self.abort_transfer();
                    return;
                }
                Event::Read(Ok(n)) => {
                    for &b in &buf[..n] {
                        match self.reader.feed(b) {
                            Fed::Nothing => {}
                            Fed::Line(l) => queue.push_back(l),
                            Fed::TooLong => queue.push_back(TOO_LONG.to_string()),
                            Fed::Reply(r) => {
                                if !self.send_raw(&r).await {
                                    return;
                                }
                            }
                        }
                    }
                    // A peer that only ever sends one unterminated line is
                    // bounded by the reader's cap; one sending lines faster
                    // than they can run is bounded here.
                    if queue.len() > 64 {
                        let _ = self.reply(421, "Too many commands queued.").await;
                        self.abort_transfer();
                        return;
                    }
                }
                Event::TransferDone(done) => {
                    self.transfer = None;
                    if let Some(l) = &done.log {
                        glog!("FTP: {} {}", self.peer, l);
                    }
                    if !self.reply(done.code, &done.text).await {
                        return;
                    }
                    last_activity = tokio::time::Instant::now();
                }
                Event::Idle => {
                    let _ = self.reply(421, "Idle timeout; closing control connection.").await;
                    return;
                }
                Event::LoginTimeout => {
                    glog!("FTP: {} did not log in within {}s", self.peer, self.login_timeout.as_secs());
                    let _ = self.reply(421, "Login timed out; closing control connection.").await;
                    return;
                }
                Event::Tick => {}
            }
        }
    }

    fn abort_transfer(&mut self) {
        if let Some(t) = self.transfer.take() {
            // Dropping the task drops its `Upload`, which removes a Replace
            // upload's temporary file.
            t.handle.abort();
        }
    }
}

/// **However the session ends, its transfer ends with it.**  A dropped
/// `JoinHandle` does not cancel its task, so a session that returned early
/// (a reply it could not write, a lost control connection mid-command) left
/// its transfer running on -- holding the file's write claim, and on a slave
/// the master channel, until the data connection timed out.
impl Drop for Session {
    fn drop(&mut self) {
        self.abort_transfer();
    }
}

impl Session {

    /// Handle one command line.
    async fn command(&mut self, line: &str) -> Flow {
        if line == TOO_LONG {
            return Self::flow(self.reply(500, "Command line too long.").await);
        }
        let (verb, mut arg) = proto::split_command(line);
        if verb == "PASS" {
            // The password exactly as sent: a space at either end is part of
            // it (telnet and SSH take it that way), and trimming it here
            // would make such a password fail -- counting toward the lockout
            // shared with telnet, SSH and the web page.
            arg = proto::raw_argument(line).to_string();
        }
        if verb.is_empty() {
            // Someone pressing Enter in a telnet window: say so rather than
            // leave them waiting for a reply that never comes.
            return Self::flow(self.reply(500, "Empty command.").await);
        }
        if Self::verbose() {
            // Never the password, verbose or not.
            let shown = if verb == "PASS" { "PASS ****".to_string() } else { line.to_string() };
            glog!("FTP: {} > {}", self.peer, shown);
        }
        // Single-use state: RNFR applies only to the RNTO right after it; REST
        // to the next transfer.  RFC 959 wants REST immediately before the
        // transfer, but a client that sets up the data connection or the type
        // in between still means it, so those commands let it through.  Any
        // other command clears both silently (FileZilla probes with `REST 0`
        // and then does other things).
        let keeps_rest = matches!(
            verb.as_str(),
            "PASV" | "EPSV" | "PORT" | "EPRT" | "TYPE" | "MODE" | "STRU" | "NOOP"
        );
        let rest = if keeps_rest { 0 } else { std::mem::take(&mut self.rest) };
        let rnfr = self.rnfr.take();

        if !self.logged_in && !allowed_before_login(&verb) {
            return Self::flow(self.reply(530, "Please log in with USER and PASS.").await);
        }

        let ok = match verb.as_str() {
            "USER" => return self.cmd_user(&arg).await,
            "PASS" => return self.cmd_pass(&arg).await,
            "QUIT" => {
                let _ = self.reply(221, "Goodbye.").await;
                return Flow::Close;
            }
            "REIN" => {
                self.abort_transfer();
                self.user = None;
                self.logged_in = false;
                self.login_started = tokio::time::Instant::now();
                self.cwd = "/".into();
                self.data = None;
                self.epsv_all = false;
                self.type_ascii = false;
                self.reply(220, "Service ready for new user.").await
            }
            "ACCT" | "ALLO" => self.reply(202, "Not needed here.").await,
            "NOOP" => self.reply(200, "OK.").await,
            "SYST" => self.reply(215, "UNIX Type: L8").await,
            "FEAT" => {
                self.reply_lines(
                    211,
                    &[
                        "Extensions supported:",
                        "EPRT",
                        "EPSV",
                        "HOST",
                        "LANG EN*",
                        "MDTM",
                        "MLST type*;size*;modify*;perm*;",
                        "PASV",
                        "REST STREAM",
                        "SIZE",
                        "TVFS",
                        "UTF8",
                        "End",
                    ],
                )
                .await
            }
            "OPTS" => self.cmd_opts(&arg).await,
            "LANG" => {
                if arg.is_empty() || arg.eq_ignore_ascii_case("EN") || arg.to_ascii_uppercase().starts_with("EN-") {
                    self.reply(200, "Language is EN.").await
                } else {
                    self.reply(504, "Only EN is available.").await
                }
            }
            "HOST" => {
                // RFC 7151 §3: HOST comes before USER, not merely before PASS.
                if self.logged_in || self.user.is_some() {
                    self.reply(503, "HOST must come before login.").await
                } else if arg.is_empty() {
                    self.reply(501, "HOST needs a host name.").await
                } else {
                    // One virtual host: any name reaches it.
                    self.reply(220, "Host accepted.").await
                }
            }
            "HELP" => self.cmd_help(&arg).await,
            "AUTH" | "ADAT" | "PBSZ" | "PROT" | "CCC" | "MIC" | "CONF" | "ENC" => {
                self.reply(502, "Security extensions are not available; use plain FTP.").await
            }
            "LPRT" | "LPSV" | "SMNT" => self.reply(502, "Command not implemented.").await,
            "SITE" => {
                if arg.trim().eq_ignore_ascii_case("HELP") {
                    self.reply(214, "No SITE commands are available.").await
                } else {
                    self.reply(504, "SITE command not supported.").await
                }
            }
            "PWD" | "XPWD" => {
                let text = format!("{} is the current directory.", proto::quote_path(&self.cwd));
                self.reply(257, &text).await
            }
            "CWD" | "XCWD" => self.cmd_cwd(&arg).await,
            "CDUP" | "XCUP" => self.cmd_cwd("..").await,
            "TYPE" => self.cmd_type(&arg).await,
            "MODE" => match arg.trim().to_ascii_uppercase().as_str() {
                "S" => self.reply(200, "Mode S.").await,
                "B" | "C" => self.reply(504, "Only stream mode is available.").await,
                _ => self.reply(501, "MODE takes S, B or C.").await,
            },
            "STRU" => match arg.trim().to_ascii_uppercase().as_str() {
                // A record is a line on a byte-stream file system.
                "F" | "R" => self.reply(200, "Structure set.").await,
                "P" => self.reply(504, "Page structure is not available.").await,
                _ => self.reply(501, "STRU takes F, R or P.").await,
            },
            "PASV" => self.cmd_pasv(false).await,
            "EPSV" => self.cmd_epsv(&arg).await,
            "PORT" => self.cmd_port(&arg).await,
            "EPRT" => self.cmd_eprt(&arg).await,
            "REST" => match arg.trim().parse::<u64>() {
                Ok(n) => {
                    self.rest = n;
                    self.reply(350, &format!("Restarting at {n}. Send RETR, STOR or APPE.")).await
                }
                Err(_) => self.reply(501, "REST needs a byte offset.").await,
            },
            "LIST" | "NLST" => self.cmd_list(&verb, &arg).await,
            "MLSD" => self.cmd_mlsd(&arg).await,
            "MLST" => self.cmd_mlst(&arg).await,
            "STAT" => self.cmd_stat(&arg).await,
            "RETR" => self.cmd_retr(&arg, rest).await,
            "STOR" => {
                let mode = if rest > 0 { WriteMode::At(rest) } else { WriteMode::Replace };
                self.cmd_store(&arg, mode).await
            }
            "APPE" => self.cmd_store(&arg, WriteMode::Append).await,
            "STOU" => self.cmd_stou(&arg).await,
            "SIZE" => self.cmd_size(&arg).await,
            "MDTM" => self.cmd_mdtm(&arg).await,
            "DELE" => self.simple(&arg, "DELE").await,
            "MKD" | "XMKD" => self.simple(&arg, "MKD").await,
            "RMD" | "XRMD" => self.simple(&arg, "RMD").await,
            "RNFR" => self.cmd_rnfr(&arg).await,
            "RNTO" => self.cmd_rnto(&arg, rnfr).await,
            "ABOR" => self.cmd_abor().await,
            _ => self.reply(500, "Command not recognized.").await,
        };
        Self::flow(ok)
    }

    fn flow(ok: bool) -> Flow {
        if ok { Flow::Continue } else { Flow::Close }
    }

    async fn cmd_user(&mut self, arg: &str) -> Flow {
        let name = arg.trim().to_string();
        if name.is_empty() {
            return Self::flow(self.reply(501, "USER needs a name.").await);
        }
        // A new USER starts the login over (RFC 959 allows changing user).
        if self.logged_in {
            self.login_started = tokio::time::Instant::now();
        }
        self.logged_in = false;
        let cfg = config::get_config();
        if cfg.security_enabled
            && is_anonymous_name(&name)
            && !crate::credential::username_matches(&name, &cfg.username)
        {
            self.user = None;
            return Self::flow(self.reply(530, "Anonymous login is not allowed here.").await);
        }
        self.user = Some(name);
        let text = if cfg.security_enabled {
            "Password required."
        } else {
            "Anonymous access; any password will do."
        };
        Self::flow(self.reply(331, text).await)
    }

    /// PASS: the same sequence as the telnet login -- the lockout checked on
    /// arrival and again inside this address's turn, the KDF off the runtime,
    /// a failure counted before the turn is given back.
    async fn cmd_pass(&mut self, arg: &str) -> Flow {
        let Some(user) = self.user.clone() else {
            return Self::flow(self.reply(503, "Send USER first.").await);
        };
        if self.logged_in {
            return Self::flow(self.reply(230, "Already logged in.").await);
        }
        let cfg = config::get_config();

        if !cfg.security_enabled {
            // The address rule is applied at accept only while security is
            // off.  A connection accepted while it was on, and kept open
            // until it was switched off, must meet it now -- or an outside
            // address would get in anonymously.
            if !cfg.disable_ip_safety
                && let Some(reason) =
                    crate::telnet::reject_insecure_ip(self.peer.ip(), cfg.disable_gateway_connections)
            {
                glog!("FTP: {} refused at login ({})", self.peer, reason);
                let _ = self.reply(421, &reason).await;
                return Flow::Close;
            }
            if !self.attach_store().await {
                return Flow::Close;
            }
            self.logged_in = true;
            glog!("FTP: {} logged in anonymously (as {:?})", self.peer, user);
            return Self::flow(self.reply(230, "Logged in.").await);
        }
        let ip = self.peer.ip();
        if crate::telnet::is_locked_out(&self.lockouts, ip) {
            glog!("FTP: login refused for {} (locked out)", ip);
            let _ = self.reply(421, "Too many failed logins; try again later.").await;
            return Flow::Close;
        }
        let turn = crate::telnet::auth_turn(ip).await;
        if crate::telnet::is_locked_out(&self.lockouts, ip) {
            drop(turn);
            glog!("FTP: login refused for {} (locked out mid-login)", ip);
            let _ = self.reply(421, "Too many failed logins; try again later.").await;
            return Flow::Close;
        }
        // Both compared before combining, so the time taken says nothing about
        // which half was wrong (as telnet and SSH do).
        let user_ok = crate::credential::username_matches(&user, &cfg.username);
        let pass_ok = crate::credential::verify_any_off_runtime(&cfg.password, arg).await;
        if user_ok && pass_ok {
            crate::telnet::clear_lockout(&self.lockouts, ip);
            drop(turn);
            if !self.attach_store().await {
                return Flow::Close;
            }
            self.logged_in = true;
            glog!("FTP: {} logged in", self.peer);
            return Self::flow(self.reply(230, "Logged in.").await);
        }
        let count = crate::telnet::record_auth_failure(&self.lockouts, ip);
        drop(turn);
        self.user = None;
        if count >= crate::telnet::MAX_AUTH_ATTEMPTS {
            glog!("FTP: {} locked out after {} failures", ip, count);
            let _ = self.reply(421, "Too many failed logins; try again later.").await;
            return Flow::Close;
        }
        glog!("FTP: failed login from {}", ip);
        // As telnet does: a second between guesses, after the turn is given
        // back (so it delays only this connection, never the address's other
        // logins).
        tokio::time::sleep(Duration::from_secs(1)).await;
        Self::flow(self.reply(530, "Login incorrect.").await)
    }

    /// Choose this session's store at login.  On a slave every file operation
    /// belongs on the master: open the channel to it now, and refuse the login
    /// (`421`) if the master can't be reached -- never fall back to the
    /// slave's own folder, which would put files where nobody looks for them.
    /// Anywhere else, the gateway's own transfer directory.
    async fn attach_store(&mut self) -> bool {
        if config::get_config().gateway_role != "slave" {
            self.store = Store::Local(LocalStore);
            return true;
        }
        if matches!(self.store, Store::Remote(_)) {
            return true;
        }
        match super::remote::RemoteStore::connect().await {
            Ok(r) => {
                self.store = Store::Remote(r);
                true
            }
            Err(why) => {
                glog!("FTP: {} refused: {}", self.peer, why);
                let _ = self.reply(421, &format!("Cannot reach the files: {why}.")).await;
                false
            }
        }
    }

    async fn cmd_opts(&mut self, arg: &str) -> bool {
        let (opt, rest) = proto::split_command(arg);
        match opt.as_str() {
            // Paths are always UTF-8 here; both answers are true.
            "UTF8" | "UTF-8" => self.reply(200, "UTF-8 is always on.").await,
            "MLST" => {
                // We always send all four facts; say which of the asked-for
                // ones are known, as RFC 3659 §7.9 asks.
                let known: Vec<&str> = rest
                    .split(';')
                    .map(str::trim)
                    .filter(|f| ["type", "size", "modify", "perm"].iter().any(|k| f.eq_ignore_ascii_case(k)))
                    .collect();
                let text = format!("MLST OPTS {}", known.iter().map(|f| format!("{f};")).collect::<String>());
                self.reply(200, &text).await
            }
            _ => self.reply(501, "Option not recognized.").await,
        }
    }

    async fn cmd_help(&mut self, arg: &str) -> bool {
        if arg.trim().is_empty() {
            let mut lines = vec!["The following commands are recognized:"];
            lines.extend_from_slice(HELP_VERBS);
            lines.push("Help OK.");
            self.reply_lines(214, &lines).await
        } else {
            let verb = arg.trim().to_ascii_uppercase();
            let known = HELP_VERBS.iter().any(|l| l.split_whitespace().any(|v| v == verb));
            if known {
                self.reply(214, &format!("{verb} is recognized; see RFC 959 and its extensions.")).await
            } else {
                self.reply(502, &format!("{verb} is not recognized.")).await
            }
        }
    }

    fn path_arg(&self, arg: &str) -> String {
        proto::resolve(&self.cwd, arg.trim_end_matches(['\r', '\n']))
    }

    async fn cmd_cwd(&mut self, arg: &str) -> bool {
        if arg.trim().is_empty() {
            return self.reply(501, "CWD needs a directory.").await;
        }
        let target = self.path_arg(arg);
        match self.store.stat(&target).await {
            Ok(e) if e.is_dir => {
                self.cwd = target;
                self.reply(250, "Directory changed.").await
            }
            Ok(_) => self.reply(550, "Not a directory.").await,
            Err(e) => self.store_error(e).await,
        }
    }

    async fn store_error(&mut self, e: StoreError) -> bool {
        let (code, text) = e.reply();
        self.reply(code, &text).await
    }

    async fn cmd_type(&mut self, arg: &str) -> bool {
        let words: Vec<String> = arg.split_whitespace().map(|w| w.to_ascii_uppercase()).collect();
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        // TYPE A is accepted and nothing is translated: the transfer folder is
        // a byte-exact store for vintage machines (CP/M text keeps its CRLFs,
        // a C64 file its CRs) -- see .claude/design/ftp.md.
        match words.as_slice() {
            ["A"] | ["A", "N"] => {
                self.type_ascii = true;
                self.reply(200, "Type set to A (files are sent unchanged).").await
            }
            ["I"] | ["L", "8"] => {
                self.type_ascii = false;
                self.reply(200, "Type set to I.").await
            }
            ["A", "T"] | ["A", "C"] => self.reply(504, "Only non-print format is available.").await,
            ["E"] | ["E", _] => self.reply(504, "EBCDIC is not available.").await,
            ["L", _] => self.reply(504, "Only 8-bit local bytes are available.").await,
            _ => self.reply(501, "TYPE takes A, I, E or L 8.").await,
        }
    }

    /// The address passive replies name and passive listeners bind: the one
    /// the client reached us on.
    fn passive_ip(&self) -> Option<std::net::Ipv4Addr> {
        proto::as_v4(self.local.ip())
    }

    async fn bind_passive(ip: std::net::Ipv4Addr) -> std::io::Result<TcpListener> {
        let range = config::parse_passive_ports(&config::get_config().ftp_passive_ports).flatten();
        let Some(range) = range else {
            return TcpListener::bind((ip, 0)).await;
        };
        let (lo, hi) = (*range.start(), *range.end());
        let span = u32::from(hi - lo) + 1;
        // A start that varies, so concurrent sessions don't all queue on `lo`.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for i in 0..span {
            let port = lo + ((seed.wrapping_add(i)) % span) as u16;
            if let Ok(l) = TcpListener::bind((ip, port)).await {
                return Ok(l);
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "passive range exhausted"))
    }

    async fn cmd_pasv(&mut self, _extended: bool) -> bool {
        if self.epsv_all {
            return self.reply(501, "EPSV ALL is in effect.").await;
        }
        let Some(ip) = self.passive_ip() else {
            return self.reply(425, "PASV needs IPv4; use EPSV.").await;
        };
        match Self::bind_passive(ip).await {
            Ok(l) => {
                let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                self.data = Some(DataPlan::Passive(l));
                self.reply(227, &proto::pasv_text(ip, port)).await
            }
            Err(_) => self.reply(425, "No passive port is free.").await,
        }
    }

    async fn cmd_epsv(&mut self, arg: &str) -> bool {
        let arg = arg.trim();
        if arg.eq_ignore_ascii_case("ALL") {
            self.epsv_all = true;
            return self.reply(200, "EPSV ALL accepted.").await;
        }
        if !arg.is_empty() && arg != "1" {
            return self.reply(522, "Network protocol not supported, use (1)").await;
        }
        let Some(ip) = self.passive_ip() else {
            return self.reply(425, "No passive address.").await;
        };
        match Self::bind_passive(ip).await {
            Ok(l) => {
                let port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                self.data = Some(DataPlan::Passive(l));
                self.reply(229, &format!("Entering Extended Passive Mode (|||{port}|)")).await
            }
            Err(_) => self.reply(425, "No passive port is free.").await,
        }
    }

    async fn set_active(&mut self, target: SocketAddr) -> bool {
        if !proto::active_target_allowed(target, self.peer.ip()) {
            glog!("FTP: {} refused active-mode target {} (bounce)", self.peer, target);
            return self.reply(500, "Illegal PORT: only your own address, port 1024 or above.").await;
        }
        self.data = Some(DataPlan::Active(target));
        self.reply(200, "Active mode set.").await
    }

    async fn cmd_port(&mut self, arg: &str) -> bool {
        if self.epsv_all {
            return self.reply(501, "EPSV ALL is in effect.").await;
        }
        match proto::parse_port(arg) {
            Some(t) => self.set_active(t).await,
            None => self.reply(501, "PORT takes h1,h2,h3,h4,p1,p2.").await,
        }
    }

    async fn cmd_eprt(&mut self, arg: &str) -> bool {
        if self.epsv_all {
            return self.reply(501, "EPSV ALL is in effect.").await;
        }
        match proto::parse_eprt(arg) {
            Some(Eprt::Addr(t)) => self.set_active(t).await,
            Some(Eprt::Unsupported) => self.reply(522, "Network protocol not supported, use (1)").await,
            None => self.reply(501, "EPRT takes |1|address|port|.").await,
        }
    }

    /// Send 150 and start a transfer task.  The data connection is opened by
    /// the task, after the 150, as RFC 959 orders it.
    async fn start(&mut self, job: Job, what: String, opening: &str) -> bool {
        let Some(plan) = self.data.take() else {
            return self.reply(425, "Use PORT, EPRT, PASV or EPSV first.").await;
        };
        if !self.reply(150, opening).await {
            return false;
        }
        let bytes = Arc::new(AtomicU64::new(0));
        let counter = bytes.clone();
        let peer_ip = self.peer.ip();
        let label = what.clone();
        let committing = Arc::new(AtomicBool::new(false));
        let flag = committing.clone();
        let handle = tokio::spawn(async move { run_job(plan, peer_ip, job, counter, label, flag).await });
        self.transfer = Some(Transfer { handle, bytes, what, committing });
        true
    }

    async fn cmd_list(&mut self, verb: &str, arg: &str) -> bool {
        let arg = proto::strip_list_options(arg);
        // A wildcard in the last component (`NLST *.prg`) -- how mget, mdelete
        // and `ls *.COM` work in Windows ftp, tnftp and the DOS clients -- is
        // matched against the folder it names, as vsftpd and wu-ftpd do.
        // Names come back with the client's own folder prefix, so each can be
        // handed straight to RETR or DELE.
        let (target, pattern, prefix) = match arg.rsplit_once('/') {
            _ if !arg.contains(['*', '?']) => (self.path_arg(arg), None, String::new()),
            Some((dir, leaf)) if !dir.contains(['*', '?']) => {
                (self.path_arg(if dir.is_empty() { "/" } else { dir }), Some(leaf.to_string()), format!("{dir}/"))
            }
            Some(_) => return self.reply(550, "Wildcards only in the last part of a path.").await,
            None => (self.cwd.clone(), Some(arg.to_string()), String::new()),
        };
        let mut entries = match self.store.list(&target).await {
            Ok(e) => e,
            Err(e) => return self.store_error(e).await,
        };
        if let Some(p) = &pattern {
            entries.retain(|e| proto::glob_match(p, &e.name));
            if entries.is_empty() {
                return self.reply(550, "No files match.").await;
            }
        }
        let now = chrono::Utc::now().timestamp();
        let mut body = Vec::new();
        for e in &entries {
            let line = if verb == "NLST" { format!("{prefix}{}", e.name) } else { proto::list_line(e, now) };
            body.extend_from_slice(line.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        self.start(Job::Send(body), format!("{verb} {target}"), "Here comes the listing.").await
    }

    async fn cmd_mlsd(&mut self, arg: &str) -> bool {
        let target = self.path_arg(arg);
        match self.store.stat(&target).await {
            Ok(e) if !e.is_dir => return self.reply(501, "MLSD lists a directory; use MLST for a file.").await,
            Ok(_) => {}
            Err(e) => return self.store_error(e).await,
        }
        let entries = match self.store.list(&target).await {
            Ok(e) => e,
            Err(e) => return self.store_error(e).await,
        };
        let mut body = Vec::new();
        for e in &entries {
            body.extend_from_slice(format!("{} {}\r\n", proto::mlsx_facts(e), e.name).as_bytes());
        }
        self.start(Job::Send(body), format!("MLSD {target}"), "Here comes the listing.").await
    }

    async fn cmd_mlst(&mut self, arg: &str) -> bool {
        let target = self.path_arg(arg);
        match self.store.stat(&target).await {
            Ok(e) => {
                let line = format!("{} {}", proto::mlsx_facts(&e), target);
                self.reply_lines(250, &[&format!("Listing {target}"), &line, "End"]).await
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_stat(&mut self, arg: &str) -> bool {
        if arg.trim().is_empty() {
            let state = match &self.transfer {
                Some(t) => format!("Transferring {}: {} bytes so far.", t.what, t.bytes.load(Ordering::Relaxed)),
                None => "No transfer in progress.".to_string(),
            };
            let who = if self.logged_in { "Logged in." } else { "Not logged in." };
            let lines = [
                "Ethernet Gateway FTP server status:",
                &format!("Connected from {}.", self.peer.ip()),
                who,
                if self.type_ascii {
                    "TYPE: A (files are sent unchanged), STRU: F, MODE: S."
                } else {
                    "TYPE: I, STRU: F, MODE: S."
                },
                &state,
                "End of status.",
            ];
            return self.reply_lines(211, &lines).await;
        }
        if self.transfer.is_some() {
            return self.reply(450, "A transfer is in progress.").await;
        }
        if !self.logged_in {
            return self.reply(530, "Please log in with USER and PASS.").await;
        }
        let target = self.path_arg(proto::strip_list_options(arg));
        match self.store.list(&target).await {
            Ok(entries) => {
                let now = chrono::Utc::now().timestamp();
                let lines: Vec<String> = entries.iter().map(|e| proto::list_line(e, now)).collect();
                let head = format!("Status of {target}:");
                let mut all: Vec<&str> = vec![&head];
                all.extend(lines.iter().map(String::as_str));
                all.push("End of status.");
                self.reply_lines(213, &all).await
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_retr(&mut self, arg: &str, rest: u64) -> bool {
        if arg.trim().is_empty() {
            return self.reply(501, "RETR needs a file name.").await;
        }
        // Before opening anything: on a slave, opening starts the master
        // sending, and a download with nowhere to go would waste the channel.
        if self.data.is_none() {
            return self.reply(425, "Use PORT, EPRT, PASV or EPSV first.").await;
        }
        let target = self.path_arg(arg);
        match self.store.open_read(&target, rest).await {
            Ok(file) => self.start(Job::Retr(file), format!("RETR {target}"), "Opening data connection.").await,
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_store(&mut self, arg: &str, mode: WriteMode) -> bool {
        if arg.trim().is_empty() {
            return self.reply(501, "A file name is needed.").await;
        }
        if self.data.is_none() {
            return self.reply(425, "Use PORT, EPRT, PASV or EPSV first.").await;
        }
        let target = self.path_arg(arg);
        match self.store.begin_write(&target, mode).await {
            Ok(up) => self.start(Job::Stor(up), format!("STOR {target}"), "Ready to receive.").await,
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_stou(&mut self, arg: &str) -> bool {
        if self.data.is_none() {
            return self.reply(425, "Use PORT, EPRT, PASV or EPSV first.").await;
        }
        // A path in the argument names the folder as well as the stem.
        let (dir, hint) = if arg.trim().is_empty() {
            (self.cwd.clone(), None)
        } else if arg.trim_end().ends_with('/') {
            // `STOU sub/`: a folder, no stem.
            (self.path_arg(arg), None)
        } else {
            proto::split_leaf(&self.path_arg(arg))
        };
        let name = match self.store.unique_name(&dir, hint).await {
            Ok(n) => n,
            Err(e) => return self.store_error(e).await,
        };
        match self.store.begin_write(&name, WriteMode::New).await {
            Ok(up) => {
                let leaf = proto::split_leaf(&name).1.unwrap_or_default();
                // RFC 1123 §4.1.2.9: the 150 names the file.
                self.start(Job::Stor(up), format!("STOU {name}"), &format!("FILE: {leaf}")).await
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_size(&mut self, arg: &str) -> bool {
        let target = self.path_arg(arg);
        match self.store.stat(&target).await {
            Ok(e) if e.is_dir => self.reply(550, "Not a plain file.").await,
            // TYPE A sends files unchanged, so the size is the same in both.
            Ok(e) => self.reply(213, &e.len.to_string()).await,
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_mdtm(&mut self, arg: &str) -> bool {
        let target = self.path_arg(arg);
        match self.store.stat(&target).await {
            Ok(e) if e.is_dir => self.reply(550, "Not a plain file.").await,
            Ok(e) => self.reply(213, &proto::time_val(e.mtime)).await,
            Err(e) => self.store_error(e).await,
        }
    }

    /// DELE, MKD and RMD: one store call, one reply.
    async fn simple(&mut self, arg: &str, op: &str) -> bool {
        if arg.trim().is_empty() {
            return self.reply(501, "A name is needed.").await;
        }
        let target = self.path_arg(arg);
        let result = match op {
            "DELE" => self.store.delete(&target).await,
            "MKD" => self.store.mkdir(&target).await,
            _ => self.store.rmdir(&target).await,
        };
        match result {
            Ok(()) => {
                glog!("FTP: {} {} {}", self.peer, op, target);
                match op {
                    "MKD" => self.reply(257, &format!("{} created.", proto::quote_path(&target))).await,
                    "DELE" => self.reply(250, "Deleted.").await,
                    _ => self.reply(250, "Directory removed.").await,
                }
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_rnfr(&mut self, arg: &str) -> bool {
        if arg.trim().is_empty() {
            return self.reply(501, "RNFR needs a name.").await;
        }
        let target = self.path_arg(arg);
        match self.store.check_rename_source(&target).await {
            Ok(()) => {
                self.rnfr = Some(target);
                self.reply(350, "Ready for RNTO.").await
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_rnto(&mut self, arg: &str, from: Option<String>) -> bool {
        let Some(from) = from else {
            return self.reply(503, "Send RNFR first.").await;
        };
        if arg.trim().is_empty() {
            return self.reply(501, "RNTO needs a name.").await;
        }
        let to = self.path_arg(arg);
        match self.store.rename(&from, &to).await {
            Ok(()) => {
                glog!("FTP: {} renamed {} to {}", self.peer, from, to);
                self.reply(250, "Renamed.").await
            }
            Err(e) => self.store_error(e).await,
        }
    }

    async fn cmd_abor(&mut self) -> bool {
        match self.transfer.take() {
            // Already done -- it finished in the same instant the ABOR arrived
            // -- or an upload whose data is all in and is being saved, which
            // is waited for.  RFC 959: a completed transfer gets its own 226,
            // not a 426; the client must not be told an upload that was saved
            // was not.
            Some(mut t) if t.handle.is_finished() || t.committing.load(Ordering::SeqCst) => {
                // Bounded: on a slave the save is the master's, and a master
                // that never answers must not hold the control loop for ever.
                let failed = || Done { code: 451, text: "Transfer failed.".into(), log: None };
                let done = match tokio::time::timeout(DATA_STALL_TIMEOUT, &mut t.handle).await {
                    Ok(r) => r.unwrap_or_else(|_| failed()),
                    Err(_) => {
                        t.handle.abort();
                        failed()
                    }
                };
                if let Some(l) = &done.log {
                    glog!("FTP: {} {}", self.peer, l);
                }
                self.reply(done.code, &done.text).await && self.reply(226, "Nothing to abort; the transfer had finished.").await
            }
            Some(t) => {
                t.handle.abort();
                glog!("FTP: {} aborted {}", self.peer, t.what);
                // RFC 959: 426 for the aborted transfer, then 226 for ABOR.
                self.reply(426, "Transfer aborted.").await && self.reply(226, "Abort successful.").await
            }
            None => {
                self.data = None;
                self.reply(225, "No transfer to abort.").await
            }
        }
    }
}

/// Open the data connection a plan describes.  A passive listener takes only
/// the control connection's own address -- anyone else connecting is dropped
/// and the wait goes on (port theft, RFC 2577 §5) -- and gives up after
/// [`DATA_CONNECT_TIMEOUT`].
async fn open_data(plan: DataPlan, peer_ip: IpAddr) -> std::io::Result<TcpStream> {
    let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, "data connection timed out");
    let stream = match plan {
        DataPlan::Passive(listener) => tokio::time::timeout(DATA_CONNECT_TIMEOUT, async {
            loop {
                let (s, from) = listener.accept().await?;
                let same = match (proto::as_v4(from.ip()), proto::as_v4(peer_ip)) {
                    (Some(a), Some(b)) => a == b,
                    _ => from.ip() == peer_ip,
                };
                if same {
                    return Ok::<TcpStream, std::io::Error>(s);
                }
                glog!("FTP: refused a data connection from {} (control is {})", from, peer_ip);
            }
        })
        .await
        .map_err(|_| timed_out())??,
        DataPlan::Active(addr) => tokio::time::timeout(DATA_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| timed_out())??,
    };
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Write all of `data`, giving up only when no byte has moved for
/// [`DATA_STALL_TIMEOUT`].  Not one timeout around `write_all`: that is a
/// deadline for the whole buffer, and a client slower than about 270 bytes a
/// second -- a C64 behind a 2400-baud WiFi modem -- needs longer than that
/// for 32 KiB while its data is still flowing.
pub(super) async fn write_progressing<W>(w: &mut W, mut data: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + ?Sized,
{
    while !data.is_empty() {
        let n = tokio::time::timeout(DATA_STALL_TIMEOUT, w.write(data))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "data connection stalled"))??;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        data = &data[n..];
    }
    Ok(())
}

/// Copy until EOF, counting bytes, giving up on a stall.
async fn pump<R, W>(mut r: R, mut w: W, counter: &AtomicU64) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let stalled = || std::io::Error::new(std::io::ErrorKind::TimedOut, "data connection stalled");
    let mut buf = vec![0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = tokio::time::timeout(DATA_STALL_TIMEOUT, r.read(&mut buf)).await.map_err(|_| stalled())??;
        if n == 0 {
            break;
        }
        write_progressing(&mut w, &buf[..n]).await?;
        total += n as u64;
        counter.store(total, Ordering::Relaxed);
    }
    tokio::time::timeout(DATA_STALL_TIMEOUT, w.flush()).await.map_err(|_| stalled())??;
    Ok(total)
}

/// A download: the store's bytes to the data connection.
async fn send_source(mut src: Source, w: &mut TcpStream, counter: &AtomicU64) -> std::io::Result<u64> {
    let stalled = || std::io::Error::new(std::io::ErrorKind::TimedOut, "data connection stalled");
    let mut buf = vec![0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = tokio::time::timeout(DATA_STALL_TIMEOUT, src.read(&mut buf)).await.map_err(|_| stalled())??;
        if n == 0 {
            break;
        }
        write_progressing(w, &buf[..n]).await?;
        total += n as u64;
        counter.store(total, Ordering::Relaxed);
    }
    tokio::time::timeout(DATA_STALL_TIMEOUT, w.flush()).await.map_err(|_| stalled())??;
    Ok(total)
}

/// An upload: the data connection's bytes to the store.
async fn receive_sink(r: &mut TcpStream, sink: &mut Sink, counter: &AtomicU64) -> std::io::Result<u64> {
    let stalled = || std::io::Error::new(std::io::ErrorKind::TimedOut, "data connection stalled");
    let mut buf = vec![0u8; 32 * 1024];
    let mut total = 0u64;
    loop {
        let n = tokio::time::timeout(DATA_STALL_TIMEOUT, r.read(&mut buf)).await.map_err(|_| stalled())??;
        if n == 0 {
            break;
        }
        // To a file or the master's channel, never a slow client: one
        // deadline for the chunk is right here.
        tokio::time::timeout(DATA_STALL_TIMEOUT, sink.write_all(&buf[..n])).await.map_err(|_| stalled())??;
        total += n as u64;
        counter.store(total, Ordering::Relaxed);
    }
    Ok(total)
}

/// One transfer, from opening the data connection to the final reply.
async fn run_job(
    plan: DataPlan,
    peer_ip: IpAddr,
    job: Job,
    counter: Arc<AtomicU64>,
    what: String,
    committing: Arc<AtomicBool>,
) -> Done {
    let started = std::time::Instant::now();
    let mut stream = match open_data(plan, peer_ip).await {
        Ok(s) => s,
        Err(_) => {
            return Done { code: 425, text: "Can't open data connection.".into(), log: Some(format!("{what}: no data connection")) };
        }
    };
    let secs = || started.elapsed().as_secs_f64();
    match job {
        Job::Send(body) => {
            let r = pump(&body[..], &mut stream, &counter).await;
            let _ = stream.shutdown().await;
            match r {
                Ok(_) => Done { code: 226, text: "Listing sent.".into(), log: None },
                Err(_) => Done { code: 426, text: "Listing interrupted.".into(), log: None },
            }
        }
        Job::Retr(source) => {
            let r = send_source(source, &mut stream, &counter).await;
            let _ = stream.shutdown().await;
            match r {
                Ok(n) => Done {
                    code: 226,
                    text: "Transfer complete.".into(),
                    log: Some(format!("{what}: sent {n} bytes in {:.1}s", secs())),
                },
                Err(e) => Done {
                    code: 426,
                    text: "Transfer interrupted.".into(),
                    log: Some(format!("{what}: interrupted ({e})")),
                },
            }
        }
        Job::Stor(mut upload) => {
            let r = receive_sink(&mut stream, &mut upload, &counter).await;
            if r.is_ok() {
                committing.store(true, Ordering::SeqCst);
            }
            match r {
                Ok(n) => match upload.finish().await {
                    Ok(()) => Done {
                        code: 226,
                        text: "Transfer complete.".into(),
                        log: Some(format!("{what}: received {n} bytes in {:.1}s", secs())),
                    },
                    Err(e) => {
                        let (code, text) = e.reply();
                        Done { code, text, log: Some(format!("{what}: not saved ({e:?})")) }
                    }
                },
                // `upload` drops here, taking a Replace upload's temp file.
                Err(e) => Done {
                    code: 426,
                    text: "Transfer interrupted.".into(),
                    log: Some(format!("{what}: interrupted ({e})")),
                },
            }
        }
    }
}

/// So `Entry` in a listing test reads naturally.
#[cfg(test)]
pub(super) fn entry(name: &str, is_dir: bool, len: u64, mtime: i64) -> proto::Entry {
    proto::Entry { name: name.into(), is_dir, len, mtime, locked: false }
}
