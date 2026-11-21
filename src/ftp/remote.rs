//! A slave's FTP file store: every file operation forwarded to its master.
//!
//! **Where the files live (Ricky, 2026-11-21):** "If the slave connects to a
//! master, all files end up on the master, and all file operations are
//! performed on the master."  So a slave runs the FTP *protocol* -- its
//! clients, data connections, firewall and address checks are all local -- and
//! sends each file operation down an SSH channel, where the master answers by
//! running [`super::store::LocalStore`] against its own `transfer_dir`.  One
//! jail, one CP/M guard, both evaluated where the files are.
//!
//! **One SSH connection per slave, one channel per FTP session.**  A new
//! connection per login would trip the master's per-IP connection rate (shared
//! with the slave's serial relays, from the same address) and pay a handshake
//! and a login each time.  The connection is opened on the first FTP login,
//! reused while any FTP session lives, and closed a minute after the last.
//! Requests are stateless (absolute virtual paths), so a dropped channel is
//! simply reopened on the next operation.
//!
//! Wire format, both directions: frames of `tag u8, len u32 BE, payload`.
//! A request's tag is its op; a reply is `OK` or `ERR`; file bytes travel as
//! `DATA` frames ended by `END`.  Payload fields are `u16`-length strings and
//! big-endian integers.  Request payloads are capped at [`MAX_REQUEST`] and data
//! frames at [`MAX_CHUNK`], so neither end can be made to allocate much.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::proto::Entry;
use super::store::{LocalStore, StoreError, WriteMode};
use crate::config;
use crate::glog;

/// The exec a slave sends to open a store channel.  The version is in the
/// command: a master that doesn't know it refuses the channel.
pub const STORE_COMMAND: &str = "ftp-store 1";

pub const MAX_REQUEST: usize = 8 * 1024;
pub const MAX_CHUNK: usize = 32 * 1024;
/// A listing reply: a directory of tens of thousands of entries still fits.
pub const MAX_REPLY: usize = 16 * 1024 * 1024;
/// The master gives up on an upload that sends nothing for this long, and
/// removes its temporary file.
pub const WRITE_STALL: Duration = Duration::from_secs(60);

const OP_STAT: u8 = 1;
const OP_LIST: u8 = 2;
const OP_READ: u8 = 3;
const OP_WRITE: u8 = 4;
const OP_DELETE: u8 = 5;
const OP_MKDIR: u8 = 6;
const OP_RMDIR: u8 = 7;
const OP_RENAME_CHECK: u8 = 8;
const OP_RENAME: u8 = 9;
const OP_UNIQUE: u8 = 10;

const TAG_OK: u8 = 0x80;
const TAG_ERR: u8 = 0x81;
const TAG_DATA: u8 = 0x82;
const TAG_END: u8 = 0x83;

// ─── framing ────────────────────────────────────────────────

fn bad(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, tag: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = tag;
    head[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    w.write_all(&head).await?;
    w.write_all(payload).await?;
    w.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head).await?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > max {
        return Err(bad("frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok((head[0], payload))
}

#[derive(Default)]
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn str(mut self, s: &str) -> Self {
        let b = s.as_bytes();
        let n = b.len().min(u16::MAX as usize);
        self.0.extend_from_slice(&(n as u16).to_be_bytes());
        self.0.extend_from_slice(&b[..n]);
        self
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn i64(mut self, v: i64) -> Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b }
    }
    fn take(&mut self, n: usize) -> std::io::Result<&'a [u8]> {
        if self.b.len() < n {
            return Err(bad("short field"));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }
    pub fn str(&mut self) -> std::io::Result<String> {
        let n = u16::from_be_bytes(self.take(2)?.try_into().unwrap_or([0, 0])) as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("not UTF-8"))
    }
    pub fn u8(&mut self) -> std::io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u64(&mut self) -> std::io::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap_or([0; 8])))
    }
    pub fn i64(&mut self) -> std::io::Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap_or([0; 8])))
    }
}

