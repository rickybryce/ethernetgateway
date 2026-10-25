//! Gopher beyond text files and menus: the RFC 1436 item types that are
//! not pages (files, telnet sessions, CSO phone books, redundant servers),
//! and Gopher+ -- item attributes, alternate views and `+ASK` forms.
//!
//! What real servers do, measured, decides several rules here:
//! - **Gopher+ is only asked of items marked for it.**  Floodgap and SDF
//!   ignore a `<TAB>+` or `<TAB>$` and send the plain reply; pygopherd
//!   (quux.org) answers in full.  A menu marks Gopher+ items with a fifth
//!   field, `+` or `?`, and only those get a Gopher+ request.  A reply with no
//!   `+`/`-` header is still taken as the plain reply it is.
//! - **A text file's doubled dots are undone, as RFC 1436 says.**  A server
//!   doubles a leading `.` in a text file so that no line of it can be the
//!   `.` line that ends it, and the client takes one off.  None of Bucktooth
//!   (Floodgap), gophernicus or pygopherd doubles them (measured), so from
//!   those a file's own line that starts `..` loses a dot: the spec is kept,
//!   for the servers that do follow it.  Gopher+ `+-1` data is unstuffed
//!   the same way.
//!
//! The URL carries everything (RFC 4266): `%09<search>%09<gopher+ string>`
//! after the selector, so Back, Reload and bookmarks keep a Gopher+ item,
//! a view or a search.  On the wire the Gopher+ spec is followed instead:
//! `selector<TAB>+` for an item, `selector<TAB>search<TAB>+` for a search.

use super::*;

/// The whole-reply deadline for a file (`fetch_file`).  A page's 15 s is
/// for 512 KB; a file may be 8 MB, which at a slow server's 100 KB/s is 80 s.
/// Still one deadline, so a server trickling bytes cannot hold the session.
const FILE_TIMEOUT_SECS: u64 = 180;

/// The well-known ports for the two item types that are not Gopher.
const TELNET_PORT: u16 = 23;
const CSO_PORT: u16 = 105;

/// A `gopher://` URL taken apart: the decoded selector, and the search
/// string and Gopher+ string that follow it after TABs (RFC 4266 2.2-2.3).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GopherUrl {
    pub host: String,
    pub port: u16,
    pub ty: char,
    /// Octets, not text (RFC 1436): any byte but TAB, CR and LF.
    pub selector: Vec<u8>,
    /// A search's words: octets too, as RFC 4266 has the whole path.
    pub search: Option<Vec<u8>>,
    pub plus: Option<String>,
}

impl GopherUrl {
    pub(crate) fn parse(url: &str) -> Result<Self, String> {
        let (host, port, ty, path) = parse_gopher_url(url)?;
        // A selector holds no TAB (RFC 1436), so the first two are the
        // search's and the Gopher+ string's.
        let mut parts = path.splitn(3, |&b| b == b'\t');
        let selector = parts.next().unwrap_or_default().to_vec();
        let search = parts.next().map(<[u8]>::to_vec);
        // A Gopher+ string is a view's name and language: ASCII.
        let plus = parts.next().map(|p| String::from_utf8_lossy(p).into_owned()).filter(|p| !p.is_empty());
        let search = search.filter(|s| !s.is_empty() || plus.is_none());
        Ok(GopherUrl { host, port, ty, selector, search, plus })
    }

    pub(crate) fn to_url(&self) -> String {
        let mut path = self.selector.clone();
        if self.search.is_some() || self.plus.is_some() {
            path.push(b'\t');
            path.extend_from_slice(self.search.as_deref().unwrap_or_default());
        }
        if let Some(plus) = &self.plus {
            path.push(b'\t');
            path.extend_from_slice(plus.as_bytes());
        }
        build_gopher_url(&self.host, self.port, self.ty, &path)
    }

    /// The same item with another Gopher+ string.
    pub(crate) fn with_plus(&self, plus: &str) -> Self {
        GopherUrl { plus: Some(plus.to_string()), ..self.clone() }
    }

    /// The request line, without its CRLF.  A plain request is the
    /// selector, and a search's TAB and words; a Gopher+ one adds a TAB and
    /// the Gopher+ string -- after an empty search for a search item, as the
    /// Gopher+ spec has it.  CR, LF and NUL are dropped: a selector is one
    /// line, and a typed search must not add a second (old daemons also
    /// stop at a NUL).
    pub(crate) fn request(&self, plus: Option<&str>) -> Vec<u8> {
        let mut line = self.selector.clone();
        if let Some(search) = &self.search {
            line.push(b'\t');
            line.extend_from_slice(search);
        }
        if let Some(plus) = plus {
            if self.search.is_none() && self.ty == '7' {
                line.push(b'\t');
            }
            line.push(b'\t');
            line.extend_from_slice(plus.as_bytes());
        }
        line.retain(|&b| b != b'\r' && b != b'\n' && b != 0);
        line
    }

    /// The selector as text, for a title or a file name: UTF-8 if it is,
    /// else Latin-1 (`decode_text`).
    pub(crate) fn selector_text(&self) -> String {
        decode_text(&self.selector).into_owned()
    }

    /// The item type the reply is drawn as: a Gopher+ view names its own
    /// MIME type, and that wins over the menu's letter.
    fn shown_as(&self) -> char {
        let Some(view) = self.plus.as_deref().and_then(|p| p.strip_prefix('+')).filter(|v| !v.is_empty()) else {
            return self.ty;
        };
        let mime = view.split(' ').next().unwrap_or_default().to_ascii_lowercase();
        match mime.as_str() {
            "text/html" => 'h',
            "application/gopher-menu" | "application/gopher+-menu" => '1',
            m if m.starts_with("text/") => '0',
            _ => '9',
        }
    }
}

/// What following a gopher link does.
#[derive(Debug, PartialEq)]
pub(crate) enum GopherAction {
    /// Fetched and drawn as a page (`fetch_gopher`).
    Page,
    /// A file to save in the transfer folder (`fetch_gopher_file`), and the
    /// name the selector suggests for it.
    Download { name: String },
    /// A telnet session (`8`), or a tn3270 one (`T`): host, port, and the
    /// name to log in as, which RFC 1436 puts in the selector.
    Telnet { host: String, port: u16, login: String, tn3270: bool },
}

/// The item types that are files, not pages: RFC 1436's `5` and `9`
/// (binary), `g` and `I` (images); Gopher+'s `:` `;` `<` (image, movie,
/// sound); and the common extensions `s` (sound), `d` (document), `P`/`p`
/// (PDF, PNG).
pub(super) fn is_file_type(ty: char) -> bool {
    matches!(ty, '5' | '9' | 'g' | 'I' | ':' | ';' | '<' | 's' | 'd' | 'P' | 'p')
}

pub(crate) fn gopher_action(url: &str) -> GopherAction {
    let Ok(g) = GopherUrl::parse(url) else {
        return GopherAction::Page;
    };
    match g.ty {
        '8' | 'T' => GopherAction::Telnet {
            host: g.host.clone(),
            port: g.port,
            login: g.selector_text(),
            tn3270: g.ty == 'T',
        },
        // Attributes and forms are pages whatever the item is.
        _ if matches!(g.plus.as_deref(), Some(p) if p.starts_with('!') || p.starts_with('$') || p == "?") => {
            GopherAction::Page
        }
        _ if is_file_type(g.shown_as()) => {
            let selector = g.selector_text();
            let name = selector.rsplit(['/', '\\', ':']).find(|s| !s.is_empty()).unwrap_or("gopher.bin");
            GopherAction::Download { name: name.to_string() }
        }
        _ => GopherAction::Page,
    }
}

