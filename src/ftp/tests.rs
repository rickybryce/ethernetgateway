//! FTP server tests: the pure protocol pieces directly, and whole sessions over
//! loopback TCP driven by a client written here.

use super::proto::{self, Eprt, Fed, LineReader};
use super::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn feed_all(r: &mut LineReader, bytes: &[u8]) -> Vec<Fed> {
    bytes.iter().map(|&b| r.feed(b)).filter(|f| *f != Fed::Nothing).collect()
}

// ─── proto ──────────────────────────────────────────────────

/// **The control line is a Telnet NVT.**  CRLF and bare LF both end a line;
/// `IAC IAC` is a literal 0xFF; option offers are refused (DO -> WONT,
/// WILL -> DONT) and the refusal is handed back to send; other Telnet commands
/// vanish.
#[test]
fn test_the_line_reader_speaks_telnet() {
    let mut r = LineReader::default();
    assert_eq!(feed_all(&mut r, b"USER x\r\n"), vec![Fed::Line("USER x".into())]);
    assert_eq!(feed_all(&mut r, b"NOOP\n"), vec![Fed::Line("NOOP".into())]);
    assert_eq!(feed_all(&mut r, &[255, 253, 1]), vec![Fed::Reply([255, 252, 1])], "DO ECHO -> WONT");
    assert_eq!(feed_all(&mut r, &[255, 251, 3]), vec![Fed::Reply([255, 254, 3])], "WILL SGA -> DONT");
    assert_eq!(feed_all(&mut r, &[255, 252, 3]), vec![], "a WONT is not acknowledged");
    let mut line = b"CWD a".to_vec();
    line.extend_from_slice(&[255, 255]);
    line.extend_from_slice(b"b\r\n");
    // One 0xFF byte; not UTF-8, so the line is read as Latin-1 (`ÿ`), as a
    // Windows ftp sending its code page needs -- never a replacement char.
    assert_eq!(feed_all(&mut r, &line), vec![Fed::Line("CWD a\u{ff}b".into())], "IAC IAC is one 0xFF");
}

/// **The out-of-band ABOR still reads as ABOR.**  RFC 959 has a client send
/// `IAC IP`, then `IAC DM` as urgent data, then `ABOR`.  Without
/// `SO_OOBINLINE` the DM byte is pulled out of the stream, leaving
/// `IAC IP IAC A B O R` -- and with it inline, `IAC IP IAC DM ABOR`.  Both
/// must give `ABOR`.
#[test]
fn test_the_out_of_band_abor_reads_as_abor() {
    let mut r = LineReader::default();
    assert_eq!(feed_all(&mut r, &[255, 244, 255, b'A', b'B', b'O', b'R', b'\r', b'\n']), vec![Fed::Line("ABOR".into())]);
    assert_eq!(feed_all(&mut r, &[255, 244, 255, 242, b'A', b'B', b'O', b'R', b'\r', b'\n']), vec![Fed::Line("ABOR".into())]);
}

/// A line past the cap is refused once and discarded to its end; the next line
/// is read normally, so a long line can't wedge the session or grow a buffer.
#[test]
fn test_an_overlong_line_is_refused_and_skipped() {
    let mut r = LineReader::default();
    let mut long = vec![b'A'; proto::MAX_LINE + 10];
    long.extend_from_slice(b"\r\nNOOP\r\n");
    assert_eq!(feed_all(&mut r, &long), vec![Fed::TooLong, Fed::Line("NOOP".into())]);
}

#[test]
fn test_split_command_keeps_the_argument_whole() {
    assert_eq!(proto::split_command("stor my file.txt"), ("STOR".into(), "my file.txt".into()));
    assert_eq!(proto::split_command("PWD"), ("PWD".into(), String::new()));
    assert_eq!(proto::split_command("CWD  FTP"), ("CWD".into(), "FTP".into()), "a doubled space is a slip");
    assert_eq!(proto::split_command("  noop  "), ("NOOP".into(), String::new()));
    // Only ASCII blanks are trimmed: a U+00A0 from a Latin-1 line stays.
    assert_eq!(proto::split_command("PASS caf\u{a0}"), ("PASS".into(), "caf\u{a0}".into()));
}

/// Multi-line replies use `code-` ... `code `, middle lines indented so none
/// can be read as the end; a 0xFF in text is doubled for the NVT.
#[test]
fn test_reply_formatting() {
    assert_eq!(proto::reply(200, &["OK."]), b"200 OK.\r\n");
    assert_eq!(proto::reply(211, &["a", "b", "c"]), b"211-a\r\n b\r\n211 c\r\n");
    assert_eq!(proto::nvt_escape("x\u{ff}"), "x\u{ff}".as_bytes().to_vec(), "UTF-8 ÿ is two bytes, neither 0xFF");
    assert_eq!(proto::nvt_escape("a\r\nb"), b"a  b");
}

#[test]
fn test_port_and_eprt_parse() {
    let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 0x1234);
    assert_eq!(proto::parse_port("192,168,1,5,18,52"), Some(a));
    assert_eq!(proto::parse_port("192,168,1,5,18"), None);
    assert_eq!(proto::parse_port("192,168,1,500,18,52"), None);
    assert_eq!(proto::parse_eprt("|1|192.168.1.5|4660|"), Some(Eprt::Addr(a)));
    assert_eq!(proto::parse_eprt("!1!192.168.1.5!4660!"), Some(Eprt::Addr(a)), "any delimiter");
    assert_eq!(proto::parse_eprt("|2|fe80::1|4660|"), Some(Eprt::Unsupported));
    assert_eq!(proto::parse_eprt("|2|nonsense|4660|"), None);
    assert_eq!(proto::parse_eprt("|1|192.168.1.5|4660"), None);
}

/// **No FTP bounce (RFC 2577 §3).**  An active-mode target must be the
/// control connection's own address -- through `::ffff:` mapping too -- and
/// not a privileged port.  This is the hole libunftp leaves open.
#[test]
fn test_active_mode_only_reaches_the_client_itself() {
    let me: IpAddr = "192.168.1.5".parse().unwrap();
    let mapped: IpAddr = "::ffff:192.168.1.5".parse().unwrap();
    let ok: SocketAddr = "192.168.1.5:40000".parse().unwrap();
    assert!(proto::active_target_allowed(ok, me));
    assert!(proto::active_target_allowed(ok, mapped), "a mapped control address is the same host");
    assert!(!proto::active_target_allowed("192.168.1.6:40000".parse().unwrap(), me), "another host");
    assert!(!proto::active_target_allowed("192.168.1.5:25".parse().unwrap(), me), "a privileged port");
    // The bound itself: 1024 is the first port allowed.
    assert!(proto::active_target_allowed("192.168.1.5:1024".parse().unwrap(), me));
    assert!(!proto::active_target_allowed("192.168.1.5:1023".parse().unwrap(), me));
}