pub fn encode_entry(e: Enc, entry: &Entry) -> Enc {
    let flags = u8::from(entry.is_dir) | (u8::from(entry.locked) << 1);
    e.str(&entry.name).u8(flags).u64(entry.len).i64(entry.mtime)
}

pub fn decode_entry(d: &mut Dec) -> std::io::Result<Entry> {
    let name = d.str()?;
    let flags = d.u8()?;
    Ok(Entry { name, is_dir: flags & 1 != 0, locked: flags & 2 != 0, len: d.u64()?, mtime: d.i64()? })
}

pub fn encode_error(e: &StoreError) -> Vec<u8> {
    let (code, text): (u8, &str) = match e {
        StoreError::NotFound => (0, ""),
        StoreError::NotADirectory => (1, ""),
        StoreError::NotAFile => (2, ""),
        StoreError::BadName(t) => (3, t),
        StoreError::Locked(t) => (4, t),
        StoreError::Busy => (5, ""),
        StoreError::Exists => (6, ""),
        StoreError::NotEmpty => (7, ""),
        StoreError::DiskFull => (8, ""),
        StoreError::BadOffset => (9, ""),
        StoreError::Denied(t) => (10, t),
        StoreError::Io(t) => (11, t),
    };
    Enc::default().u8(code).str(text).0
}

pub fn decode_error(payload: &[u8]) -> StoreError {
    let mut d = Dec::new(payload);
    let (Ok(code), Ok(text)) = (d.u8(), d.str()) else {
        return StoreError::Io("unreadable reply from the master".into());
    };
    match code {
        0 => StoreError::NotFound,
        1 => StoreError::NotADirectory,
        2 => StoreError::NotAFile,
        3 => StoreError::BadName(Cow::Owned(text)),
        4 => StoreError::Locked(Cow::Owned(text)),
        5 => StoreError::Busy,
        6 => StoreError::Exists,
        7 => StoreError::NotEmpty,
        8 => StoreError::DiskFull,
        9 => StoreError::BadOffset,
        10 => StoreError::Denied(Cow::Owned(text)),
        _ => StoreError::Io(text),
    }
}

fn encode_mode(e: Enc, mode: WriteMode) -> Enc {
    match mode {
        WriteMode::Replace => e.u8(0).u64(0),
        WriteMode::Append => e.u8(1).u64(0),
        WriteMode::At(n) => e.u8(2).u64(n),
        WriteMode::New => e.u8(3).u64(0),
    }
}

fn decode_mode(d: &mut Dec) -> std::io::Result<WriteMode> {
    let (m, n) = (d.u8()?, d.u64()?);
    Ok(match m {
        0 => WriteMode::Replace,
        1 => WriteMode::Append,
        2 => WriteMode::At(n),
        3 => WriteMode::New,
        _ => return Err(bad("unknown write mode")),
    })
}

// ─── the master: serve one store channel ────────────────────

/// Serve one slave's store channel until it closes.  Runs [`LocalStore`], so
/// every rule is this machine's.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, slave: String) {
    let store = LocalStore;
    loop {
        let Ok((op, payload)) = read_frame(&mut s, MAX_REQUEST).await else {
            return;
        };
        if serve_one(&store, &mut s, op, &payload, &slave).await.is_err() {
            return;
        }
    }
}

async fn reply<S: AsyncWrite + Unpin>(s: &mut S, r: Result<Vec<u8>, StoreError>) -> std::io::Result<()> {
    match r {
        Ok(p) => write_frame(s, TAG_OK, &p).await,
        Err(e) => write_frame(s, TAG_ERR, &encode_error(&e)).await,
    }
}