/// A TCP connection to a gopher (or CSO) server, the address checked
/// first: the server names hosts in its menus, so a menu could point the
/// gateway at the LAN behind it.
pub(crate) fn connect(host: &str, port: u16) -> Result<std::net::TcpStream, String> {
    let addr = format!("{}:{}", host, port);
    let sock_addr = {
        use std::net::ToSocketAddrs;
        addr.to_socket_addrs()
            .map_err(|e| format!("{DOWN_DNS}: {e}"))?
            .next()
            .ok_or_else(|| DOWN_NO_HOST.to_string())?
    };
    check_dial(sock_addr.ip(), host)?;
    let stream = std::net::TcpStream::connect_timeout(
        &sock_addr,
        std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS),
    )
    .map_err(|e| format!("{DOWN_CONNECT}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS)))
        .ok();
    Ok(stream)
}

/// Whether `connect` failed because the server could not be reached -- its
/// name did not resolve, or nothing answered -- so a `+` alternate is worth
/// trying.  Its own error messages, so a test pins the two together.
pub(crate) fn server_seems_down(err: &str) -> bool {
    [DOWN_DNS, DOWN_NO_HOST, DOWN_CONNECT].iter().any(|p| err.starts_with(p))
}

const DOWN_DNS: &str = "DNS error";
const DOWN_NO_HOST: &str = "Could not resolve host";
const DOWN_CONNECT: &str = "Connection failed";

/// The SSRF rule for an address a gopher server named: an internal one is
/// refused unless internal fetches are allowed.  Shared with the telnet
/// hand-off, which dials a host a menu chose.
pub(crate) fn check_dial(ip: std::net::IpAddr, host: &str) -> Result<(), String> {
    if !internal_fetch_allowed() && is_internal_ip(ip) {
        return Err(format!("Blocked: {} is an internal address", host));
    }
    Ok(())
}

/// The address to dial for a host a menu named: the first it resolves to,
/// once every one of them has passed `check` -- a connect by name would
/// try the others too.
pub(crate) fn checked_addr(
    addrs: &[std::net::SocketAddr],
    host: &str,
    check: &dyn Fn(std::net::IpAddr, &str) -> Result<(), String>,
) -> Result<std::net::SocketAddr, String> {
    for a in addrs {
        check(a.ip(), host)?;
    }
    addrs.first().copied().ok_or_else(|| "Could not resolve host".to_string())
}

/// Send one request (and any data after it) and read the reply whole,
/// against one deadline.  `done` lets a Gopher+ reply that said how long
/// it is end the read without waiting for the server to close.
fn exchange(
    host: &str,
    port: u16,
    request: &[u8],
    max: usize,
    secs: u64,
    done: &dyn Fn(&[u8]) -> bool,
    ended: &dyn Fn(&[u8]) -> bool,
) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut stream = connect(host, port)?;
    // One deadline for the whole exchange, the request included: a form can
    // send a file, and a server reading it a byte at a time must not hold
    // the session past it.  The time left is set before every write, not
    // every chunk: a write that sends part of one starts its timeout over.
    let slow = || "Write error: the server took too long to take the request".to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut rest = request;
    while !rest.is_empty() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(slow());
        }
        stream
            .set_write_timeout(Some(left.max(std::time::Duration::from_millis(1))))
            .map_err(|e| format!("Write error: {}", e))?;
        match stream.write(&rest[..rest.len().min(8192)]) {
            Ok(0) => return Err("Write error: the server closed the connection".into()),
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                return Err(slow())
            }
            Err(e) => return Err(format!("Write error: {}", e)),
        }
    }
    stream.flush().map_err(|e| format!("Flush error: {}", e))?;
    read_gopher_body_until(&mut stream, max, deadline, done, ended)
}

/// A Gopher+ reply's header: success, and the transfer -- a byte count,
/// `-1` (ends with a `.` line) or `-2` (ends when the server closes) --
/// and the header's length.  `None` when the reply has no header, which is
/// what a server that does not speak Gopher+ sends.
fn plus_header(body: &[u8]) -> Option<(bool, i64, usize)> {
    let end = body.iter().take(24).position(|&b| b == b'\n')?;
    let line = std::str::from_utf8(&body[..end]).ok()?.trim_end_matches('\r');
    let (ok, rest) = match line.as_bytes().first()? {
        b'+' => (true, &line[1..]),
        b'-' => (false, &line[1..]),
        _ => return None,
    };
    let n: i64 = rest.parse().ok()?;
    if n < -2 {
        return None;
    }
    Some((ok, n, end + 1))
}

/// Whether a Gopher+ reply read so far is all there: a counted one has its
/// bytes, a `.`-terminated one its `.` line.  A `-2` one, or one with no
/// header, ends only when the server closes.
fn plus_reply_complete(body: &[u8]) -> bool {
    match plus_header(body) {
        // In u64: a 32-bit `usize` overflows on a header of `+4294967295`.
        Some((_, n, at)) if n >= 0 => body.len() as u64 >= at as u64 + n as u64,
        Some((_, -1, at)) => {
            let data = &body[at..];
            data == b".\r\n" || data.ends_with(b"\n.\r\n")
        }
        _ => false,
    }
}

/// Text framed by a `.` line: up to that line, each other line's doubled
/// leading `.` undone (RFC 1436's rule, kept by Gopher+ `-1` data).
fn unstuff_dot_terminated(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for line in data.split_inclusive(|&b| b == b'\n') {
        if line == b".\r\n" || line == b".\n" || line == b"." {
            break;
        }
        out.extend_from_slice(line.strip_prefix(b".").filter(|l| l.starts_with(b".")).unwrap_or(line));
    }
    out
}

/// A Gopher+ reply's data, or why it failed.
#[derive(Debug, PartialEq)]
pub(crate) enum PlusReply {
    Data(Vec<u8>),
    /// `-` reply: the error code, if it gave one, and its message lines.
    /// Code 3 is "moved", and its first line ends with where to.  Lines
    /// as sent, octets: a "moved" item's selector goes back as it came,
    /// and each line is decoded on its own when shown (`decode_text`).
    Error { code: Option<u32>, lines: Vec<Vec<u8>> },
}

/// Whether a Gopher+ reply arrived whole: a counted one has all its bytes,
/// a `.`-terminated one its `.` line.  A `-2` one, or one with no header,
/// ends when the server closes, so whatever came is the whole of it.
fn plus_reply_whole(body: &[u8]) -> bool {
    match plus_header(body) {
        Some((_, n, _)) if n >= 0 || n == -1 => plus_reply_complete(body),
        _ => true,
    }
}

pub(crate) fn split_plus_reply(body: &[u8]) -> PlusReply {
    let Some((ok, n, at)) = plus_header(body) else {
        return PlusReply::Data(body.to_vec());
    };
    let rest = &body[at..];
    let data = match n {
        -1 => unstuff_dot_terminated(rest),
        -2 => rest.to_vec(),
        n => rest[..rest.len().min(usize::try_from(n).unwrap_or(usize::MAX))].to_vec(),
    };
    if ok {
        return PlusReply::Data(data);
    }
    let mut lines: Vec<Vec<u8>> = byte_lines(&data).map(<[u8]>::to_vec).collect();
    // The first token of the first line is the code; the administrator's
    // address may follow it in angle brackets.
    let code = lines
        .first()
        .and_then(|l| l.split(u8::is_ascii_whitespace).find(|t| !t.is_empty()))
        .and_then(|t| std::str::from_utf8(t).ok()?.parse().ok());
    if code.is_some()
        && let Some(first) = lines.first_mut()
    {
        let line = first.trim_ascii_start();
        let rest = line.iter().position(u8::is_ascii_whitespace).map_or(&[][..], |at| line[at..].trim_ascii());
        *first = rest.to_vec();
    }
    lines.retain(|l| !l.trim_ascii().is_empty());
    PlusReply::Error { code, lines }
}

/// Text a gopher server sent: UTF-8 if it is, else ISO Latin-1, which
/// both specs name for 8-bit text (RFC 1436 and Gopher+, the display
/// string).  Every Latin-1 byte is a character, so a Latin-1 `é` is drawn
/// as itself, not as `?`.
pub(super) fn decode_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => std::borrow::Cow::Borrowed(s),
        Err(_) => std::borrow::Cow::Owned(bytes.iter().map(|&b| b as char).collect()),
    }
}

/// `data`'s lines as `str::lines` splits them, each without its LF or CR LF.
fn byte_lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let any = !data.is_empty();
    let data = data.strip_suffix(b"\n").unwrap_or(data);
    data.split(|&b| b == b'\n').filter(move |_| any).map(|l| l.strip_suffix(b"\r").unwrap_or(l))
}

/// A text reply's lines, each decoded on its own (`decode_text`): one
/// Latin-1 line does not turn a UTF-8 file's other lines to Latin-1.
pub(super) fn text_lines(data: &[u8]) -> Vec<String> {
    byte_lines(data).map(|l| decode_text(l).into_owned()).collect()
}

/// One line of a gopher menu.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuItem {
    pub ty: char,
    pub display: String,
    /// Octets, as the server wrote them (RFC 1436).
    pub selector: Vec<u8>,
    pub host: String,
    pub port: u16,
    /// Gopher+'s fifth field: `+` for a Gopher+ item, `?` for one with a
    /// `+ASK` form.
    pub plus: Option<char>,
}

/// A menu line taken apart.  An empty or missing host is this server, as a
/// missing port is its port -- except for a telnet or CSO item, whose
/// missing (or `0`) port is its own protocol's.
///
/// Taken apart as bytes: the selector is octets, and the display string
/// text in UTF-8 or Latin-1 (`decode_text`).
pub(crate) fn parse_menu_line(line: impl AsRef<[u8]>, current_host: &str, current_port: u16) -> Option<MenuItem> {
    let line = line.as_ref();
    let ty = char::from(*line.first()?);
    let fields: Vec<&[u8]> = line[1..].split(|&b| b == b'\t').collect();
    let field = |i: usize| String::from_utf8_lossy(fields.get(i).copied().unwrap_or_default()).trim().to_string();
    let host = Some(field(2)).filter(|h| !h.is_empty()).unwrap_or_else(|| current_host.to_string());
    let port = field(3).parse::<u16>().ok().filter(|&p| p > 0);
    let port = match ty {
        '8' | 'T' => port.unwrap_or(TELNET_PORT),
        '2' => port.unwrap_or(CSO_PORT),
        _ => port.unwrap_or(current_port),
    };
    let plus = field(4).chars().next().filter(|c| matches!(c, '+' | '?'));
    Some(MenuItem {
        ty,
        display: decode_text(fields.first().copied().unwrap_or_default()).into_owned(),
        selector: fields.get(1).copied().unwrap_or_default().to_vec(),
        host,
        port,
        plus,
    })
}