/// Virtual paths never climb above the root, and come out normalised.
#[test]
fn test_resolve_stays_in_the_root() {
    assert_eq!(proto::resolve("/", "a/b"), "/a/b");
    assert_eq!(proto::resolve("/a", "../../../etc/passwd"), "/etc/passwd");
    assert_eq!(proto::resolve("/a/b", ".."), "/a");
    assert_eq!(proto::resolve("/a", "/x/./y/"), "/x/y");
    assert_eq!(proto::resolve("/a", "\\DOS\\FILE"), "/DOS/FILE", "backslashes separate");
    assert_eq!(proto::resolve("/a", ".."), "/");
    assert_eq!(proto::resolve("/", ".."), "/");
    assert_eq!(proto::split_leaf("/a/b"), ("/a".into(), Some("b".into())));
    assert_eq!(proto::split_leaf("/b"), ("/".into(), Some("b".into())));
    assert_eq!(proto::split_leaf("/"), ("/".into(), None));
    assert_eq!(proto::quote_path("/a\"b"), "\"/a\"\"b\"");
}

#[test]
fn test_list_options_are_stripped() {
    assert_eq!(proto::strip_list_options("-la"), "");
    assert_eq!(proto::strip_list_options("-a -l /dir"), "/dir");
    assert_eq!(proto::strip_list_options("dir"), "dir");
}

/// The LIST line shape every client parses, and the MLSD facts.
#[test]
fn test_listing_formats() {
    let now = 1_800_000_000;
    let recent = session::entry("GAME.PRG", false, 1234, now - 3600);
    let line = proto::list_line(&recent, now);
    assert!(line.starts_with("-rw-r--r--   1 ftp      ftp              1234 "), "{line}");
    assert!(line.ends_with(" GAME.PRG"), "{line}");
    assert!(line.contains(':'), "a recent file shows the time: {line}");
    let old = session::entry("OLD", true, 0, now - 400 * 24 * 3600);
    let line = proto::list_line(&old, now);
    assert!(line.starts_with("drwxr-xr-x"), "{line}");
    assert!(!line.contains(':'), "an old one shows the year: {line}");
    assert_eq!(proto::time_val(0), "19700101000000");
    let mut f = session::entry("F", false, 5, 0);
    assert_eq!(proto::mlsx_facts(&f), "type=file;size=5;modify=19700101000000;perm=rwadf;");
    f.locked = true;
    assert_eq!(proto::mlsx_facts(&f), "type=file;size=5;modify=19700101000000;perm=r;");
}

// ─── the accept gate ────────────────────────────────────────

fn gate() -> Gate {
    Gate {
        security: false,
        disable_ip_safety: false,
        block_gateway: false,
        rate_count: 1,
        rate_max: 20,
        sessions_before: 0,
        max_sessions: 50,
    }
}

/// **Telnet's rules, in telnet's order.**  Rate first (silent), then the cap,
/// then -- with security off and IP safety on -- the private-address
/// allowlist.  Security on opens it to any address (the login is the gate);
/// `disable_ip_safety` opens it too; block-gateway refuses the router.
#[test]
fn test_the_accept_gate_follows_the_security_settings() {
    let public: IpAddr = "8.8.8.8".parse().unwrap();
    let lan: IpAddr = "192.168.1.20".parse().unwrap();
    let router: IpAddr = "192.168.1.1".parse().unwrap();
    let routers = [router];
    assert_eq!(refusal(&gate(), lan, &routers), None);
    assert!(matches!(refusal(&gate(), public, &routers), Some(Refusal::Address(_))));
    assert_eq!(refusal(&Gate { security: true, ..gate() }, public, &routers), None);
    assert_eq!(refusal(&Gate { disable_ip_safety: true, ..gate() }, public, &routers), None);
    assert_eq!(refusal(&gate(), router, &routers), None, "the router is allowed by default");
    assert!(matches!(
        refusal(&Gate { block_gateway: true, ..gate() }, router, &routers),
        Some(Refusal::Address(_))
    ));
    assert_eq!(refusal(&Gate { rate_count: 21, ..gate() }, lan, &routers), Some(Refusal::RateLimited));
    assert_eq!(refusal(&Gate { rate_count: 20, ..gate() }, lan, &routers), None, "exactly the limit is allowed");
    assert_eq!(refusal(&Gate { rate_count: 99, rate_max: 0, ..gate() }, lan, &routers), None, "0 = no limit");
    assert_eq!(
        refusal(&Gate { sessions_before: 50, ..gate() }, lan, &routers),
        Some(Refusal::TooManySessions)
    );
    // Order: a rate-limited address is dropped silently even when it would
    // also have been refused by address.
    assert_eq!(refusal(&Gate { rate_count: 21, ..gate() }, public, &routers), Some(Refusal::RateLimited));
}

/// The passive range setting: empty = OS-chosen; `low-high` or one port;
/// nothing below 1024; reversed or junk refused.
#[test]
fn test_passive_ports_parse() {
    assert_eq!(config::parse_passive_ports(""), Some(None));
    assert_eq!(config::parse_passive_ports("50000-50100"), Some(Some(50000..=50100)));
    assert_eq!(config::parse_passive_ports(" 50000 - 50001 "), Some(Some(50000..=50001)));
    assert_eq!(config::parse_passive_ports("50000"), Some(Some(50000..=50000)));
    assert_eq!(config::parse_passive_ports("21-30"), None);
    assert_eq!(config::parse_passive_ports("1024-1025"), Some(Some(1024..=1025)), "1024 is the first allowed");
    assert_eq!(config::parse_passive_ports("1023-1025"), None);
    assert_eq!(config::parse_passive_ports("50100-50000"), None);
    assert_eq!(config::parse_passive_ports("lots"), None);
}

// ─── whole sessions over loopback ───────────────────────────

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct Client {
    r: BufReader<tokio::net::tcp::OwnedReadHalf>,
    w: tokio::net::tcp::OwnedWriteHalf,
}

impl Client {
    /// Read one reply, multi-line included: returns (code, all text).
    async fn reply(&mut self) -> (u16, String) {
        // Bounded: a server that stalls fails the test instead of hanging the
        // whole suite, and an EOF says so rather than panicking on a slice.
        tokio::time::timeout(std::time::Duration::from_secs(30), self.reply_inner())
            .await
            .expect("no reply from the server within 30 s")
    }