async fn serve_one<S: AsyncRead + AsyncWrite + Unpin>(
    store: &LocalStore,
    s: &mut S,
    op: u8,
    payload: &[u8],
    slave: &str,
) -> std::io::Result<()> {
    let mut d = Dec::new(payload);
    match op {
        OP_STAT => {
            let path = d.str()?;
            let r = store.stat(&path).await.map(|e| encode_entry(Enc::default(), &e).0);
            reply(s, r).await
        }
        OP_LIST => {
            let path = d.str()?;
            let r = store.list(&path).await.map(|entries| {
                let mut e = Enc::default().u64(entries.len() as u64);
                for entry in &entries {
                    e = encode_entry(e, entry);
                }
                e.0
            });
            reply(s, r).await
        }
        OP_DELETE | OP_MKDIR | OP_RMDIR | OP_RENAME_CHECK => {
            let path = d.str()?;
            let r = match op {
                OP_DELETE => store.delete(&path).await,
                OP_MKDIR => store.mkdir(&path).await,
                OP_RMDIR => store.rmdir(&path).await,
                _ => store.check_rename_source(&path).await,
            };
            if r.is_ok() && op != OP_RENAME_CHECK {
                glog!("FTP (for slave {}): {} {}", slave, ["", "", "", "", "", "DELE", "MKD", "RMD"][op as usize], path);
            }
            reply(s, r.map(|()| Vec::new())).await
        }
        OP_RENAME => {
            let (from, to) = (d.str()?, d.str()?);
            let r = store.rename(&from, &to).await;
            if r.is_ok() {
                glog!("FTP (for slave {}): renamed {} to {}", slave, from, to);
            }
            reply(s, r.map(|()| Vec::new())).await
        }
        OP_UNIQUE => {
            let dir = d.str()?;
            let hint = d.str()?;
            let hint = (!hint.is_empty()).then_some(hint);
            let r = store.unique_name(&dir, hint).await.map(|n| Enc::default().str(&n).0);
            reply(s, r).await
        }
        OP_READ => {
            let path = d.str()?;
            let offset = d.u64()?;
            let mut file = match store.open_read(&path, offset).await {
                Ok(f) => f,
                Err(e) => return reply(s, Err(e)).await,
            };
            write_frame(s, TAG_OK, &[]).await?;
            let mut buf = vec![0u8; MAX_CHUNK];
            let mut sent = 0u64;
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        write_frame(s, TAG_DATA, &buf[..n]).await?;
                        sent += n as u64;
                    }
                    Err(e) => {
                        write_frame(s, TAG_END, &[]).await?;
                        return reply(s, Err(StoreError::Io(e.kind().to_string()))).await;
                    }
                }
            }
            write_frame(s, TAG_END, &[]).await?;
            glog!("FTP (for slave {}): sent {} ({} bytes)", slave, path, sent);
            reply(s, Ok(Vec::new())).await
        }
        OP_WRITE => {
            let path = d.str()?;
            let mode = decode_mode(&mut d)?;
            let mut upload = match store.begin_write(&path, mode).await {
                Ok(u) => u,
                Err(e) => return reply(s, Err(e)).await,
            };
            write_frame(s, TAG_OK, &[]).await?;
            let mut got = 0u64;
            loop {
                // A stalled or vanished slave ends the upload here; dropping
                // `upload` removes a Replace upload's temporary file.
                let (tag, data) = tokio::time::timeout(WRITE_STALL, read_frame(s, MAX_CHUNK))
                    .await
                    .map_err(|_| bad("upload stalled"))??;
                match tag {
                    TAG_DATA => {
                        upload.file.write_all(&data).await?;
                        got += data.len() as u64;
                    }
                    TAG_END => break,
                    _ => return Err(bad("unexpected frame in an upload")),
                }
            }
            // Flushed, synced and renamed before the answer: the slave says
            // 226 only once the bytes are on this disk.
            let r = upload.finish().await;
            if r.is_ok() {
                glog!("FTP (for slave {}): received {} ({} bytes)", slave, path, got);
            }
            reply(s, r.map(|()| Vec::new())).await
        }
        _ => Err(bad("unknown store operation")),
    }
}

// ─── the slave: the shared connection ───────────────────────