impl MenuItem {
    /// The link a menu item makes, under `ty` (a `+` redundant server's
    /// is its primary's).  A search or phone book is marked `?search`, so
    /// the browser asks for the words first; a Gopher+ item carries its
    /// marker as the URL's Gopher+ string.
    pub(crate) fn url(&self, ty: char) -> String {
        if ty == 'h'
            && let Some(web) = self.selector.strip_prefix(b"URL:")
        {
            return String::from_utf8_lossy(web).into_owned();
        }
        let g = GopherUrl {
            host: self.host.clone(),
            port: self.port,
            ty,
            selector: self.selector.clone(),
            search: None,
            plus: self.plus.map(|c| c.to_string()),
        };
        let url = g.to_url();
        if matches!(ty, '7' | '2') { format!("{url}?search") } else { url }
    }
}

/// One item's Gopher+ attribute blocks, as `!` and `$` return them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ItemAttrs {
    pub blocks: Vec<AttrBlock>,
}

#[derive(Debug, PartialEq)]
pub(crate) struct AttrBlock {
    /// With its `+`: `+INFO`, `+ADMIN`...
    pub name: String,
    /// What follows the name on its line (`+INFO`'s item, a reference):
    /// octets, as a menu line's are.
    pub head: Vec<u8>,
    /// The block's lines, their leading space taken off.
    pub lines: Vec<String>,
}

impl ItemAttrs {
    pub(crate) fn block(&self, name: &str) -> Option<&AttrBlock> {
        self.blocks.iter().find(|b| b.name == name)
    }
}

/// Attribute information, split per item: each `+INFO` starts one.  Taken
/// apart as bytes, each line decoded on its own (`decode_text`): one
/// Latin-1 line does not garble the others, and a selector stays octets.
pub(crate) fn parse_attributes(data: &[u8]) -> Vec<ItemAttrs> {
    let mut items: Vec<ItemAttrs> = Vec::new();
    for line in byte_lines(data) {
        if let Some(rest) = line.strip_prefix(b"+") {
            let (name, head) = match rest.iter().position(|&b| b == b':') {
                Some(at) => (format!("+{}", decode_text(&rest[..at])), rest[at + 1..].trim_ascii().to_vec()),
                None => (format!("+{}", decode_text(rest.trim_ascii())), Vec::new()),
            };
            if name == "+INFO" || items.is_empty() {
                items.push(ItemAttrs::default());
            }
            if let Some(item) = items.last_mut() {
                item.blocks.push(AttrBlock { name, head, lines: Vec::new() });
            }
        } else if let Some(text) = line.strip_prefix(b" ")
            && let Some(block) = items.last_mut().and_then(|i| i.blocks.last_mut())
        {
            block.lines.push(decode_text(text).into_owned());
        }
    }
    items
}

/// `<YYYYMMDDhhmmss>` from an `+ADMIN` date line, written readably.
fn admin_date(line: &str) -> Option<String> {
    let inner = line.rsplit_once('<')?.1.split('>').next()?;
    if inner.len() != 14 || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let d = |a: usize, b: usize| &inner[a..b];
    Some(format!("{}-{}-{} {}:{}:{}", d(0, 4), d(4, 6), d(6, 8), d(8, 10), d(10, 12), d(12, 14)))
}

