//! The pure parts of the FTP protocol: reading a command line off a Telnet
//! NVT, splitting it, the PORT/EPRT/PASV address forms, virtual paths, and the
//! listing formats.  Nothing here touches a socket or a file, so all of it is
//! tested directly (`super::tests`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Longest command line we accept.  RFC 959 sets no limit; a path is the long
/// part, and 4 KiB is far past any real one while capping what a peer can make
/// us buffer.
pub const MAX_LINE: usize = 4096;

// Telnet command bytes (RFC 854) that can appear on an FTP control connection.
const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;

/// Reads command lines off the control connection, which RFC 959 says is a
/// Telnet NVT: option negotiation is refused, other Telnet commands dropped.
///
/// **Tolerant of the out-of-band ABOR.**  RFC 959 §4.1.3 has a client send
/// `IAC IP` then `IAC DM` with the DM as TCP urgent data, then `ABOR`.
/// Without `SO_OOBINLINE` the urgent byte is taken out of the stream, so what
/// arrives is `IAC IP IAC A B O R` -- an IAC followed by a letter.  That IAC
/// is dropped and the letter kept, so the line still reads `ABOR`.  No socket
/// option needed (and none on Windows).
#[derive(Default)]
pub struct LineReader {
    line: Vec<u8>,
    /// Bytes still to come of a Telnet sequence: 0 none, 1 after IAC, 2 after
    /// IAC + a negotiation verb (the option byte is next).
    pending: u8,
    verb: u8,
    /// The line overran [`MAX_LINE`]; discard to the end of it.
    overflow: bool,
}

/// What feeding a byte produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Fed {
    Nothing,
    /// A complete line (CR stripped), as text.
    Line(String),
    /// A line too long to take; answer 500 and carry on.
    TooLong,
    /// Send these bytes back: a refusal of a Telnet option.
    Reply([u8; 3]),
}

impl LineReader {
    pub fn feed(&mut self, b: u8) -> Fed {
        match self.pending {
            1 => {
                self.pending = 0;
                return match b {
                    IAC => self.push(IAC),
                    WILL | WONT | DO | DONT => {
                        self.pending = 2;
                        self.verb = b;
                        Fed::Nothing
                    }
                    // Any other Telnet command (IP, DM, AO, ...): dropped.
                    240..=250 => Fed::Nothing,
                    // An IAC before an ordinary byte: the urgent DM went
                    // out-of-band.  Drop the IAC, keep the byte.
                    _ => self.push(b),
                };
            }
            2 => {
                self.pending = 0;
                // Refuse everything: DO -> WONT, WILL -> DONT.  The negative
                // answers need no reply (RFC 854 forbids acknowledging them).
                return match self.verb {
                    DO => Fed::Reply([IAC, WONT, b]),
                    WILL => Fed::Reply([IAC, DONT, b]),
                    _ => Fed::Nothing,
                };
            }
            _ => {}
        }
        if b == IAC {
            self.pending = 1;
            return Fed::Nothing;
        }
        self.push(b)
    }