type Chan = russh::ChannelStream<russh::client::Msg>;

struct Shared {
    key: (String, u16, String),
    session: Arc<crate::relay::RelaySession>,
}

fn shared() -> &'static tokio::sync::Mutex<Option<Shared>> {
    static S: std::sync::OnceLock<tokio::sync::Mutex<Option<Shared>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Live `RemoteStore`s; the shared connection closes a minute after this
/// reaches zero.
static LIVE: AtomicUsize = AtomicUsize::new(0);
const LINGER: Duration = Duration::from_secs(60);
const CHANNEL_WAIT: Duration = Duration::from_secs(15);

/// Open a store channel to the master, connecting first if need be.  The text
/// of an error is what the FTP client is told.
async fn open_channel() -> Result<Chan, String> {
    let cfg = config::get_config();
    if cfg.slave_master_host.trim().is_empty() {
        return Err("no master gateway is configured".into());
    }
    let key = (cfg.slave_master_host.clone(), cfg.slave_master_port, cfg.slave_master_username.clone());
    let mut guard = shared().lock().await;
    if let Some(sh) = guard.as_ref()
        && sh.key == key
        && !sh.session.is_closed()
    {
        match crate::relay::open_exec_channel(&sh.session, STORE_COMMAND, CHANNEL_WAIT).await {
            Ok(c) => return Ok(c),
            // The connection died under us: reconnect below.
            Err(crate::relay::RelayConnectError::Network(_)) => {}
            Err(e) => return Err(describe(e)),
        }
    }
    let session = tokio::time::timeout(
        Duration::from_secs(20),
        crate::relay::connect_master_session(&key.0, key.1, &key.2, &cfg.slave_master_password),
    )
    .await
    .map_err(|_| "the master gateway did not answer".to_string())?
    .map_err(describe)?;
    let session = Arc::new(session);
    *guard = Some(Shared { key, session: session.clone() });
    crate::relay::open_exec_channel(&session, STORE_COMMAND, CHANNEL_WAIT).await.map_err(describe)
}

fn describe(e: crate::relay::RelayConnectError) -> String {
    match e {
        crate::relay::RelayConnectError::Network(m) => {
            glog!("FTP (slave): master unreachable: {}", m);
            "the master gateway is unavailable".into()
        }
        crate::relay::RelayConnectError::Auth(m) => {
            glog!("FTP (slave): master refused this slave: {}", m);
            "the master gateway refused this slave's login".into()
        }
        crate::relay::RelayConnectError::Refused(m) => {
            glog!("FTP (slave): master declined the FTP channel: {}", m);
            "the master is not accepting relays, or is too old for FTP".into()
        }
    }
}

/// One FTP session's view of the master.
pub struct RemoteStore {
    chan: Arc<tokio::sync::Mutex<Option<Chan>>>,
}

impl Drop for RemoteStore {
    fn drop(&mut self) {
        if LIVE.fetch_sub(1, Ordering::SeqCst) == 1 {
            // The last one: close the connection unless another FTP login
            // arrives within the linger.
            tokio::spawn(async {
                tokio::time::sleep(LINGER).await;
                if LIVE.load(Ordering::SeqCst) == 0 {
                    let mut guard = shared().lock().await;
                    if LIVE.load(Ordering::SeqCst) == 0
                        && let Some(sh) = guard.take()
                    {
                        let _ = sh.session.disconnect(russh::Disconnect::ByApplication, "FTP idle", "").await;
                    }
                }
            });
        }
    }
}

type Guard = tokio::sync::OwnedMutexGuard<Option<Chan>>;

fn lost(e: std::io::Error) -> StoreError {
    StoreError::Io(format!("lost the master ({})", e.kind()))
}

impl RemoteStore {
    /// Open this session's channel to the master.  Called at login, so a
    /// master that can't be reached is said at once (`421`).
    pub async fn connect() -> Result<RemoteStore, String> {
        let chan = open_channel().await?;
        LIVE.fetch_add(1, Ordering::SeqCst);
        Ok(RemoteStore { chan: Arc::new(tokio::sync::Mutex::new(Some(chan))) })
    }