    async fn reply_inner(&mut self) -> (u16, String) {
        let mut all = String::new();
        let mut line = String::new();
        let n = self.r.read_line(&mut line).await.unwrap();
        assert!(n >= 4, "the server closed the connection or sent {line:?}");
        let code: u16 = line[..3].parse().unwrap_or_else(|_| panic!("not a reply: {line:?}"));
        all.push_str(&line);
        if line.as_bytes().get(3) == Some(&b'-') {
            loop {
                line.clear();
                if self.r.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                all.push_str(&line);
                if line.starts_with(&format!("{code} ")) {
                    break;
                }
            }
        }
        (code, all)
    }

    async fn cmd(&mut self, c: &str) -> (u16, String) {
        self.w.write_all(format!("{c}\r\n").as_bytes()).await.unwrap();
        self.reply().await
    }

    /// PASV, returning the data address.
    async fn pasv(&mut self) -> SocketAddr {
        let (code, text) = self.cmd("PASV").await;
        assert_eq!(code, 227, "{text}");
        let inner = &text[text.find('(').unwrap() + 1..text.find(')').unwrap()];
        proto::parse_port(inner).unwrap()
    }

    /// A whole download through PASV.
    async fn get(&mut self, cmd: &str) -> (u16, Vec<u8>) {
        let addr = self.pasv().await;
        self.w.write_all(format!("{cmd}\r\n").as_bytes()).await.unwrap();
        let mut data = TcpStream::connect(addr).await.unwrap();
        let (code, text) = self.reply().await;
        if code != 150 {
            return (code, text.into_bytes());
        }
        let mut body = Vec::new();
        data.read_to_end(&mut body).await.unwrap();
        (self.reply().await.0, body)
    }

    /// A whole upload through PASV.
    async fn put(&mut self, cmd: &str, body: &[u8]) -> (u16, String) {
        let addr = self.pasv().await;
        self.w.write_all(format!("{cmd}\r\n").as_bytes()).await.unwrap();
        let mut data = TcpStream::connect(addr).await.unwrap();
        let (code, text) = self.reply().await;
        if code != 150 {
            return (code, text);
        }
        data.write_all(body).await.unwrap();
        data.shutdown().await.unwrap();
        drop(data);
        self.reply().await
    }
}

/// A server session on loopback, with its own lockout map, and a client on it.
async fn connect(lockouts: crate::telnet::LockoutMap) -> Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let s = session::Session::new(
            stream,
            peer,
            lockouts,
            Arc::new(AtomicBool::new(false)),
            Arc::new(tokio::sync::Notify::new()),
        )
        .unwrap();
        s.run().await;
    });
    let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
    let mut c = Client { r: BufReader::new(r), w };
    assert_eq!(c.reply().await.0, 220);
    c
}

/// Point `transfer_dir` (and security) at a scratch folder for one test.
struct Scratch {
    dir: std::path::PathBuf,
    old: config::Config,
}