/// A page of attribute information: each item, linked, with who keeps it,
/// when it changed, its abstract, its views (each a link that fetches that
/// view) and whether it has a form.  Blocks this does not know are shown as
/// sent.
fn render_attributes(items: &[ItemAttrs], at: &GopherUrl, width: usize, final_url: String) -> WebPage {
    let mut lines: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut link = |lines: &mut Vec<String>, label: &str, url: String| {
        links.push(url);
        let n = links.len();
        for (i, wrapped) in wrap_line(label, width.saturating_sub(5)).iter().enumerate() {
            lines.push(if i == 0 { format!("{wrapped}\x02{n}\x03") } else { format!("  {wrapped}") });
        }
    };
    let text = |lines: &mut Vec<String>, s: &str| lines.extend(wrap_line(s, width));
    for item in items {
        // The item itself, from its `+INFO`.
        let info = item.block("+INFO").and_then(|b| parse_menu_line(&b.head, &at.host, at.port));
        // A menu's `i` line is its text, not an item: shown as text, without
        // the administrator a server repeats for every one of them (quux.org
        // does, measured).
        if let Some(i) = info.as_ref().filter(|i| i.ty == 'i') {
            text(&mut lines, &i.display);
            continue;
        }
        if !lines.is_empty() {
            lines.push(String::new());
        }
        let item_url = info.as_ref().map(|i| GopherUrl::parse(&i.url(i.ty)).ok());
        if let Some(i) = &info {
            link(&mut lines, &i.display, i.url(i.ty));
        }
        let base = item_url.flatten().unwrap_or_else(|| at.clone());
        for block in &item.blocks {
            match block.name.as_str() {
                "+INFO" => {}
                "+ADMIN" => {
                    for l in &block.lines {
                        match admin_date(l) {
                            Some(date) if l.trim_start().starts_with("Mod-Date") => {
                                text(&mut lines, &format!("  Modified: {date}"))
                            }
                            _ => text(&mut lines, &format!("  {}", l.trim())),
                        }
                    }
                }
                "+ABSTRACT" => {
                    if let Some(r) = parse_menu_line(&block.head, &at.host, at.port).filter(|r| !r.selector.is_empty()) {
                        link(&mut lines, "  Abstract (in full)", r.url(r.ty));
                    }
                    for l in &block.lines {
                        text(&mut lines, &format!("  {}", l.trim()));
                    }
                }
                "+VIEWS" => {
                    for view in &block.lines {
                        // `Content-Type[ Language]: [<size>]`
                        let (name, size) = view.split_once(':').unwrap_or((view.as_str(), ""));
                        let name = name.trim();
                        if name.is_empty() {
                            continue;
                        }
                        let size = size.trim().trim_start_matches('<').trim_end_matches('>');
                        let label = if size.is_empty() { format!("  View: {name}") } else { format!("  View: {name} ({size})") };
                        link(&mut lines, &label, base.with_plus(&format!("+{name}")).to_url());
                    }
                }
                "+ASK" => {
                    let questions = parse_ask(&block.lines).1.len();
                    link(&mut lines, &format!("  Form: {questions} question(s)"), base.with_plus("?").to_url());
                }
                other => {
                    text(&mut lines, &format!("  {other}: {}", decode_text(&block.head)));
                    for l in &block.lines {
                        text(&mut lines, &format!("    {}", l.trim()));
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        lines.push("No attributes were sent for this item.".into());
    }
    WebPage {
        title: Some(format!(
            "Gopher+ info: {}",
            at.selector_text().rsplit('/').find(|s| !s.is_empty()).unwrap_or(&at.host)
        )),
        lines,
        links,
        url: final_url,
        forms: Vec::new(),
        fallback_sites: Vec::new(),
        alternates: Vec::new(),
    }
}

/// How an `+ASK` form's fields are sent back -- what `submit_ask` needs
/// beyond the field itself.
pub(crate) const ASK_METHOD: &str = "gopher+ask";
/// `AskF`: a filename the reply is also saved under.
pub(crate) const ASK_SAVE_AS: &str = "askf";
/// `ChooseF`: a file from the transfer folder, sent after the answers.  Its
/// options are filled in by the session, which knows the folder.
pub(crate) const ASK_SEND_FILE: &str = "choosef";
/// `AskL`'s field: several lines, sent after a count of them.
const ASK_LINES: &str = "askl";

/// An `+ASK` block: its `Note:` lines, and its questions as form fields,
/// in order.  The keywords are the Gopher+ spec's, and the 1993 forms UMN
/// published use six of them (`Ask`, `AskP`, `AskL`, `Select`, `Choose`,
/// `Note`).  `Ask: Question<TAB>default`; `Choose: Question<TAB>A<TAB>B`;
/// `Select: Label:1` (checked by default).  An unknown keyword is shown as
/// a note, so no question is silently dropped.
pub(crate) fn parse_ask(lines: &[String]) -> (Vec<String>, Vec<FormField>) {
    let mut notes = Vec::new();
    let mut fields = Vec::new();
    for line in lines {
        let Some((keyword, rest)) = line.split_once(':') else {
            if !line.trim().is_empty() {
                notes.push(line.trim().to_string());
            }
            continue;
        };
        let mut parts = rest.trim_start().split('\t');
        let question = parts.next().unwrap_or_default().trim().to_string();
        let default = parts.next().unwrap_or_default().to_string();
        let name = fields.len().to_string();
        let text = |label: String, value: String, input_type: &str| FormField::Text {
            name: name.clone(),
            value,
            label,
            input_type: input_type.to_string(),
        };
        match keyword.trim() {
            "Note" => notes.push(rest.trim().to_string()),
            "Ask" => fields.push(text(question, default, "text")),
            "AskP" => fields.push(text(question, default, "password")),
            "AskF" => fields.push(text(question, default, ASK_SAVE_AS)),
            "AskL" => fields.push(FormField::TextArea {
                name: format!("{name}:{ASK_LINES}"),
                value: default,
                label: if question.is_empty() { "Text".into() } else { question },
            }),
            "Select" => {
                let (label, on) = match rest.trim().rsplit_once(':') {
                    Some((l, v)) if matches!(v.trim(), "0" | "1") => (l.trim().to_string(), v.trim() == "1"),
                    _ => (rest.trim().to_string(), false),
                };
                fields.push(FormField::Checkbox { name, value: "1".into(), checked: on, label });
            }
            "Choose" => {
                let options: Vec<(String, String)> = std::iter::once(default)
                    .chain(parts.map(str::to_string))
                    .filter(|o| !o.is_empty())
                    .map(|o| (o.clone(), o))
                    .collect();
                fields.push(FormField::Select { name, options, selected: 0, label: question });
            }
            "ChooseF" => fields.push(FormField::Select {
                name: ASK_SEND_FILE.into(),
                options: Vec::new(),
                selected: usize::MAX,
                label: if question.is_empty() { "File to send".into() } else { question },
            }),
            _ => notes.push(line.trim().to_string()),
        }
    }
    (notes, fields)
}

/// The page an `+ASK` item opens on: its notes and abstract, and the form.
fn render_ask(item: &ItemAttrs, at: &GopherUrl, width: usize, final_url: String) -> Result<WebPage, String> {
    let ask = item.block("+ASK").ok_or("This item's form was not sent (no +ASK block).")?;
    let (notes, fields) = parse_ask(&ask.lines);
    let display = item
        .block("+INFO")
        .and_then(|b| parse_menu_line(&b.head, &at.host, at.port))
        .map(|i| i.display)
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "Form".to_string());
    let mut lines: Vec<String> = Vec::new();
    for note in &notes {
        lines.extend(wrap_line(note, width));
    }
    if let Some(abstract_) = item.block("+ABSTRACT") {
        for l in &abstract_.lines {
            lines.extend(wrap_line(l.trim(), width));
        }
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }
    if fields.is_empty() {
        lines.push("This form has no questions; F sends it.".into());
    } else {
        lines.extend(wrap_line(&format!("{} question(s): F to answer them and send.", fields.len()), width));
    }
    Ok(WebPage {
        title: Some(display.clone()),
        lines,
        links: Vec::new(),
        url: final_url,
        forms: vec![WebForm {
            action: at.with_plus("+").to_url(),
            method: ASK_METHOD.into(),
            label: display,
            fields,
        }],
        fallback_sites: Vec::new(),
        alternates: Vec::new(),
    })
}

/// The answers to an `+ASK` form, as its data block carries them: one line
/// per question in order (`Note` has none), a checkbox `1` or `0`, `AskL`'s
/// count of lines then the lines; then the `ChooseF` file, if any.
fn ask_answers(form: &WebForm, file: Option<&[u8]>) -> Vec<u8> {
    let one_line = |s: &str| s.chars().filter(|&c| c != '\r' && c != '\n' && c != '\0').collect::<String>();
    let mut out = Vec::new();
    let mut line = |s: &str| {
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\r\n");
    };
    for field in &form.fields {
        match field {
            FormField::Text { value, .. } => line(&one_line(value)),
            FormField::TextArea { value, .. } => {
                let rows: Vec<String> = value.lines().map(one_line).collect();
                line(&rows.len().to_string());
                for row in &rows {
                    line(row);
                }
            }
            FormField::Checkbox { checked, .. } => line(if *checked { "1" } else { "0" }),
            FormField::Select { options, selected, .. } => {
                line(&one_line(options.get(*selected).map(|(v, _)| v.as_str()).unwrap_or("")))
            }
            FormField::Radio { .. } | FormField::Hidden { .. } => {}
        }
    }
    if let Some(file) = file {
        out.extend_from_slice(file);
    }
    out
}

/// Send an `+ASK` form's answers and draw the reply.  Also returns the
/// reply's bytes, for an `AskF` to save.
pub(crate) fn submit_ask(form: &WebForm, file: Option<&[u8]>, width: usize) -> Result<(WebPage, Vec<u8>), String> {
    let g = GopherUrl::parse(&form.action)?;
    let data = ask_answers(form, file);
    // `selector<TAB>+<TAB>1`, then the data block -- counted, so nothing in
    // the answers can end it early.
    let mut request = g.request(Some("+"));
    request.extend_from_slice(format!("\t1\r\n+{}\r\n", data.len()).as_bytes());
    request.extend_from_slice(&data);
    let body = exchange(&g.host, g.port, &request, GOPHER_MAX_BODY, ask_secs(file), &plus_reply_complete, &|_| false)?;
    let data = match split_plus_reply(&body) {
        PlusReply::Data(d) => d,
        PlusReply::Error { lines, .. } => return Err(plus_error_text(&lines)),
    };
    let page = render_gopher_reply(&g, &data, width, g.to_url())?;
    Ok((page, data))
}

fn too_large(max: usize) -> String {
    format!("The file is larger than {} KB, the most the gateway can send on.", max / 1024)
}

/// How long a form's exchange may take: a file's worth of time when a file
/// goes with the answers.
fn ask_secs(file: Option<&[u8]>) -> u64 {
    if file.is_some() { FILE_TIMEOUT_SECS } else { GOPHER_TIMEOUT_SECS }
}

fn plus_error_text(lines: &[Vec<u8>]) -> String {
    let text = lines.iter().map(|l| decode_text(l)).collect::<Vec<_>>().join(" ");
    if text.trim().is_empty() { "The server refused the request.".into() } else { format!("Server: {text}") }
}

/// A CSO phone-book query (RFC 1436 type `2`): `query`, then `quit`, and
/// the replies drawn one entry at a time.  A reply line is
/// `code:text`, or `-code:entry:field: value` for an entry's fields;
/// 5xx is an error.
fn cso_query(g: &GopherUrl, width: usize) -> Result<WebPage, String> {
    let mut words = g.search.clone().unwrap_or_default();
    words.retain(|&b| b != b'\r' && b != b'\n' && b != 0);
    let words = words.trim_ascii();
    if words.is_empty() {
        return Err("Type a name to look up.".into());
    }
    let request = [b"query ", words, b"\r\nquit\r\n"].concat();
    let body = exchange(&g.host, g.port, &request, GOPHER_MAX_BODY, GOPHER_TIMEOUT_SECS, &|_| false, &|_| false)?;
    // Line by line (`text_lines`): one Latin-1 entry does not garble the rest.
    let text = text_lines(&body).join("\n");
    let mut lines: Vec<String> = Vec::new();
    let mut entry: Option<String> = None;
    for raw in text.lines().map(|l| l.trim_end_matches('\r')) {
        let line = raw.strip_prefix('-').unwrap_or(raw);
        let Some((code, rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(code) = code.trim().parse::<u32>() else {
            continue;
        };
        match (code / 100, rest.split_once(':')) {
            // An entry's field: `index:field: value`.
            (2, Some((index, field))) if index.trim().parse::<u32>().is_ok() => {
                if entry.as_deref() != Some(index.trim()) {
                    if entry.is_some() {
                        lines.push(String::new());
                    }
                    entry = Some(index.trim().to_string());
                }
                lines.extend(wrap_line(field.trim(), width));
            }
            // `200:Bye!` and `200:Ok.` say nothing worth showing.
            (2, _) => {}
            (5, _) => lines.extend(wrap_line(rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == ':').trim(), width)),
            _ => lines.extend(wrap_line(rest.trim(), width)),
        }
    }
    if lines.is_empty() {
        lines.push("The phone book sent nothing back.".into());
    }
    Ok(WebPage {
        title: Some(format!("Phone book: {}", g.host)),
        lines,
        links: Vec::new(),
        url: g.to_url(),
        forms: Vec::new(),
        fallback_sites: Vec::new(),
        alternates: Vec::new(),
    })
}

/// A line of a text file as RFC 1436 sends it: a leading `.` doubled, so
/// that it cannot be the `.` line ending the file.  One is taken off.
fn unstuff_line(line: &str) -> &str {
    line.strip_prefix('.').filter(|l| l.starts_with('.')).unwrap_or(line)
}

/// Draw a reply's data as the item type it is shown as (`shown_as`).
fn render_gopher_reply(g: &GopherUrl, data: &[u8], width: usize, final_url: String) -> Result<WebPage, String> {
    let ty = g.shown_as();
    // A text file or HTML page ends with the protocol's `.` line on a
    // server that sends one (RFC 1436), and not on one that sends the file
    // as it is.  Bucktooth (Floodgap) writes `\r\n.\r\n`; SDF writes the
    // file alone (both measured).  Only that CRLF line is dropped: a file
    // whose own last line is `.` keeps it, Unix line ends and all.
    let data = match ty {
        '0' | '4' | '6' | 'h' => strip_gopher_terminator(data),
        _ => data,
    };
    Ok(match ty {
        // Text, and the two text encodings of a file (BinHex, uuencode),
        // which RFC 1436 sends as text.
        '0' | '4' | '6' => {
            let lines: Vec<String> = text_lines(data)
                .iter()
                .flat_map(|line| wrap_line(unstuff_line(line), width))
                .take(MAX_RENDERED_LINES)
                .collect();
            WebPage {
                title: Some(g.selector_text().rsplit('/').next().unwrap_or("Text").to_string()),
                lines,
                links: Vec::new(),
                url: final_url,
                forms: Vec::new(),
                fallback_sites: Vec::new(),
                alternates: Vec::new(),
            }
        }
        '1' | '7' => render_gopher_directory(data, &g.host, g.port, width, final_url)?,
        // An HTML document the gopher server holds itself (an `h` item
        // whose selector is not a `URL:` link): drawn as a web page is.
        // Its refresh, if any, is not followed -- that is an HTTP fetch.
        'h' => render_html_body(data, final_url, width)?.0,
        _ => {
            let lines: Vec<String> = text_lines(data)
                .iter()
                .flat_map(|line| wrap_line(line, width))
                .take(MAX_RENDERED_LINES)
                .collect();
            WebPage {
                title: Some(format!("Gopher (type {})", ty)),
                lines,
                links: Vec::new(),
                url: final_url,
                forms: Vec::new(),
                fallback_sites: Vec::new(),
                alternates: Vec::new(),
            }
        }
    })
}

/// A page for a Gopher+ error: its message, and for "moved" (code 3), a
/// link to where the item went.
fn render_plus_error(code: Option<u32>, lines: &[Vec<u8>], g: &GopherUrl, width: usize) -> Result<WebPage, String> {
    let moved = (code == Some(3)).then(|| {
        lines.iter().filter(|l| l.contains(&b'\t')).find_map(|l| parse_menu_line(l.trim_ascii(), &g.host, g.port))
    }).flatten();
    let Some(to) = moved else {
        return Err(plus_error_text(lines));
    };
    let mut out: Vec<String> = Vec::new();
    for l in lines.iter().filter(|l| !l.contains(&b'\t')) {
        out.extend(wrap_line(&decode_text(l), width));
    }
    out.push(String::new());
    out.push(format!("Moved to: {}\x021\x03", to.display));
    Ok(WebPage {
        title: Some("Item has moved".into()),
        lines: out,
        links: vec![to.url(to.ty)],
        url: g.to_url(),
        forms: Vec::new(),
        fallback_sites: Vec::new(),
        alternates: Vec::new(),
    })
}

/// Fetch a gopher item and draw it.  See the module notes for which
/// requests are Gopher+ ones.
pub(crate) fn fetch(url: &str, width: usize) -> Result<WebPage, String> {
    let g = GopherUrl::parse(url)?;
    let final_url = g.to_url();
    if g.ty == '2' {
        return cso_query(&g, width);
    }
    // Which Gopher+ request, if any: a form's attributes, attributes asked
    // for, a view, or the item's default view.
    let plus = match g.plus.as_deref() {
        Some("?") => Some("!"),
        p => p,
    };
    // A plain text file or menu ends with the `.` line (RFC 1436), and a
    // server that keeps the connection open after it is not waited out --
    // nor one that ignored a Gopher+ request and sent the plain reply.
    let lined = matches!(g.shown_as(), '0' | '1' | '4' | '6' | '7' | 'h');
    let body = exchange(
        &g.host,
        g.port,
        &[g.request(plus), b"\r\n".to_vec()].concat(),
        GOPHER_MAX_BODY,
        GOPHER_TIMEOUT_SECS,
        &|b| plus.is_some() && plus_reply_complete(b),
        // A plain reply's first line is text, even one that looks like a
        // Gopher+ header.
        &|b| lined && (plus.is_none() || plus_header(b).is_none()) && ends_with_gopher_lastline(b),
    )?;
    let Some(plus) = plus else {
        return render_gopher_reply(&g, &body, width, final_url);
    };
    let attributes = plus.starts_with('!') || plus.starts_with('$');
    if attributes && plus_header(&body).is_none() && !body.starts_with(b"+INFO") {
        // A server that does not speak Gopher+ sent the item itself.
        return Err("No Gopher+ information: this server does not speak Gopher+.".into());
    }
    let data = match split_plus_reply(&body) {
        PlusReply::Data(d) => d,
        PlusReply::Error { code, lines } => return render_plus_error(code, &lines, &g, width),
    };
    if !attributes {
        return render_gopher_reply(&g, &data, width, final_url);
    }
    let items = parse_attributes(&data);
    if g.plus.as_deref() == Some("?") {
        let item = items.first().ok_or("This item's form was not sent.")?;
        return render_ask(item, &g, width, final_url);
    }
    Ok(render_attributes(&items, &g, width, final_url))
}

/// Fetch a file item whole, for the transfer folder, refusing one larger
/// than `max`.
pub(crate) fn fetch_file(url: &str, max: usize) -> Result<Vec<u8>, String> {
    let g = GopherUrl::parse(url)?;
    let plus = g.plus.as_deref();
    // One byte past the limit, to tell "exactly max" from "more".
    let body = exchange(
        &g.host,
        g.port,
        &[g.request(plus), b"\r\n".to_vec()].concat(),
        max + 64,
        FILE_TIMEOUT_SECS,
        &|b| plus.is_some() && plus_reply_complete(b),
        // A binary file is read until the server closes (RFC 1436).
        &|_| false,
    )?;
    // Stopped at the cap, or said it is bigger: too large, not cut short.
    let said = if plus.is_some() { plus_header(&body).map_or(0, |(_, n, _)| n) } else { 0 };
    if body.len() >= max + 64 || said > max as i64 {
        return Err(too_large(max));
    }
    // A file cut short is not saved as if it were whole.
    if plus.is_some() && !plus_reply_whole(&body) {
        return Err("The download was cut short; nothing was saved.".into());
    }
    let data = if plus.is_some() {
        match split_plus_reply(&body) {
            PlusReply::Data(d) => d,
            PlusReply::Error { lines, .. } => return Err(plus_error_text(&lines)),
        }
    } else {
        body
    };
    if data.len() > max {
        return Err(too_large(max));
    }
    if data.is_empty() {
        return Err("The server sent an empty file.".into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one connection: everything the client sent (read until it
    /// pauses), answered with `reply`.
    fn serve(reply: Vec<u8>) -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_millis(300))).ok();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(&reply).unwrap();
            got
        });
        (port, h)
    }

    #[test]
    fn test_a_gopher_url_round_trips_its_search_and_plus_string() {
        let g = GopherUrl::parse("gopher://h.example:7070/7/find%09c64%09+").unwrap();
        assert_eq!((g.ty, g.selector.as_slice(), g.search.as_deref(), g.plus.as_deref()), ('7', &b"/find"[..], Some(&b"c64"[..]), Some("+")));
        assert_eq!(g.to_url(), "gopher://h.example:7070/7/find%09c64%09+");
        // RFC 4266 2.3: a Gopher+ string after an empty search.
        let g = GopherUrl::parse("gopher://h.example/0/a%09%09+text/plain%20En_US").unwrap();
        assert_eq!((g.search.as_deref(), g.plus.as_deref()), (None::<&[u8]>, Some("+text/plain En_US")));
        assert_eq!(g.to_url(), "gopher://h.example/0/a%09%09+text/plain%20En_US");
        // A plain search, and a plain item, are as they always were.
        assert_eq!(GopherUrl::parse("gopher://h/7/v%09hi").unwrap().to_url(), "gopher://h/7/v%09hi");
        assert_eq!(GopherUrl::parse("gopher://h/1/x").unwrap().to_url(), "gopher://h/1/x");
        assert_eq!(build_gopher_search_url("gopher://h/7/v%09%09+?search", b"c 64"), "gopher://h/7/v%09c%2064%09+");
    }

    /// On the wire the Gopher+ spec, not RFC 4266's example: one TAB before
    /// the Gopher+ string for an item, an empty search before it for a search.
    #[test]
    fn test_a_gopher_plus_request_line() {
        let item = GopherUrl::parse("gopher://h/0/a%09%09+").unwrap();
        assert_eq!(item.request(None), b"/a");
        assert_eq!(item.request(Some("+")), b"/a\t+");
        assert_eq!(item.request(Some("!")), b"/a\t!");
        let search = GopherUrl::parse("gopher://h/7/s%09%09+").unwrap();
        assert_eq!(search.request(Some("!")), b"/s\t\t!");
        let asked = GopherUrl::parse("gopher://h/7/s%09c64%09+").unwrap();
        assert_eq!(asked.request(Some("+")), b"/s\tc64\t+");
        // Nothing typed can add a line.
        let typed = GopherUrl::parse("gopher://h/7/s%09a%0D%0Ab").unwrap();
        assert_eq!(typed.request(None), b"/s\tab");
        // A search's words are octets too (RFC 4266): a Latin-1 word goes
        // as itself, and the URL keeps it.
        let latin = GopherUrl::parse("gopher://h/7/find%09caf%E9").unwrap();
        assert_eq!(latin.request(None), b"/find\tcaf\xe9");
        assert_eq!(latin.to_url(), "gopher://h/7/find%09caf%E9");
    }

    #[test]
    fn test_gopher_plus_reply_headers() {
        assert_eq!(split_plus_reply(b"+5\r\nhello world"), PlusReply::Data(b"hello".to_vec()));
        assert_eq!(split_plus_reply(b"+-2\r\nall of it"), PlusReply::Data(b"all of it".to_vec()));
        // `-1`: up to the `.` line, its doubled dots undone.
        assert_eq!(
            split_plus_reply(b"+-1\r\n..dot\r\n...and so\r\nplain\r\n.\r\nafter"),
            PlusReply::Data(b".dot\r\n..and so\r\nplain\r\n".to_vec())
        );
        // No header: a server that does not speak Gopher+, taken as is.
        assert_eq!(split_plus_reply(b"just text\r\n"), PlusReply::Data(b"just text\r\n".to_vec()));
        assert_eq!(
            split_plus_reply(b"--2\r\n1 Admin <a@b>\r\nNot here\r\n"),
            PlusReply::Error { code: Some(1), lines: vec![b"Admin <a@b>".to_vec(), b"Not here".to_vec()] }
        );
        // A length past what a 32-bit `usize` holds is a reply not yet
        // whole, not an overflow.
        assert!(!plus_reply_complete(b"+4294967295\r\nabc"));
        assert!(!plus_reply_complete(b"+9223372036854775807\r\nabc"));
        assert!(plus_reply_complete(b"+3\r\nabc"));
        assert!(!plus_reply_complete(b"+3\r\nab"));
        assert!(plus_reply_complete(b"+-1\r\nx\r\n.\r\n"));
        assert!(!plus_reply_complete(b"+-2\r\nx"));
        assert!(!plus_reply_complete(b"plain"));
    }

    #[test]
    fn test_a_menu_line_with_a_gopher_plus_field() {
        let item = parse_menu_line("0Doc\t/d\th.example\t70\t+", "x", 70).unwrap();
        assert_eq!(item.plus, Some('+'));
        assert_eq!(item.url('0'), "gopher://h.example/0/d%09%09+");
        let ask = parse_menu_line("1Form\t/f\th.example\t70\t?", "x", 70).unwrap();
        assert_eq!(ask.url('1'), "gopher://h.example/1/f%09%09%3F");
        assert_eq!(GopherUrl::parse(&ask.url('1')).unwrap().plus.as_deref(), Some("?"));
        // Anything else in the fifth field is not Gopher+.
        assert_eq!(parse_menu_line("0Doc\t/d\th\t70\tjunk", "x", 70).unwrap().plus, None);
        // A Gopher+ search keeps its marker through the search prompt.
        let search = parse_menu_line("7Find\t/s\th\t70\t+", "x", 70).unwrap().url('7');
        assert_eq!(search, "gopher://h/7/s%09%09+?search");
    }

    /// The `+ASK` keywords UMN's published forms use, and the spec's two
    /// file ones, become form fields; `Note` is shown, not asked.
    #[test]
    fn test_an_ask_block_becomes_a_form() {
        let lines: Vec<String> = [
            "Note: Mix paints together",
            "Ask: Name:\tRicky",
            "AskP: Password:",
            "AskL: Message",
            "Select: Red:1",
            "Select: Blue:0",
            "Choose: Size:\tXL\tL",
            "AskF: Save as:\treply.txt",
            "ChooseF: Send which file?",
            "Wibble: what",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (notes, fields) = parse_ask(&lines);
        assert_eq!(notes, vec!["Mix paints together", "Wibble: what"]);
        assert_eq!(fields.len(), 8);
        assert!(matches!(&fields[0], FormField::Text { value, input_type, label, .. } if value == "Ricky" && input_type == "text" && label == "Name:"));
        assert!(matches!(&fields[1], FormField::Text { input_type, .. } if input_type == "password"));
        assert!(matches!(&fields[2], FormField::TextArea { label, .. } if label == "Message"));
        assert!(matches!(&fields[3], FormField::Checkbox { checked: true, label, .. } if label == "Red"));
        assert!(matches!(&fields[4], FormField::Checkbox { checked: false, .. }));
        assert!(matches!(&fields[5], FormField::Select { options, selected: 0, .. } if options.len() == 2 && options[0].0 == "XL"));
        assert!(matches!(&fields[6], FormField::Text { input_type, value, .. } if input_type == ASK_SAVE_AS && value == "reply.txt"));
        assert!(matches!(&fields[7], FormField::Select { name, .. } if name == ASK_SEND_FILE));
    }

    /// The answers go one line per question in order; a checkbox as 1 or 0;
    /// `AskL` as a count and its lines; then the chosen file.  The data
    /// block is counted, so an answer that is a `.` cannot end it.
    #[test]
    fn test_an_ask_form_is_sent_as_the_spec_says() {
        let (_, mut fields) = parse_ask(&[
            "Ask: Name:".to_string(),
            "AskL: Message".to_string(),
            "Select: Red:0".to_string(),
            "Choose: Size:\tXL\tL".to_string(),
            "ChooseF: File".to_string(),
        ]);
        if let FormField::Text { value, .. } = &mut fields[0] {
            *value = ".".into();
        }
        if let FormField::TextArea { value, .. } = &mut fields[1] {
            *value = "one\ntwo".into();
        }
        if let FormField::Select { selected, .. } = &mut fields[3] {
            *selected = 1;
        }
        if let FormField::Select { options, selected, .. } = &mut fields[4] {
            *options = vec![("f.txt".into(), "f.txt".into())];
            *selected = 0;
        }
        let (port, server) = serve(b"+-1\r\nThanks\r\n.\r\n".to_vec());
        let form = WebForm {
            action: format!("gopher://127.0.0.1:{port}/0/order%09%09+"),
            method: ASK_METHOD.into(),
            label: "Order".into(),
            fields,
        };
        let (page, raw) = submit_ask(&form, Some(b"FILE"), 72).unwrap();
        let sent = server.join().unwrap();
        let answers = b".\r\n2\r\none\r\ntwo\r\n0\r\nL\r\nf.txt\r\nFILE";
        let mut want = format!("/order\t+\t1\r\n+{}\r\n", answers.len()).into_bytes();
        want.extend_from_slice(answers);
        assert_eq!(String::from_utf8_lossy(&sent), String::from_utf8_lossy(&want));
        assert_eq!(page.lines, vec!["Thanks"]);
        assert_eq!(raw, b"Thanks\r\n");
    }

    /// A `?` item opens on its form: its attributes asked for with `!`.
    #[test]
    fn test_a_form_item_opens_on_its_questions() {
        let attrs = b"+-2\r\n+INFO: 0Order a shirt\t/shirt\th\t70\t?\r\n+ADMIN:\r\n Admin: A <a@b>\r\n+ASK:\r\n Note: Pick one\r\n Ask: Name:\r\n Choose: Size:\tXL\tL\r\n";
        let (port, server) = serve(attrs.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/shirt%09%09%3F"), 72).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "/shirt\t!\r\n");
        assert_eq!(page.title.as_deref(), Some("Order a shirt"));
        assert!(page.lines[0].contains("Pick one"), "{:?}", page.lines);
        assert_eq!(page.forms.len(), 1);
        assert_eq!(page.forms[0].method, ASK_METHOD);
        assert_eq!(page.forms[0].fields.len(), 2);
        assert_eq!(page.forms[0].action, format!("gopher://127.0.0.1:{port}/0/shirt%09%09+"));
    }

    /// A selector is octets (RFC 1436, RFC 4266): a Latin-1 one goes from
    /// the menu, through the URL as `%E9`, to the server byte for byte.  A
    /// display string is UTF-8 if it is, else Latin-1 (both specs name it).
    #[test]
    fn test_a_latin1_selector_reaches_its_server_as_it_was() {
        let menu = b"0Caf\xe9 menu\t/caf\xe9.txt\th.example\t70\r\n0Na\xc3\xafve\t/na\xc3\xafve\th.example\t70\r\n.\r\n";
        let (port, server) = serve(menu.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/1/"), 72).unwrap();
        server.join().unwrap();
        assert!(page.lines[0].starts_with("Caf\u{e9} menu"), "{:?}", page.lines);
        assert!(page.lines[1].starts_with("Na\u{ef}ve"), "{:?}", page.lines);
        assert_eq!(page.links[0], "gopher://h.example/0/caf%E9.txt");
        assert_eq!(page.links[1], "gopher://h.example/0/na%C3%AFve");
        let g = GopherUrl::parse(&page.links[0]).unwrap();
        assert_eq!(g.selector, b"/caf\xe9.txt");
        assert_eq!(g.request(None), b"/caf\xe9.txt");
        // And on the wire.
        let (port, server) = serve(b"x\r\n".to_vec());
        fetch(&format!("gopher://127.0.0.1:{port}/0/caf%E9.txt"), 72).unwrap();
        assert_eq!(server.join().unwrap(), b"/caf\xe9.txt\r\n");
        // A text file: each line read as UTF-8 or Latin-1 on its own.
        let (port, server) = serve(b"caf\xe9\r\ncaf\xc3\xa9\r\n".to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/t"), 72).unwrap();
        server.join().unwrap();
        assert_eq!(page.lines, vec!["caf\u{e9}", "caf\u{e9}"]);
        assert_eq!(text_lines(b"a\nb\n"), vec!["a", "b"]);
        assert_eq!(text_lines(b"a\nb"), vec!["a", "b"]);
        assert!(text_lines(b"").is_empty());
        assert_eq!(text_lines(b"\n"), vec![""]);
        assert_eq!(text_lines(b"a\r\n\r\nb"), vec!["a", "", "b"]);
    }

    /// RFC 1436: either side may close after the `.` line.  A server that
    /// keeps the connection open is not waited out to an error; one that
    /// sends a `.` line mid-file (it does not double dots) still delivers
    /// the rest.
    #[test]
    fn test_the_lastline_ends_a_reply_the_server_holds_open() {
        // `parts` sent with a pause between, then held open `hold`.
        fn hold_open(parts: Vec<&'static [u8]>, hold: std::time::Duration) -> (u16, std::thread::JoinHandle<()>) {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let h = std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0u8; 256];
                let _ = s.read(&mut buf);
                for part in parts {
                    s.write_all(part).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                std::thread::sleep(hold);
            });
            (port, h)
        }
        let (port, server) = hold_open(vec![b"Hello\r\n.\r\n"], std::time::Duration::from_secs(4));
        let started = std::time::Instant::now();
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/t"), 72).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(3500), "{:?}", started.elapsed());
        assert_eq!(page.lines, vec!["Hello"]);
        let (port, server2) = hold_open(vec![b"0Doc\t/d\th\t70\r\n.\r\n"], std::time::Duration::from_secs(4));
        let started = std::time::Instant::now();
        let page = fetch(&format!("gopher://127.0.0.1:{port}/1/"), 72).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(3500), "{:?}", started.elapsed());
        assert_eq!(page.links, vec!["gopher://h/0/d".to_string()]);
        // More after a `.` line: it is not the end after all.
        let (port, server3) = hold_open(vec![b"a\r\n.\r\n", b"b\r\n"], std::time::Duration::ZERO);
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/t"), 72).unwrap();
        assert_eq!(page.lines, vec!["a", ".", "b"]);
        // A plain text file whose first line looks like a Gopher+ header is
        // still text, and still ends at its `.` line.
        let (port, server5) = hold_open(vec![b"-1\r\nminus one\r\n.\r\n"], std::time::Duration::from_secs(4));
        let started = std::time::Instant::now();
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/t"), 72).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(3500), "{:?}", started.elapsed());
        assert_eq!(page.lines, vec!["-1", "minus one"]);
        server5.join().unwrap();
        // A binary file is read to the close, `.` line or not.
        let (port, server4) = hold_open(vec![b"x\r\n.\r\n"], std::time::Duration::ZERO);
        let data = fetch_file(&format!("gopher://127.0.0.1:{port}/9/f"), 100).unwrap();
        assert_eq!(data, b"x\r\n.\r\n");
        for s in [server, server2, server3, server4] {
            s.join().unwrap();
        }
    }

    /// A Gopher+ reply is framed by its own header, not by RFC 1436's `.`
    /// line: one whose data has a `.` line and then a pause is read on, not
    /// ended by the grace.
    #[test]
    fn test_the_lastline_grace_leaves_a_gopher_plus_reply_alone() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf);
            // Read to the close (`-2`), with a pause longer than the grace.
            s.write_all(b"+-2\r\nfirst\r\n.\r\n").unwrap();
            std::thread::sleep(GOPHER_LASTLINE_GRACE + std::time::Duration::from_millis(800));
            s.write_all(b"second\r\n").unwrap();
        });
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/t%09%09+"), 72).unwrap();
        server.join().unwrap();
        assert_eq!(page.lines.last().map(String::as_str), Some("second"), "{:?}", page.lines);
    }

    /// The same octets inside Gopher+ text: an attribute page's `+INFO`, and
    /// where a "moved" error says the item went.
    #[test]
    fn test_a_latin1_selector_in_gopher_plus_text_is_kept() {
        let attrs = b"+-1\r\n+INFO: 0Caf\xe9\t/caf\xe9\th.example\t70\t+\r\n+ADMIN:\r\n Admin: A <a@b>\r\n.\r\n";
        let (port, server) = serve(attrs.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/caf%E9%09%09!"), 72).unwrap();
        assert_eq!(server.join().unwrap(), b"/caf\xe9\t!\r\n");
        assert_eq!(page.links[0], "gopher://h.example/0/caf%E9%09%09+");
        assert!(page.lines[0].starts_with("Caf\u{e9}"), "{:?}", page.lines);
        // Each line is read on its own: a Latin-1 one leaves a UTF-8 one
        // (here an abstract) as written.
        let attrs = b"+-1\r\n+INFO: 0Caf\xe9\t/caf\xe9\th.example\t70\t+\r\n+ABSTRACT:\r\n Na\xc3\xafve caf\xc3\xa9\r\n Caf\xe9 au lait\r\n.\r\n";
        let (port, server) = serve(attrs.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/caf%E9%09%09!"), 72).unwrap();
        server.join().unwrap();
        assert!(page.lines[0].starts_with("Caf\u{e9}"), "{:?}", page.lines);
        assert!(page.lines.iter().any(|l| l.trim() == "Na\u{ef}ve caf\u{e9}"), "{:?}", page.lines);
        assert!(page.lines.iter().any(|l| l.trim() == "Caf\u{e9} au lait"), "{:?}", page.lines);
        let moved = b"--1\r\n3 <a@b>\r\nCaf\xe9 moved\r\nNa\xc3\xafve\r\n0Caf\xe9\t/new/caf\xe9\tn.example\t70\r\n.\r\n";
        let (port, server) = serve(moved.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/old%09%09+"), 72).unwrap();
        server.join().unwrap();
        assert_eq!(page.links[0], "gopher://n.example/0/new/caf%E9");
        assert_eq!(page.lines[..3], ["<a@b>", "Caf\u{e9} moved", "Na\u{ef}ve"]);
        // An error that is not "moved": its lines, each read on its own.
        let (port, server) = serve(b"--1\r\n1 <a@b>\r\nNo caf\xe9\r\nhere caf\xc3\xa9\r\n.\r\n".to_vec());
        let Err(err) = fetch(&format!("gopher://127.0.0.1:{port}/0/x%09%09+"), 72) else { panic!("an error made a page") };
        server.join().unwrap();
        assert_eq!(err, "Server: <a@b> No caf\u{e9} here caf\u{e9}");
    }

    /// `!` gives a page of who keeps the item, when it changed, its
    /// abstract, and a link per view; a server that does not speak Gopher+
    /// is said to be one, not drawn as attributes.
    #[test]
    fn test_item_attributes_make_a_page() {
        let attrs = b"+-1\r\n+INFO: 0Doc\t/d\th.example\t70\t+\r\n+ADMIN:\r\n Admin: Frodo <f@b>\r\n Mod-Date: Wed Jul 28 17:02:01 1993 <19930728170201>\r\n+VIEWS:\r\n Text/plain: <10k>\r\n application/postscript De_DE: <100k>\r\n+ABSTRACT:\r\n A short one.\r\n+SMELL:\r\n Roses\r\n.\r\n";
        let (port, server) = serve(attrs.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/d%09%09!"), 72).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "/d\t!\r\n");
        let text = page.lines.join("\n");
        assert!(text.contains("Admin: Frodo <f@b>"), "{text}");
        assert!(text.contains("Modified: 1993-07-28 17:02:01"), "{text}");
        assert!(text.contains("A short one."), "{text}");
        assert!(text.contains("+SMELL") && text.contains("Roses"), "{text}");
        assert_eq!(page.links, vec![
            "gopher://h.example/0/d%09%09+".to_string(),
            "gopher://h.example/0/d%09%09+Text/plain".to_string(),
            "gopher://h.example/0/d%09%09+application/postscript%20De_DE".to_string(),
        ]);
        // A view is fetched as its MIME type says: PostScript is a file.
        assert!(matches!(gopher_action(&page.links[2]), GopherAction::Download { .. }));
        assert_eq!(gopher_action(&page.links[1]), GopherAction::Page);
        // A menu's `$`: its `i` lines are its text, without the administrator
        // a server repeats for each.
        let menu = b"+-2\r\n+INFO: iWelcome!\tfake\t(NULL)\t0\r\n+ADMIN:\r\n Admin: Repeated <r@b>\r\n+INFO: 1Docs\t/docs\th.example\t70\t+\r\n+ADMIN:\r\n Admin: Frodo <f@b>\r\n";
        let (port, server) = serve(menu.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/1%09%09$"), 72).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "\t$\r\n");
        assert_eq!(page.lines[0], "Welcome!");
        assert!(!page.lines.iter().any(|l| l.contains("Repeated")), "{:?}", page.lines);
        assert!(page.lines.iter().any(|l| l.contains("Frodo")), "{:?}", page.lines);
        assert_eq!(page.title.as_deref(), Some("Gopher+ info: 127.0.0.1"));
        let (port, server) = serve(b"0Just a plain menu\t/x\th\t70\r\n.\r\n".to_vec());
        let err = fetch(&format!("gopher://127.0.0.1:{port}/1/x%09%09!"), 72).err().expect("an error");
        server.join().unwrap();
        assert!(err.contains("does not speak Gopher+"), "{err}");
    }

    /// A Gopher+ item's own reply: the header read and taken off; a
    /// counted reply ends the read without waiting for the close; an error
    /// says what the server said, and "moved" links to where.
    #[test]
    fn test_a_gopher_plus_item_reply() {
        let (port, server) = serve(b"+11\r\nhello world".to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/a%09%09+"), 72).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "/a\t+\r\n");
        assert_eq!(page.lines, vec!["hello world"]);
        let (port, server) = serve(b"--1\r\n1 <a@b>\r\nItem is not available.\r\n.\r\n".to_vec());
        let err = fetch(&format!("gopher://127.0.0.1:{port}/0/a%09%09+"), 72).err().expect("an error");
        server.join().unwrap();
        assert!(err.contains("Item is not available."), "{err}");
        let (port, server) = serve(b"--2\r\n3 0New home\t/new\tnew.example\t70\r\n".to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/0/a%09%09+"), 72).unwrap();
        server.join().unwrap();
        assert_eq!(page.links, vec!["gopher://new.example/0/new"]);
        assert!(page.lines.iter().any(|l| l.contains("Moved to: New home")), "{:?}", page.lines);
    }

    /// A CSO query: `query` then `quit`; each entry's fields together.
    #[test]
    fn test_a_cso_phone_book_query() {
        let reply = b"102:There were 2 matches to your request.\r\n-200:1:     name: Bryce Ricky\r\n-200:1:    email: r@example.org\r\n-200:2:     name: Bryce Sam\r\n200:Ok.\r\n200:Bye!\r\n";
        let (port, server) = serve(reply.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/2%09bryce"), 72).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "query bryce\r\nquit\r\n");
        assert_eq!(page.lines, vec![
            "There were 2 matches to your request.",
            "name: Bryce Ricky",
            "email: r@example.org",
            "",
            "name: Bryce Sam",
        ]);
        let (port, server) = serve(b"501:No matches to your query.\r\n200:Bye!\r\n".to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/2%09nobody"), 72).unwrap();
        server.join().unwrap();
        assert_eq!(page.lines, vec!["No matches to your query."]);
        // A name typed as Latin-1 is asked as it is, and each reply line is
        // read on its own: a Latin-1 entry leaves a UTF-8 one alone.
        let reply = b"-200:1:     name: Caf\xe9 Ann\r\n-200:2:     name: Caf\xc3\xa9 Bo\r\n200:Ok.\r\n";
        let (port, server) = serve(reply.to_vec());
        let page = fetch(&format!("gopher://127.0.0.1:{port}/2%09caf%E9"), 72).unwrap();
        assert_eq!(server.join().unwrap(), b"query caf\xe9\r\nquit\r\n");
        assert_eq!(page.lines, vec!["name: Caf\u{e9} Ann", "", "name: Caf\u{e9} Bo"]);
    }

    /// A file item is fetched whole, its Gopher+ header off, and one larger
    /// than the limit is refused rather than cut short.
    #[test]
    fn test_a_file_item_is_fetched_whole_or_refused() {
        let (port, server) = serve(b"\x00\x01binary\xff".to_vec());
        let data = fetch_file(&format!("gopher://127.0.0.1:{port}/9/f.bin"), 100).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "/f.bin\r\n");
        assert_eq!(data, b"\x00\x01binary\xff");
        let (port, server) = serve(b"+4\r\nGIF8".to_vec());
        let data = fetch_file(&format!("gopher://127.0.0.1:{port}/:/p.gif%09%09+"), 100).unwrap();
        assert_eq!(String::from_utf8_lossy(&server.join().unwrap()), "/p.gif\t+\r\n");
        assert_eq!(data, b"GIF8");
        // Cut short: a counted reply with fewer bytes, or a `.`-ended one
        // with no `.` line, is not saved as if it were whole.
        for reply in [&b"+10\r\nabc"[..], &b"+-1\r\nabc\r\n"[..]] {
            let (port, server) = serve(reply.to_vec());
            let err = fetch_file(&format!("gopher://127.0.0.1:{port}/9/f%09%09+"), 100).unwrap_err();
            server.join().unwrap();
            assert!(err.contains("cut short"), "{err}");
        }
        let (port, server) = serve(vec![b'x'; 101]);
        let err = fetch_file(&format!("gopher://127.0.0.1:{port}/9/big"), 100).unwrap_err();
        server.join().unwrap();
        assert!(err.contains("larger than"), "{err}");
        // A Gopher+ file too large is that, not cut short: one that says so,
        // and one that runs past the cap.
        let mut over = b"+-1\r\n".to_vec();
        over.extend(std::iter::repeat_n(b'x', 400));
        for reply in [b"+10000\r\nxx".to_vec(), over] {
            let (port, server) = serve(reply);
            let err = fetch_file(&format!("gopher://127.0.0.1:{port}/9/big%09%09+"), 100).unwrap_err();
            server.join().unwrap();
            assert!(err.contains("larger than"), "{err}");
        }
    }

    /// Every address a menu's host resolves to is checked, not just the
    /// first: a connect by name would go on to the others.
    #[test]
    fn test_every_address_of_a_hand_off_host_is_checked() {
        let refuse_lan = |ip: std::net::IpAddr, host: &str| -> Result<(), String> {
            if ip.to_string().starts_with("192.168.") { Err(format!("Blocked: {host}")) } else { Ok(()) }
        };
        let public: std::net::SocketAddr = "203.0.113.5:23".parse().unwrap();
        let lan: std::net::SocketAddr = "192.168.1.9:23".parse().unwrap();
        assert_eq!(checked_addr(&[public], "h", &refuse_lan), Ok(public));
        assert!(checked_addr(&[public, lan], "h", &refuse_lan).unwrap_err().contains("Blocked"));
        assert!(checked_addr(&[], "h", &refuse_lan).unwrap_err().contains("resolve"));
    }

    /// A form that sends a file gets a file's worth of time; one that does
    /// not, the page's.
    #[test]
    fn test_a_form_with_a_file_gets_a_files_time() {
        assert_eq!(ask_secs(Some(b"data")), FILE_TIMEOUT_SECS);
        assert_eq!(ask_secs(None), GOPHER_TIMEOUT_SECS);
        const { assert!(FILE_TIMEOUT_SECS > GOPHER_TIMEOUT_SECS) };
    }

    /// The deadline covers the request too: a server that takes it slowly
    /// and then stops reading ends the exchange at the deadline, not one
    /// whole timeout after the write that stalled.
    #[test]
    fn test_a_stalled_request_write_ends_at_the_deadline() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read slower than the client writes for 1.5 s, then nothing.
            let until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            let mut buf = vec![0u8; 64 << 10];
            while std::time::Instant::now() < until {
                if s.read(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::thread::sleep(std::time::Duration::from_secs(4));
        });
        let request = vec![b'x'; 64 << 20];
        let started = std::time::Instant::now();
        let err = exchange("127.0.0.1", port, &request, 100, 2, &|_| false, &|_| false).unwrap_err();
        let took = started.elapsed();
        assert!(err.contains("Write error: the server took too long"), "{err}");
        assert!(took < std::time::Duration::from_millis(2800), "took {took:?}");
        server.join().unwrap();
    }
}