    async fn channel(&self) -> Result<Guard, StoreError> {
        let mut g = self.chan.clone().lock_owned().await;
        if g.is_none() {
            *g = Some(open_channel().await.map_err(StoreError::Io)?);
        }
        Ok(g)
    }

    /// One request, one reply.  A broken channel is dropped, so the next
    /// operation reopens it.
    async fn call(&self, op: u8, payload: Vec<u8>) -> Result<Vec<u8>, StoreError> {
        let mut g = self.channel().await?;
        let Some(ch) = g.as_mut() else { return Err(StoreError::Io("no channel".into())) };
        let r = async {
            write_frame(ch, op, &payload).await?;
            read_frame(ch, MAX_REPLY).await
        }
        .await;
        match r {
            Ok((TAG_OK, p)) => Ok(p),
            Ok((TAG_ERR, p)) => Err(decode_error(&p)),
            Ok(_) => {
                *g = None;
                Err(StoreError::Io("unexpected reply from the master".into()))
            }
            Err(e) => {
                *g = None;
                Err(lost(e))
            }
        }
    }

    fn path_req(path: &str) -> Vec<u8> {
        Enc::default().str(path).0
    }

    pub async fn stat(&self, path: &str) -> Result<Entry, StoreError> {
        let p = self.call(OP_STAT, Self::path_req(path)).await?;
        decode_entry(&mut Dec::new(&p)).map_err(lost)
    }

    pub async fn list(&self, path: &str) -> Result<Vec<Entry>, StoreError> {
        let p = self.call(OP_LIST, Self::path_req(path)).await?;
        let mut d = Dec::new(&p);
        let n = d.u64().map_err(lost)?;
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(decode_entry(&mut d).map_err(lost)?);
        }
        Ok(out)
    }

    pub async fn delete(&self, path: &str) -> Result<(), StoreError> {
        self.call(OP_DELETE, Self::path_req(path)).await.map(|_| ())
    }
    pub async fn mkdir(&self, path: &str) -> Result<(), StoreError> {
        self.call(OP_MKDIR, Self::path_req(path)).await.map(|_| ())
    }
    pub async fn rmdir(&self, path: &str) -> Result<(), StoreError> {
        self.call(OP_RMDIR, Self::path_req(path)).await.map(|_| ())
    }
    pub async fn check_rename_source(&self, path: &str) -> Result<(), StoreError> {
        self.call(OP_RENAME_CHECK, Self::path_req(path)).await.map(|_| ())
    }
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), StoreError> {
        self.call(OP_RENAME, Enc::default().str(from).str(to).0).await.map(|_| ())
    }
    pub async fn unique_name(&self, dir: &str, hint: Option<String>) -> Result<String, StoreError> {
        let p = self.call(OP_UNIQUE, Enc::default().str(dir).str(hint.as_deref().unwrap_or("")).0).await?;
        Dec::new(&p).str().map_err(lost)
    }

    /// Start a download: the master opens the file and the bytes follow.
    pub async fn open_read(&self, path: &str, offset: u64) -> Result<RemoteRead, StoreError> {
        let mut g = self.channel().await?;
        let Some(ch) = g.as_mut() else { return Err(StoreError::Io("no channel".into())) };
        let r = async {
            write_frame(ch, OP_READ, &Enc::default().str(path).u64(offset).0).await?;
            read_frame(ch, MAX_REPLY).await
        }
        .await;
        match r {
            Ok((TAG_OK, _)) => Ok(RemoteRead { guard: Some(g), pending: Vec::new(), at: 0, done: false }),
            Ok((TAG_ERR, p)) => Err(decode_error(&p)),
            Ok(_) => {
                *g = None;
                Err(StoreError::Io("unexpected reply from the master".into()))
            }
            Err(e) => {
                *g = None;
                Err(lost(e))
            }
        }
    }

    /// Start an upload: the master opens the target (all its checks run
    /// there) before the client is told to send.
    pub async fn begin_write(&self, path: &str, mode: WriteMode) -> Result<RemoteWrite, StoreError> {
        let mut g = self.channel().await?;
        let Some(ch) = g.as_mut() else { return Err(StoreError::Io("no channel".into())) };
        let r = async {
            write_frame(ch, OP_WRITE, &encode_mode(Enc::default().str(path), mode).0).await?;
            read_frame(ch, MAX_REPLY).await
        }
        .await;
        match r {
            Ok((TAG_OK, _)) => Ok(RemoteWrite { guard: Some(g) }),
            Ok((TAG_ERR, p)) => Err(decode_error(&p)),
            Ok(_) => {
                *g = None;
                Err(StoreError::Io("unexpected reply from the master".into()))
            }
            Err(e) => {
                *g = None;
                Err(lost(e))
            }
        }
    }
}