impl Scratch {
    fn new(tag: &str, security: bool) -> Scratch {
        let old = config::get_config();
        let dir = std::env::temp_dir().join(format!("egw_ftp_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        config::update_config_value("transfer_dir", &dir.to_string_lossy());
        config::update_config_value("security_enabled", if security { "true" } else { "false" });
        config::update_config_value("username", "admin");
        config::update_config_value("password", &crate::credential::hash_for_test("secret"));
        config::update_config_value("idle_timeout_secs", "0");
        // A slave would send every login to a master that isn't here.
        config::update_config_value("gateway_role", "standalone");
        Scratch { dir, old }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for (k, v) in [
            ("transfer_dir", self.old.transfer_dir.clone()),
            ("security_enabled", self.old.security_enabled.to_string()),
            ("username", self.old.username.clone()),
            ("password", self.old.password.clone()),
            ("idle_timeout_secs", self.old.idle_timeout_secs.to_string()),
            ("gateway_role", self.old.gateway_role.clone()),
        ] {
            config::update_config_value(k, &v);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Wait up to 5 s for a condition that follows a task's teardown -- polled,
/// not slept for, so a loaded suite run neither flakes nor waits needlessly.
async fn poll(mut ok: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if ok() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    ok()
}

fn no_lockouts() -> crate::telnet::LockoutMap {
    Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// **An anonymous session does the whole job.**  Login with any password,
/// upload, list, download, resume from an offset, append, rename, delete, make
/// and remove a folder, and move about -- each reply code as the RFCs give it.
#[tokio::test]
async fn test_an_anonymous_session_round_trip() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("roundtrip", false);
    let mut c = connect(no_lockouts()).await;

    assert_eq!(c.cmd("LIST").await.0, 530, "nothing before login");
    assert_eq!(c.cmd("USER anonymous").await.0, 331);
    assert_eq!(c.cmd("PASS me@example").await.0, 230);
    assert_eq!(c.cmd("TYPE I").await.0, 200);
    let (code, text) = c.cmd("PWD").await;
    assert_eq!((code, text.trim_end()), (257, "257 \"/\" is the current directory."));

    assert_eq!(c.cmd("MKD SUB").await.0, 257);
    assert_eq!(c.cmd("CWD SUB").await.0, 250);
    assert_eq!(c.put("STOR HELLO.TXT", b"hello world").await.0, 226);
    assert_eq!(std::fs::read(s.dir.join("SUB").join("HELLO.TXT")).unwrap(), b"hello world");
    let (code, body) = c.get("RETR HELLO.TXT").await;
    assert_eq!((code, body.as_slice()), (226, &b"hello world"[..]));
    assert_eq!(c.cmd("SIZE HELLO.TXT").await.1.trim_end(), "213 11");
    assert_eq!(c.cmd("MDTM HELLO.TXT").await.0, 213);

    assert_eq!(c.cmd("REST 6").await.0, 350);
    let (code, body) = c.get("RETR HELLO.TXT").await;
    assert_eq!((code, body.as_slice()), (226, &b"world"[..]), "REST resumes the download");
    let (code, body) = c.get("RETR HELLO.TXT").await;
    assert_eq!(body.len(), 11, "and REST was used up: {code}");

    assert_eq!(c.put("APPE HELLO.TXT", b"!").await.0, 226);
    assert_eq!(std::fs::read(s.dir.join("SUB").join("HELLO.TXT")).unwrap(), b"hello world!");

    let (code, list) = c.get("LIST").await;
    let list = String::from_utf8(list).unwrap();
    assert!(code == 226 && list.contains(" HELLO.TXT\r\n"), "{list}");
    let (_, names) = c.get("NLST").await;
    assert_eq!(names, b"HELLO.TXT\r\n");
    let (_, mlsd) = c.get("MLSD").await;
    assert_eq!(String::from_utf8(mlsd).unwrap().trim_end(), {
        let m = std::fs::metadata(s.dir.join("SUB").join("HELLO.TXT")).unwrap();
        let t = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        format!("type=file;size=12;modify={};perm=rwadf; HELLO.TXT", proto::time_val(t))
    });

    assert_eq!(c.cmd("RNFR HELLO.TXT").await.0, 350);
    assert_eq!(c.cmd("RNTO BYE.TXT").await.0, 250);
    assert!(s.dir.join("SUB").join("BYE.TXT").exists());
    assert_eq!(c.cmd("RNTO X.TXT").await.0, 503, "RNTO needs a fresh RNFR");
    assert_eq!(c.cmd("DELE BYE.TXT").await.0, 250);
    assert_eq!(c.cmd("CDUP").await.0, 250);
    assert_eq!(c.cmd("RMD SUB").await.0, 250);
    assert!(!s.dir.join("SUB").exists());
    assert_eq!(c.cmd("CWD ../../..").await.0, 250, "above the root is the root");
    assert_eq!(c.cmd("PWD").await.1.trim_end(), "257 \"/\" is the current directory.");
    assert_eq!(c.cmd("QUIT").await.0, 221);
}

/// **The corners answer as the spec says.**  Unsupported types, modes and
/// structures get 504; the X- aliases work; FEAT lists what we do; security
/// extensions are refused so a client falls back to plain FTP; a bounce
/// target is refused; EPRT for IPv6 asks for (1); nothing to abort is 225.
#[tokio::test]
async fn test_the_command_corners() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let _s = Scratch::new("corners", false);
    let mut c = connect(no_lockouts()).await;
    assert_eq!(c.cmd("AUTH TLS").await.0, 502, "before login too");
    assert_eq!(c.cmd("HOST example.com").await.0, 220);
    let (code, feat) = c.cmd("FEAT").await;
    assert_eq!(code, 211);
    for f in ["EPRT", "EPSV", "MDTM", "MLST type*;size*;modify*;perm*;", "REST STREAM", "SIZE", "UTF8", "TVFS"] {
        assert!(feat.contains(&format!(" {f}\r\n")), "FEAT lacks {f}: {feat}");
    }
    c.cmd("USER x").await;
    assert_eq!(c.cmd("PASS y").await.0, 230);
    assert_eq!(c.cmd("HOST example.com").await.0, 503, "HOST only before login");
    assert_eq!(c.cmd("TYPE A").await.0, 200);
    assert_eq!(c.cmd("TYPE L 8").await.0, 200);
    assert_eq!(c.cmd("TYPE E").await.0, 504);
    assert_eq!(c.cmd("TYPE A C").await.0, 504);
    assert_eq!(c.cmd("TYPE Q").await.0, 501);
    assert_eq!(c.cmd("MODE S").await.0, 200);
    assert_eq!(c.cmd("MODE B").await.0, 504);
    assert_eq!(c.cmd("STRU R").await.0, 200);
    assert_eq!(c.cmd("STRU P").await.0, 504);
    assert_eq!(c.cmd("XMKD D").await.0, 257);
    assert_eq!(c.cmd("XCWD D").await.0, 250);
    assert_eq!(c.cmd("XPWD").await.0, 257);
    assert_eq!(c.cmd("XCUP").await.0, 250);
    assert_eq!(c.cmd("XRMD D").await.0, 250);
    assert_eq!(c.cmd("PORT 10,9,9,9,200,1").await.0, 500, "another host is a bounce");
    assert_eq!(c.cmd("PORT 127,0,0,1,0,25").await.0, 500, "a privileged port too");
    assert_eq!(c.cmd("PORT 127,0,0,1,200,1").await.0, 200, "the client itself is fine");
    assert_eq!(c.cmd("EPRT |2|::1|50000|").await.0, 522);
    assert_eq!(c.cmd("EPSV 2").await.0, 522);
    assert_eq!(c.cmd("ABOR").await.0, 225);
    assert_eq!(c.cmd("RNTO X").await.0, 503);
    assert_eq!(c.cmd("SMNT /").await.0, 502);
    assert_eq!(c.cmd("ALLO 100").await.0, 202);
    assert_eq!(c.cmd("FROB").await.0, 500);
    assert_eq!(c.cmd("STAT").await.0, 211);
    assert_eq!(c.cmd("EPSV ALL").await.0, 200);
    assert_eq!(c.cmd("PASV").await.0, 501, "EPSV ALL forbids the rest");
    assert_eq!(c.cmd("REIN").await.0, 220);
    assert_eq!(c.cmd("PWD").await.0, 530, "REIN logs out");
}

/// **New names follow the gateway's rule; existing ones don't have to.**  A
/// name with a space is refused 553 and nothing is written -- but a file that
/// arrived some other way with such a name can still be downloaded.
#[tokio::test]
async fn test_new_names_follow_the_gateway_rule() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("names", false);
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let (code, text) = c.cmd("STOR my file.txt").await;
    assert_eq!(code, 425, "no data connection yet: {text}");
    let _ = c.pasv().await;
    let (code, text) = c.cmd("STOR my file.txt").await;
    assert_eq!(code, 553, "{text}");
    assert!(!s.dir.join("my file.txt").exists());
    assert_eq!(c.cmd("MKD .hidden").await.0, 553);
    std::fs::write(s.dir.join("Old Name.txt"), b"legacy").unwrap();
    let (code, body) = c.get("RETR Old Name.txt").await;
    assert_eq!((code, body.as_slice()), (226, &b"legacy"[..]));
}

/// **A disk the CP/M emulator is using is not touched.**  Overwrite, append,
/// delete and rename of a booted image are refused with the emulator's reason
/// and the file survives; reading it still works; MLSD shows it read-only.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // test serializers, held for the whole test
async fn test_a_disk_in_use_is_refused() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let _reg = crate::cpm::image::registry::tests_lock();
    let s = Scratch::new("inuse", false);
    let disk = s.dir.join("GAME.DSK");
    std::fs::write(&disk, b"guest").unwrap();
    let key = crate::cpm::image::registry::claim_booted_image(&disk).unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let _ = c.pasv().await;
    let (code, text) = c.cmd("STOR GAME.DSK").await;
    assert_eq!(code, 550, "{text}");
    assert!(text.contains("In use by the CP/M emulator."), "{text}");
    let _ = c.pasv().await;
    assert_eq!(c.cmd("APPE GAME.DSK").await.0, 550);
    assert_eq!(c.cmd("DELE GAME.DSK").await.0, 550);
    assert_eq!(c.cmd("RNFR GAME.DSK").await.0, 550);
    assert_eq!(std::fs::read(&disk).unwrap(), b"guest");
    let (code, body) = c.get("RETR GAME.DSK").await;
    assert_eq!((code, body.as_slice()), (226, &b"guest"[..]), "reading is always allowed");
    let (_, mlsd) = c.get("MLSD").await;
    assert!(String::from_utf8(mlsd).unwrap().contains("perm=r; GAME.DSK"));
    crate::cpm::image::registry::release_booted_image(&key);
    assert_eq!(c.cmd("DELE GAME.DSK").await.0, 250, "free again once the boot ends");
}

/// **Refusals leave nothing behind.**  REST past the end of a file -- or into
/// a file that does not exist -- is refused (554) without creating an empty
/// file, and a RETR with no data connection set up is refused (425) before the
/// file is opened.
#[tokio::test]
async fn test_refused_transfers_leave_no_trace() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("refused", false);
    std::fs::write(s.dir.join("SHORT.TXT"), b"abc").unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    assert_eq!(c.cmd("RETR SHORT.TXT").await.0, 425, "no data connection yet");
    let addr = c.pasv().await;
    assert_eq!(c.cmd("REST 10").await.0, 350);
    assert_eq!(c.cmd("STOR NEW.TXT").await.0, 554);
    assert!(!s.dir.join("NEW.TXT").exists(), "a refused REST STOR created the file");
    assert_eq!(c.cmd("REST 10").await.0, 350);
    assert_eq!(c.cmd("RETR SHORT.TXT").await.0, 554);
    // The same passive port as before the refusals: they must not have used it.
    assert_eq!(c.cmd("REST 1").await.0, 350);
    c.w.write_all(b"RETR SHORT.TXT\r\n").await.unwrap();
    let mut data = TcpStream::connect(addr).await.unwrap();
    assert_eq!(c.reply().await.0, 150);
    let mut body = Vec::new();
    data.read_to_end(&mut body).await.unwrap();
    assert_eq!((c.reply().await.0, body.as_slice()), (226, &b"bc"[..]), "the data plan survived the refusals");
}

/// **ABOR mid-upload leaves nothing behind.**  The upload is written beside
/// the target under a temporary name; aborting answers 426 then 226, and
/// neither the target nor the temporary file remains.
#[tokio::test]
async fn test_an_aborted_upload_leaves_no_file() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("abort", false);
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let addr = c.pasv().await;
    c.w.write_all(b"STOR BIG.BIN\r\n").await.unwrap();
    let mut data = TcpStream::connect(addr).await.unwrap();
    assert_eq!(c.reply().await.0, 150);
    data.write_all(&[7u8; 50_000]).await.unwrap();
    // The upload is being written beside the target, under a temporary name.
    let names = || -> Vec<String> {
        std::fs::read_dir(&s.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    };
    assert!(
        poll(|| names().iter().any(|n| n.starts_with(super::store::TEMP_PREFIX))).await,
        "no temporary file appeared: {:?}",
        names()
    );
    assert!(!s.dir.join("BIG.BIN").exists(), "the target was written in place");
    assert_eq!(c.cmd("ABOR").await.0, 426);
    assert_eq!(c.reply().await.0, 226);
    assert!(poll(|| names().is_empty()).await, "an aborted upload left {:?}", names());
}

/// **With security on, the login is the gateway's.**  Anonymous is refused
/// without costing the address a strike; three wrong passwords lock the
/// address out in the shared map (so telnet, SSH and the web page see it); a
/// right one logs in.
#[tokio::test]
async fn test_a_secured_login_and_its_lockout() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let _s = Scratch::new("secure", true);
    let lockouts = no_lockouts();
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    let mut c = connect(lockouts.clone()).await;
    assert_eq!(c.cmd("USER anonymous").await.0, 530);
    assert!(lockouts.lock().unwrap().is_empty(), "anonymous is not a strike");
    assert_eq!(c.cmd("USER admin").await.0, 331);
    assert_eq!(c.cmd("PASS secret").await.0, 230);
    drop(c);