    fn push(&mut self, b: u8) -> Fed {
        if b == b'\n' {
            let overflow = std::mem::take(&mut self.overflow);
            let mut line = std::mem::take(&mut self.line);
            if overflow {
                return Fed::TooLong;
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            // UTF-8 per RFC 2640.  A line that isn't UTF-8 is read as Latin-1
            // (each byte its own character) rather than with replacement
            // characters, so its bytes survive: a Windows ftp sends the
            // console code page, and a name or password arrives as the same
            // characters telnet would have read from those bytes.
            return Fed::Line(match String::from_utf8(line) {
                Ok(text) => text,
                Err(e) => e.into_bytes().iter().map(|&b| b as char).collect(),
            });
        }
        if self.overflow {
            return Fed::Nothing;
        }
        if self.line.len() >= MAX_LINE {
            self.line.clear();
            self.overflow = true;
            return Fed::Nothing;
        }
        self.line.push(b);
        Fed::Nothing
    }
}

/// The argument exactly as sent: everything after the first space, with only
/// the line ending removed.  For PASS, whose argument is a secret that may
/// begin or end with a space.
pub fn raw_argument(line: &str) -> &str {
    let line = line.trim_end_matches(['\r', '\n']);
    line.split_once(' ').map_or("", |(_, arg)| arg)
}

/// Split a command line into an upper-cased verb and its argument.  The
/// argument keeps the spaces inside it (a path may contain them) but loses
/// any before it: RFC 959 says one space, and a doubled one -- a common slip
/// typing into telnet -- would otherwise make the name start with a space.
pub fn split_command(line: &str) -> (String, String) {
    // Trailing blanks are never part of an argument: no name we create can
    // end in one, and a stray space after a bare verb must not become one.
    // ASCII blanks only: `trim()` would also strip U+0085 and U+00A0, which a
    // Latin-1-read line holds for the code-page letters `à` and `á`.
    let line = line.trim_matches(|c: char| c.is_ascii_whitespace());
    match line.split_once(' ') {
        Some((verb, arg)) => (verb.to_ascii_uppercase(), arg.trim_start_matches(' ').to_string()),
        None => (line.trim_end().to_ascii_uppercase(), String::new()),
    }
}

/// Escape a reply for the Telnet control connection: a 0xFF byte in a path is
/// doubled (RFC 2640 §3.1), and CR/LF -- which no reply text may contain
/// mid-line -- become spaces.
pub fn nvt_escape(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for &b in text.as_bytes() {
        match b {
            IAC => out.extend_from_slice(&[IAC, IAC]),
            b'\r' | b'\n' => out.push(b' '),
            _ => out.push(b),
        }
    }
    out
}

/// Format a reply.  One line is `code text`; several are `code-first`,
/// indented middle lines, and `code last` (RFC 959 §4.2).  Middle lines start
/// with a space so none can be mistaken for the end.
pub fn reply(code: u16, lines: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    match lines {
        [] => out.extend_from_slice(format!("{code} \r\n").as_bytes()),
        [one] => {
            out.extend_from_slice(format!("{code} ").as_bytes());
            out.extend_from_slice(&nvt_escape(one));
            out.extend_from_slice(b"\r\n");
        }
        [first, middle @ .., last] => {
            out.extend_from_slice(format!("{code}-").as_bytes());
            out.extend_from_slice(&nvt_escape(first));
            out.extend_from_slice(b"\r\n");
            for m in middle {
                out.push(b' ');
                out.extend_from_slice(&nvt_escape(m));
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(format!("{code} ").as_bytes());
            out.extend_from_slice(&nvt_escape(last));
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

/// The IPv4 form of an address, unwrapping `::ffff:a.b.c.d`.
pub fn as_v4(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    }
}

/// Parse a PORT argument, `h1,h2,h3,h4,p1,p2`.
pub fn parse_port(arg: &str) -> Option<SocketAddr> {
    let parts: Vec<u8> = arg
        .trim()
        .split(',')
        .map(|p| p.trim().parse::<u8>())
        .collect::<Result<_, _>>()
        .ok()?;
    if parts.len() != 6 {
        return None;
    }
    let ip = Ipv4Addr::new(parts[0], parts[1], parts[2], parts[3]);
    let port = u16::from(parts[4]) << 8 | u16::from(parts[5]);
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}

/// What an EPRT argument said (RFC 2428 §2: `<d>proto<d>addr<d>port<d>`).
#[derive(Debug, PartialEq, Eq)]
pub enum Eprt {
    Addr(SocketAddr),
    /// A well-formed request for a protocol we don't speak (`2` = IPv6).
    Unsupported,
}

pub fn parse_eprt(arg: &str) -> Option<Eprt> {
    let arg = arg.trim();
    let d = arg.chars().next()?;
    // RFC 2428: the delimiter is any printable ASCII outside 33..=126's digits
    // and '.'/':' that the fields use; '|' is the recommended one.
    if !d.is_ascii_graphic() || d.is_ascii_digit() || d == '.' || d == ':' {
        return None;
    }
    let fields: Vec<&str> = arg.split(d).collect();
    // "", proto, addr, port, ""
    if fields.len() != 5 || !fields[0].is_empty() || !fields[4].is_empty() {
        return None;
    }
    let port: u16 = fields[3].parse().ok()?;
    match fields[1] {
        "1" => {
            let ip: Ipv4Addr = fields[2].parse().ok()?;
            Some(Eprt::Addr(SocketAddr::new(IpAddr::V4(ip), port)))
        }
        "2" => {
            // Validate it, so a malformed v6 address is a syntax error rather
            // than "unsupported".
            fields[2].parse::<std::net::Ipv6Addr>().ok()?;
            Some(Eprt::Unsupported)
        }
        _ => Some(Eprt::Unsupported),
    }
}

/// May the server open an active-mode data connection to `target`?  Only to
/// the control connection's own peer, and never a privileged port: anything
/// else is the FTP bounce attack (RFC 2577 §3), which would let any client use
/// the gateway to connect anywhere.
pub fn active_target_allowed(target: SocketAddr, control_peer: IpAddr) -> bool {
    let same_host = match (as_v4(target.ip()), as_v4(control_peer)) {
        (Some(a), Some(b)) => a == b,
        _ => target.ip() == control_peer,
    };
    same_host && target.port() >= 1024
}

/// The PASV reply text for `ip:port` (`227 Entering Passive Mode (...)`).
pub fn pasv_text(ip: Ipv4Addr, port: u16) -> String {
    let o = ip.octets();
    format!(
        "Entering Passive Mode ({},{},{},{},{},{})",
        o[0],
        o[1],
        o[2],
        o[3],
        port >> 8,
        port & 0xff
    )
}

/// Resolve a client path against the current virtual directory.
///
/// Virtual paths are always absolute, `/`-separated, with no `.`/`..`
/// components and no trailing slash (except the root itself).  `..` above the
/// root stays at the root -- the client can't name anything outside it, which
/// is the first half of the jail; the store canonicalises for the second.
/// Backslashes are treated as separators too, so a DOS client's `\DIR\FILE`
/// means what it looks like.
pub fn resolve(cwd: &str, arg: &str) -> String {
    let arg = arg.replace('\\', "/");
    let mut parts: Vec<&str> = if arg.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|p| !p.is_empty()).collect()
    };
    for comp in arg.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// The parent of a virtual path, and its last component.  The root has no leaf.
pub fn split_leaf(vpath: &str) -> (String, Option<String>) {
    match vpath.rsplit_once('/') {
        Some((_, "")) | None => ("/".to_string(), None),
        Some(("", leaf)) => ("/".to_string(), Some(leaf.to_string())),
        Some((dir, leaf)) => (dir.to_string(), Some(leaf.to_string())),
    }
}

/// Quote a path for a 257 reply: wrapped in quotes, embedded quotes doubled
/// (RFC 959 Appendix II).
pub fn quote_path(vpath: &str) -> String {
    format!("\"{}\"", vpath.replace('"', "\"\""))
}

/// Strip the `ls`-style options clients put in front of a LIST/NLST path
/// (`LIST -la`, `LIST -a /dir`).  We list everything regardless; an argument
/// that is only options means "the current directory".
pub fn strip_list_options(arg: &str) -> &str {
    let mut rest = arg.trim();
    while rest.starts_with('-') {
        match rest.split_once(' ') {
            Some((_, tail)) => rest = tail.trim_start(),
            None => return "",
        }
    }
    rest
}

/// What a listing needs to know about one entry.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub len: u64,
    /// Seconds since the Unix epoch, UTC.
    pub mtime: i64,
    /// Whether a write/delete/rename of it would be refused: for a file, an
    /// image the CP/M emulator is using or a file a CP/M program is writing;
    /// for a folder, one of the emulator's own.  Shown in MLSD `perm`.
    pub locked: bool,
}

/// One `ls -l` style LIST line.  There is no standard (RFC 959 leaves LIST
/// "human readable"), but every client parses this shape: permissions, links,
/// owner, group, size, date, name.  Within six months the date shows the time,
/// otherwise the year -- as `ls` does, and as clients expect.
pub fn list_line(e: &Entry, now: i64) -> String {
    use chrono::{DateTime, Utc};
    let when = DateTime::<Utc>::from_timestamp(e.mtime, 0).unwrap_or_default();
    let recent = (now - e.mtime).abs() < 180 * 24 * 3600;
    let date = if recent {
        when.format("%b %e %H:%M").to_string()
    } else {
        when.format("%b %e  %Y").to_string()
    };
    let (mode, links) = if e.is_dir { ("drwxr-xr-x", 2) } else { ("-rw-r--r--", 1) };
    format!("{mode} {links:>3} ftp      ftp      {:>12} {date} {}", e.len, e.name)
}

/// The `YYYYMMDDHHMMSS` timestamp MDTM and MLSD use (RFC 3659 §2.3), UTC.
pub fn time_val(mtime: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(mtime, 0)
        .unwrap_or_default()
        .format("%Y%m%d%H%M%S")
        .to_string()
}

/// The MLSD/MLST facts for one entry (RFC 3659 §7): `type`, `size`, `modify`,
/// `perm`.  `perm` says what this session may do: `r` retrieve, `w` store,
/// `a` append, `d` delete, `f` rename for a file; `e` enter, `l` list, `c`
/// create files in, `m` make directories in, `p` delete files in, `d` remove
/// (when empty) and `f` rename for a directory.  A locked file keeps only
/// `r`; a locked folder -- one of the CP/M emulator's own -- loses `d` and `f`.
pub fn mlsx_facts(e: &Entry) -> String {
    if e.is_dir {
        // A locked folder is one of the CP/M emulator's own: entered, listed
        // and written into, never renamed or removed.
        let perm = if e.locked { "elcmp" } else { "elcmpdf" };
        format!("type=dir;modify={};perm={perm};", time_val(e.mtime))
    } else {
        let perm = if e.locked { "r" } else { "rwadf" };
        format!("type=file;size={};modify={};perm={perm};", e.len, time_val(e.mtime))
    }
}

/// Whether a name can appear in a listing at all: a CR or LF would end the
/// line early and forge another entry.
pub fn listable(name: &str) -> bool {
    !name.contains('\r') && !name.contains('\n')
}

/// Match an FTP wildcard: `*` any run of characters, `?` exactly one.  Case
/// is significant, as on the Unix servers clients expect.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None::<usize>, 0);
    while ni < n.len() {
        // A `*` is always a wildcard, even facing a literal `*` in the name.
        if pi < p.len() && p[pi] != '*' && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}