/// A download from the master.  Holds the session's channel for its length;
/// dropped unfinished (ABOR, a lost client), it drops the channel too, since
/// the stream can't be resynchronised -- the next operation reopens one.
pub struct RemoteRead {
    guard: Option<Guard>,
    pending: Vec<u8>,
    at: usize,
    done: bool,
}

impl RemoteRead {
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.at < self.pending.len() {
                let n = (self.pending.len() - self.at).min(buf.len());
                buf[..n].copy_from_slice(&self.pending[self.at..self.at + n]);
                self.at += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            let Some(ch) = self.guard.as_mut().and_then(|g| g.as_mut()) else {
                return Err(bad("no channel"));
            };
            let (tag, data) = read_frame(ch, MAX_CHUNK).await?;
            match tag {
                TAG_DATA => {
                    self.pending = data;
                    self.at = 0;
                }
                TAG_END => {
                    let (tag, p) = read_frame(ch, MAX_REPLY).await?;
                    self.done = true;
                    // The master answered, so the channel is in step either
                    // way and is kept for the next operation.
                    self.guard.take();
                    if tag == TAG_ERR {
                        let (_, text) = decode_error(&p).reply();
                        return Err(std::io::Error::other(text));
                    }
                    return Ok(0);
                }
                _ => return Err(bad("unexpected frame in a download")),
            }
        }
    }
}

impl Drop for RemoteRead {
    fn drop(&mut self) {
        if let Some(mut g) = self.guard.take() {
            *g = None;
        }
    }
}

/// An upload to the master; see [`RemoteRead`] for the channel's fate.
pub struct RemoteWrite {
    guard: Option<Guard>,
}

impl RemoteWrite {
    pub async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        let Some(ch) = self.guard.as_mut().and_then(|g| g.as_mut()) else {
            return Err(bad("no channel"));
        };
        for chunk in data.chunks(MAX_CHUNK) {
            write_frame(ch, TAG_DATA, chunk).await?;
        }
        Ok(())
    }

    /// End the upload; the master flushes, syncs and renames before answering.
    pub async fn finish(mut self) -> Result<(), StoreError> {
        let Some(ch) = self.guard.as_mut().and_then(|g| g.as_mut()) else {
            return Err(StoreError::Io("no channel".into()));
        };
        let r = async {
            write_frame(ch, TAG_END, &[]).await?;
            read_frame(ch, MAX_REPLY).await
        }
        .await;
        match r {
            Ok((TAG_OK, _)) => {
                self.guard.take();
                Ok(())
            }
            Ok((TAG_ERR, p)) => {
                self.guard.take();
                Err(decode_error(&p))
            }
            Ok(_) => Err(StoreError::Io("unexpected reply from the master".into())),
            Err(e) => Err(lost(e)),
        }
    }
}

impl Drop for RemoteWrite {
    fn drop(&mut self) {
        if let Some(mut g) = self.guard.take() {
            *g = None;
        }
    }
}