    let mut c = connect(lockouts.clone()).await;
    for attempt in 1..=3 {
        assert_eq!(c.cmd("USER admin").await.0, 331);
        let (code, _) = c.cmd("PASS wrong").await;
        assert_eq!(code, if attempt < 3 { 530 } else { 421 }, "attempt {attempt}");
    }
    assert!(crate::telnet::is_locked_out(&lockouts, ip), "three failures lock the address");
    let mut c = connect(lockouts.clone()).await;
    c.cmd("USER admin").await;
    assert_eq!(c.cmd("PASS secret").await.0, 421, "even the right password, while locked out");
}

/// **A passive data connection takes only the client's own address.**  Port
/// theft (RFC 2577 §5): someone else connecting to the announced port is
/// dropped and the real client still gets the data.  Driven by connecting
/// from a second loopback address, 127.0.0.2, which Linux routes to us.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_a_stranger_cannot_take_the_passive_port() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("theft", false);
    std::fs::write(s.dir.join("F.TXT"), b"mine").unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let addr = c.pasv().await;
    // The thief connects first, from another address.
    let thief = tokio::net::TcpSocket::new_v4().unwrap();
    thief.bind("127.0.0.2:0".parse().unwrap()).unwrap();
    let mut thief = thief.connect(addr).await.unwrap();
    c.w.write_all(b"RETR F.TXT\r\n").await.unwrap();
    let mut data = TcpStream::connect(addr).await.unwrap();
    assert_eq!(c.reply().await.0, 150);
    let mut body = Vec::new();
    data.read_to_end(&mut body).await.unwrap();
    assert_eq!(body, b"mine");
    assert_eq!(c.reply().await.0, 226);
    let mut stolen = Vec::new();
    let _ = thief.read_to_end(&mut stolen).await;
    assert!(stolen.is_empty(), "the thief got {stolen:?}");
}

/// Wildcards: `*` any run, `?` one character, case significant.
#[test]
fn test_glob_match() {
    assert!(proto::glob_match("*.PRG", "GAME.PRG"));
    assert!(!proto::glob_match("*.PRG", "GAME.prg"), "case is significant");
    assert!(proto::glob_match("G?ME*", "GAME.PRG"));
    assert!(proto::glob_match("*", ""));
    assert!(!proto::glob_match("?", ""));
    assert!(proto::glob_match("a*b*c", "aXbYc"));
    assert!(!proto::glob_match("a*b*c", "aXbY"));
    assert!(proto::glob_match("*", "*foo"), "a star facing a literal star is still a wildcard");
    assert!(proto::glob_match("*.c", "*x.c"));
}

/// **`mget *.PRG` works.**  Windows ftp, tnftp and the DOS clients expand a
/// wildcard by sending it to NLST; the server matches it in the folder it
/// names and answers with names RETR can take -- the client's own folder
/// prefix kept.  No match is a 550, as vsftpd answers.
#[tokio::test]
async fn test_nlst_expands_wildcards() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("glob", false);
    for n in ["A.PRG", "B.PRG", "C.TXT"] {
        std::fs::write(s.dir.join(n), b"x").unwrap();
    }
    std::fs::create_dir_all(s.dir.join("SUB")).unwrap();
    std::fs::write(s.dir.join("SUB").join("D.PRG"), b"x").unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let (code, names) = c.get("NLST *.PRG").await;
    assert_eq!((code, String::from_utf8(names).unwrap()), (226, "A.PRG\r\nB.PRG\r\n".to_string()));
    let (_, names) = c.get("NLST SUB/*.PRG").await;
    assert_eq!(String::from_utf8(names).unwrap(), "SUB/D.PRG\r\n", "the folder prefix is kept for RETR");
    let (code, _) = c.get("NLST *.ZIP").await;
    assert_eq!(code, 550);
    let (code, list) = c.get("LIST -l *.TXT").await;
    assert!(code == 226 && String::from_utf8(list).unwrap().ends_with(" C.TXT\r\n"));
}

/// **The jail holds against symlinks.**  A link inside the transfer folder
/// that leads out of it can't be read through, and STOR never writes through
/// a link -- the two ways a link could reach outside.
#[cfg(unix)]
#[tokio::test]
async fn test_a_symlink_cannot_lead_out_of_the_jail() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("symlink", false);
    let outside = std::env::temp_dir().join(format!("egw_ftp_outside_{}", std::process::id()));
    std::fs::write(&outside, b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, s.dir.join("LINK.TXT")).unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let _ = c.pasv().await;
    let (code, text) = c.cmd("RETR LINK.TXT").await;
    assert_eq!(code, 550, "{text}");
    assert!(text.contains("Outside the transfer folder"), "{text}");
    let (code, text) = c.cmd("STOR LINK.TXT").await;
    assert_eq!(code, 550, "{text}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"secret", "nothing was written through the link");
    let _ = std::fs::remove_file(&outside);
}

/// **The CP/M emulator's folders are left alone.**  The `CPM` container and
/// a drive folder can't be renamed or removed (even empty), MLSD says so in
/// `perm`, and files inside them are still reachable.
#[tokio::test]
async fn test_the_emulator_folders_cannot_be_moved() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("cpmdirs", false);
    std::fs::create_dir_all(s.dir.join("CPM").join("A")).unwrap();
    std::fs::write(s.dir.join("CPM").join("A").join("X.COM"), b"x").unwrap();
    std::fs::create_dir_all(s.dir.join("CPM").join("B")).unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    assert_eq!(c.cmd("RNFR CPM").await.0, 550);
    assert_eq!(c.cmd("RNFR CPM/A").await.0, 550);
    assert_eq!(c.cmd("RMD CPM/B").await.0, 550, "even an empty drive folder");
    assert!(s.dir.join("CPM").join("B").is_dir());
    let (_, mlsd) = c.get("MLSD").await;
    assert!(String::from_utf8(mlsd).unwrap().contains("perm=elcmp; CPM"));
    let (code, body) = c.get("RETR CPM/A/X.COM").await;
    assert_eq!((code, body.as_slice()), (226, &b"x"[..]));
    assert_eq!(c.cmd("RNFR CPM/A/X.COM").await.0, 350, "files inside are guarded one by one");
}

/// **A symlinked `CPM` folder is still the emulator's.**  A supported layout
/// keeps the container elsewhere and links it in; neither the link nor a
/// drive folder reached through it may be renamed -- the rename path reads
/// the entry without following links, which is how this slipped past once.
#[cfg(unix)]
#[tokio::test]
async fn test_a_symlinked_cpm_folder_cannot_be_moved() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("cpmlink", false);
    // The real folder inside the jail, so a refusal can only come from the
    // CP/M guard, not from the link leading out of the transfer folder.
    let real = s.dir.join("cpm-store");
    std::fs::create_dir_all(real.join("A")).unwrap();
    std::os::unix::fs::symlink(&real, s.dir.join("CPM")).unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let (code, text) = c.cmd("RNFR CPM").await;
    assert_eq!(code, 550, "the link to the container: {text}");
    assert!(text.contains("Used by the CP/M emulator"), "{text}");
    let (code, text) = c.cmd("RNFR CPM/A").await;
    assert_eq!(code, 550, "a drive folder through it: {text}");
    assert!(text.contains("Used by the CP/M emulator"), "{text}");
    assert!(std::fs::symlink_metadata(s.dir.join("CPM")).unwrap().file_type().is_symlink());
}

/// **One writer per file.**  While one session uploads a file, a second
/// session's STOR, DELE or RNFR of it is told the file is busy (450).
#[tokio::test]
async fn test_a_file_being_uploaded_is_busy() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("busy", false);
    std::fs::write(s.dir.join("F.BIN"), b"old").unwrap();
    let mut a = connect(no_lockouts()).await;
    a.cmd("USER x").await;
    a.cmd("PASS y").await;
    let addr = a.pasv().await;
    a.w.write_all(b"STOR F.BIN\r\n").await.unwrap();
    let mut data = TcpStream::connect(addr).await.unwrap();
    assert_eq!(a.reply().await.0, 150);
    data.write_all(b"new").await.unwrap();
    let mut b = connect(no_lockouts()).await;
    b.cmd("USER x").await;
    b.cmd("PASS y").await;
    let _ = b.pasv().await;
    assert_eq!(b.cmd("STOR F.BIN").await.0, 450);
    assert_eq!(b.cmd("DELE F.BIN").await.0, 450);
    assert_eq!(b.cmd("RNFR F.BIN").await.0, 450);
    data.shutdown().await.unwrap();
    drop(data);
    assert_eq!(a.reply().await.0, 226);
    assert_eq!(std::fs::read(s.dir.join("F.BIN")).unwrap(), b"new");
    assert_eq!(b.cmd("DELE F.BIN").await.0, 250, "free once the upload is done");
}

/// STOU names a fresh file (RFC 1123: the 150 carries it) in the folder the
/// argument gives; MLST answers on the control connection; EPRT carries a
/// real transfer; STAT reports the type; HOST belongs before USER; CWD
/// needs an argument.
#[tokio::test]
async fn test_stou_mlst_eprt_and_small_answers() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let s = Scratch::new("misc", false);
    std::fs::create_dir_all(s.dir.join("SUB")).unwrap();
    std::fs::write(s.dir.join("SUB").join("NOTE"), b"taken").unwrap();
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    assert_eq!(c.cmd("HOST example.com").await.0, 503, "after USER");
    c.cmd("PASS y").await;
    assert_eq!(c.cmd("CWD").await.0, 501);

    let addr = c.pasv().await;
    c.w.write_all(b"STOU SUB/NOTE\r\n").await.unwrap();
    let mut data = TcpStream::connect(addr).await.unwrap();
    let (code, text) = c.reply().await;
    assert_eq!(code, 150);
    assert!(text.contains("FILE: NOTE.1"), "{text}");
    data.write_all(b"fresh").await.unwrap();
    data.shutdown().await.unwrap();
    drop(data);
    assert_eq!(c.reply().await.0, 226);
    assert_eq!(std::fs::read(s.dir.join("SUB").join("NOTE.1")).unwrap(), b"fresh");
    assert_eq!(std::fs::read(s.dir.join("SUB").join("NOTE")).unwrap(), b"taken");

    let (code, text) = c.cmd("MLST SUB/NOTE").await;
    assert_eq!(code, 250);
    assert!(text.contains("type=file;size=5;") && text.contains(" /SUB/NOTE\r\n"), "{text}");

    // EPRT: the server connects back to a listener of ours.
    let back = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = back.local_addr().unwrap().port();
    assert_eq!(c.cmd(&format!("EPRT |1|127.0.0.1|{port}|")).await.0, 200);
    c.w.write_all(b"RETR SUB/NOTE\r\n").await.unwrap();
    let (mut d, _) = back.accept().await.unwrap();
    assert_eq!(c.reply().await.0, 150);
    let mut body = Vec::new();
    d.read_to_end(&mut body).await.unwrap();
    assert_eq!((c.reply().await.0, body.as_slice()), (226, &b"taken"[..]));

    c.cmd("TYPE A").await;
    assert!(c.cmd("STAT").await.1.contains("TYPE: A"));
    c.cmd("TYPE I").await;
    assert!(c.cmd("STAT").await.1.contains("TYPE: I"));
}

/// **An idle session is closed with 421**, as `idle_timeout_secs` says --
/// every other test sets it to 0.
#[tokio::test]
async fn test_an_idle_session_is_closed() {
    let _lock = config::CONFIG_TEST_LOCK.lock().await;
    let _s = Scratch::new("idle", false);
    config::update_config_value("idle_timeout_secs", "1");
    let mut c = connect(no_lockouts()).await;
    c.cmd("USER x").await;
    c.cmd("PASS y").await;
    let (code, text) = c.reply().await;
    assert_eq!(code, 421, "{text}");
}

// ─── the slave -> master store protocol ─────────────────────

mod store_wire {
    use super::super::proto::Entry;
    use super::super::remote::{self, Dec, Enc, MAX_CHUNK, MAX_REPLY, read_frame, write_frame};
    use super::super::store::StoreError;
    use super::*;

    /// A master serving one store channel over an in-memory pipe, and the
    /// slave's end of it.
    fn pair() -> tokio::io::DuplexStream {
        let (slave, master) = tokio::io::duplex(1 << 20);
        tokio::spawn(remote::serve(master, "test-slave".into()));
        slave
    }

    async fn call(ch: &mut tokio::io::DuplexStream, op: u8, payload: Vec<u8>) -> (u8, Vec<u8>) {
        write_frame(ch, op, &payload).await.unwrap();
        read_frame(ch, MAX_REPLY).await.unwrap()
    }

    /// **Every operation runs on the master, and its answers cross intact.**
    /// Upload, stat, list, download from an offset, rename, delete, mkdir and
    /// rmdir, all through `serve` -- which is `LocalStore` on the master's own
    /// transfer directory.  The bytes land in that directory.
    #[tokio::test]
    async fn test_the_master_serves_every_operation() {
        let _lock = config::CONFIG_TEST_LOCK.lock().await;
        let s = Scratch::new("wire", false);
        let mut ch = pair();

        // WRITE /A.TXT, Replace.
        let (tag, _) = call(&mut ch, 4, Enc::default().str("/A.TXT").u8(0).u64(0).0).await;
        assert_eq!(tag, 0x80, "the master opened the upload");
        write_frame(&mut ch, 0x82, b"hello ").await.unwrap();
        write_frame(&mut ch, 0x82, b"master").await.unwrap();
        write_frame(&mut ch, 0x83, &[]).await.unwrap();
        let (tag, _) = read_frame(&mut ch, MAX_REPLY).await.unwrap();
        assert_eq!(tag, 0x80, "saved");
        assert_eq!(std::fs::read(s.dir.join("A.TXT")).unwrap(), b"hello master");

        let (tag, p) = call(&mut ch, 1, Enc::default().str("/A.TXT").0).await;
        assert_eq!(tag, 0x80);
        let e: Entry = remote::decode_entry(&mut Dec::new(&p)).unwrap();
        assert_eq!((e.name.as_str(), e.len, e.is_dir), ("A.TXT", 12, false));

        let (_, p) = call(&mut ch, 2, Enc::default().str("/").0).await;
        let mut d = Dec::new(&p);
        assert_eq!(d.u64().unwrap(), 1);
        assert_eq!(remote::decode_entry(&mut d).unwrap().name, "A.TXT");

        // READ from offset 6.
        let (tag, _) = call(&mut ch, 3, Enc::default().str("/A.TXT").u64(6).0).await;
        assert_eq!(tag, 0x80);
        let mut got = Vec::new();
        loop {
            let (tag, data) = read_frame(&mut ch, MAX_CHUNK).await.unwrap();
            if tag == 0x83 {
                break;
            }
            got.extend_from_slice(&data);
        }
        assert_eq!(read_frame(&mut ch, MAX_REPLY).await.unwrap().0, 0x80);
        assert_eq!(got, b"master");

        assert_eq!(call(&mut ch, 9, Enc::default().str("/A.TXT").str("/B.TXT").0).await.0, 0x80);
        assert!(s.dir.join("B.TXT").exists());
        assert_eq!(call(&mut ch, 6, Enc::default().str("/D").0).await.0, 0x80);
        assert!(s.dir.join("D").is_dir());
        assert_eq!(call(&mut ch, 7, Enc::default().str("/D").0).await.0, 0x80);
        assert_eq!(call(&mut ch, 5, Enc::default().str("/B.TXT").0).await.0, 0x80);
        assert!(!s.dir.join("B.TXT").exists());
        let (tag, p) = call(&mut ch, 10, Enc::default().str("/").str("UP").0).await;
        assert_eq!((tag, Dec::new(&p).str().unwrap()), (0x80, "/UP".to_string()));
    }

    /// **The master's refusals reach the slave's client in its words.**  A
    /// name rule, a missing file and the CP/M guard, each decoded back into
    /// the same `StoreError` -- so the slave's FTP reply is the master's.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test serializers, held for the whole test
    async fn test_refusals_cross_the_wire_intact() {
        let _lock = config::CONFIG_TEST_LOCK.lock().await;
        let _reg = crate::cpm::image::registry::tests_lock();
        let s = Scratch::new("wire_refuse", false);
        let disk = s.dir.join("GAME.DSK");
        std::fs::write(&disk, b"guest").unwrap();
        let key = crate::cpm::image::registry::claim_booted_image(&disk).unwrap();
        let mut ch = pair();

        let (tag, p) = call(&mut ch, 5, Enc::default().str("/GAME.DSK").0).await;
        assert_eq!(tag, 0x81);
        assert_eq!(remote::decode_error(&p), StoreError::Locked("In use by the CP/M emulator.".into()));
        let (_, p) = call(&mut ch, 4, Enc::default().str("/bad name").u8(0).u64(0).0).await;
        assert!(matches!(remote::decode_error(&p), StoreError::BadName(_)));
        let (_, p) = call(&mut ch, 1, Enc::default().str("/NOPE").0).await;
        assert_eq!(remote::decode_error(&p), StoreError::NotFound);
        assert_eq!(std::fs::read(&disk).unwrap(), b"guest");
        crate::cpm::image::registry::release_booted_image(&key);
    }

    /// **A slave that vanishes mid-upload leaves nothing on the master.**
    /// The channel closes after some data and no END: the master drops the
    /// upload, removing its temporary file, and the target never appears.
    #[tokio::test]
    async fn test_a_dropped_upload_leaves_nothing_on_the_master() {
        let _lock = config::CONFIG_TEST_LOCK.lock().await;
        let s = Scratch::new("wire_drop", false);
        let mut ch = pair();
        assert_eq!(call(&mut ch, 4, Enc::default().str("/HALF.BIN").u8(0).u64(0).0).await.0, 0x80);
        write_frame(&mut ch, 0x82, &[1u8; 1000]).await.unwrap();
        let names = || -> Vec<std::ffi::OsString> { std::fs::read_dir(&s.dir).unwrap().map(|e| e.unwrap().file_name()).collect() };
        assert!(poll(|| !names().is_empty()).await, "the upload never started");
        drop(ch);
        assert!(poll(|| names().is_empty()).await, "a dropped upload left {:?}", names());
    }

    /// **The master re-checks every path a slave sends.**  A local session's
    /// paths are normalised before the store sees them; a slave's arrive raw.
    /// `..` components, an empty component, a backslash or a relative path are
    /// refused before anything is touched -- `RNFR /..` would otherwise have
    /// named the transfer directory's parent.
    #[tokio::test]
    async fn test_the_master_refuses_unclean_paths() {
        let _lock = config::CONFIG_TEST_LOCK.lock().await;
        let s = Scratch::new("wire_paths", false);
        std::fs::create_dir_all(s.dir.join("SUB")).unwrap();
        let mut ch = pair();
        for (op, path) in [(8u8, "/.."), (9, "/.."), (1, "/../etc"), (1, "/SUB/../.."), (5, "/a\\b"), (6, "//x"), (1, "SUB"), (6, "/SUB/./X")] {
            let payload = if op == 9 { Enc::default().str(path).str("/MOVED").0 } else { Enc::default().str(path).0 };
            let (tag, p) = call(&mut ch, op, payload).await;
            assert_eq!(tag, 0x81, "op {op} {path:?} was accepted");
            assert_eq!(
                remote::decode_error(&p),
                StoreError::Denied("That path is not allowed.".into()),
                "op {op} {path:?}"
            );
        }
        assert!(s.dir.exists() && s.dir.join("SUB").exists(), "nothing was moved");
        assert!(!s.dir.join("MOVED").exists() && !s.dir.join("X").exists());
    }

    /// **A download is paced by the slave.**  The master sends at most four
    /// frames ahead and then waits; each credit lets one more through.  A
    /// slave that stops reading therefore stops the master -- one slow client
    /// can no longer fill russh's queue and stall every session sharing the
    /// connection.  Credits that arrive after the download ended are ignored.
    #[tokio::test]
    async fn test_a_download_waits_for_credit() {
        let _lock = config::CONFIG_TEST_LOCK.lock().await;
        let s = Scratch::new("wire_credit", false);
        let body: Vec<u8> = (0..10 * MAX_CHUNK).map(|i| (i % 251) as u8).collect();
        std::fs::write(s.dir.join("BIG.BIN"), &body).unwrap();
        let mut ch = pair();
        assert_eq!(call(&mut ch, 3, Enc::default().str("/BIG.BIN").u64(0).0).await.0, 0x80);
        let mut got = Vec::new();
        for _ in 0..4 {
            let (tag, data) = read_frame(&mut ch, MAX_CHUNK).await.unwrap();
            assert_eq!(tag, 0x82);
            got.extend_from_slice(&data);
        }
        let more = tokio::time::timeout(std::time::Duration::from_millis(300), read_frame(&mut ch, MAX_CHUNK)).await;
        assert!(more.is_err(), "the master sent a fifth frame without credit");
        loop {
            write_frame(&mut ch, 0x84, &[]).await.unwrap();
            let (tag, data) = read_frame(&mut ch, MAX_CHUNK).await.unwrap();
            if tag == 0x83 {
                break;
            }
            got.extend_from_slice(&data);
        }
        assert_eq!(read_frame(&mut ch, MAX_REPLY).await.unwrap().0, 0x80);
        assert_eq!(got, body);
        // Stray credits, then an ordinary request: still in step.
        write_frame(&mut ch, 0x84, &[]).await.unwrap();
        write_frame(&mut ch, 0x84, &[]).await.unwrap();
        assert_eq!(call(&mut ch, 1, Enc::default().str("/BIG.BIN").0).await.0, 0x80);
    }

    /// A frame claiming more than the cap is refused before anything is
    /// allocated for it, and the channel is closed.
    #[tokio::test]
    async fn test_an_oversized_request_closes_the_channel() {
        let mut ch = pair();
        let mut head = vec![1u8];
        head.extend_from_slice(&(u32::MAX).to_be_bytes());
        ch.write_all(&head).await.unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(ch.read(&mut buf).await.unwrap(), 0, "the master hung up");
    }
}
