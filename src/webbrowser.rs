//! Text-based web browser for telnet sessions.
//!
//! Fetches HTTP(S) pages, converts HTML to wrapped plain text with numbered
//! link references, and supports page-by-page navigation. Designed to work
//! within the 40-column PETSCII constraint as well as wider ANSI/ASCII terminals.

use html2text::render::{RichAnnotation, TaggedLine, TaggedLineElement};
use html2text::{config, Element, Handle, RcDom};
use std::io::Read;

use crate::logger::glog;

/// Bookmarks file, inside the data directory (see `config::DATA_DIR`).
///
/// It used to say "next to the binary" and be exactly that, which is what
/// the one-folder move exists to end.  Missed in the first pass: the doc
/// comment named the old location, so grepping for the *paths* found it and
/// grepping for the rule would not have.
const BOOKMARKS_FILE: &str = "ethernetgateway-data/bookmarks.txt";
/// Maximum number of bookmarks.
const MAX_BOOKMARKS: usize = 100;

/// Maximum HTTP response body size (1 MB).
const MAX_BODY_SIZE: usize = 1024 * 1024;
/// Maximum rendered lines to keep (prevents memory bloat on huge pages).
const MAX_RENDERED_LINES: usize = 5000;

/// Columns a page line may have beyond the width html2text wraps it to: the
/// room a `[NNN]` link number takes.  The screen shows `width + this`, so a
/// line is never cut there -- see [`rewrap_for_screen`].
pub(crate) const LINK_MARKER_ROOM: usize = 5;
/// The narrowest a table column may be made: a short word and its link
/// number, `login[123]`.  See [`render_html_body`].
const LINK_WORD_ROOM: usize = 10;
/// HTTP request timeout in seconds.
const HTTP_TIMEOUT_SECS: u64 = 15;
/// How long to wait for the TCP connection itself, as opposed to the reply.
///
/// Separate from `HTTP_TIMEOUT_SECS` because the two failures are different
/// and deserve different patience.  A host that offers no HTTPS usually
/// *drops* packets on 443 rather than refusing them, so the only way to learn
/// that is to stop waiting -- and charging that to the whole-request budget
/// made a bare hostname for an HTTP-only site sit for fifteen seconds before
/// falling back.  A TCP handshake to any reachable host completes in well
/// under five; it is the *response* that can legitimately be slow, and that
/// still gets the full `HTTP_TIMEOUT_SECS`.
///
/// A TLS error needs none of this: the far end answers immediately, so that
/// fallback was always instant.  This shortens only the silent case.
const CONNECT_TIMEOUT_SECS: u64 = 5;
/// Maximum HTTP redirects to follow.  We follow them manually (ureq's
/// auto-follow is disabled) so each hop is SSRF-checked before we connect;
/// 10 matches ureq's former default.
const MAX_REDIRECTS: usize = 10;
/// Maximum DOM nesting depth we will render.  html5ever parses without a
/// depth limit, so an adversarial page of deeply-nested tags (e.g. tens of
/// thousands of unclosed `<div>`s, well under `MAX_BODY_SIZE`) builds a very
/// deep tree.  Our own title/form extractors (`extract_title_from_dom`,
/// `extract_forms_from_dom`, and the `collect_field_labels` / `extract_form_fields`
/// helpers) recurse over `node.children`, so on such a tree they overflow the
/// modest `spawn_blocking` thread stack and abort the *entire* gateway process
/// (SIGABRT), not just the one request.  (html2text's own render pass and its
/// `RcDom` `Drop` are iterative — see markup5ever_rcdom's worklist `Drop` — so
/// they tolerate arbitrarily deep trees; only our recursive walkers need this
/// bound.)  Measured empirically, the full unguarded pipeline survives ~2048
/// levels and overflows by ~4096, so 512 — the depth real browsers historically
/// cap near — keeps a comfortable margin while sitting far above any legitimate
/// page (which nests only tens deep).  See `render_html_body`.
const MAX_DOM_DEPTH: usize = 512;

/// Result of fetching and rendering a web page.
pub(crate) struct WebPage {
    /// Page title extracted from `<title>`, if any.
    pub title: Option<String>,
    /// Rendered text lines (plain text, already wrapped to target width).
    pub lines: Vec<String>,
    /// Extracted link URLs, indexed starting at 1 (`links[0]` = link `[1]`).
    pub links: Vec<String>,
    /// Final URL after redirects.
    pub url: String,
    /// HTML forms found on the page.
    pub forms: Vec<WebForm>,
}

impl WebPage {
    /// Strip terminal-control bytes from all remote-derived text before it
    /// reaches a retro terminal.  A hostile or MITM'd web page / gopher
    /// server can embed ANSI/CSI/OSC escape sequences to move the cursor,
    /// recolor the screen, or spoof the UI — the same threat the AI-chat
    /// path defeats with `aichat::sanitize_for_terminal`.  We reuse that
    /// exact filter here (rather than a second copy of the rule), applied to
    /// the title and every rendered line.  The one wrinkle is the `\x02N\x03`
    /// link-marker sentinels the renderers embed and the telnet consumer
    /// parses: those framing bytes are C0 controls the filter would strip,
    /// so `sanitize_line_keep_markers` splits them out and sanitizes only the
    /// human-readable segments around them.  A page that injects literal
    /// 0x02/0x03 can at most spoof a link *number* on screen (the
    /// authoritative `links` array is built separately) — the pre-existing,
    /// cosmetic sentinel-collision behavior, not an escape-injection vector.
    /// Idempotent.
    pub(crate) fn sanitize(&mut self) {
        use crate::aichat::sanitize_for_terminal;
        if let Some(title) = self.title.as_mut() {
            // Folded like the body: the title is drawn on the same screen.
            *title = crate::aichat::display_for_terminal(title);
        }
        for line in self.lines.iter_mut() {
            *line = sanitize_line_keep_markers(line);
        }
        // The URL is shown in the status line; a gopher selector can carry
        // attacker-chosen bytes into it (`build_gopher_url`).
        self.url = sanitize_for_terminal(&self.url);
        // Form text is rendered by the telnet form UI (`web_show_forms` /
        // `web_edit_form`).  Sanitize only the DISPLAY-only strings here —
        // form/field labels and Select option display text — never a field
        // `value` or `name`: those are submitted back to the server, and
        // `sanitize_for_terminal` strips control bytes (incl. newlines) that a
        // legitimate textarea value may need.  Field values are sanitized at
        // display time instead (see `web_edit_form`), so the submitted copy
        // stays byte-exact while the terminal never sees raw escapes (M-8).
        for form in self.forms.iter_mut() {
            form.label = sanitize_for_terminal(&form.label);
            for field in form.fields.iter_mut() {
                match field {
                    FormField::Text { label, .. }
                    | FormField::TextArea { label, .. }
                    | FormField::Checkbox { label, .. }
                    | FormField::Radio { label, .. } => {
                        *label = sanitize_for_terminal(label);
                    }
                    FormField::Select { label, options, .. } => {
                        *label = sanitize_for_terminal(label);
                        for (_value, display) in options.iter_mut() {
                            *display = sanitize_for_terminal(display);
                        }
                    }
                    FormField::Hidden { .. } => {}
                }
            }
        }
    }
}


/// Apply `aichat::sanitize_for_terminal` to a rendered line while preserving
/// the `\x02N\x03` link-marker sentinels (see [`WebPage::sanitize`]).
fn sanitize_line_keep_markers(line: &str) -> String {
    // Fast path: no sentinels, sanitize the whole line in one pass.
    if !line.contains(['\u{02}', '\u{03}']) {
        return crate::aichat::display_for_terminal(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut segment = String::new();
    for c in line.chars() {
        if c == '\u{02}' || c == '\u{03}' {
            out.push_str(&crate::aichat::display_for_terminal(&segment));
            segment.clear();
            out.push(c);
        } else {
            segment.push(c);
        }
    }
    out.push_str(&crate::aichat::display_for_terminal(&segment));
    out
}

/// A single field within an HTML form.
#[derive(Clone, Debug)]
pub(crate) enum FormField {
    /// Text-like input (text, search, email, url, tel, number, password, etc.)
    Text {
        name: String,
        value: String,
        label: String,
        input_type: String,
    },
    /// Hidden input — not displayed but included in submission.
    Hidden { name: String, value: String },
    /// Textarea element.
    TextArea { name: String, value: String, label: String },
    /// Select dropdown with options.
    Select {
        name: String,
        options: Vec<(String, String)>, // (value, display_text)
        selected: usize,
        label: String,
    },
    /// Checkbox input.
    Checkbox {
        name: String,
        value: String,
        checked: bool,
        label: String,
    },
    /// Radio button input.
    Radio {
        name: String,
        value: String,
        checked: bool,
        label: String,
    },
}

/// A parsed HTML form.
#[derive(Clone, Debug)]
pub(crate) struct WebForm {
    /// Form action URL (may be relative).
    pub action: String,
    /// HTTP method: "get" or "post" (lowercase).
    pub method: String,
    /// Human-readable label for the form.
    pub label: String,
    /// Fields in document order.
    pub fields: Vec<FormField>,
}

/// Map a 1-based display number (which skips Hidden fields) to the real index.
pub(crate) fn visible_field_index(fields: &[FormField], display_num: usize) -> Option<usize> {
    let mut count = 0;
    for (i, f) in fields.iter().enumerate() {
        if matches!(f, FormField::Hidden { .. }) {
            continue;
        }
        count += 1;
        if count == display_num {
            return Some(i);
        }
    }
    None
}

/// Check whether a ureq error indicates the server doesn't speak TLS at all
/// (e.g. responds with plain HTTP to a TLS ClientHello).  Does NOT match
/// certificate validation errors — those mean TLS is working but the cert is bad.
fn is_tls_error(e: &ureq::Error) -> bool {
    let msg = e.to_string();
    msg.contains("corrupt message") || msg.contains("InvalidContentType")
}

/// Does this error mean the host simply serves no HTTPS at all?
///
/// A great deal of the web this browser exists for is HTTP-only, with nothing
/// listening on 443 -- textfiles.com is the canonical example.  Typing a bare
/// hostname gives `https://` (see `normalize_url`), so without this those
/// sites were unreachable by name: the connection was refused and the error
/// was returned rather than retried.  Only an explicit `http://` worked, which
/// is not something a reader should have to know.
///
/// Matched on the ERROR VARIANT rather than on message text, and narrowly:
///
/// * `Io` with a refused/reset/aborted connection means nothing is answering
///   on 443.  Retrying over HTTP is the useful thing to do.
/// * `HostNotFound` is deliberately excluded -- DNS failed, so HTTP would fail
///   identically and a retry only doubles the wait before the same error.
/// * `Timeout` IS included, via `https_timed_out` -- and that is a judgement
///   call worth stating.  A filtered 443 and a merely slow HTTPS server look
///   identical from here, so a genuinely slow site can be dropped to
///   cleartext.  It is included anyway because DROPPING packets on 443 is how
///   most HTTP-only hosts present (textfiles.com among them), the fallback is
///   announced rather than silent, and a site that cannot answer within
///   `HTTP_TIMEOUT_SECS` is already unusable here.  The alternative was that
///   a large part of the old web could not be reached by name at all.
///
/// The downgrade stays **visible** either way -- see the notice prepended by
/// the caller.  An attacker able to refuse 443 can force this, exactly as one
/// able to break the handshake could already; the answer to that is to tell
/// the reader, not to make honest HTTP-only sites unreachable.
fn https_timed_out(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Timeout(_))
}

fn no_https_service(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
            )
    )
}

/// True if `ip` is an address the text browser must never reach — a basic
/// SSRF guard so a telnet/SSH user (or an attacker-controlled redirect)
/// can't pivot to the gateway's own services (e.g. the web-config server
/// on 127.0.0.1), cloud metadata (169.254.169.254), or other LAN hosts.
///
/// Known limitation: on the HTTP path the host is resolved here and then
/// again by ureq at connect time, so a hostile resolver could hand this
/// check a public IP and ureq an internal one (DNS rebinding).  Closing
/// that would need ureq's unstable custom-resolver API and only matters
/// when the browser is exposed to untrusted callers, so it's left as-is.
/// The gopher path has no such gap — it checks the exact address it dials.
fn is_internal_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                // IPv4-mapped (::ffff:0:0/96) and the deprecated IPv4-
                // compatible (::/96) form — classify by the embedded v4.
                || v6
                    .to_ipv4()
                    .is_some_and(|m| is_internal_ip(IpAddr::V4(m)))
                // Two more prefixes that carry an IPv4 address a router will
                // deliver to: NAT64's well-known 64:ff9b::/96 (the v4 is the
                // last 32 bits) and 6to4's 2002::/16 (bits 16..48). On a
                // network with either gateway, `[64:ff9b::7f00:1]` is
                // 127.0.0.1 -- so judge them by the address inside.
                || embedded_v4(v6).is_some_and(|m| is_internal_ip(IpAddr::V4(m)))
        }
    }
}

/// The IPv4 address a NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`) address
/// is routed to, if it is one.
fn embedded_v4(v6: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let s = v6.segments();
    let v4 = |hi: u16, lo: u16| {
        std::net::Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
    };
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        Some(v4(s[6], s[7]))
    } else if s[0] == 0x2002 {
        Some(v4(s[1], s[2]))
    } else {
        None
    }
}

/// True when the operator has opted out of IP-safety (the same flag that
/// opens the inbound listeners to any address) or we're in an in-crate
/// test that fetches from a loopback fixture server.
fn internal_fetch_allowed() -> bool {
    cfg!(test) || crate::config::get_config().disable_ip_safety
}

/// Classify a URL host that is an IP literal.  `url::Url::host_str()`
/// hands IPv6 literals back **bracketed** (e.g. `"[::1]"`), a form
/// `IpAddr`'s parser rejects — strip the brackets first so a bracketed
/// IPv6 literal is classified here instead of falling through to the
/// resolver path (which can't resolve a bracketed string and would let it
/// through, bypassing the SSRF guard for the entire IPv6 space).
/// Returns `Some(internal?)` for an IP literal, or `None` when `host` is a
/// DNS name that still needs resolution.
fn host_literal_is_internal(host: &str) -> Option<bool> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>().ok().map(is_internal_ip)
}

/// Reject a URL whose host is — or resolves to — an internal/loopback
/// address, unless `internal_fetch_allowed()`.  Applied to the initial
/// request and the post-redirect landing URL so a telnet/SSH user can't
/// use the browser to reach the gateway's own services or the LAN.
fn guard_public_url(url_str: &str) -> Result<(), String> {
    use std::net::ToSocketAddrs;
    if internal_fetch_allowed() {
        return Ok(());
    }
    let parsed =
        url::Url::parse(url_str).map_err(|_| "Blocked: unparseable URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "Blocked: URL has no host".to_string())?;
    // IP literal — check directly, no DNS.
    if let Some(internal) = host_literal_is_internal(host) {
        return if internal {
            Err(format!("Blocked: {} is an internal address", host))
        } else {
            Ok(())
        };
    }
    // Hostname — reject if ANY resolved address is internal (defends
    // against a name that points at an internal IP).
    let port = parsed.port_or_known_default().unwrap_or(80);
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            for a in addrs {
                if is_internal_ip(a.ip()) {
                    return Err(format!(
                        "Blocked: {} resolves to an internal address",
                        host
                    ));
                }
            }
            Ok(())
        }
        // Resolution failure: let the real fetch surface the DNS error.
        Err(_) => Ok(()),
    }
}

/// Fetch a URL and render it as wrapped plain text with numbered links.
///
/// This is a blocking call (uses ureq) and should be run via `spawn_blocking`.
/// `width` is the target column count for word-wrapping (32 for PETSCII, 72 for ANSI).
pub(crate) fn fetch_and_render(url: &str, width: usize) -> Result<WebPage, String> {
    fetch_and_render_from(url, width, 0, None)
}

/// Turn a response into a page -- one function for a page load and a form
/// submission, which each had their own copy of this.
///
/// Three things a reader was not told before, all decided here:
///
/// * **A file that is not text is refused by name** rather than drawn.  A
///   PDF link filled screen after screen with its compressed bytes, and on a
///   Commodore the bytes above 0x7F are graphics characters.  Decided from
///   the declared type before the body is read, and from the first bytes
///   when a server declares none.
/// * **An HTTP error says so** at the top of what it sent.  The body of a 404
///   is still shown -- it may hold directions -- but a 404 with an empty body
///   left the reader on the browser's home screen with no word of why.
/// * **A page with no text says so**, rather than being an empty screen --
///   which on this browser looks exactly like "nothing was loaded".
fn render_response(
    response: ureq::http::Response<ureq::Body>,
    final_url: String,
    width: usize,
) -> Result<(WebPage, Option<String>), String> {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    // An error is the news, whatever its body is: a 404 served as a picture
    // is still a 404.
    let refuse = |why: String| -> String {
        if status.as_u16() >= 400 {
            match status.canonical_reason() {
                Some(reason) => format!("HTTP error {} - {}.", status.as_u16(), reason),
                None => format!("HTTP error {}.", status.as_u16()),
            }
        } else {
            why
        }
    };
    if let Some(why) = unshowable(&content_type, None) {
        return Err(refuse(why));
    }

    let mut body_bytes = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(MAX_BODY_SIZE as u64)
        .read_to_end(&mut body_bytes)
        .map_err(|e| format!("Read error: {}", e))?;
    if let Some(why) = unshowable(&content_type, Some(&body_bytes)) {
        return Err(refuse(why));
    }

    let plain = is_plain_text(&content_type);
    let (mut page, meta_refresh) = if plain {
        let text = String::from_utf8_lossy(&body_bytes);
        let lines: Vec<String> = text
            .lines()
            .flat_map(|line| wrap_line(line, width))
            .take(MAX_RENDERED_LINES)
            .collect();
        (
            WebPage { title: None, lines, links: Vec::new(), url: final_url, forms: Vec::new() },
            None,
        )
    } else {
        render_html_body(&body_bytes, final_url, width)?
    };

    let has_text = page
        .lines
        .iter()
        .any(|l| l.chars().any(|c| !c.is_whitespace() && c != '\u{02}' && c != '\u{03}'));
    if !has_text {
        // JavaScript is the likely reason only for an HTML page that rendered
        // to nothing; an empty text file or error body is simply empty.
        let note = if plain || body_bytes.is_empty() {
            "(This page has no text to show.)"
        } else {
            "(This page has no text to show - it may need JavaScript.)"
        };
        page.lines = wrap_line(note, width);
    }
    let mut meta_refresh = meta_refresh;
    if status.as_u16() >= 400 {
        // An error page that refreshes elsewhere would take its own error
        // line with it; the reader is told, and can follow a link on.
        meta_refresh = None;
        let what = match status.canonical_reason() {
            Some(reason) => format!("[!] HTTP error {} - {}.", status.as_u16(), reason),
            None => format!("[!] HTTP error {}.", status.as_u16()),
        };
        let mut head = wrap_line(&what, width);
        head.push(String::new());
        head.append(&mut page.lines);
        page.lines = head.into_iter().take(MAX_RENDERED_LINES).collect();
    }
    Ok((page, meta_refresh))
}

/// Whether a response is text to show line for line rather than markup.
///
/// JSON went through the HTML renderer, which folds whitespace the way a page
/// does, so an API's pretty-printed reply arrived as one wrapped paragraph.
/// Every text type that is not markup keeps its own lines: `text/*` except
/// HTML and XML, and JSON under either of its names.
fn is_plain_text(content_type: &str) -> bool {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    let markup = mime.contains("html") || mime.contains("xml");
    !markup && (mime.starts_with("text/") || mime == "application/json" || mime.ends_with("+json"))
}

/// Why a response cannot be shown as text, or `None` if it can.
///
/// `body` is `None` for the check made from the declared type alone, before
/// anything is downloaded; with no declared type the first bytes decide.
fn unshowable(content_type: &str, body: Option<&[u8]>) -> Option<String> {
    let mime = content_type.split(';').next().unwrap_or("").trim();
    // A real type decides by itself.  `octet-stream` is what a server says
    // when it does not know (a `.txt` it has no mapping for), and a value with
    // no `/` is not a type at all: for those the bytes decide, as for none.
    if mime.contains('/') && mime != "application/octet-stream" {
        // "xml" only as the whole subtype or a `+xml` suffix: Word, Excel and
        // PowerPoint declare `...openxmlformats-officedocument...`, which
        // contains it and is a ZIP.
        let sub = mime.split('/').nth(1).unwrap_or("");
        // A picture first: `image/svg+xml` is XML and is still a picture.
        let textual = !mime.starts_with("image/")
            && (mime.starts_with("text/")
            || sub == "xml"
            || sub.ends_with("+xml")
            || ["html", "json", "javascript"].iter().any(|t| sub.contains(t)));
        if textual {
            return None;
        }
        let kind = if mime == "application/pdf" {
            "a PDF file".to_string()
        } else if mime.starts_with("image/") {
            "a picture".to_string()
        } else if mime.starts_with("audio/") {
            "a sound file".to_string()
        } else if mime.starts_with("video/") {
            "a video".to_string()
        } else if ["zip", "gzip", "x-tar", "x-7z", "rar", "x-bzip"].iter().any(|t| mime.contains(t)) {
            "a compressed archive".to_string()
        } else {
            format!("a file of type {}", sanitize_for_terminal_ascii(mime))
        };
        return Some(format!("That link is {kind}, which can't be shown as text."));
    }
    let body = body?;
    let head = &body[..body.len().min(1024)];
    if head.starts_with(b"%PDF") {
        Some("That link is a PDF file, which can't be shown as text.".to_string())
    } else if head.contains(&0) {
        Some("That link is a binary file, which can't be shown as text.".to_string())
    } else {
        None
    }
}

/// A server-supplied string cut down to printable ASCII, for a message.
fn sanitize_for_terminal_ascii(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_graphic()).take(60).collect()
}

/// What went wrong, in words a reader can act on.
///
/// The raw error was shown before, cut to fit: `io: invalid peer
/// certificate: certificate expir...` -- the right facts in the wrong words,
/// with the important one truncated.  The certificate case is matched on the
/// message because rustls reaches us wrapped in an I/O error.
fn friendly_fetch_error(e: &ureq::Error) -> String {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if matches!(e, ureq::Error::HostNotFound) {
        return "Could not find that site. Check the address.".to_string();
    }
    if matches!(e, ureq::Error::Timeout(_)) {
        return "The site did not answer in time.".to_string();
    }
    if lower.contains("certificate") {
        let why = if lower.contains("expired") {
            "has expired"
        } else if lower.contains("notvalidforname") || lower.contains("not valid for") {
            "is for a different site"
        } else if lower.contains("unknownissuer") || lower.contains("unknown issuer") {
            "is not from a trusted authority"
        } else {
            "is not valid"
        };
        return format!("This site's security certificate {why}, so the page was not loaded.");
    }
    if let ureq::Error::Io(io) = e
        && io.kind() == std::io::ErrorKind::ConnectionRefused
    {
        return "The site refused the connection.".to_string();
    }
    if matches!(e, ureq::Error::ConnectionFailed) {
        return "Could not connect to the site.".to_string();
    }
    format!("Could not load the page ({msg}).")
}

/// Whether a downgrade reason from an earlier hop still describes `url`.
///
/// **A reason must not outlive the cleartext run it belongs to.** It says how
/// the page in front of the reader was fetched, so carrying it across every
/// `<meta refresh>` unconditionally mislabels a later page: `https://a` fails
/// TLS, is refetched as `http://a`, refreshes to a working `https://b`, which
/// refreshes to an ordinary `http://c` -- and `c`, never tried over TLS at
/// all, would be announced as a TLS failure.  Reaching an HTTPS URL ends the
/// run and drops the reason; a fresh downgrade on a later hop sets its own.
fn carry_downgrade(url: &str, carried: Option<DowngradeReason>) -> Option<DowngradeReason> {
    if url.starts_with("https://") { None } else { carried }
}

/// `start_hops` carries the redirect budget across a `<meta refresh>` hop.
///
/// A meta refresh is a redirect the *document* asks for rather than the
/// server, and it has to share one budget with the HTTP ones -- otherwise a
/// page that 302s to a page that meta-refreshes back could ping-pong for ever,
/// each side resetting the other's count.  Recursion depth is therefore
/// bounded by `MAX_REDIRECTS`.
///
/// `carried` is the HTTPS-downgrade reason from an earlier hop, and it has to
/// be threaded through or it is lost: the notice used to be prepended *after*
/// the meta-refresh returned, so a site that downgraded to HTTP and then
/// refreshed to another page handed the reader cleartext with no warning at
/// all -- and `no_https_service` / `https_timed_out` sites are exactly the
/// ones the downgrade exists for.
fn fetch_and_render_from(
    url: &str,
    width: usize,
    start_hops: usize,
    carried: Option<DowngradeReason>,
) -> Result<WebPage, String> {
    // Follow redirects manually (auto-follow disabled below) so EVERY hop
    // is SSRF-checked before we connect — otherwise ureq would follow a
    // public→internal redirect and dial the internal host before our guard
    // saw it.  (DNS rebinding between this check and ureq's own connect-time
    // resolution remains theoretically possible; it only matters for an
    // untrusted caller with a hostile resolver — documented on is_internal_ip.)
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            // Render what the server sent on 4xx/5xx instead of discarding it.
            //
            // ureq's default turns any non-2xx into an Err, which threw away
            // the response body -- so a 404 with directions, or a 403 challenge
            // page, reached the reader as an opaque error and the page they
            // were told to read was never shown.  `aichat.rs` hit the same
            // default and opted out for the same reason.
            //
            // Redirects are unaffected: `max_redirects_will_error(false)`
            // already hands 3xx back as a response for the hop logic below.
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build(),
    );

    let mut current = url.to_string();
    // Seeded from the hop that got us here, so a downgrade survives a
    // `<meta refresh>` -- but only while the chain stays cleartext.
    let mut downgrade: Option<DowngradeReason> = carry_downgrade(url, carried);
    let mut hops = start_hops;
    let (response, final_url) = loop {
        guard_public_url(&current)?;
        let resp = match agent
            .get(&current)
            .header("User-Agent", "EthernetGateway/1.0 (text-mode browser)")
            .header("Accept", "text/html, text/plain;q=0.9, */*;q=0.1")
            .call()
        {
            Ok(r) => r,
            // No usable HTTPS: retry the same resource over HTTP (re-guarded
            // at the top of the next iteration).  Two distinct reasons, kept
            // apart so the warning can say which -- "TLS error" is alarming
            // and wrong for a site that simply never offered HTTPS.
            Err(e)
                if current.starts_with("https://")
                    && (is_tls_error(&e) || no_https_service(&e) || https_timed_out(&e)) =>
            {
                downgrade = Some(if is_tls_error(&e) {
                    DowngradeReason::TlsFailed
                } else if https_timed_out(&e) {
                    DowngradeReason::NoResponse
                } else {
                    DowngradeReason::NoHttps
                });
                current = format!("http://{}", &current["https://".len()..]);
                continue;
            }
            Err(e) => return Err(friendly_fetch_error(&e)),
        };
        // Redirect: resolve + guard the next hop before following it.
        if matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
                let next = resolve_url(&current, loc);
                if next != current {
                    hops += 1;
                    if hops > MAX_REDIRECTS {
                        return Err("Too many redirects".to_string());
                    }
                    current = next;
                    continue;
                }
            }
            // 3xx with no usable / self-referential Location — render as-is.
        }
        break (resp, current.clone());
    };

    let (mut page, meta_refresh) = render_response(response, final_url, width)?;

    // A `<meta refresh>` redirect.  Followed through the same entry point, so
    // the new URL is SSRF-guarded exactly like an HTTP hop, and against the
    // shared budget.  A target equal to the page we are on is a self-refresh
    // ("reload me"), which must not be chased.
    if let Some(target) = meta_refresh {
        let next = resolve_url(&page.url, &target);
        if next != page.url {
            let hops = hops + 1;
            if hops > MAX_REDIRECTS {
                return Err("Too many redirects".to_string());
            }
            return fetch_and_render_from(&next, width, hops, downgrade);
        }
    }

    page.sanitize();

    // The notice describes the page in front of the reader -- "fetched over
    // plain HTTP" -- so it is shown when *this* page is cleartext, not merely
    // because some earlier hop was.  A refresh that lands back on a working
    // HTTPS URL is encrypted and must not be flagged; one that stays on HTTP
    // carries the original reason forward, which is the case that used to be
    // silent.
    if let Some(reason) = downgrade
        && page.url.starts_with("http://")
    {
        prepend_downgrade_notice(&mut page, width, reason);
    }
    Ok(page)
}

/// Why a page ended up being fetched over cleartext HTTP.
///
/// Kept apart because the two say different things to a reader: one is a
/// site that never offered HTTPS, which is ordinary on the web this browser
/// serves; the other is HTTPS that was offered and did not work, which is
/// worth a raised eyebrow.  Reporting both as "TLS error" made the common,
/// harmless case look like the alarming one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DowngradeReason {
    /// Nothing is listening for HTTPS on this host.
    NoHttps,
    /// HTTPS never answered -- 443 is filtered rather than refused, which is
    /// how most HTTP-only hosts present.
    NoResponse,
    /// HTTPS answered but the handshake failed.
    TlsFailed,
}

/// Insert a visible warning at the top of the page when we fell back from
/// HTTPS to HTTP.  Without this, the user has no signal that their request is
/// now in the clear - dangerous for any page that reads cookies, form data,
/// or authentication.
fn prepend_downgrade_notice(page: &mut WebPage, width: usize, reason: DowngradeReason) {
    // ASCII ONLY, deliberately.  This notice is prepended AFTER
    // `page.sanitize()`, so it never passes through the terminal-safe fold
    // that page text does -- whatever is written here reaches the wire as-is.
    // It used to carry an em-dash, which is three bytes of UTF-8 and three
    // unrenderable characters on a 7-bit console: our own security warning
    // arrived as garbage on exactly the terminals this gateway exists for.
    // Found by surveying real sites, on a site whose TLS actually fails.
    let notice = match reason {
        DowngradeReason::NoHttps => "[!] No HTTPS on this site - fetched over plain HTTP.",
        DowngradeReason::NoResponse => "[!] HTTPS did not respond - fetched over plain HTTP.",
        DowngradeReason::TlsFailed => "[!] HTTPS failed (TLS error) - fetched over plain HTTP.",
    };
    // Char count rather than byte length: the two agree while this is ASCII,
    // and this stays correct if it ever legitimately gains a wider character.
    let separator = "-".repeat(notice.chars().count().min(width));
    let mut header: Vec<String> = Vec::new();
    header.extend(wrap_line(notice, width));
    header.push(separator);
    header.push(String::new());
    // Prepend (cap total to MAX_RENDERED_LINES so we don't blow past
    // the rendering budget).  Link indices live in page.links and
    // don't address into page.lines, so prepending plain rendered
    // lines doesn't disturb link selection.
    header.append(&mut page.lines);
    page.lines = header.into_iter().take(MAX_RENDERED_LINES).collect();
}

/// Submit a form (GET or POST) and return the resulting page.
///
/// Blocking call — run via `spawn_blocking`.
pub(crate) fn submit_form(base_url: &str, form: &WebForm, width: usize) -> Result<WebPage, String> {
    // Collect name/value pairs from form fields
    let mut pairs: Vec<(String, String)> = Vec::new();
    for field in &form.fields {
        match field {
            FormField::Text { name, value, .. }
            | FormField::Hidden { name, value }
            | FormField::TextArea { name, value, .. } => {
                pairs.push((name.clone(), value.clone()));
            }
            FormField::Select { name, options, selected, .. } => {
                if let Some((val, _)) = options.get(*selected) {
                    pairs.push((name.clone(), val.clone()));
                }
            }
            FormField::Checkbox { name, value, checked, .. } => {
                if *checked {
                    pairs.push((name.clone(), value.clone()));
                }
            }
            FormField::Radio { name, value, checked, .. } => {
                if *checked {
                    pairs.push((name.clone(), value.clone()));
                }
            }
        }
    }

    let action_url = if form.action.is_empty() {
        base_url.to_string()
    } else {
        resolve_url(base_url, &form.action)
    };
    guard_public_url(&action_url)?;

    // Disable ureq's auto-redirect (matching fetch_and_render): a POST to a
    // public action that returns a redirect to an internal address would
    // otherwise have ureq dial the internal host before any guard ran, and
    // the post-request final_url check only blocks rendering — not the
    // connection.  We follow a redirect manually below, guarding the target.
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .timeout_connect(Some(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS)))
            // Render what the server sent on 4xx/5xx instead of discarding it.
            //
            // ureq's default turns any non-2xx into an Err, which threw away
            // the response body -- so a 404 with directions, or a 403 challenge
            // page, reached the reader as an opaque error and the page they
            // were told to read was never shown.  `aichat.rs` hit the same
            // default and opted out for the same reason.
            //
            // Redirects are unaffected: `max_redirects_will_error(false)`
            // already hands 3xx back as a response for the hop logic below.
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build(),
    );

    if form.method == "post" {
        // A form POST is NEVER auto-downgraded to cleartext HTTP (M-10).
        // Unlike an idempotent GET, retrying a POST over http:// re-sends the
        // form fields (which may be credentials) in the clear.  An active
        // MITM can force a TLS error (`is_tls_error` matches "corrupt
        // message" / "InvalidContentType") specifically to strip TLS and
        // capture the body — and the old code sent it before the user ever
        // saw a downgrade notice.  Refuse instead: the form is not
        // resubmitted, and the user is told why.
        let post_url = action_url.clone();
        let response = match agent
            .post(&post_url)
            .header("User-Agent", "EthernetGateway/1.0 (text-mode browser)")
            .send_form(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        {
            Ok(r) => r,
            Err(e) if post_url.starts_with("https://") && is_tls_error(&e) => {
                return Err(
                    "Secure connection failed and this form was NOT resubmitted \
                     over an insecure (http://) link — form data can include \
                     passwords. Try again later or use a secure site."
                        .to_string(),
                );
            }
            Err(e) => return Err(friendly_fetch_error(&e)),
        };

        // POST-redirect: follow it through fetch_and_render, which SSRF-guards
        // every hop before connecting.  301/302/303 become a GET (standard
        // browser POST-redirect-GET); 307/308 strictly want a re-POST, but we
        // follow them as a guarded GET too — that's rare for form actions and
        // far preferable to the unguarded auto-follow this replaced.
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            if let Some(loc) =
                response.headers().get("location").and_then(|v| v.to_str().ok())
            {
                let next = resolve_url(&post_url, loc);
                if next != post_url {
                    let page = fetch_and_render(&next, width)?;
                    return Ok(page);
                }
            }
            // 3xx with no usable / self-referential Location — render as-is.
        }

        let final_url = post_url.clone();
        guard_public_url(&final_url)?;
        let (mut page, meta_refresh) = render_response(response, final_url, width)?;

        // Same treatment a POST already gives an HTTP redirect: follow it
        // through the guarded fetch path rather than rendering the notice.
        if let Some(target) = meta_refresh {
            let next = resolve_url(&page.url, &target);
            if next != page.url {
                return fetch_and_render(&next, width);
            }
        }

        page.sanitize();
        Ok(page)
    } else {
        // GET: append query string to URL
        let mut url = url::Url::parse(&action_url)
            .map_err(|e| format!("Bad URL: {}", e))?;
        {
            let mut query = url.query_pairs_mut();
            query.clear();
            for (k, v) in &pairs {
                query.append_pair(k, v);
            }
        }
        fetch_and_render(url.as_str(), width)
    }
}

/// One rendered line as rows that fit `max` columns, with every link number
/// kept whole.
///
/// html2text wraps to `width` and leaves room for ONE `[NNN]`; a line with
/// several links ran past the screen and was cut there, taking the numbers of
/// its later links with it -- a link you can see and cannot follow.  So a line
/// too wide moves its overflow to the next row: at a space, never inside a
/// link number, a continuation indented like the line (and past its bullet).
/// Width is counted as the narrowest terminal will draw it -- a `ß` is two
/// columns once a C64 has it as `ss` -- and a line that fits is left exactly
/// as it was, spacing and all.
fn rewrap_for_screen(line: &str, max: usize) -> Vec<String> {
    // The widest any terminal draws it: an ASCII-only one draws the fold
    // (`ss`), an ANSI one draws CJK and emoji two columns wide.
    let cols = |c: char| -> usize {
        match c {
            '\u{2}' | '\u{3}' => 1, // drawn as '[' and ']'
            c if c.is_ascii() => 1,
            c => {
                let folded = crate::aichat::fold_to_ascii(c.encode_utf8(&mut [0; 4])).chars().count();
                let wide = matches!(c,
                    '\u{1100}'..='\u{115F}' | '\u{2E80}'..='\u{A4CF}' | '\u{AC00}'..='\u{D7A3}'
                    | '\u{F900}'..='\u{FAFF}' | '\u{FE30}'..='\u{FE4F}' | '\u{FF00}'..='\u{FF60}'
                    | '\u{FFE0}'..='\u{FFE6}' | '\u{1F300}'..='\u{1FAFF}' | '\u{20000}'..='\u{3FFFD}');
                folded.max(if wide { 2 } else { 1 })
            }
        }
    };
    if line.chars().map(cols).sum::<usize>() <= max {
        return vec![line.to_string()];
    }
    let body = line.trim_start_matches(' ');
    let indent = line.len() - body.len();
    // A bullet, or a list number like `12. `, which the text hangs under.
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let bullet = if body.starts_with("* ") || body.starts_with("- ") {
        2
    } else if digits > 0 && body[digits..].starts_with(". ") {
        digits + 2
    } else {
        0
    };
    let cont = " ".repeat((indent + bullet).min(max / 2));

    let mut rows = Vec::new();
    let mut rest = line;
    let mut prefix = "";
    loop {
        let budget = max.saturating_sub(prefix.len()).max(1);
        // On the first row, a break inside the indent or the bullet would
        // leave a blank row or a bullet on its own.
        let lead = if prefix.is_empty() { indent + bullet } else { 0 };
        let (mut used, mut last_space, mut marker_start, mut cut) = (0, None, None, None);
        for (i, c) in rest.char_indices() {
            if c == '\u{2}' {
                marker_start = Some(i);
            }
            let w = cols(c);
            if used + w > budget {
                cut = Some(i);
                break;
            }
            used += w;
            if c == '\u{3}' {
                marker_start = None;
            }
            if c == ' ' && marker_start.is_none() && i > lead {
                last_space = Some(i);
            }
        }
        let Some(hard) = cut else {
            rows.push(format!("{prefix}{rest}"));
            break;
        };
        // A space if there is one; else just before a link number the cut
        // would split; else wherever the row is full.
        let at = last_space
            .or(marker_start.filter(|&m| m > 0))
            .unwrap_or(hard)
            .max(rest.chars().next().map_or(1, char::len_utf8));
        rows.push(format!("{prefix}{}", rest[..at].trim_end()));
        rest = rest[at..].trim_start();
        prefix = &cont;
        if rest.is_empty() {
            break;
        }
    }
    rows
}

/// Remove every element named in `tags` (and what is inside it) from the tree.
///
/// Iterative, like the tree's own `Drop`: the depth is bounded before this
/// runs, but a stack here costs nothing and cannot overflow.
fn prune_elements(root: &Handle, tags: &[&str]) {
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        node.children.borrow_mut().retain(|child| {
            !matches!(child.data, Element { ref name, .. } if tags.contains(&name.local.as_ref()))
        });
        stack.extend(node.children.borrow().iter().cloned());
    }
}

/// A space between two elements that touch where a real browser would have
/// shown a gap, decided before the layout so the space is counted in it.
///
/// Two cases, both measured.  **Two links**, side by side with no space in the
/// HTML, are parted by CSS in a browser and ran together here: `Hacker
/// News[1]new[2]`, `Jump to navigationJump to search`.  (This was done on the
/// rendered line once; a space added after html2text has laid a row out
/// makes the row one column too wide, and HN's `login` went to a row of its
/// own.)  **Lower case meeting a capital**: GitHub writes `<span>GitHub
/// Copilot</span><span>Write better code</span>` and its stylesheet puts the
/// two on separate lines; we have no stylesheet, so they read `GitHub
/// CopilotWrite`.  Neither is a word split for styling -- `<b>Wiki</b>pedia`
/// is lower meeting lower, and a link that ends mid-word (`<a>Wiki</a>pedia`)
/// is not two links -- and those are left alone.  Preformatted text and code
/// are left alone entirely: a highlighter splits tokens into spans and every
/// byte there is meant.
///
/// One pass over the tree: each node's edges are worked out from its
/// children's, never by re-reading a subtree.
fn space_touching_elements(dom: &RcDom) {
    use html5ever::tree_builder::{NodeOrText, TreeSink};
    use std::collections::HashMap;

    /// A node's first and last visible character (whitespace as ' '), and
    /// the link each one is inside, if any.
    #[derive(Clone, Copy)]
    struct Edge {
        first: char,
        last: char,
        first_link: Option<usize>,
        last_link: Option<usize>,
    }
    let key = |h: &Handle| std::rc::Rc::as_ptr(h) as usize;
    // `None`: a node with no text at all (an icon, a comment).
    let mut edges: HashMap<usize, Option<Edge>> = HashMap::new();
    let mut inserts: Vec<Handle> = Vec::new();

    // Post-order without recursion: a node is visited again once its
    // children have been.  The flag: inside `pre`, `code` and the like, at
    // any depth.
    let mut stack: Vec<(Handle, bool, bool)> = vec![(dom.document.clone(), false, false)];
    while let Some((node, children_done, inside_verbatim)) = stack.pop() {
        let children = node.children.borrow();
        let is_element = matches!(node.data, Element { .. });
        let verbatim = inside_verbatim || matches!(&node.data,
            Element { name, .. } if matches!(name.local.as_ref(), "pre" | "code" | "textarea" | "script" | "style"));
        // An inline SVG is a picture to html2text (see `number_links`); its
        // insides are never drawn, so they decide no spacing.
        if is_svg(&node) {
            drop(children);
            edges.insert(key(&node), None);
            continue;
        }
        if !children_done && !children.is_empty() {
            stack.push((node.clone(), true, inside_verbatim));
            stack.extend(children.iter().map(|c| (c.clone(), false, verbatim)));
            continue;
        }
        let mut own = if children.is_empty() && !is_element {
            // A leaf that is not an element is text, or a doctype or comment,
            // which show nothing.  Blank text reads as `None` from `text_of`.
            let ws = |c: char| if c.is_whitespace() { ' ' } else { c };
            match text_of(&node) {
                Some(t) => t.chars().next().zip(t.chars().last()).map(|(f, l)| Edge {
                    first: ws(f), last: ws(l), first_link: None, last_link: None,
                }),
                None if !matches!(node.data, html2text::Comment { .. } | html2text::Document) => Some(Edge {
                    first: ' ', last: ' ', first_link: None, last_link: None,
                }),
                None => None,
            }
        } else {
            let mut whole: Option<Edge> = None;
            let mut prev: Option<(&Handle, Option<Edge>)> = None;
            for child in children.iter() {
                let e = edges.get(&key(child)).copied().flatten();
                if let Some(c) = e {
                    whole = Some(match whole {
                        Some(w) => Edge { last: c.last, last_link: c.last_link, ..w },
                        None => c,
                    });
                }
                if let (Some((p, Some(l))), Some(r)) = (prev, e) {
                    let both_elements = matches!(p.data, Element { .. }) && matches!(child.data, Element { .. });
                    let two_links = l.last_link.is_some() && r.first_link.is_some() && l.last_link != r.first_link
                        && l.last.is_alphanumeric() && r.first.is_alphanumeric();
                    let case_change = l.last.is_lowercase() && r.first.is_uppercase();
                    if !verbatim && both_elements && (two_links || case_change) {
                        inserts.push(child.clone());
                    }
                }
                // An element with no text (an icon) does not part two words.
                if e.is_some() || !matches!(child.data, Element { .. }) {
                    prev = Some((child, e));
                }
            }
            whole
        };
        // Inside a link, both edges are that link's.
        if matches!(&node.data, Element { name, .. } if name.local.as_ref() == "a")
            && get_attr(&node, "href").is_some()
        {
            if let Some(e) = own.as_mut() {
                e.first_link = Some(key(&node));
                e.last_link = Some(key(&node));
            }
        }
        drop(children);
        edges.insert(key(&node), own);
    }
    for sibling in inserts {
        dom.append_before_sibling(&sibling, NodeOrText::AppendText(" ".into()));
    }
}

/// Parse HTML the way a browser that does not run JavaScript should.
///
/// **`scripting_enabled: false`, and that is the whole point.**  html5ever
/// parses `<noscript>` content as raw TEXT when scripting is on, which is its
/// default and what `html2text::Config::parse_html` gives you -- it hardcodes
/// `TreeBuilderOpts::default()` and exposes no way to change it.  For us that
/// default is simply wrong: we are a text browser with no JavaScript, so
/// `<noscript>` holds the markup written *for* us.
///
/// With the default, a `<noscript>` block renders as visible tag soup -- the
/// reader sees `<style>...</style><meta ...>` as text -- and nothing inside it
/// is reachable by a DOM walk, so a fallback link or a meta refresh in there
/// may as well not exist.  That is exactly what Google's no-JavaScript
/// interstitial does, and why following meta refresh did not rescue it.
///
/// The `RcDom` returned is html2text's own re-exported type, so it feeds
/// `dom_to_render_tree` unchanged -- see the pin note in `Cargo.toml` for why
/// the html5ever version must track html2text's.
fn parse_html_no_scripting(body_bytes: &[u8]) -> Result<RcDom, String> {
    use html5ever::tendril::TendrilSink;
    let opts = html5ever::driver::ParseOpts {
        tree_builder: html5ever::tree_builder::TreeBuilderOpts {
            scripting_enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    html5ever::parse_document(RcDom::default(), opts)
        .from_utf8()
        .read_from(&mut &body_bytes[..])
        .map_err(|e| format!("Parse error: {}", e))
}

/// Longest `<meta refresh>` delay we treat as a redirect, in seconds.
///
/// A refresh with a real delay is a page saying "reload me periodically" -- a
/// scoreboard, a status display -- and jumping instantly would be wrong.  A
/// redirect is written as 0, or a small courtesy delay so the reader can see
/// the "click here if you are not redirected" line.  Five is comfortably above
/// the latter and below any sane refresh interval.
const MAX_META_REFRESH_DELAY: f64 = 5.0;

/// The URL out of a `<meta http-equiv="refresh" content="...">`, if it is a
/// redirect worth following.
///
/// The content syntax is loose and old: a delay, then optionally `; url=...`,
/// with the keyword in any case and the value sometimes quoted.  Returns
/// `None` when there is no URL at all -- that is a self-refresh, and following
/// it would spin -- or when the delay is longer than a redirect would use.
fn parse_meta_refresh(content: &str) -> Option<String> {
    let parts: Vec<&str> = content.split(';').collect();
    // The delay is first *when there is one*.  It is read without consuming
    // the part, because some pages write only `url=...` with no delay at all
    // -- consuming it unconditionally swallowed exactly that form.  An absent
    // or non-numeric delay is treated as 0; a parseable one that is too long
    // is a periodic refresh and is rejected here.
    let delay: f64 = parts
        .first()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(0.0);
    if delay > MAX_META_REFRESH_DELAY {
        return None;
    }
    for part in &parts {
        // Split on the FIRST '=' so a query string in the target keeps its
        // own: `url=/a?b=c` gives key `url`, value `/a?b=c`.  Trimming each
        // side is what admits `url = "..."`, which real pages write and a
        // fixed `"url="` prefix test misses -- found by the test, not by
        // reading the spec.
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("url") {
            let raw = value.trim();
            // Strip one layer of matching quotes, which both forms use.
            let unquoted = raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .or_else(|| raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
                .unwrap_or(raw);
            let target = unquoted.trim();
            if !target.is_empty() {
                return Some(target.to_string());
            }
        }
    }
    None
}

/// Find a followable `<meta http-equiv="refresh">` target in the document.
fn meta_refresh_from_dom(dom: &RcDom) -> Option<String> {
    fn walk(node: &Handle) -> Option<String> {
        if let Element { ref name, .. } = node.data
            && name.local.as_ref() == "meta"
            && get_attr(node, "http-equiv")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("refresh"))
            && let Some(content) = get_attr(node, "content")
            && let Some(target) = parse_meta_refresh(&content) {
                return Some(target);
            }
        for child in node.children.borrow().iter() {
            if let Some(found) = walk(child) {
                return Some(found);
            }
        }
        None
    }
    walk(&dom.document)
}

/// Opens and closes a link number while html2text lays the page out (see
/// [`number_links`]); turned into the `\x02`/`\x03` sentinels once it has.
/// Not the sentinels themselves: html2text measures a control byte as zero
/// columns, so every number would be two columns wider than the wrap
/// believed, and one of its hard-wrap paths `unwrap`s that measurement.
/// Noncharacters, because one cannot arrive in a page's own text.
const MARK_OPEN: char = '\u{FDD0}';
const MARK_CLOSE: char = '\u{FDD1}';

/// Number every link by writing its number into the page, as text at the end
/// of the link, before html2text lays the page out.
///
/// The number used to be added after html2text had wrapped the page, so a
/// link spanning three rows got it three times -- once at the end of each
/// row's piece, which reads as three links -- and a row the number no longer
/// fit on spilled a fragment onto a row of its own.  As text inside the link
/// it is one more word to html2text: counted in the wrap, kept against the
/// last word of the link, written exactly once.  And counted when a table
/// sizes its columns, which a number added at render time (a `TextDecorator`,
/// tried first) is not: HN's `login` cell was given room for `login` and the
/// number was cut through the middle.
///
/// In document order, one number per distinct target.  A link with nothing
/// to show -- an icon with no alt text -- gets none, as before: html2text
/// draws nothing for it, and a number on its own is a link to nowhere visible.
fn number_links(dom: &RcDom) -> Vec<String> {
    use html5ever::tree_builder::{NodeOrText, TreeSink};

    let mut links: Vec<String> = Vec::new();
    let mut marks: Vec<(Handle, usize)> = Vec::new();
    let mut stack = vec![dom.document.clone()];
    while let Some(node) = stack.pop() {
        if let Element { ref name, .. } = node.data {
            if name.local.as_ref() == "a" {
                if let Some(href) = get_attr(&node, "href") {
                    // An anchor on this page goes nowhere a number could take you.
                    let shown = if href.is_empty() || href.starts_with('#') { None } else { last_shown(&node) };
                    if let Some(leaf) = shown {
                        let num = match links.iter().position(|l| *l == href) {
                            Some(pos) => pos + 1,
                            None => {
                                links.push(href);
                                links.len()
                            }
                        };
                        marks.push((leaf, num));
                    }
                }
            }
        }
        // html2text draws an inline SVG as one picture, never its insides: a
        // link in there is never seen and must not take a number.
        if !is_svg(&node) {
            stack.extend(node.children.borrow().iter().rev().cloned());
        }
    }
    // Against the last thing the link shows, not at the end of the element:
    // pretty-printed HTML ends a link in whitespace (`<a>\n  Forums\n</a>`,
    // VCFed's logo) and html2text may break a row there, leaving the number
    // alone on the next.  Whitespace after the number is harmless -- and a
    // space inside the link (`<a>Log in </a><a>Register</a>`) now lands
    // after the number instead of between it and its word.
    for (leaf, num) in marks {
        let mark = format!("{MARK_OPEN}{num}{MARK_CLOSE}");
        let Some(parent) = leaf.parent.take().and_then(|w| {
            let up = w.upgrade();
            leaf.parent.set(Some(w));
            up
        }) else {
            continue;
        };
        let next = {
            let siblings = parent.children.borrow();
            siblings.iter().position(|c| std::rc::Rc::ptr_eq(c, &leaf))
                .and_then(|i| siblings.get(i + 1).cloned())
        };
        let insert = |text: String| match &next {
            Some(n) => dom.append_before_sibling(n, NodeOrText::AppendText(text.into())),
            None => dom.append(&parent, NodeOrText::AppendText(text.into())),
        };
        match text_of(&leaf) {
            // Text: replaced by itself with the number before its trailing
            // whitespace.
            Some(text) => {
                let body = text.trim_end();
                let tail = &text[body.len()..];
                dom.remove_from_parent(&leaf);
                insert(format!("{body}{mark}{tail}"));
            }
            // An image: the number straight after its alt text.
            None => insert(mark),
        }
    }
    links
}

/// Take [`MARK_OPEN`] and [`MARK_CLOSE`] out of everything a page can make
/// html2text draw, before any number is written.
///
/// They are noncharacters, but html5ever passes them through (a parse error,
/// not a refusal), so `Click here&#xFDD0;1&#xFDD1;` drew as a link number
/// pointing at whatever link 1 really was.  Text, and the attributes html2text
/// draws (`alt`); an SVG's title is text like any other.
fn strip_mark_chars(dom: &RcDom) {
    use html5ever::tree_builder::{NodeOrText, TreeSink};

    let is_mark = |c: char| c == MARK_OPEN || c == MARK_CLOSE;
    let mut forged: Vec<(Handle, String)> = Vec::new();
    let mut stack = vec![dom.document.clone()];
    while let Some(node) = stack.pop() {
        if let Element { ref attrs, .. } = node.data {
            for attr in attrs.borrow_mut().iter_mut() {
                if attr.value.contains(is_mark) {
                    attr.value = attr.value.replace(is_mark, "").into();
                }
            }
        } else if let Some(text) = text_of(&node) {
            if text.contains(is_mark) {
                forged.push((node.clone(), text.replace(is_mark, "")));
            }
        }
        stack.extend(node.children.borrow().iter().cloned());
    }
    for (node, clean) in forged {
        // Before the old node and then without it, so it lands in its place.
        dom.append_before_sibling(&node, NodeOrText::AppendText(clean.into()));
        dom.remove_from_parent(&node);
    }
}

/// A text node's exact contents, or `None` for anything else.
///
/// html2text keeps the text type private; its debug rendering is the one
/// public reading (see `get_text_content`), and for a single leaf it is
/// `Text:` + the contents verbatim + a newline.  Blank text is not rendered
/// at all, so it reads as `None` -- which [`last_shown`] never asks about.
fn text_of(node: &Handle) -> Option<String> {
    if matches!(node.data, Element { .. }) || !node.children.borrow().is_empty() {
        return None;
    }
    let s = RcDom::node_as_dom_string(node);
    let t = s.strip_prefix("Text:")?;
    Some(t.strip_suffix('\n').unwrap_or(t).to_string())
}

fn is_svg(node: &Handle) -> bool {
    matches!(&node.data, Element { name, .. } if name.local.as_ref() == "svg")
}

/// The text html2text draws for an inline SVG: its `<title>`, and only when
/// that is the first element inside it.
fn svg_title(svg: &Handle) -> Option<String> {
    let children = svg.children.borrow();
    let first = children.iter().find(|c| matches!(c.data, Element { .. }))?;
    let is_title = matches!(&first.data, Element { name, .. } if name.local.as_ref() == "title");
    let text = get_text_content(first);
    (is_title && !text.is_empty()).then_some(text)
}

/// The last thing html2text will draw for this subtree -- text that is not
/// all whitespace, an image with alt text, or an SVG with a title -- or
/// `None` if it draws nothing.
fn last_shown(node: &Handle) -> Option<Handle> {
    // Children pushed in order, so they come off last first: the first
    // leaf found is the last in the document.
    let mut stack = vec![node.clone()];
    while let Some(n) = stack.pop() {
        let shown = match n.data {
            Element { ref name, .. } => match name.local.as_ref() {
                "script" | "style" => continue,
                // html2text draws no image without a `src`, whatever its alt.
                "img" => get_attr(&n, "alt").is_some_and(|a| !a.trim().is_empty())
                    && get_attr(&n, "src").is_some_and(|s| !s.is_empty()),
                // Drawn as its title alone, and nothing inside it otherwise.
                "svg" => {
                    if svg_title(&n).is_some() {
                        return Some(n);
                    }
                    continue;
                }
                _ => false,
            },
            _ => text_of(&n).is_some_and(|t| !t.trim().is_empty()),
        };
        if shown {
            return Some(n);
        }
        stack.extend(n.children.borrow().iter().cloned());
    }
    None
}

/// A rendered line with its link numbers as the `\x02N\x03` sentinels the
/// screen colours, and nothing else that could be read as one.
fn place_link_markers(line: &str) -> String {
    line.chars()
        .filter_map(|c| match c {
            MARK_OPEN => Some('\u{2}'),
            MARK_CLOSE => Some('\u{3}'),
            // The page's own sentinel bytes are not link numbers.
            '\u{2}' | '\u{3}' => None,
            c => Some(c),
        })
        .collect()
}

/// Parse an HTML body into a rendered WebPage with title, links, and forms.
/// Returns the page and, if the document carries one, a `<meta refresh>`
/// target for the caller to follow.  Reported rather than followed here
/// because following it means another guarded fetch, and this function does
/// not fetch -- the redirect budget lives with the caller that does.
fn render_html_body(
    body_bytes: &[u8],
    final_url: String,
    width: usize,
) -> Result<(WebPage, Option<String>), String> {
    // No table borders.  Most tables on the web are LAYOUT, not data -- a
    // search result or a news item boxed in rules -- and the boxes cost more
    // rows than the content: a DuckDuckGo result took a whole screen, with
    // borders off about six lines (measured, 139 -> 95 lines a page; Hacker
    // News 147 -> 83).  A data table keeps its columns, aligned by spacing.
    // `min_wrap_width`: no column narrower than this, so a short word and
    // its link number are never cut apart by a cell too small for both --
    // a table that cannot give every column that much is drawn one cell
    // under another instead, which on a 40-column screen is the better
    // layout anyway.
    let cfg = config::rich().no_table_borders().min_wrap_width(LINK_WORD_ROOM);
    let dom = parse_html_no_scripting(body_bytes)?;

    // Guard against pathologically deep DOMs before our recursive title/form
    // extractors walk them and overflow the stack, aborting the whole process.
    // On rejection `dom` drops safely at scope end: html2text's `RcDom` `Drop`
    // is iterative, so even a tens-of-thousands-deep tree unwinds without
    // recursion (`test_deeply_nested_html_rejected_without_stack_overflow`
    // exercises exactly this and would SIGABRT-fail if that ever regressed).
    if dom_depth_exceeds(&dom, MAX_DOM_DEPTH) {
        return Err("Page is too deeply nested to render.".to_string());
    }

    let title = extract_title_from_dom(&dom);
    let forms = extract_forms_from_dom(&dom);
    let meta_refresh = meta_refresh_from_dom(&dom);
    // After the forms have their options: a drop-down's choices are not page
    // text, and rendered as text they were -- DuckDuckGo's region picker put
    // sixty country names above the first search result.  They stay
    // reachable through the form screen (F).
    prune_elements(&dom.document, &["select", "datalist"]);
    strip_mark_chars(&dom);
    space_touching_elements(&dom);
    let links = number_links(&dom);

    // The numbers are inside the wrap now, so the wrap gets their room.
    let render = |cfg: &config::Config<html2text::render::RichDecorator>| {
        cfg.dom_to_render_tree(&dom)
            .and_then(|tree| cfg.render_to_lines(tree, width + LINK_MARKER_ROOM))
            .map_err(|e| format!("Render error: {}", e))
    };
    // That minimum applies to every block, not only to table columns: each
    // list or quote level takes its prefix from the width, and ~13 levels
    // down a 40-column screen has less than ten left -- a whole page refused
    // that rendered before the minimum was raised.  So it is a preference,
    // and html2text's own minimum is the fallback.
    let tagged_lines: Vec<TaggedLine<Vec<RichAnnotation>>> =
        render(&cfg).or_else(|_| render(&config::rich().no_table_borders()))?;

    let mut rendered_lines: Vec<String> = Vec::new();
    // A word wider than a whole row is cut at the row's edge by html2text,
    // which knows nothing of link numbers, so the cut can fall inside one.
    // The opened part is carried to the next row and put back against the
    // rest of it -- after whatever prefix that row has (an indent, a quote's
    // `> `), so found by the closing mark rather than by position.
    let mut carry = String::new();
    for tagged_line in &tagged_lines {
        let mut line_text: String = tagged_line.iter().filter_map(|element| match element {
            TaggedLineElement::Str(ts) => Some(ts.s.as_str()),
            _ => None,
        }).collect();
        let opened = std::mem::take(&mut carry);
        if let Some(close) = line_text.find(MARK_CLOSE).filter(|_| !opened.is_empty()) {
            // The rest is the digits the cut left, then the closing mark.
            let rest = line_text[..close].trim_end_matches(|c: char| c.is_ascii_digit()).len();
            line_text.insert_str(rest, &opened);
        }
        if let Some(open) = line_text.rfind(MARK_OPEN)
            && !line_text[open..].contains(MARK_CLOSE)
        {
            carry = line_text.split_off(open);
        }
        let line_text = place_link_markers(&line_text);
        rendered_lines.extend(rewrap_for_screen(&line_text, width + LINK_MARKER_ROOM));
        if rendered_lines.len() >= MAX_RENDERED_LINES {
            rendered_lines.truncate(MAX_RENDERED_LINES);
            break;
        }
    }

    // Post-process: collapse consecutive blank lines and trim trailing whitespace.
    // The html2text library inserts blank lines between block-level elements
    // which causes excessive vertical spacing on narrow/slow terminals.
    let mut cleaned: Vec<String> = Vec::with_capacity(rendered_lines.len());
    let mut prev_blank = false;
    for line in rendered_lines {
        let trimmed = line.trim_end().to_string();
        let is_blank = trimmed.is_empty();
        if is_blank && prev_blank {
            continue; // collapse consecutive blank lines
        }
        prev_blank = is_blank;
        cleaned.push(trimmed);
    }

    Ok((
        WebPage {
            title,
            lines: cleaned,
            links,
            url: final_url,
            forms,
        },
        meta_refresh,
    ))
}

/// Resolve a potentially relative URL against a base URL.
/// Also unwraps DuckDuckGo redirect URLs (`/l/?uddg=<actual_url>`) so that
/// search-result links navigate directly to the target site.
pub(crate) fn resolve_url(base: &str, relative: &str) -> String {
    let resolved = if relative.starts_with("http://") || relative.starts_with("https://") || relative.starts_with("gopher://") {
        relative.to_string()
    } else {
        match url::Url::parse(base) {
            Ok(base_url) => match base_url.join(relative) {
                Ok(r) => r.to_string(),
                Err(_) => relative.to_string(),
            },
            Err(_) => relative.to_string(),
        }
    };

    // Unwrap DuckDuckGo redirect links: extract the real URL from the uddg parameter
    unwrap_ddg_redirect(&resolved)
}

/// If `url` is a DuckDuckGo `/l/?uddg=<encoded_url>` redirect, return the
/// decoded target URL.  Otherwise return the input unchanged.
fn unwrap_ddg_redirect(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url)
        && parsed.host_str() == Some("duckduckgo.com")
        && parsed.path() == "/l/"
        && let Some(target) = parsed.query_pairs().find_map(|(k, v)| {
            if k == "uddg" { Some(v.into_owned()) } else { None }
        })
        && (target.starts_with("http://") || target.starts_with("https://"))
    {
        return target;
    }
    url.to_string()
}

/// Ensure a URL has a scheme, defaulting to https://.
/// If the input has no dots and no scheme, treat it as a search query.
pub(crate) fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") || trimmed.starts_with("gopher://") {
        return trimmed.to_string();
    }
    // If no dots, treat as a search query (DuckDuckGo Lite for text browsers)
    if !trimmed.contains('.') {
        let encoded: String = trimmed
            .bytes()
            .flat_map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                    vec![b as char]
                } else if b == b' ' {
                    vec!['+']
                } else {
                    format!("%{:02X}", b).chars().collect()
                }
            })
            .collect();
        return format!("https://lite.duckduckgo.com/lite/?q={}", encoded);
    }
    format!("https://{}", trimmed)
}

/// Truncate a string to fit within `max_width` visible characters, appending "..." if truncated.
/// Safe for multi-byte UTF-8: always truncates on a char boundary.
pub(crate) fn truncate_to_width(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else if max_width <= 3 {
        ".".repeat(max_width)
    } else {
        let truncated: String = s.chars().take(max_width - 3).collect();
        format!("{}...", truncated)
    }
}

/// Fit a **path** into `max_width` columns by dropping the front, not the tail.
///
/// **A path's tail is the part that changes.** The base is a directory the
/// operator chose once and can read on the configuration screen; what they are
/// looking at a path row to learn is which sub-directory they are in, or which
/// file it is. [`truncate_to_width`] keeps the head, which for a path means
/// every row shows the same constant prefix and hides the answer.
///
/// It became visible when the data directory moved: the default transfer
/// directory went from `transfer/` to `ethernetgateway-data/transfer/`, which is
/// 30 columns of constant, and the PETSCII path rows allow 26. A C64 operator
/// changing directory saw `ethernetgateway-data/tr...` before and after the
/// change — the same text at the root and three levels down. Measured on a live
/// session, not reasoned.
///
/// Cut exactly rather than at a component boundary: rounding to the next `/`
/// would spend columns unpredictably, and on the narrowest screen every column
/// is the difference between showing the sub-directory and not. The leading
/// `...` is what says the front was dropped.
pub(crate) fn truncate_path_to_width(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let keep = max_width - 3;
    let count = s.chars().count();
    let tail: String = s.chars().skip(count - keep).collect();
    format!("...{}", tail)
}

/// Return `true` if the DOM nests deeper than `limit` element levels.
/// Iterative (explicit stack) so it never recurses, and short-circuits as
/// soon as the limit is exceeded — safe on adversarially deep trees.
fn dom_depth_exceeds(dom: &RcDom, limit: usize) -> bool {
    let mut stack = vec![(dom.document.clone(), 1usize)];
    while let Some((node, depth)) = stack.pop() {
        if depth > limit {
            return true;
        }
        for child in node.children.borrow().iter() {
            stack.push((child.clone(), depth + 1));
        }
    }
    false
}

/// Extract the `<title>` text by walking the parsed DOM tree.
fn extract_title_from_dom(dom: &RcDom) -> Option<String> {
    fn find_title(node: &Handle) -> Option<String> {
        if let Element { ref name, .. } = node.data
            && name.local.as_ref() == "title" {
                // NB: parses html2text's *debug* DOM rendering for "Text:"
                // lines — html2text 0.14 doesn't expose the Text node variant
                // for a direct walk.  Pinned by test_dom_text_extraction_
                // debug_format_canary + the test_extract_title_* set (A2).
                let rendered = RcDom::node_as_dom_string(node);
                let text: String = rendered
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("Text:"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let trimmed = text.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        for child in node.children.borrow().iter() {
            if let Some(title) = find_title(child) {
                return Some(title);
            }
        }
        None
    }
    find_title(&dom.document)
}

/// Get an attribute value from an element node.
fn get_attr(node: &Handle, attr_name: &str) -> Option<String> {
    if let Element { ref attrs, .. } = node.data {
        attrs.borrow().iter().find_map(|a| {
            if a.name.local.as_ref() == attr_name {
                Some(a.value.to_string())
            } else {
                None
            }
        })
    } else {
        None
    }
}

/// Extract text content from a node's subtree using RcDom's debug rendering.
///
/// This parses html2text's *debug* DOM output (`node_as_dom_string`) for
/// `"Text:"` lines rather than walking Text nodes directly — html2text 0.14
/// vendors `markup5ever_rcdom` privately and does not expose the `Text`
/// variant, so there is no stable-API alternative.  The debug-format
/// dependency is pinned by `test_dom_text_extraction_debug_format_canary`
/// so a dependency bump can't silently break form-label/option extraction (A2).
fn get_text_content(node: &Handle) -> String {
    let rendered = RcDom::node_as_dom_string(node);
    rendered
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Text:"))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Extract all `<form>` elements from the DOM.
fn extract_forms_from_dom(dom: &RcDom) -> Vec<WebForm> {
    let mut forms = Vec::new();
    find_forms(&dom.document, &mut forms);
    forms
}

fn find_forms(node: &Handle, forms: &mut Vec<WebForm>) {
    if let Element { ref name, .. } = node.data
        && name.local.as_ref() == "form" {
            let action = get_attr(node, "action").unwrap_or_default();
            let method = get_attr(node, "method")
                .unwrap_or_else(|| "get".to_string())
                .to_lowercase();

            let mut fields = Vec::new();
            let mut submit_label = None;
            // Build the id→label map once for the whole form (O(subtree)),
            // then per-field lookup is O(1) — see get_field_label (F1).
            let mut labels = std::collections::HashMap::new();
            collect_field_labels(node, &mut labels);
            // Per-form, and reset for each: "have we already taken this form's
            // submit button?"  Scoped here rather than inline so the recursion
            // below plainly shares one flag across the whole form.
            let mut submit_taken = false;
            extract_form_fields(node, &mut fields, &mut submit_label, &mut submit_taken, &labels, None);

            let label = submit_label.unwrap_or_else(|| {
                format!("Form {}", forms.len() + 1)
            });

            forms.push(WebForm { action, method, label, fields });
            return; // don't recurse into nested forms
        }
    for child in node.children.borrow().iter() {
        find_forms(child, forms);
    }
}

/// Try to find a human-readable label for a form field, checking (in order):
/// placeholder, aria-label, title, associated `<label>` element, then field name.
///
/// `labels` is a pre-built `id → label-text` map for the whole form (see
/// [`collect_field_labels`]).  It replaces a per-field recursive subtree scan
/// for `<label for="id">`: that was O(fields × subtree), so a hostile page of
/// tens of thousands of bare `<input id=…>` (still under `MAX_BODY_SIZE`) cost
/// quadratic CPU on a shared render thread with no time budget — a soft-DoS
/// (round-6 F1).  The one-pass map makes the whole form O(subtree).
///
/// Then a `<label>` wrapping the field (`enclosing`), and only then a name
/// made readable by [`fallback_label`] -- the raw name is the page's internal
/// identifier, and DuckDuckGo's search box, region and date fields showed as
/// `q`, `kl` and `df`.
fn get_field_label(
    node: &Handle,
    field_name: &str,
    labels: &std::collections::HashMap<String, String>,
    enclosing: Option<&str>,
    kind: &str,
) -> String {
    // Each source must have something in it: `placeholder=""` is present
    // and says nothing, and must not stop the search for the next one.
    let some = |v: Option<String>| v.filter(|l| !l.trim().is_empty());
    some(get_attr(node, "placeholder"))
        .or_else(|| some(get_attr(node, "aria-label")))
        .or_else(|| some(get_attr(node, "title")))
        .or_else(|| some(get_attr(node, "id").and_then(|id| labels.get(&id).cloned())))
        .or_else(|| some(enclosing.map(str::to_string)))
        .unwrap_or_else(|| fallback_label(field_name, kind))
}

/// A label for a field whose page gave it none: from what kind of field it
/// is, from a name everybody uses (`q` is a search box), or from the name
/// tidied up.  A one- or two-letter name says nothing, so it gets a word.
fn fallback_label(field_name: &str, kind: &str) -> String {
    let by_kind = match kind {
        "search" => Some("Search"),
        "email" => Some("Email"),
        "password" => Some("Password"),
        "url" => Some("Web address"),
        "tel" => Some("Phone"),
        "number" => Some("Number"),
        "date" => Some("Date"),
        _ => None,
    };
    let name = field_name.to_ascii_lowercase();
    let by_name = match name.as_str() {
        "q" | "query" | "search" | "s" | "keyword" | "keywords" | "term" | "terms" | "search_query" => {
            Some("Search")
        }
        "user" | "username" | "login" | "userid" | "user_id" => Some("Username"),
        "pass" | "passwd" | "password" | "pwd" => Some("Password"),
        "email" | "mail" => Some("Email"),
        _ => None,
    };
    if let Some(l) = by_kind.or(by_name) {
        return l.to_string();
    }
    let words: String = field_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if words.chars().count() <= 2 {
        return if kind == "select" { "Choose" } else { "Text" }.to_string();
    }
    let mut c = words.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or(words)
}

/// A `<label>`'s own words, without the text of a field inside it -- a label
/// wrapping a drop-down would otherwise read as every option it offers.
///
/// **Bounded, because it runs once per label.**  html5ever does not close a
/// `<label>` when another opens, so labels nest; walking each one's whole
/// subtree made a page of nested labels over a megabyte of text cost the
/// square of both on the shared render thread -- the soft-DoS round-6 F1
/// closed for `<label for>`.  So: no descent into a nested label or form,
/// and no more than `LABEL_MAX` characters gathered.
fn label_own_text(node: &Handle) -> String {
    const LABEL_MAX: usize = 100;
    let mut out = String::new();
    let mut stack: Vec<Handle> = node.children.borrow().iter().rev().cloned().collect();
    while let Some(child) = stack.pop() {
        if out.chars().count() >= LABEL_MAX {
            break;
        }
        match child.data {
            Element { ref name, .. }
                if matches!(
                    name.local.as_ref(),
                    "select" | "textarea" | "option" | "datalist" | "label" | "form"
                ) => {}
            Element { .. } => stack.extend(child.children.borrow().iter().rev().cloned()),
            _ => {
                let text = get_text_content(&child);
                let text = text.trim();
                if !text.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.extend(text.chars().take(LABEL_MAX));
                }
            }
        }
    }
    out.chars().take(LABEL_MAX).collect()
}

/// Walk a form subtree ONCE, mapping each `<label for="id">`'s id to its text.
/// First occurrence wins (matching the old depth-first-first search).  Built
/// once per form so per-field label lookup is O(1) (round-6 F1).
fn collect_field_labels(node: &Handle, labels: &mut std::collections::HashMap<String, String>) {
    if let Element { ref name, .. } = node.data
        && name.local.as_ref() == "label"
        && let Some(for_attr) = get_attr(node, "for")
    {
        let text = get_text_content(node);
        if !text.is_empty() {
            labels.entry(for_attr).or_insert(text);
        }
    }
    for child in node.children.borrow().iter() {
        collect_field_labels(child, labels);
    }
}

fn extract_form_fields(
    node: &Handle,
    fields: &mut Vec<FormField>,
    submit_label: &mut Option<String>,
    submit_taken: &mut bool,
    labels: &std::collections::HashMap<String, String>,
    enclosing: Option<&str>,
) {
    // Inside a `<label>`, its words name the field it wraps.
    let own;
    let mut enclosing = enclosing;
    if let Element { ref name, .. } = node.data
        && name.local.as_ref() == "label"
    {
        own = label_own_text(node);
        enclosing = Some(own.as_str());
    }
    if let Element { ref name, .. } = node.data {
        let tag = name.local.as_ref();
        match tag {
            "input" => {
                let input_type = get_attr(node, "type")
                    .unwrap_or_else(|| "text".to_string())
                    .to_lowercase();
                let field_name = get_attr(node, "name").unwrap_or_default();
                let value = get_attr(node, "value").unwrap_or_default();

                match input_type.as_str() {
                    "hidden" => {
                        if !field_name.is_empty() {
                            fields.push(FormField::Hidden { name: field_name, value });
                        }
                    }
                    "submit" => {
                        // ONLY THE FIRST submit control is sent.
                        //
                        // A submit button is a "successful control" only when
                        // it is the one that submitted the form -- a browser
                        // sends the button you pressed, never all of them.
                        // This sent every named one, and that is not a
                        // theoretical difference: Google's home page carries
                        // `btnG` (Search) AND `btnI` (I'm Feeling Lucky), so
                        // searching from it sent both, and `btnI` means "skip
                        // the results and jump to the first hit".  The reply
                        // was a 302 to google.com/url?q=..., whose interstitial
                        // is where the search appeared to get stuck.
                        //
                        // First, because that is the form's DEFAULT button --
                        // the one a browser activates when you press Enter in a
                        // text field, which is what submitting from our form UI
                        // means.
                        //
                        // **The label and the name come from this one
                        // decision**, so the button shown and the button sent
                        // cannot disagree.  They used to be decided
                        // separately -- the label by the first submit with a
                        // non-empty *value*, the name by the first with a
                        // non-empty *name* -- so an unnamed `value="Go"`
                        // followed by `name="btnI" value="Lucky"` displayed
                        // "Go" and posted `btnI`, which is the very swap the
                        // comment above says was fixed.
                        //
                        // An unnamed default button therefore sends nothing,
                        // which is also what a real browser does: a control
                        // with no name is not successful.
                        if !*submit_taken {
                            *submit_taken = true;
                            if !value.is_empty() {
                                *submit_label = Some(value.clone());
                            }
                            if !field_name.is_empty() {
                                fields.push(FormField::Hidden { name: field_name, value });
                            }
                        }
                    }
                    "checkbox" => {
                        if !field_name.is_empty() {
                            let label = get_field_label(node, &field_name, labels, enclosing, "checkbox");
                            let val = if value.is_empty() { "on".to_string() } else { value };
                            let checked = get_attr(node, "checked").is_some();
                            fields.push(FormField::Checkbox { name: field_name, value: val, checked, label });
                        }
                    }
                    "radio" => {
                        if !field_name.is_empty() {
                            let label = get_attr(node, "aria-label")
                                .filter(|l| !l.trim().is_empty())
                                .or_else(|| enclosing.filter(|l| !l.trim().is_empty()).map(str::to_string))
                                .unwrap_or_else(|| value.clone());
                            let checked = get_attr(node, "checked").is_some();
                            fields.push(FormField::Radio { name: field_name, value, checked, label });
                        }
                    }
                    "image" | "button" | "reset" | "file" => {} // skip
                    _ => {
                        if !field_name.is_empty() {
                            let label = get_field_label(node, &field_name, labels, enclosing, &input_type);
                            fields.push(FormField::Text {
                                name: field_name, value, label, input_type,
                            });
                        }
                    }
                }
            }
            "textarea" => {
                let field_name = get_attr(node, "name").unwrap_or_default();
                if !field_name.is_empty() {
                    let value = get_text_content(node);
                    let label = get_field_label(node, &field_name, labels, enclosing, "textarea");
                    fields.push(FormField::TextArea { name: field_name, value, label });
                }
            }
            "select" => {
                let field_name = get_attr(node, "name").unwrap_or_default();
                if !field_name.is_empty() {
                    let mut options = Vec::new();
                    let mut selected = 0;
                    extract_select_options(node, &mut options, &mut selected);
                    let label = get_field_label(node, &field_name, labels, enclosing, "select");
                    fields.push(FormField::Select { name: field_name, options, selected, label });
                }
            }
            "button" => {
                // Lower-cased like `input`'s above: the HTML `type` attribute
                // is case-insensitive, so `<button type="Submit">` is a submit
                // control.  Missing it let a later `<input type=submit>` take
                // both the label and the posted name -- the very disagreement
                // this first-one-wins rule exists to close.
                let btn_type = get_attr(node, "type")
                    .unwrap_or_else(|| "submit".to_string())
                    .to_lowercase();
                // A `<button type=submit>` is a submit control too, so it goes
                // through the same first-one-wins claim as `<input
                // type=submit>` above -- otherwise a leading `<button>` names
                // the UI while a later `<input>` is what gets posted, which is
                // the same disagreement by another route.  A named one is a
                // successful control and is sent.
                if btn_type == "submit" && !*submit_taken {
                    *submit_taken = true;
                    let text = get_text_content(node);
                    if !text.is_empty() {
                        *submit_label = Some(text);
                    }
                    let name = get_attr(node, "name").unwrap_or_default();
                    if !name.is_empty() {
                        let value = get_attr(node, "value").unwrap_or_default();
                        fields.push(FormField::Hidden { name, value });
                    }
                }
            }
            _ => {}
        }
    }
    for child in node.children.borrow().iter() {
        if let Element { ref name, .. } = child.data
            && name.local.as_ref() == "form" {
                continue;
            }
        extract_form_fields(child, fields, submit_label, submit_taken, labels, enclosing);
    }
}

fn extract_select_options(node: &Handle, options: &mut Vec<(String, String)>, selected: &mut usize) {
    for child in node.children.borrow().iter() {
        if let Element { ref name, .. } = child.data {
            if name.local.as_ref() == "option" {
                let value = get_attr(child, "value")
                    .unwrap_or_else(|| get_text_content(child));
                let display = get_text_content(child);
                if get_attr(child, "selected").is_some() {
                    *selected = options.len();
                }
                options.push((value, display));
            } else if name.local.as_ref() == "optgroup" {
                extract_select_options(child, options, selected);
            }
        }
    }
}

// ─── Bookmarks ────────────────────────────────────────────────

/// A single bookmark entry.
#[derive(Clone, Debug)]
pub(crate) struct Bookmark {
    pub title: String,
    pub url: String,
}

/// Load bookmarks from the bookmarks file. Returns an empty list on any error.
pub(crate) fn load_bookmarks() -> Vec<Bookmark> {
    let content = match std::fs::read_to_string(BOOKMARKS_FILE) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut bookmarks = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((url, title)) = trimmed.split_once(' ') {
            bookmarks.push(Bookmark {
                url: url.to_string(),
                title: title.to_string(),
            });
        } else {
            bookmarks.push(Bookmark {
                url: trimmed.to_string(),
                title: trimmed.to_string(),
            });
        }
    }
    bookmarks
}

/// Save bookmarks to the bookmarks file. Returns true on success.
fn save_bookmarks(bookmarks: &[Bookmark]) -> bool {
    // The directory must exist before anything in it can be written.
    // `main` creates it at startup, but relying on that alone was wrong twice
    // over: a unit test reaches this writer without going through `main` (which
    // is how the missing directory was found), and an operator can remove the
    // folder while the gateway is running.  The parent of the path we are about
    // to write, never a constant -- see `config::ensure_parent_dir`.
    crate::config::ensure_parent_dir(BOOKMARKS_FILE);
    let content: String = bookmarks
        .iter()
        .map(|b| {
            // Sanitize: strip newlines and spaces from URL, collapse whitespace in title
            let safe_url: String = b.url.chars().filter(|&c| !c.is_whitespace()).collect();
            let safe_title: String = b.title.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{} {}", safe_url, safe_title)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = format!("{}.{}.tmp", BOOKMARKS_FILE, std::process::id());
    if let Err(e) = std::fs::write(&tmp, &content).and_then(|()| std::fs::rename(&tmp, BOOKMARKS_FILE)) {
        glog!("Warning: could not save bookmarks: {}", e);
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Add a bookmark. Returns true if added, false if duplicate or at capacity.
pub(crate) fn add_bookmark(url: &str, title: &str) -> bool {
    let mut bookmarks = load_bookmarks();
    if bookmarks.iter().any(|b| b.url == url) {
        return false; // duplicate
    }
    if bookmarks.len() >= MAX_BOOKMARKS {
        return false; // at capacity
    }
    bookmarks.push(Bookmark {
        url: url.to_string(),
        title: title.to_string(),
    });
    save_bookmarks(&bookmarks)
}

/// Remove a bookmark by index (0-based). Returns true if removed.
pub(crate) fn remove_bookmark(index: usize) -> bool {
    let mut bookmarks = load_bookmarks();
    if index >= bookmarks.len() {
        return false;
    }
    bookmarks.remove(index);
    save_bookmarks(&bookmarks)
}

use crate::aichat::wrap_line;

// ─── Gopher protocol ───────────────────────────────────────

/// Default Gopher port.
const GOPHER_PORT: u16 = 70;
/// Timeout for Gopher TCP connections.
const GOPHER_TIMEOUT_SECS: u64 = 15;
/// Maximum Gopher response size (512 KB).
const GOPHER_MAX_BODY: usize = 512 * 1024;

/// Read a response of at most `max` bytes, giving up at `deadline`.
///
/// **One deadline for the whole body**, the counterpart of HTTP's
/// `timeout_global`.  The socket's read timeout alone bounds each *read*, so a
/// server sending one byte every fourteen seconds kept a 512 KB fetch -- and
/// the session waiting on it, which no key can interrupt -- alive for weeks.
fn read_gopher_body(
    stream: &mut std::net::TcpStream,
    max: usize,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut body = Vec::new();
    let mut buf = [0u8; 8192];
    while body.len() < max {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("Read error: the server took too long to send the page".into());
        }
        // `set_read_timeout(Some(0))` is an error, hence the floor.
        stream
            .set_read_timeout(Some(left.max(std::time::Duration::from_millis(1))))
            .map_err(|e| format!("Read error: {}", e))?;
        let want = buf.len().min(max - body.len());
        match stream.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("Read error: {}", e)),
        }
    }
    Ok(body)
}

/// Parse a gopher:// URL into (host, port, item_type, selector).
///
/// Format: `gopher://host[:port][/[type][selector]]`
/// Default port is 70, default type is '1' (directory), default selector is empty.
fn parse_gopher_url(url: &str) -> Result<(String, u16, char, String), String> {
    let rest = url.strip_prefix("gopher://").ok_or("Not a gopher URL")?;

    // Split host[:port] from /path
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };

    // Parse host and optional port. Port 0 is not a valid network
    // port, so fall back to the default rather than attempt to connect.
    let (host, port) = if let Some(colon) = host_port.rfind(':') {
        let port_str = &host_port[colon + 1..];
        match port_str.parse::<u16>() {
            Ok(p) if p > 0 => (host_port[..colon].to_string(), p),
            _ => (host_port.to_string(), GOPHER_PORT),
        }
    } else {
        (host_port.to_string(), GOPHER_PORT)
    };

    if host.is_empty() {
        return Err("Empty host".into());
    }

    // Parse item type and selector from path
    let (item_type, selector) = if path.is_empty() {
        ('1', String::new()) // root directory
    } else {
        let first = path.chars().next().unwrap();
        if first.is_ascii_alphanumeric() || first == 'i' || first == '+' {
            (first, path[1..].to_string())
        } else {
            ('1', path.to_string())
        }
    };

    Ok((host, port, item_type, selector))
}

/// Build a gopher:// URL from components.
fn build_gopher_url(host: &str, port: u16, item_type: char, selector: &str) -> String {
    if port == GOPHER_PORT {
        format!("gopher://{}/{}{}", host, item_type, selector)
    } else {
        format!("gopher://{}:{}/{}{}", host, port, item_type, selector)
    }
}

/// Fetch a Gopher resource and render it as a `WebPage`.
///
/// Blocking call — run via `spawn_blocking`.
pub(crate) fn fetch_gopher(url: &str, width: usize) -> Result<WebPage, String> {
    let (host, port, item_type, selector) = parse_gopher_url(url)?;

    // Connect and send selector
    let addr = format!("{}:{}", host, port);
    let sock_addr = {
        use std::net::ToSocketAddrs;
        addr.to_socket_addrs()
            .map_err(|e| format!("DNS error: {}", e))?
            .next()
            .ok_or_else(|| "Could not resolve host".to_string())?
    };
    // SSRF guard: gopher resolves and connects directly, so block an
    // internal/loopback target before we dial it.
    if !internal_fetch_allowed() && is_internal_ip(sock_addr.ip()) {
        return Err(format!("Blocked: {} is an internal address", host));
    }
    let stream = std::net::TcpStream::connect_timeout(
        &sock_addr,
        std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS),
    )
    .map_err(|e| format!("Connection failed: {}", e))?;

    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS)))
        .ok();

    let mut stream = std::io::BufWriter::new(stream);
    use std::io::Write;
    // Strip CR/LF from the selector so a user-supplied search query
    // containing literal \r\n can't inject extra protocol lines.
    // Gopher selectors are single-line by spec; only TAB is meaningful
    // (delimits item-type 7 search queries).  NUL is also stripped to
    // avoid C-string-style truncation by old gopher daemons.
    let safe_selector: String = selector
        .chars()
        .filter(|&c| c != '\r' && c != '\n' && c != '\0')
        .collect();
    stream
        .write_all(format!("{}\r\n", safe_selector).as_bytes())
        .map_err(|e| format!("Write error: {}", e))?;
    stream.flush().map_err(|e| format!("Flush error: {}", e))?;

    // Read response, against one deadline for the whole of it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(GOPHER_TIMEOUT_SECS);
    let body = read_gopher_body(stream.get_mut(), GOPHER_MAX_BODY, deadline)?;

    let text = String::from_utf8_lossy(&body);
    let final_url = build_gopher_url(&host, port, item_type, &selector);

    let mut page = match item_type {
        '0' => {
            // Plain text file — just wrap and display
            let lines: Vec<String> = text
                .lines()
                .flat_map(|line| {
                    let clean = line.trim_end_matches('\r');
                    wrap_line(clean, width)
                })
                .take(MAX_RENDERED_LINES)
                .collect();
            WebPage {
                title: Some(selector.rsplit('/').next().unwrap_or("Text").to_string()),
                lines,
                links: Vec::new(),
                url: final_url,
                forms: Vec::new(),
            }
        }
        '1' | '7' => {
            // Directory listing or search results — parse Gopher menu
            render_gopher_directory(&text, &host, port, width, final_url)?
        }
        _ => {
            // Unsupported type — show as plain text
            let lines: Vec<String> = text
                .lines()
                .flat_map(|line| wrap_line(line.trim_end_matches('\r'), width))
                .take(MAX_RENDERED_LINES)
                .collect();
            WebPage {
                title: Some(format!("Gopher (type {})", item_type)),
                lines,
                links: Vec::new(),
                url: final_url,
                forms: Vec::new(),
            }
        }
    };

    // Strip any terminal-control bytes a hostile gopher server smuggled into
    // the menu labels or text before they reach the terminal (see
    // WebPage::sanitize).
    page.sanitize();
    Ok(page)
}

/// Parse a Gopher directory listing into a WebPage with numbered links.
fn render_gopher_directory(
    text: &str,
    current_host: &str,
    current_port: u16,
    width: usize,
    final_url: String,
) -> Result<WebPage, String> {
    let mut lines: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');

        // End of listing
        if line == "." {
            break;
        }
        if line.is_empty() {
            lines.push(String::new());
            continue;
        }

        let item_type = line.chars().next().unwrap_or('i');
        let rest = &line[item_type.len_utf8()..];
        let fields: Vec<&str> = rest.split('\t').collect();

        let display = fields.first().unwrap_or(&"");
        let selector = fields.get(1).unwrap_or(&"");
        let host = fields.get(2).unwrap_or(&current_host);
        let port: u16 = fields
            .get(3)
            .and_then(|p| p.parse().ok())
            .unwrap_or(current_port);

        match item_type {
            'i' | '3' => {
                // Informational text or error — display as-is, wrapped
                let prefix = if item_type == '3' { "ERR: " } else { "" };
                let full = format!("{}{}", prefix, display);
                for wrapped in wrap_line(&full, width) {
                    lines.push(wrapped);
                }
            }
            '0' | '1' | '7' => {
                // Text file, directory, or search — create a link
                let link_url = if item_type == '7' {
                    // Search items: mark with ?search so the browser knows to prompt
                    format!("{}?search", build_gopher_url(host, port, item_type, selector))
                } else {
                    build_gopher_url(host, port, item_type, selector)
                };
                links.push(link_url);
                let link_num = links.len();
                let type_marker = match item_type {
                    '1' => "/",
                    '7' => "?",
                    _ => "",
                };
                let label = format!("{}{}", display, type_marker);
                for (i, wrapped) in wrap_line(&label, width.saturating_sub(5)).iter().enumerate() {
                    if i == 0 {
                        lines.push(format!("{}\x02{}\x03", wrapped, link_num));
                    } else {
                        lines.push(format!("  {}", wrapped));
                    }
                }
            }
            'h' => {
                // HTML link — extract URL if selector starts with "URL:"
                let url = selector.strip_prefix("URL:").unwrap_or(selector);
                links.push(url.to_string());
                let link_num = links.len();
                for (i, wrapped) in wrap_line(display, width.saturating_sub(5)).iter().enumerate() {
                    if i == 0 {
                        lines.push(format!("{}\x02{}\x03", wrapped, link_num));
                    } else {
                        lines.push(format!("  {}", wrapped));
                    }
                }
            }
            _ => {
                // Binary, image, etc. — show label but no link
                let type_label = match item_type {
                    '9' => "[BIN]",
                    'g' | 'I' | 'p' => "[IMG]",
                    's' => "[SND]",
                    _ => "[???]",
                };
                for wrapped in wrap_line(&format!("{} {}", type_label, display), width) {
                    lines.push(wrapped);
                }
            }
        }

        if lines.len() >= MAX_RENDERED_LINES {
            break;
        }
    }

    // Extract a title from the URL selector
    let title = {
        let (_, _, _, sel) = parse_gopher_url(&final_url).unwrap_or_default();
        if sel.is_empty() {
            Some(format!("Gopher: {}", current_host))
        } else {
            Some(format!("Gopher: {}", sel.rsplit('/').next().unwrap_or(&sel)))
        }
    };

    Ok(WebPage {
        title,
        lines,
        links,
        url: final_url,
        forms: Vec::new(),
    })
}

/// Returns true if the URL is a Gopher search that needs a query term.
pub(crate) fn is_gopher_search(url: &str) -> bool {
    url.starts_with("gopher://") && url.ends_with("?search")
}

/// Strip the `?search` sentinel and append a tab + query to form the search URL.
pub(crate) fn build_gopher_search_url(url: &str, query: &str) -> String {
    let base = url.strip_suffix("?search").unwrap_or(url);
    // For Gopher search, the query is appended to the selector after a tab.
    // Re-parse, append query to selector, rebuild.
    if let Ok((host, port, item_type, selector)) = parse_gopher_url(base) {
        let search_selector = format!("{}\t{}", selector, query);
        build_gopher_url(&host, port, item_type, &search_selector)
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::aichat::fold_terminal_safe;
    use super::*;

    /// A trickling server cannot hold a gopher fetch past its deadline.
    #[test]
    fn test_a_trickling_gopher_server_is_cut_off_at_the_deadline() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // One byte every 50 ms -- each read well inside any per-read
            // timeout, which is exactly what used to keep the fetch alive.
            for _ in 0..200 {
                if s.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let start = std::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(400);
        let got = super::read_gopher_body(&mut stream, 1 << 20, deadline);
        assert!(got.is_err(), "the fetch outlived its deadline: {got:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        drop(stream);
        let _ = server.join();
    }

    /// The positive control: a server that answers and closes is read whole.
    #[test]
    fn test_a_prompt_gopher_server_is_read_whole() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"iHello\tfake\t(NULL)\t0\r\n.\r\n").unwrap();
        });
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let got = super::read_gopher_body(&mut stream, 1 << 20, deadline).unwrap();
        assert!(got.starts_with(b"iHello"));
        server.join().unwrap();
    }

    /// The measured defect: an HTML table renders as box-drawing characters.
    ///
    /// These five are exactly what a live capture of `telnetbible.com` page 3
    /// contained — 284 of the first alone — and each is three bytes of UTF-8
    /// that a 7-bit console draws as three unrenderable characters.
    /// `<meta refresh>` content is loose, old syntax: a delay, then an
    /// optional `url=`, keyword in any case, value sometimes quoted.
    #[test]
    fn test_parse_meta_refresh_handles_the_old_syntax() {
        // The shape Google's JS interstitial uses.
        assert_eq!(
            parse_meta_refresh("0;url=/httpservice/retry/enablejs?sei=abc"),
            Some("/httpservice/retry/enablejs?sei=abc".to_string()),
        );
        // Case, spacing and quotes all vary in the wild.
        assert_eq!(parse_meta_refresh("0; URL=/a"), Some("/a".to_string()));
        assert_eq!(parse_meta_refresh("0;Url='/b'"), Some("/b".to_string()));
        assert_eq!(parse_meta_refresh("2 ; url = \"/c\""), Some("/c".to_string()));
        // Some pages omit the delay entirely.
        assert_eq!(parse_meta_refresh("url=/d"), Some("/d".to_string()));
    }

    /// Strings WE generate must be ASCII, because they bypass the fold.
    ///
    /// `prepend_tls_downgrade_notice` runs after `WebPage::sanitize`, so its
    /// text reaches the wire exactly as written -- it never passes through the
    /// terminal-safe fold that page content does.  The notice used to carry an
    /// em-dash: three bytes of UTF-8, three unrenderable characters on a 7-bit
    /// console, which made our own security warning arrive as garbage on
    /// precisely the terminals this gateway serves.
    #[test]
    fn test_tls_downgrade_notice_is_pure_ascii() {
        let mut page = WebPage {
            title: None,
            lines: vec!["body".to_string()],
            links: vec![],
            forms: vec![],
            url: "http://example.com/".to_string(),
        };
        prepend_downgrade_notice(&mut page, 80, DowngradeReason::TlsFailed);
        for line in &page.lines {
            assert!(
                line.is_ascii(),
                "a line we generate ourselves is not ASCII and bypasses the fold: {line:?}",
            );
        }
        assert!(
            page.lines.iter().any(|l| l.contains("HTTPS failed")),
            "the warning must actually be there: {:?}",
            page.lines,
        );

        // The other reason takes the same path and must be ASCII too -- and
        // must NOT cry "TLS error" at a site that simply has no HTTPS, which
        // is ordinary on the web this browser is for.
        let mut plain = WebPage {
            title: None,
            lines: vec!["body".to_string()],
            links: vec![],
            forms: vec![],
            url: "http://textfiles.com/".to_string(),
        };
        prepend_downgrade_notice(&mut plain, 80, DowngradeReason::NoHttps);
        for line in &plain.lines {
            assert!(line.is_ascii(), "not ASCII: {line:?}");
        }
        let text = plain.lines.join(" ");
        assert!(text.contains("No HTTPS"), "must say what happened: {text:?}");
        assert!(!text.contains("TLS error"), "must not blame TLS: {text:?}");
    }

    /// `<noscript>` content is markup for us, not text.
    ///
    /// html5ever parses `<noscript>` as raw TEXT when scripting is enabled,
    /// which is its default.  For a browser that runs no JavaScript that is
    /// backwards: the block holds the markup written *for* us.  With the
    /// default, this meta is invisible to any DOM walk and the tags render as
    /// visible tag soup -- which is exactly what Google's no-JS interstitial
    /// does, and why following meta refresh alone did not rescue it.
    #[test]
    fn test_noscript_content_is_parsed_as_markup() {
        let html = br#"<html><body><noscript>
            <style>p{display:none}</style>
            <meta http-equiv="refresh" content="0;url=/fallback">
            <div>Please click <a href="/fallback">here</a>.</div>
            </noscript></body></html>"#;
        let (page, refresh) =
            render_html_body(html, "https://example.com/x".to_string(), 80).expect("renders");

        assert_eq!(
            refresh.as_deref(),
            Some("/fallback"),
            "a meta refresh inside <noscript> must be reachable",
        );
        // The link inside it is a real link, not text.
        assert!(
            page.links.iter().any(|l| l.contains("/fallback")),
            "the <noscript> link should be a link: {:?}",
            page.links,
        );
        // And the markup must not be rendered as visible text.
        let text = page.lines.join("\n");
        assert!(!text.contains("<style"), "tag soup leaked into the page: {text:?}");
        assert!(!text.contains("http-equiv"), "tag soup leaked into the page: {text:?}");
    }

    /// The two cases that must NOT be followed.
    #[test]
    fn test_parse_meta_refresh_refuses_self_refresh_and_slow_reloads() {
        // No URL is a self-refresh -- "reload me".  Following it would spin.
        assert_eq!(parse_meta_refresh("30"), None);
        assert_eq!(parse_meta_refresh("0"), None);
        assert_eq!(parse_meta_refresh(""), None);
        // A long delay is a periodic reload (a scoreboard, a status page),
        // not a redirect; jumping instantly would be wrong.
        assert_eq!(parse_meta_refresh("30;url=/live"), None);
        assert_eq!(parse_meta_refresh("6;url=/live"), None);
        // ...but a short courtesy delay is still a redirect.
        assert_eq!(parse_meta_refresh("5;url=/x"), Some("/x".to_string()));
    }

    /// The target is found through the document, and only on a refresh.
    #[test]
    fn test_meta_refresh_is_read_from_the_document() {
        let html = br#"<html><head>
            <meta http-equiv="Content-Type" content="text/html; charset=utf-8">
            <meta http-equiv="REFRESH" content="0; url=/next">
            </head><body>hi</body></html>"#;
        let (_page, refresh) =
            render_html_body(html, "https://example.com/a".to_string(), 80).expect("renders");
        assert_eq!(refresh.as_deref(), Some("/next"), "should find the refresh");

        // A document with no refresh reports none -- and Content-Type, which
        // is also an http-equiv, must not be mistaken for one.
        let plain = br#"<html><head>
            <meta http-equiv="Content-Type" content="text/html; charset=utf-8">
            </head><body>hi</body></html>"#;
        let (_p2, none) =
            render_html_body(plain, "https://example.com/b".to_string(), 80).expect("renders");
        assert_eq!(none, None, "Content-Type is not a refresh");
    }

    /// A form sends the button you pressed, not every button on it.
    ///
    /// This is the shape of Google's home page, and it was a real defect:
    /// `btnG` (Search) and `btnI` (I'm Feeling Lucky) are both named submit
    /// buttons, we sent both, and `btnI` means "skip the results, jump to the
    /// first hit".  Google answered 302 to its `/url?q=...` interstitial, which
    /// is where a search appeared to get stuck.  Measured: with `btnI` the
    /// reply is 302, without it 200.
    ///
    /// The first one is kept because that is the form's DEFAULT button -- what
    /// a browser activates on Enter in a text field -- and it is the same one
    /// `submit_label` names, so the button shown and the button sent agree.
    #[test]
    fn test_form_submits_only_the_first_submit_button() {
        let html = br#"<html><body><form action="/search">
            <input type="hidden" name="hl" value="en">
            <input name="q" value="">
            <input type="submit" name="btnG" value="Google Search">
            <input type="submit" name="btnI" value="I&#39;m Feeling Lucky">
        </form></body></html>"#;
        let (page, _refresh) = render_html_body(html, "https://example.com/".to_string(), 80)
            .expect("renders");
        let form = page.forms.first().expect("one form");

        let names: Vec<&str> = form
            .fields
            .iter()
            .map(|f| match f {
                FormField::Text { name, .. }
                | FormField::Hidden { name, .. }
                | FormField::TextArea { name, .. }
                | FormField::Select { name, .. }
                | FormField::Checkbox { name, .. }
                | FormField::Radio { name, .. } => name.as_str(),
            })
            .collect();

        assert!(names.contains(&"hl"), "ordinary hidden fields must survive: {names:?}");
        assert!(names.contains(&"q"), "the text field must survive: {names:?}");
        assert!(
            names.contains(&"btnG"),
            "the form's default (first) submit button must be sent: {names:?}",
        );
        assert!(
            !names.contains(&"btnI"),
            "a second submit button must NOT be sent -- sending btnI is what turned \
             a search into an I'm Feeling Lucky redirect: {names:?}",
        );
        // And the button named on screen is the one actually sent.
        assert_eq!(form.label, "Google Search", "label should name the sent button");
    }

    /// Collect every field name on the first form of a rendered document.
    fn form_field_names(html: &[u8]) -> (Vec<String>, String) {
        let (page, _refresh) = render_html_body(html, "https://example.com/".to_string(), 80)
            .expect("renders");
        let form = page.forms.first().expect("one form");
        let names = form
            .fields
            .iter()
            .map(|f| match f {
                FormField::Text { name, .. }
                | FormField::Hidden { name, .. }
                | FormField::TextArea { name, .. }
                | FormField::Select { name, .. }
                | FormField::Checkbox { name, .. }
                | FormField::Radio { name, .. } => name.clone(),
            })
            .collect();
        (names, form.label.clone())
    }

    /// **The button shown and the button sent must be the same control.**
    ///
    /// They used to be two separate decisions -- the label taken from the
    /// first submit with a non-empty *value*, the posted name from the first
    /// with a non-empty *name* -- which agree on every ordinary form and
    /// diverge as soon as the default button is unnamed.  Then the screen says
    /// "Go" and the server is told `btnI`, which is exactly the swap the
    /// Google fix above was about, arriving by a different route.
    ///
    /// An unnamed default button sends no button field at all, which is what a
    /// browser does: a control with no name is not a successful control.
    #[test]
    fn test_an_unnamed_default_button_is_not_replaced_by_a_later_named_one() {
        let html = br#"<html><body><form action="/s">
            <input name="q" value="">
            <input type="submit" value="Go">
            <input type="submit" name="btnI" value="Lucky">
        </form></body></html>"#;
        let (names, label) = form_field_names(html);

        assert_eq!(label, "Go", "the first submit control names the UI");
        assert!(
            !names.iter().any(|n| n == "btnI"),
            "a later submit button must not be posted under the first one's label: {names:?}",
        );
        assert!(names.iter().any(|n| n == "q"), "the text field must survive: {names:?}");
    }

    /// **A downgrade reason must not outlive its cleartext run.**
    ///
    /// It is threaded across `<meta refresh>` hops so a site fetched over
    /// cleartext that then refreshes still warns the reader. Carried
    /// unconditionally, though, it mislabels a later page: `https://a` fails
    /// TLS → `http://a` → refresh to a working `https://b` → refresh to an
    /// ordinary `http://c`, and `c` is announced as a TLS failure though it
    /// was never tried over TLS. Reaching HTTPS ends the run.
    #[test]
    fn test_a_downgrade_reason_does_not_outlive_the_cleartext_run() {
        let tls = Some(DowngradeReason::TlsFailed);

        // Still cleartext: the reason describes this page, so it is kept.
        assert_eq!(carry_downgrade("http://c/", tls), tls, "http keeps the reason");

        // The run ended at an HTTPS hop: dropped, and stays dropped for
        // whatever that page refreshes to.
        assert_eq!(carry_downgrade("https://b/", tls), None, "https ends the run");
        assert_eq!(
            carry_downgrade("http://c/", carry_downgrade("https://b/", tls)),
            None,
            "a page reached via a working HTTPS hop is not a TLS failure",
        );

        // Nothing carried is still nothing, either way.
        assert_eq!(carry_downgrade("http://c/", None), None);
        assert_eq!(carry_downgrade("https://b/", None), None);
    }

    /// **The HTML `type` attribute is case-insensitive.** `<button
    /// type="Submit">` was not recognised as a submit control, so it did not
    /// claim the form and a later `<input type=submit>` took both the label and
    /// the posted name -- reintroducing the disagreement the claim exists to
    /// close.  `<input>` was already lower-cased; `<button>` was not.
    #[test]
    fn test_a_submit_type_is_matched_whatever_its_case() {
        for ty in ["Submit", "SUBMIT", "submit"] {
            let html = format!(
                r#"<html><body><form action="/s">
                <input name="q" value="">
                <button type="{ty}" name="go" value="1">Search</button>
                <input type="submit" name="btnI" value="Lucky">
            </form></body></html>"#
            );
            let (names, label) = form_field_names(html.as_bytes());
            assert_eq!(label, "Search", "type={ty}: the button must name the UI");
            assert!(
                !names.iter().any(|n| n == "btnI"),
                "type={ty}: a later submit must not be posted under its label: {names:?}",
            );
        }

        // `type=button` is *not* a submit control and must not claim anything.
        let html = br#"<html><body><form action="/s">
            <input name="q" value="">
            <button type="button" name="nope">Click</button>
            <input type="submit" name="btnG" value="Search">
        </form></body></html>"#;
        let (names, label) = form_field_names(html);
        assert_eq!(label, "Search", "a non-submit button must not name the form");
        assert!(names.iter().any(|n| n == "btnG"), "the real submit is sent: {names:?}");
    }

    /// A `<button type=submit>` is a submit control, so it claims the form the
    /// same way -- otherwise a leading `<button>` names the screen while a
    /// later `<input type=submit>` is what gets posted.  A named one is
    /// successful and is sent.
    #[test]
    fn test_a_leading_button_element_claims_the_form() {
        let named = br#"<html><body><form action="/s">
            <input name="q" value="">
            <button type="submit" name="go" value="1">Search</button>
            <input type="submit" name="btnI" value="Lucky">
        </form></body></html>"#;
        let (names, label) = form_field_names(named);
        assert_eq!(label, "Search", "the button's text names the UI");
        assert!(names.iter().any(|n| n == "go"), "a named button is sent: {names:?}");
        assert!(
            !names.iter().any(|n| n == "btnI"),
            "and it stops a later submit being sent under its label: {names:?}",
        );

        // The same, unnamed: it still claims the form, and still posts nothing.
        let unnamed = br#"<html><body><form action="/s">
            <input name="q" value="">
            <button type="submit">Search</button>
            <input type="submit" name="btnI" value="Lucky">
        </form></body></html>"#;
        let (names, label) = form_field_names(unnamed);
        assert_eq!(label, "Search");
        assert!(
            !names.iter().any(|n| n == "btnI"),
            "an unnamed default button posts nothing, it does not defer: {names:?}",
        );
    }

    #[test]
    fn test_box_drawing_folds_to_ascii() {
        assert_eq!(fold_terminal_safe("─│┼┴┬"), "-|+++");
        // Heavy and double variants land in the same three buckets.
        assert_eq!(fold_terminal_safe("━┃═║"), "-|-|");
        // Anything else in the block is a corner or junction: '+'.
        assert_eq!(fold_terminal_safe("┌┐└┘├┤╔╗"), "++++++++");
        // The real shape of a rendered table row survives as a table row.
        assert_eq!(fold_terminal_safe("│Bible   │Verse"), "|Bible   |Verse");
    }

    #[test]
    fn test_block_elements_and_typography_fold() {
        assert_eq!(fold_terminal_safe("█▄▀"), "###");
        assert_eq!(fold_terminal_safe("\u{2018}a\u{2019} \u{201C}b\u{201D}"), "'a' \"b\"");
        assert_eq!(fold_terminal_safe("en\u{2013}dash em\u{2014}dash"), "en-dash em-dash");
        assert_eq!(fold_terminal_safe("wait\u{2026}"), "wait...");
        assert_eq!(fold_terminal_safe("a\u{00A0}b"), "a b");
        assert_eq!(fold_terminal_safe("\u{2022} item"), "* item");
        // A soft hyphen is an invisible break hint — dropped, not turned into
        // a visible '-' that was never in the text.
        assert_eq!(fold_terminal_safe("soft\u{00AD}hyphen"), "softhyphen");
    }

    /// The non-regression half: this fold is narrow on purpose.
    ///
    /// Accented Latin and non-Latin scripts are left alone, because a modern
    /// terminal over SSH renders them and replacing them with `?` would swap
    /// one kind of wrong output for another.  Plain ASCII must be untouched,
    /// and the fold must be idempotent — `WebPage::sanitize` documents itself
    /// as idempotent and is called on several paths.
    #[test]
    fn test_fold_leaves_everything_else_alone_and_is_idempotent() {
        let ascii = "Plain ASCII: 0-9 a-z A-Z !@#$%^&*()_+[]{};'\\:\"|,./<>? \t";
        assert_eq!(fold_terminal_safe(ascii), ascii);
        for s in ["café", "naïve", "Grüße", "日本語", "Ω≈ç√"] {
            assert_eq!(fold_terminal_safe(s), s, "{s} should pass through untouched");
        }
        let mixed = "─ café \u{201C}x\u{201D} 日本 █";
        let once = fold_terminal_safe(mixed);
        assert_eq!(fold_terminal_safe(&once), once, "fold must be idempotent");
        assert_eq!(fold_terminal_safe(""), "");
    }

    /// Link markers are C0 sentinels the telnet consumer parses; folding runs
    /// on the segments *between* them and must not disturb them.
    #[test]
    fn test_fold_preserves_link_markers_around_folded_text() {
        let line = "see \u{02}7\u{03} and ─table─ \u{02}8\u{03}";
        let out = sanitize_line_keep_markers(line);
        assert!(out.contains('\u{02}') && out.contains('\u{03}'), "markers lost: {out:?}");
        assert!(out.contains("-table-"), "box drawing not folded: {out:?}");
        assert!(!out.contains('─'), "box drawing survived: {out:?}");
        // The numbers between the sentinels are what the consumer reads.
        assert!(out.contains("\u{02}7\u{03}") && out.contains("\u{02}8\u{03}"), "{out:?}");
    }

    /// The trap: `page.url` is stored as `web_url` and used as the base for
    /// resolving relative links, so it must NOT be folded even though it is
    /// also displayed.  Form field values are submitted back to the server and
    /// are likewise left alone.
    #[test]
    fn test_sanitize_does_not_fold_the_url_or_form_values() {
        let mut page = WebPage {
            title: Some("─title".to_string()),
            lines: vec!["─line".to_string()],
            links: vec![],
            forms: vec![],
            url: "https://ex.com/caf\u{e9}/\u{2014}path".to_string(),
        };
        page.sanitize();
        assert_eq!(page.lines[0], "-line", "lines should fold");
        assert_eq!(page.title.as_deref(), Some("-title"), "title should fold");
        assert_eq!(
            page.url, "https://ex.com/caf\u{e9}/\u{2014}path",
            "folding the URL would break relative-link resolution",
        );
    }

    #[test]
    fn test_sanitize_strips_terminal_escapes_from_lines_and_title() {
        // A remote page must not smuggle ANSI/CSI escapes to the terminal:
        // WebPage::sanitize reuses aichat's filter on the title and lines.
        let mut page = WebPage {
            title: Some("evil\x1b[2Jtitle".to_string()),
            lines: vec![
                "before\x1b[31mred\x1b[0m".to_string(),
                "bell\x07 and null\0".to_string(),
                "\u{9b}C1-CSI".to_string(),
            ],
            links: Vec::new(),
            url: "http://example.com/".to_string(),
            forms: Vec::new(),
        };
        page.sanitize();
        assert_eq!(page.title.as_deref(), Some("evil[2Jtitle"));
        assert_eq!(page.lines[0], "before[31mred[0m");
        assert_eq!(page.lines[1], "bell and null");
        assert_eq!(page.lines[2], "C1-CSI");
        // No raw control bytes survive (tab is the only allowed C0).
        for line in &page.lines {
            assert!(!line.bytes().any(|b| b < 0x20 && b != b'\t'));
        }
    }

    #[test]
    fn test_sanitize_preserves_link_marker_sentinels() {
        // The \x02N\x03 sentinels are C0 bytes the raw filter would strip,
        // but they carry the link numbering the telnet consumer parses, so
        // sanitize must keep them while still cleaning the text around them.
        let mut page = WebPage {
            title: None,
            lines: vec!["Click here\x02\x1b[5m3\x03 now\x07".to_string()],
            links: vec!["http://example.com/x".to_string()],
            url: "http://example.com/".to_string(),
            forms: Vec::new(),
        };
        page.sanitize();
        // Sentinels intact; the escape inside and the bell outside are gone.
        assert_eq!(page.lines[0], "Click here\x02[5m3\x03 now");
    }

    #[test]
    fn test_sanitize_is_idempotent() {
        let mut page = WebPage {
            title: Some("t\x1b[Jt".to_string()),
            lines: vec!["a\x02\x1b1\x03b\x07".to_string()],
            links: Vec::new(),
            url: "http://example.com/".to_string(),
            forms: Vec::new(),
        };
        page.sanitize();
        let title_once = page.title.clone();
        let lines_once = page.lines.clone();
        page.sanitize();
        assert_eq!(page.title, title_once);
        assert_eq!(page.lines, lines_once);
    }

    #[test]
    fn test_sanitize_covers_url_and_form_display_text() {
        // M-8: form/field labels and Select option text are rendered by the
        // telnet form UI, and the page URL by the status line — all must be
        // sanitized.  A field `value` is submitted verbatim, so it must NOT be
        // mutated here (it is sanitized at display time instead).
        let mut page = WebPage {
            title: None,
            lines: Vec::new(),
            links: Vec::new(),
            url: "gopher://h/1sel\x1b[2J".to_string(),
            forms: vec![WebForm {
                action: "/go".to_string(),
                method: "post".to_string(),
                label: "Login\x1b[31m form".to_string(),
                fields: vec![
                    FormField::Text {
                        name: "u".to_string(),
                        value: "keep\x1b[5mthis".to_string(),
                        label: "User\x07name".to_string(),
                        input_type: "text".to_string(),
                    },
                    FormField::Select {
                        name: "c".to_string(),
                        options: vec![("us".to_string(), "United\x1b[0m States".to_string())],
                        selected: 0,
                        label: "Country\u{9b}".to_string(),
                    },
                ],
            }],
        };
        page.sanitize();
        assert_eq!(page.url, "gopher://h/1sel[2J");
        assert_eq!(page.forms[0].label, "Login[31m form");
        match &page.forms[0].fields[0] {
            FormField::Text { label, value, .. } => {
                assert_eq!(label, "Username"); // display label cleaned
                assert_eq!(value, "keep\x1b[5mthis"); // submitted value untouched
            }
            _ => panic!("expected Text field"),
        }
        match &page.forms[0].fields[1] {
            FormField::Select { label, options, .. } => {
                assert_eq!(label, "Country");
                assert_eq!(options[0].1, "United[0m States"); // display text cleaned
                assert_eq!(options[0].0, "us"); // submitted value untouched
            }
            _ => panic!("expected Select field"),
        }
    }

    #[test]
    fn test_moderately_nested_html_renders() {
        // A genuinely deep but under-MAX_DOM_DEPTH page must render normally —
        // this exercises the recursive extractors well past the old 256 cap
        // yet below the current one, confirming the guard doesn't false-reject.
        let depth = 400usize;
        let mut body = String::with_capacity(depth * 11);
        for _ in 0..depth { body.push_str("<div>"); }
        body.push_str("hello world");
        for _ in 0..depth { body.push_str("</div>"); }
        let (page, _refresh) = render_html_body(body.as_bytes(), "http://x/".to_string(), 73)
            .expect("moderately-nested page should render");
        assert!(
            page.lines.iter().any(|l| l.contains("hello world")),
            "rendered text should contain the body content"
        );
    }

    /// A web page cannot drive the reader's terminal.
    ///
    /// Page text is third-party data printed onto a real terminal — often a
    /// C64 — so an `ESC` reaching it is a cursor move, a screen clear, or on
    /// some terminals worse. We are protected today by html2text, which drops
    /// C0 controls while rendering, so the escape arrives as visible text.
    ///
    /// That is a dependency's behaviour, not ours, which is exactly why it is
    /// pinned here: it holds for 0.14.x, and the notes on this crate already
    /// record one other behaviour that has to be re-checked when it is bumped
    /// (`Node::Drop` staying iterative). If a future html2text passes controls
    /// through, this fails and the answer is our own filter —
    /// `aichat::sanitize_for_terminal` is the one the AI-chat and weather
    /// paths use.
    #[test]
    fn test_page_text_cannot_carry_escapes_to_the_terminal() {
        let html = b"<html><body><p>hello \x1b[2J\x1b[31mred\x1b]0;title\x07 \x7f done</p></body></html>";
        let (page, _refresh) = render_html_body(html, "http://example.invalid/".into(), 80)
            .expect("a small page must render");
        let text = page.lines.join("\n");
        for (name, ch) in [("ESC", '\u{1b}'), ("BEL", '\u{7}'), ("DEL", '\u{7f}')] {
            assert!(
                !text.contains(ch),
                "{name} survived into rendered page text: {text:?}"
            );
        }
        // The readable remains of the sequence are still there — this is a
        // filter somewhere upstream, not a rejection of the page.
        assert!(text.contains("hello"), "page body lost: {text:?}");
        assert!(text.contains("red"), "page body lost: {text:?}");
    }

    #[test]
    fn test_deeply_nested_html_rejected_without_stack_overflow() {
        // ~20k nested <div>s parses into a tree far deeper than MAX_DOM_DEPTH
        // and far past the point where our recursive extractors overflow the
        // (~2 MB) thread stack.  Rendering must instead return a clean Err.
        // Reaching the assert proves (a) the depth guard rejects before any
        // recursive walk, and (b) dropping the rejected 20k-deep tree at scope
        // end does NOT overflow — i.e. html2text's RcDom Drop is iterative; a
        // future regression to a recursive Drop would SIGABRT-fail this test.
        // (Depth kept at 20k, not higher, because html5ever's nested-element
        // parse cost grows ~quadratically with depth.)
        let depth = 20_000usize;
        let mut body = String::with_capacity(depth * 6);
        for _ in 0..depth { body.push_str("<div>"); }
        body.push_str("boom");
        for _ in 0..depth { body.push_str("</div>"); }
        match render_html_body(body.as_bytes(), "http://x/".to_string(), 73) {
            Err(e) => assert!(
                e.contains("deeply nested"),
                "rejection should explain the reason, got: {e}"
            ),
            Ok(_) => panic!("deeply-nested page must be rejected"),
        }
    }

    #[test]
    fn test_is_internal_ip_classification() {
        use std::net::IpAddr;
        // Internal / loopback / link-local (incl. cloud metadata) / ULA /
        // CGNAT — the browser must never reach these.
        for s in [
            "127.0.0.1", "10.1.2.3", "172.16.5.5", "192.168.1.1",
            "169.254.169.254", "0.0.0.0", "100.64.0.1", "::1", "fc00::1",
            "fe80::1",
            // NAT64 and 6to4 carrying a loopback / private / metadata v4.
            "64:ff9b::7f00:1", "64:ff9b::a9fe:a9fe", "2002:c0a8:101::1",
            "2002:7f00:1::",
        ] {
            assert!(
                is_internal_ip(s.parse::<IpAddr>().unwrap()),
                "{} should be classified internal",
                s
            );
        }
        // Public addresses — must be allowed.
        for s in [
            "8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111",
            // The same prefixes carrying a public v4 stay reachable.
            "64:ff9b::808:808", "2002:808:808::1",
        ] {
            assert!(
                !is_internal_ip(s.parse::<IpAddr>().unwrap()),
                "{} should be classified public",
                s
            );
        }
    }

    #[test]
    fn test_host_literal_is_internal_handles_bracketed_ipv6() {
        // url::Url::host_str() hands IPv6 literals back bracketed; the guard
        // must classify them, not punt them to the resolver path (which
        // can't resolve a bracketed string and would let them through).
        // Regression for the SSRF bypass over the whole IPv6 space.
        for s in ["[::1]", "[::ffff:127.0.0.1]", "[fe80::1]", "[fc00::1]"] {
            assert_eq!(host_literal_is_internal(s), Some(true), "{} must block", s);
        }
        // Bare (un-bracketed) literals still classify correctly.
        assert_eq!(host_literal_is_internal("::1"), Some(true));
        assert_eq!(host_literal_is_internal("127.0.0.1"), Some(true));
        // Public literals (bracketed or not) are allowed through.
        assert_eq!(host_literal_is_internal("8.8.8.8"), Some(false));
        assert_eq!(
            host_literal_is_internal("[2606:4700:4700::1111]"),
            Some(false)
        );
        // DNS names are not literals — they need resolution.
        assert_eq!(host_literal_is_internal("example.com"), None);
    }

    #[test]
    fn test_normalize_url_adds_https() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("http://example.com"), "http://example.com");
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn test_normalize_url_trims_whitespace() {
        assert_eq!(normalize_url("  example.com  "), "https://example.com");
    }

    #[test]
    fn test_resolve_url_absolute() {
        assert_eq!(
            resolve_url("https://example.com/page", "https://other.com/foo"),
            "https://other.com/foo"
        );
    }

    #[test]
    fn test_resolve_url_relative() {
        assert_eq!(
            resolve_url("https://example.com/dir/page", "other.html"),
            "https://example.com/dir/other.html"
        );
    }

    #[test]
    fn test_resolve_url_absolute_path() {
        assert_eq!(
            resolve_url("https://example.com/dir/page", "/foo/bar"),
            "https://example.com/foo/bar"
        );
    }

    /// Helper: parse HTML and extract title via DOM.
    fn title_from_html(html: &[u8]) -> Option<String> {
        let cfg = config::rich();
        let dom = cfg.parse_html(html).unwrap();
        extract_title_from_dom(&dom)
    }

    #[test]
    fn test_extract_title() {
        let html = b"<html><head><title>Hello World</title></head><body></body></html>";
        assert_eq!(title_from_html(html), Some("Hello World".to_string()));
    }

    #[test]
    fn test_extract_title_none() {
        let html = b"<html><body>No title here</body></html>";
        assert_eq!(title_from_html(html), None);
    }

    #[test]
    fn test_extract_title_empty() {
        let html = b"<html><head><title>  </title></head></html>";
        assert_eq!(title_from_html(html), None);
    }

    /// A2 canary: `extract_title_from_dom` and `get_text_content` recover
    /// text by parsing html2text's *debug* DOM rendering
    /// (`RcDom::node_as_dom_string`) for `"Text:"` lines — a non-stable-API
    /// dependency.  html2text 0.14 vendors `markup5ever_rcdom` as a private
    /// module and re-exports only the `Element`/`Document`/`Comment` node
    /// variants, not `Text`, so a direct text-node walk isn't possible; the
    /// debug-string parse is the only available route.  This test (together
    /// with the `test_extract_title_*` set) pins the format so a dependency
    /// bump that changes it fails loudly here instead of silently returning
    /// empty titles / form labels in production.
    #[test]
    fn test_dom_text_extraction_debug_format_canary() {
        let cfg = config::rich();
        let dom = cfg
            .parse_html(&b"<html><body><p>Canary Text Contract</p></body></html>"[..])
            .unwrap();
        let text = get_text_content(&dom.document);
        assert!(
            text.contains("Canary Text Contract"),
            "get_text_content lost its text — html2text debug-format drift? got {:?}",
            text
        );
    }

    /// F1 (behavior-preserving): the one-pass id→label map must still resolve
    /// a field's label from a matching `<label for="id">`, exactly as the old
    /// per-field subtree walk did.
    #[test]
    fn test_form_field_label_from_for_attribute() {
        let cfg = config::rich();
        let dom = cfg
            .parse_html(
                &b"<html><body><form><label for=\"q\">Search Terms</label>\
                   <input id=\"q\" name=\"query\" type=\"text\"></form></body></html>"[..],
            )
            .unwrap();
        let forms = extract_forms_from_dom(&dom);
        assert_eq!(forms.len(), 1);
        let label = forms[0].fields.iter().find_map(|f| match f {
            FormField::Text { name, label, .. } if name == "query" => Some(label.clone()),
            _ => None,
        });
        assert_eq!(label.as_deref(), Some("Search Terms"));
    }

    /// F1: a field with no matching `<label for>` (and no placeholder/aria/
    /// title) falls back to a label made from its name -- `query`, a name
    /// every search box uses, reads as "Search" (see `fallback_label`).
    #[test]
    fn test_form_field_label_falls_back_to_name() {
        let cfg = config::rich();
        let dom = cfg
            .parse_html(
                &b"<html><body><form><input id=\"q\" name=\"query\" type=\"text\"></form></body></html>"[..],
            )
            .unwrap();
        let forms = extract_forms_from_dom(&dom);
        let label = forms[0].fields.iter().find_map(|f| match f {
            FormField::Text { name, label, .. } if name == "query" => Some(label.clone()),
            _ => None,
        });
        assert_eq!(label.as_deref(), Some("Search"));
    }

    #[test]
    fn test_wrap_line_short() {
        assert_eq!(wrap_line("hello", 40), vec!["hello"]);
    }

    #[test]
    fn test_wrap_line_long() {
        let lines = wrap_line("the quick brown fox jumps over the lazy dog", 20);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.len() <= 20, "line too long: '{}'", line);
        }
    }

    #[test]
    fn test_wrap_line_empty() {
        assert_eq!(wrap_line("", 40), vec![""]);
    }

    #[test]
    fn test_wrap_line_multibyte() {
        let s = "caf\u{e9} caf\u{e9} caf\u{e9} caf\u{e9}";
        let lines = wrap_line(s, 10);
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(line.len() <= 12, "line too long: '{}' ({} bytes)", line, line.len());
        }
    }

    #[test]
    fn test_web_browser_menu_fits_petscii() {
        let line = "  B  Simple Browser";
        assert!(line.len() <= 40, "menu line too long: {}", line.len());
    }

    #[test]
    fn test_web_browser_footer_fits_petscii() {
        let footer = "  P=Pv N=Nx R=Re G=Go L=Lk B=Bk Q=X";
        assert!(footer.len() <= 40, "footer too long: {} chars", footer.len());
    }

    #[test]
    fn test_web_browser_home_lines_fit_petscii() {
        let lines = [
            "  WEB BROWSER",
            "  G=Go to URL",
            "  R=Refresh Q=Back",
        ];
        for line in &lines {
            assert!(line.len() <= 40, "line too long: '{}' = {} chars", line, line.len());
        }
    }

    #[test]
    fn test_web_browser_status_line_fits_petscii() {
        let status = format!("  ({}-{} of {})", 4983, 5000, 5000);
        assert!(status.len() <= 40, "status too long: '{}' = {} chars", status, status.len());
    }

    #[test]
    fn test_truncate_to_width_multibyte() {
        let s = "caf\u{e9} latt\u{e9}";
        let result = truncate_to_width(s, 6);
        assert!(result.chars().count() <= 6);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_to_width_ascii() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello world", 8), "hello...");
        assert_eq!(truncate_to_width("hi", 2), "hi");
        assert_eq!(truncate_to_width("hello", 3), "...");
    }

    #[test]
    fn test_is_tls_error_corrupt_message() {
        let e = ureq::Error::Io(std::io::Error::other(
            "received corrupt message of type InvalidContentType",
        ));
        assert!(is_tls_error(&e));
    }

    #[test]
    fn test_is_tls_error_invalid_content_type() {
        let e = ureq::Error::Io(std::io::Error::other("InvalidContentType"));
        assert!(is_tls_error(&e));
    }

    #[test]
    fn test_is_tls_error_not_tls() {
        let e = ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        assert!(!is_tls_error(&e));
    }

    #[test]
    fn test_is_tls_error_not_certificate() {
        let e = ureq::Error::Io(std::io::Error::other("certificate verify failed"));
        assert!(!is_tls_error(&e));
    }

    #[test]
    fn test_is_tls_error_timeout() {
        let e = ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ));
        assert!(!is_tls_error(&e));
    }

    #[test]
    fn test_constants_sanity() {
        const _: () = assert!(MAX_BODY_SIZE > 0);
        const _: () = assert!(MAX_BODY_SIZE <= 10 * 1024 * 1024, "body limit should be reasonable");
        const _: () = assert!(MAX_RENDERED_LINES > 0);
        const _: () = assert!(HTTP_TIMEOUT_SECS > 0);
        const _: () = assert!(HTTP_TIMEOUT_SECS <= 60, "timeout should not be excessive");
    }

    #[test]
    fn test_extract_title_with_attributes() {
        let html = b"<html><head><title lang=\"en\">Attributed</title></head></html>";
        assert_eq!(title_from_html(html), Some("Attributed".to_string()));
    }

    #[test]
    fn test_extract_title_mixed_case_tag() {
        let html = b"<html><head><TITLE>Upper</TITLE></head></html>";
        assert_eq!(title_from_html(html), Some("Upper".to_string()));
    }

    #[test]
    fn test_extract_title_whitespace_trimmed() {
        let html = b"<title>  spaced out  </title>";
        assert_eq!(title_from_html(html), Some("spaced out".to_string()));
    }

    #[test]
    fn test_extract_title_ignores_comment() {
        let html = b"<html><head><!-- <title>Fake</title> --><title>Real</title></head></html>";
        assert_eq!(title_from_html(html), Some("Real".to_string()));
    }

    #[test]
    fn test_extract_title_ignores_script() {
        let html = b"<html><head><script>var t = '<title>Fake</title>';</script><title>Real</title></head></html>";
        assert_eq!(title_from_html(html), Some("Real".to_string()));
    }

    #[test]
    fn test_normalize_url_search_no_dots() {
        let result = normalize_url("rust programming");
        assert!(result.starts_with("https://lite.duckduckgo.com/lite/?q="));
        assert!(result.contains("rust+programming"));
    }

    #[test]
    fn test_normalize_url_search_single_word() {
        let result = normalize_url("wikipedia");
        assert!(result.starts_with("https://lite.duckduckgo.com/lite/?q="));
    }

    #[test]
    fn test_normalize_url_with_dot_is_url() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
    }

    #[test]
    fn test_normalize_url_empty() {
        // Empty input has no dots, treated as a search query
        let result = normalize_url("");
        assert!(result.starts_with("https://lite.duckduckgo.com/lite/?q="));
    }

    #[test]
    fn test_normalize_url_preserves_path() {
        assert_eq!(normalize_url("example.com/page?q=1"), "https://example.com/page?q=1");
    }

    #[test]
    fn test_resolve_url_unwraps_ddg_redirect() {
        // DuckDuckGo Lite result links go through //duckduckgo.com/l/?uddg=<encoded_url>
        let base = "https://lite.duckduckgo.com/lite/?q=test";
        let relative = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc123";
        assert_eq!(resolve_url(base, relative), "https://example.com/page");
    }

    #[test]
    fn test_resolve_url_unwraps_ddg_absolute() {
        let base = "https://lite.duckduckgo.com/lite/?q=test";
        let absolute = "https://duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org&rut=xyz";
        assert_eq!(resolve_url(base, absolute), "https://rust-lang.org");
    }

    #[test]
    fn test_resolve_url_no_unwrap_for_non_ddg() {
        // Regular redirect-style URLs should not be unwrapped
        let base = "https://example.com";
        let relative = "/redirect?url=https%3A%2F%2Fother.com";
        let result = resolve_url(base, relative);
        assert!(result.contains("redirect?url="), "should not unwrap non-DDG redirects");
    }

    #[test]
    fn test_unwrap_ddg_redirect_direct() {
        assert_eq!(
            unwrap_ddg_redirect("https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com&rut=abc"),
            "https://example.com"
        );
        // Non-DDG URL passes through
        assert_eq!(
            unwrap_ddg_redirect("https://example.com/page"),
            "https://example.com/page"
        );
        // DDG URL without uddg param passes through
        assert_eq!(
            unwrap_ddg_redirect("https://duckduckgo.com/l/?other=value"),
            "https://duckduckgo.com/l/?other=value"
        );
    }

    #[test]
    fn test_resolve_url_fragment_only() {
        let result = resolve_url("https://example.com/page", "#section");
        assert!(result.contains("example.com"), "fragment should resolve against base");
    }

    #[test]
    fn test_resolve_url_empty_relative() {
        let result = resolve_url("https://example.com/page", "");
        assert!(result.contains("example.com"));
    }

    #[test]
    fn test_visible_field_index_skips_hidden() {
        let fields = vec![
            FormField::Hidden { name: "h".into(), value: "1".into() },
            FormField::Text { name: "q".into(), value: "".into(), label: "Query".into(), input_type: "text".into() },
            FormField::Hidden { name: "h2".into(), value: "2".into() },
            FormField::Text { name: "n".into(), value: "".into(), label: "Name".into(), input_type: "text".into() },
        ];
        assert_eq!(visible_field_index(&fields, 1), Some(1));
        assert_eq!(visible_field_index(&fields, 2), Some(3));
        assert_eq!(visible_field_index(&fields, 3), None);
    }

    // ─── Bookmarks ──────────────────────────────────────────

    /// Bookmark tests use set_current_dir which is process-global, so they
    /// must be combined into a single test to avoid races with parallel tests.
    #[test]
    fn test_bookmarks() {
        let dir = std::env::temp_dir().join("xmodem_test_bookmarks_all");
        let _ = std::fs::create_dir_all(&dir);
        let saved_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        // Clean slate
        let _ = std::fs::remove_file(BOOKMARKS_FILE);

        // Round trip
        assert!(load_bookmarks().is_empty());
        assert!(add_bookmark("https://example.com", "Example"));
        assert!(add_bookmark("https://rust-lang.org", "Rust"));
        assert!(!add_bookmark("https://example.com", "Dup")); // duplicate

        let bm = load_bookmarks();
        assert_eq!(bm.len(), 2);
        assert_eq!(bm[0].url, "https://example.com");
        assert_eq!(bm[1].title, "Rust");

        assert!(remove_bookmark(0));
        let bm2 = load_bookmarks();
        assert_eq!(bm2.len(), 1);
        assert_eq!(bm2[0].url, "https://rust-lang.org");

        // Title sanitization
        let _ = std::fs::remove_file(BOOKMARKS_FILE);
        assert!(add_bookmark("https://sanitize.com", "Title\nWith\nNewlines"));
        let bm3 = load_bookmarks();
        assert_eq!(bm3.len(), 1);
        assert_eq!(bm3[0].title, "Title With Newlines");

        // Remove out of bounds
        assert!(!remove_bookmark(999));

        // Capacity test
        let _ = std::fs::remove_file(BOOKMARKS_FILE);
        for i in 0..MAX_BOOKMARKS {
            assert!(add_bookmark(&format!("https://site{}.com", i), &format!("Site {}", i)));
        }
        assert!(!add_bookmark("https://overflow.com", "Overflow"));

        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_current_dir(&saved_dir).unwrap();
    }

    #[test]
    fn test_bookmark_constants() {
        const _: () = assert!(MAX_BOOKMARKS >= 10);
        const _: () = assert!(MAX_BOOKMARKS <= 500);
    }

    // ─── Gopher ─────────────────────────────────────────────

    #[test]
    fn test_parse_gopher_url_basic() {
        let (host, port, item_type, selector) =
            parse_gopher_url("gopher://gopher.floodgap.com").unwrap();
        assert_eq!(host, "gopher.floodgap.com");
        assert_eq!(port, 70);
        assert_eq!(item_type, '1');
        assert_eq!(selector, "");
    }

    #[test]
    fn test_parse_gopher_url_with_selector() {
        let (host, port, item_type, selector) =
            parse_gopher_url("gopher://gopher.floodgap.com/1/overbite").unwrap();
        assert_eq!(host, "gopher.floodgap.com");
        assert_eq!(port, 70);
        assert_eq!(item_type, '1');
        assert_eq!(selector, "/overbite");
    }

    #[test]
    fn test_parse_gopher_url_text_file() {
        let (_, _, item_type, selector) =
            parse_gopher_url("gopher://example.com/0/docs/readme.txt").unwrap();
        assert_eq!(item_type, '0');
        assert_eq!(selector, "/docs/readme.txt");
    }

    #[test]
    fn test_parse_gopher_url_custom_port() {
        let (host, port, _, _) =
            parse_gopher_url("gopher://example.com:7070/1/test").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 7070);
    }

    #[test]
    fn test_parse_gopher_url_rejects_port_zero() {
        // Port 0 is invalid for actual connections — fall back to the
        // default instead of attempting to dial a zero port.
        let (host, port, _, _) =
            parse_gopher_url("gopher://example.com:0/1/test").unwrap();
        assert_eq!(host, "example.com:0");
        assert_eq!(port, GOPHER_PORT);
    }

    #[test]
    fn test_parse_gopher_url_rejects_overflow_port() {
        // Port out of u16 range falls back to default.
        let (host, port, _, _) =
            parse_gopher_url("gopher://example.com:99999/1/test").unwrap();
        assert_eq!(host, "example.com:99999");
        assert_eq!(port, GOPHER_PORT);
    }

    #[test]
    fn test_parse_gopher_url_root_with_slash() {
        let (_, _, item_type, selector) =
            parse_gopher_url("gopher://example.com/").unwrap();
        assert_eq!(item_type, '1');
        assert_eq!(selector, "");
    }

    #[test]
    fn test_build_gopher_url_default_port() {
        assert_eq!(
            build_gopher_url("example.com", 70, '1', "/test"),
            "gopher://example.com/1/test"
        );
    }

    #[test]
    fn test_build_gopher_url_custom_port() {
        assert_eq!(
            build_gopher_url("example.com", 7070, '0', "/file.txt"),
            "gopher://example.com:7070/0/file.txt"
        );
    }

    #[test]
    fn test_gopher_search_detection() {
        assert!(is_gopher_search("gopher://example.com/7/search?search"));
        assert!(!is_gopher_search("gopher://example.com/1/dir"));
        assert!(!is_gopher_search("https://example.com?search"));
    }

    #[test]
    fn test_build_gopher_search_url() {
        let url = build_gopher_search_url(
            "gopher://example.com/7/v2/vs?search",
            "hello world",
        );
        assert!(url.starts_with("gopher://example.com/7/v2/vs"));
        assert!(url.contains("hello world"));
    }

    #[test]
    fn test_render_gopher_directory() {
        let menu = "iWelcome to Gopher!\tfake\t(null)\t0\r\n\
                     1Floodgap\t/\tgopher.floodgap.com\t70\r\n\
                     0About\t/about.txt\tgopher.floodgap.com\t70\r\n\
                     iBlank line\tfake\t(null)\t0\r\n\
                     .\r\n";
        let page = render_gopher_directory(menu, "localhost", 70, 40, "gopher://localhost/1".into()).unwrap();
        assert!(!page.lines.is_empty());
        assert_eq!(page.links.len(), 2);
        assert!(page.links[0].starts_with("gopher://gopher.floodgap.com"));
        // First line should be the info text
        assert!(page.lines[0].contains("Welcome to Gopher"));
    }

    #[test]
    fn test_normalize_url_gopher() {
        assert_eq!(
            normalize_url("gopher://gopher.floodgap.com"),
            "gopher://gopher.floodgap.com"
        );
    }

    #[test]
    fn test_gopher_constants() {
        const _: () = assert!(GOPHER_PORT == 70);
        const _: () = assert!(GOPHER_TIMEOUT_SECS > 0);
        const _: () = assert!(GOPHER_MAX_BODY > 0);
    }

    // ─── RFC 1436 (Gopher) conformance tests ─────────────────
    //
    // Each test cites the exact RFC 1436 section it locks down so a
    // future reader can audit our behavior against the spec without
    // chasing through render code.  Format under test (RFC 1436 §3):
    //   <type><display>\t<selector>\t<host>\t<port>\r\n
    // Terminator (§3.8): a line containing only "." + CRLF.
    // URL format (RFC 4266): gopher://host[:port]/<type><selector>.

    /// Build a one-line gopher menu fragment for a single item, then
    /// wrap it in a valid menu (with terminator) and parse it through
    /// `render_gopher_directory`.  Returns the parsed `WebPage`.
    fn render_one_item(line: &str) -> WebPage {
        let menu = format!("{line}\r\n.\r\n");
        render_gopher_directory(&menu, "localhost", 70, 73, "gopher://localhost/1".into())
            .unwrap()
    }

    #[test]
    fn test_rfc1436_item_type_0_text_creates_link() {
        // §3.6: type '0' = "Item is a file", linkable.  Menu-line
        // type prefix is separate from the selector field per §3.5.
        let page = render_one_item("0README\t/readme.txt\texample.org\t70");
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0], "gopher://example.org/0/readme.txt");
    }

    #[test]
    fn test_rfc1436_item_type_1_directory_creates_link() {
        // §3.6: type '1' = "Item is a directory", linkable.
        let page = render_one_item("1Sub\t/sub\texample.org\t70");
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0], "gopher://example.org/1/sub");
    }

    #[test]
    fn test_rfc1436_item_type_7_search_marks_query_url() {
        // §3.9: type '7' = "Item is an Index-Search server", the
        // selector is the search target and the client is expected
        // to prompt the user for a query string.  We tag the URL
        // with `?search` so the browser layer knows to prompt.
        let page = render_one_item("7Search\t/q\texample.org\t70");
        assert_eq!(page.links.len(), 1);
        assert!(
            page.links[0].ends_with("?search"),
            "type-7 link should be tagged with ?search marker, got {}",
            page.links[0]
        );
        assert!(is_gopher_search(&page.links[0]));
    }

    #[test]
    fn test_rfc1436_item_type_h_html_extracts_url_prefix() {
        // Gopher+ extension: type 'h' carries an HTTP/HTTPS URL in
        // the selector field with a "URL:" prefix.  Our renderer
        // strips the prefix and uses the trailing URL as the link.
        let page = render_one_item("hExample\tURL:https://example.org/\texample.org\t70");
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0], "https://example.org/");
    }

    #[test]
    fn test_rfc1436_item_type_i_info_no_link() {
        // Gopher+ extension: type 'i' is purely informational —
        // displayed text with no associated selector/host/port.  Must
        // NOT produce a link.
        let page = render_one_item("iJust some text\t\t\t0");
        assert!(
            page.links.is_empty(),
            "informational lines should not produce links, got {} links",
            page.links.len()
        );
        assert!(page.lines.iter().any(|l| l.contains("Just some text")));
    }

    #[test]
    fn test_rfc1436_item_type_3_error_prefixed() {
        // §3.6: type '3' = error.  We prefix the display text with
        // "ERR:" so users can distinguish error rows from info rows.
        let page = render_one_item("3Permission denied\t\t\t0");
        assert!(
            page.lines.iter().any(|l| l.starts_with("ERR:")),
            "type-3 error rows should be prefixed with 'ERR:', got: {:?}",
            page.lines
        );
        assert!(page.links.is_empty());
    }

    #[test]
    fn test_rfc1436_item_type_9_binary_label_no_link() {
        // §3.6: type '9' = binary file.  We can't render a binary
        // through a text terminal, so we display "[BIN]" and offer
        // no link.
        let page = render_one_item("9archive.zip\t/9/a.zip\texample.org\t70");
        assert!(page.links.is_empty(), "binary items should not be linkable");
        assert!(
            page.lines.iter().any(|l| l.contains("[BIN]")),
            "expected [BIN] label, got: {:?}",
            page.lines
        );
    }

    #[test]
    fn test_rfc1436_item_type_image_label_no_link() {
        // §3.6: 'g' = GIF, 'I' = image (generic), 'p' = PNG (gopher+).
        // All non-linkable in a text browser, all rendered as [IMG].
        for ty in ['g', 'I', 'p'] {
            let line = format!("{ty}pic.png\t/sel\texample.org\t70");
            let page = render_one_item(&line);
            assert!(
                page.links.is_empty(),
                "type {} should not produce a link",
                ty
            );
            assert!(
                page.lines.iter().any(|l| l.contains("[IMG]")),
                "type {} should render with [IMG] label",
                ty
            );
        }
    }

    #[test]
    fn test_rfc1436_item_type_s_sound_label_no_link() {
        // §3.6: 's' = sound.  Same treatment as binary/image —
        // labeled, not linked.
        let page = render_one_item("ssong.mp3\t/s\texample.org\t70");
        assert!(page.links.is_empty());
        assert!(page.lines.iter().any(|l| l.contains("[SND]")));
    }

    #[test]
    fn test_rfc1436_item_type_unknown_label_no_link() {
        // §3.6 lists 2/4/5/6/8/T/+ as defined types we don't have
        // first-class rendering for.  Falling back to [???] is the
        // safe behavior: never offer a link for a type we can't
        // safely follow, but still surface that something is there.
        for ty in ['2', '4', '5', '6', '8', 'T', '+'] {
            let line = format!("{ty}Mystery\t/m\texample.org\t70");
            let page = render_one_item(&line);
            assert!(
                page.links.is_empty(),
                "unknown type {} must not produce a link",
                ty
            );
            assert!(
                page.lines.iter().any(|l| l.contains("[???]")),
                "unknown type {} should render with [???] label",
                ty
            );
        }
    }

    #[test]
    fn test_rfc1436_menu_terminator_period_ends_parsing() {
        // §3.8: a line containing only "." (followed by CRLF) marks
        // the end of a menu.  Anything after must be ignored.
        let menu = "0First\t/0/a\texample.org\t70\r\n\
                    .\r\n\
                    0AfterTerminator\t/0/b\texample.org\t70\r\n";
        let page =
            render_gopher_directory(menu, "localhost", 70, 73, "gopher://localhost/1".into())
                .unwrap();
        assert_eq!(
            page.links.len(),
            1,
            "items after the '.' terminator must be ignored"
        );
        assert!(page.links[0].contains("/0/a"));
    }

    #[test]
    fn test_rfc1436_blank_line_preserved() {
        // §3.5 allows blank lines for visual spacing.  We pass them
        // through as empty lines in the rendered output.
        let menu = "iAbove\t\t\t0\r\n\
                    \r\n\
                    iBelow\t\t\t0\r\n\
                    .\r\n";
        let page =
            render_gopher_directory(menu, "localhost", 70, 73, "gopher://localhost/1".into())
                .unwrap();
        assert!(page.lines.iter().any(|l| l.is_empty()));
        assert!(page.lines.iter().any(|l| l.contains("Above")));
        assert!(page.lines.iter().any(|l| l.contains("Below")));
    }

    #[test]
    fn test_rfc4266_url_default_port_omitted() {
        // RFC 4266 §2.1: if the gopher port is the default (70), it
        // SHOULD be omitted from the URL.  Our build_gopher_url
        // honors this.
        assert_eq!(
            build_gopher_url("example.org", 70, '1', "/sub"),
            "gopher://example.org/1/sub"
        );
    }

    #[test]
    fn test_rfc4266_url_non_default_port_included() {
        // RFC 4266 §2.1: non-default ports MUST be included.
        assert_eq!(
            build_gopher_url("example.org", 7070, '1', "/sub"),
            "gopher://example.org:7070/1/sub"
        );
    }

    #[test]
    fn test_rfc4266_url_round_trip_preserves_components() {
        // Parse → build → parse stability: every component (host,
        // port, type, selector) must round-trip identically.
        let original = "gopher://example.org:9999/0/path/to/file.txt";
        let (host, port, ty, sel) = parse_gopher_url(original).unwrap();
        assert_eq!(host, "example.org");
        assert_eq!(port, 9999);
        assert_eq!(ty, '0');
        assert_eq!(sel, "/path/to/file.txt");
        let rebuilt = build_gopher_url(&host, port, ty, &sel);
        assert_eq!(rebuilt, original);
    }

    #[test]
    fn test_rfc1436_menu_line_uses_tab_separator() {
        // §3.5: fields within a menu line are separated by a single
        // ASCII TAB (0x09).  Lock down our renderer's tolerance: a
        // line missing the tabs falls back to defaults rather than
        // panicking.  Forwards-compatible for malformed peers.
        let line = "1NoTabs"; // missing all field separators
        let menu = format!("{line}\r\n.\r\n");
        // Should not panic.  The renderer falls back to the current
        // host/port and an empty selector for the missing fields.
        let _ =
            render_gopher_directory(&menu, "localhost", 70, 73, "gopher://localhost/1".into())
                .unwrap();
    }

    // ─── End-to-end tests against an in-process server ────────
    //
    // These tests bind a `std::net::TcpListener` to 127.0.0.1:0, get
    // the OS-assigned port, and run a one-shot handler in a background
    // thread that serves a hand-rolled response.  Hermetic — no
    // external network, no flakes from public servers — so they run
    // in the standard `cargo test` suite (no `#[ignore]`).

    /// Spawn a one-shot TCP test server on 127.0.0.1:0.  The handler
    /// runs on a background thread, accepts one connection, runs to
    /// completion, then exits.  Returns the allocated port.
    fn spawn_oneshot_server<F>(handler: F) -> u16
    where
        F: FnOnce(std::net::TcpStream) + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handler(stream);
            }
        });
        port
    }

    /// Read an HTTP request from a server-side stream until the client
    /// stops sending.  Uses a short read timeout so we don't depend on
    /// ureq closing the connection (it doesn't until it has read our
    /// response).  Localhost messages are small enough that 200 ms is
    /// plenty.
    fn read_request_blob(stream: &std::net::TcpStream) -> Vec<u8> {
        use std::io::Read;
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .ok();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut s = stream;
        loop {
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    // Stop once we've seen end-of-headers and any
                    // declared Content-Length body bytes.
                    if let Some(headers_end) = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                    {
                        let headers = &buf[..headers_end];
                        let cl = std::str::from_utf8(headers)
                            .ok()
                            .and_then(|h| {
                                h.lines().find_map(|l| {
                                    let lower = l.to_ascii_lowercase();
                                    lower
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().to_string())
                                })
                            })
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= headers_end + 4 + cl {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        buf
    }

    /// Build an HTTP/1.1 200 response with `Connection: close` so the
    /// client knows when the body ends.  Used by every HTTP e2e test.
    fn http_200(content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            content_type,
            body.len(),
            body
        )
    }

    /// Serve one raw HTTP response and return its URL.
    fn serve_raw(raw: Vec<u8>) -> String {
        let port = spawn_oneshot_server(move |mut stream| {
            use std::io::Write;
            let _ = read_request_blob(&stream);
            stream.write_all(&raw).unwrap();
        });
        format!("http://127.0.0.1:{}/", port)
    }

    fn response(status: &str, content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
        let mut raw = format!("HTTP/1.1 {status}\r\n").into_bytes();
        if let Some(ct) = content_type {
            raw.extend(format!("Content-Type: {ct}\r\n").bytes());
        }
        raw.extend(format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()).bytes());
        raw.extend_from_slice(body);
        raw
    }

    /// A file that is not text is refused by name rather than drawn as its
    /// bytes -- by its declared type, and by its first bytes when it has none.
    #[test]
    fn test_a_file_that_is_not_text_is_refused_by_name() {
        let pdf = b"%PDF-1.4\n\xe4\xfc\xf6\xdf 2 0 obj <<>> stream\x00\x01";
        let err = fetch_and_render(&serve_raw(response("200 OK", Some("application/pdf"), pdf)), 73)
            .err()
            .expect("a PDF is refused");
        assert!(err.contains("PDF file") && err.contains("can't be shown"), "{err}");
        let err = fetch_and_render(&serve_raw(response("200 OK", None, pdf)), 73)
            .err()
            .expect("an undeclared PDF is refused too");
        assert!(err.contains("PDF file"), "{err}");

        for (ct, want) in [
            ("image/png", "a picture"),
            ("image/svg+xml", "a picture"),
            ("audio/mpeg", "a sound file"),
            ("video/mp4", "a video"),
            ("application/zip", "a compressed archive"),
            ("application/x-foo; charset=x", "a file of type application/x-foo"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "a file of type application/vnd.openxmlformats",
            ),
        ] {
            assert!(unshowable(ct, None).is_some_and(|m| m.contains(want)), "{ct}");
        }
        for ct in [
            "text/html; charset=utf-8", "text/plain", "application/xhtml+xml", "application/json",
            "application/xml", "application/rss+xml", "application/ld+json", "",
        ] {
            assert_eq!(unshowable(ct, None), None, "{ct:?} is text");
        }
        assert!(unshowable("", Some(b"abc\x00def")).is_some(), "NUL bytes with no type");
        // A server that does not know what it has: the bytes decide.
        assert_eq!(unshowable("application/octet-stream", None), None);
        assert_eq!(unshowable("application/octet-stream", Some(b"just a text file\n")), None);
        assert!(unshowable("application/octet-stream", Some(b"%PDF-1.7")).is_some());
        assert_eq!(unshowable("none", Some(b"<p>hi</p>")), None, "not a type at all");
        assert_eq!(unshowable("", Some(b"<html>plain</html>")), None);
    }

    /// An HTTP error says so, above whatever the server sent; an empty one
    /// says the page has no text rather than leaving a blank screen.
    #[test]
    fn test_an_http_error_and_an_empty_page_say_so() {
        let page = fetch_and_render(&serve_raw(response("404 Not Found", Some("text/html"), b"")), 73).unwrap();
        assert!(page.lines[0].contains("HTTP error 404") && page.lines[0].contains("Not Found"), "{:?}", page.lines);
        assert!(page.lines.iter().any(|l| l.contains("no text to show")), "{:?}", page.lines);
        assert!(!page.lines.iter().any(|l| l.contains("JavaScript")), "an empty body is just empty");

        // An error served as a file that is not text is reported as the error.
        let err = fetch_and_render(&serve_raw(response("404 Not Found", Some("image/png"), b"\x89PNG")), 73)
            .err()
            .expect("refused");
        assert!(err.contains("HTTP error 404"), "{err}");
        // An error page that refreshes away keeps its error in front of the reader.
        let refresh = b"<html><head><meta http-equiv=refresh content=\"0;url=/elsewhere\"></head><body>Moved</body></html>";
        let page = fetch_and_render(&serve_raw(response("404 Not Found", Some("text/html"), refresh)), 73).unwrap();
        assert!(page.lines[0].contains("HTTP error 404"), "{:?}", page.lines);

        let body = b"<html><body><p>Try the archive instead.</p></body></html>";
        let page = fetch_and_render(&serve_raw(response("410 Gone", Some("text/html"), body)), 73).unwrap();
        assert!(page.lines[0].contains("HTTP error 410"), "{:?}", page.lines);
        assert!(page.lines.iter().any(|l| l.contains("Try the archive")), "the body is still shown");

        let js = b"<html><body><script>render()</script></body></html>";
        let page = fetch_and_render(&serve_raw(response("200 OK", Some("text/html"), js)), 73).unwrap();
        assert!(page.lines.iter().any(|l| l.contains("may need JavaScript")), "{:?}", page.lines);
        assert!(!page.lines.iter().any(|l| l.contains("HTTP error")), "a 200 is not an error");
    }

    /// Errors in words a reader can act on, never cut off mid-word.
    #[test]
    fn test_fetch_errors_are_explained() {
        let cert = |m: &str| friendly_fetch_error(&ureq::Error::Io(std::io::Error::other(m.to_string())));
        assert!(cert("invalid peer certificate: certificate expired: verification time").contains("has expired"));
        assert!(cert("invalid peer certificate: NotValidForName").contains("different site"));
        assert!(cert("invalid peer certificate: UnknownIssuer").contains("trusted authority"));
        assert!(friendly_fetch_error(&ureq::Error::HostNotFound).contains("Could not find that site"));
        let refused = ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(friendly_fetch_error(&refused).contains("refused the connection"));
        assert!(friendly_fetch_error(&ureq::Error::TooManyRedirects).starts_with("Could not load the page ("));
    }

    /// Layout tables are drawn without boxes; a drop-down's choices are left
    /// out of the page and kept in its form; two touching links are spaced.
    #[test]
    fn test_tables_drop_downs_and_touching_links() {
        let html = br##"<html><body>
            <form action="/s"><select name="kl"><option value="ar">Argentina</option>
            <option value="br" selected>Brazil</option></select><input name="q" value="c64"></form>
            <table><tr><td>1.</td><td>First result</td></tr><tr><td>2.</td><td>Second result</td></tr></table>
            <p><a href="/home">Hacker News</a><a href="/new">new</a> | <a href="/past">past</a></p>
            <p><a href="#n">Jump to navigation</a><a href="#s">Jump to search</a></p>
            </body></html>"##;
        let (page, _) = render_html_body(html, "http://x.test/".to_string(), 73).unwrap();
        let text = page.lines.join("\n");
        // Box characters, as html2text draws them before any folding.
        assert!(!text.contains(['\u{2500}', '\u{2502}', '\u{253C}']), "no table rules: {text}");
        assert!(text.contains("First result") && text.contains("Second result"), "{text}");
        assert!(!text.contains("Argentina") && !text.contains("Brazil"), "drop-down text: {text}");
        match &page.forms[0].fields[0] {
            FormField::Select { options, selected, .. } => {
                assert_eq!(options.len(), 2);
                assert_eq!(*selected, 1, "the form keeps the choices");
            }
            other => panic!("{other:?}"),
        }
        assert!(text.contains("Hacker News\u{2}1\u{3} new\u{2}2\u{3}"), "{text:?}");
        assert!(text.contains("Jump to navigation Jump to search"), "{text:?}");
        // A link that ends mid-word is not split from the rest of it.
        let (page, _) = render_html_body(br#"<p><a href="/w">Wiki</a>pedia</p>"#, "http://x.test/".into(), 73).unwrap();
        assert!(page.lines.join("").contains("Wiki\u{2}1\u{3}pedia"), "{:?}", page.lines);
    }

    /// A line with more links than fit keeps every link number: the overflow
    /// moves to the next row, a number is never split, a list item's
    /// continuation is indented past its bullet, and a line that fits is
    /// untouched.
    #[test]
    fn test_no_link_number_is_lost_to_the_screen_edge() {
        let links: String = (1..=15).map(|i| format!("<a href=\"/p{i}\">w{i}</a> ")).collect();
        for width in [32, 72] {
            let html = format!("<html><body><p>{links}</p><ul><li>{links}</li></ul></body></html>");
            let (page, _) = render_html_body(html.as_bytes(), "http://x.test/".to_string(), width).unwrap();
            let max = width + LINK_MARKER_ROOM;
            for line in &page.lines {
                assert!(line.chars().count() <= max, "{width}: {line:?} is wider than {max}");
                assert_eq!(line.matches('\u{2}').count(), line.matches('\u{3}').count(), "split: {line:?}");
            }
            let text = page.lines.join("\n");
            for i in 1..=15 {
                assert!(text.contains(&format!("w{i}\u{2}{i}\u{3}")), "{width}: link {i} lost: {text:?}");
            }
        }
        // The list continuation sits under the text, not under the bullet.
        let rows = rewrap_for_screen("  * one two three four five six", 16);
        assert!(rows.len() > 1 && rows[1..].iter().all(|r| r.starts_with("    ") && !r.starts_with("     ")), "{rows:?}");
        // Folded width counts: twelve `ß` are 24 columns on a C64.
        assert!(rewrap_for_screen("ßßßßßß ßßßßßß", 20).len() == 2);
        // A long first word on an indented line or after a bullet breaks
        // inside the word, not at the indent: no blank row, no lone bullet.
        for line in [format!("    {}", "u".repeat(30)), format!("  * {}", "u".repeat(30))] {
            let rows = rewrap_for_screen(&line, 20);
            assert!(rows.iter().all(|r| r.trim().len() > 1), "{rows:?}");
        }
        // A numbered item hangs under its text, like a bullet.
        let rows = rewrap_for_screen("  12. one two three four five six", 16);
        assert!(rows[1..].iter().all(|r| r.starts_with("      ") && !r.starts_with("       ")), "{rows:?}");
        // A CJK character is two columns on an ANSI terminal.
        assert_eq!(rewrap_for_screen(&"\u{4E2D}".repeat(12), 20).len(), 2);
        // A line that fits is left exactly as it was.
        assert_eq!(rewrap_for_screen("a   b\u{2}1\u{3}", 37), ["a   b\u{2}1\u{3}"]);
        // No space to break at and the row ends inside a link number: the
        // break goes before the number, not through it.
        let long = format!("{}\u{2}12\u{3}", "x".repeat(18));
        assert_eq!(rewrap_for_screen(&long, 20), ["x".repeat(18), "\u{2}12\u{3}".to_string()]);
        // A word longer than a row is cut, and the loop ends.
        assert_eq!(rewrap_for_screen(&"x".repeat(50), 20).len(), 3);
    }

    /// A link that wraps is numbered once, at its end, and a number is never
    /// left alone on a row; a space inside a link goes after its number.
    /// Measured live: a CNN headline on a C64 showed `[2]` on all three of
    /// its rows, and Hacker News left `6[13]` on a row of its own.
    #[test]
    fn test_a_wrapped_link_is_numbered_once() {
        let html = br#"<html><body><ul>
            <li><a href="/a">Trump, crime and corruption loom over Brazil's presidential vote</a></li>
            <li>388 points by <a href="/u">snehesht</a> <a href="/i">6 hours ago</a> | <a href="/h">hide</a></li>
            </ul><p><a href="/l">Log in </a><a href="/r">Register</a></p></body></html>"#;
        for width in [32, 72] {
            let (page, _) = render_html_body(html, "http://x.test/".to_string(), width).unwrap();
            let text = page.lines.join("\n");
            for n in 1..=6 {
                assert_eq!(text.matches(&format!("\u{2}{n}\u{3}")).count(), 1, "{width}: [{n}] in {text:?}");
            }
            assert!(text.contains("vote\u{2}1\u{3}"), "{width}: {text:?}");
            // No row is a lone fragment of a link: every row with a number
            // has more than a word before it.
            for line in &page.lines {
                assert!(line.chars().count() <= width + LINK_MARKER_ROOM, "{line:?}");
                if let Some(at) = line.find('\u{2}') {
                    assert!(line[..at].trim().len() > 1, "{width}: orphan {line:?}");
                }
            }
            assert!(text.contains("Log in\u{2}5\u{3} Register\u{2}6\u{3}"), "{width}: {text:?}");
        }
        // Pretty-printed links end in whitespace; the number still sits on
        // its word.  At 32 the title and `[1]` exactly fill the row, which is
        // where a break before the number left it alone on the next (VCFed).
        let pretty = b"<div><a href=\"/f\">\n  <img src=l.png alt=\"Vintage Computer Federation Forums\">\n  </a></div>\
            <div><a href=\"/t\">\n  Vintage Computer Federation Forums\n</a></div>";
        let (page, _) = render_html_body(pretty, "http://x.test/".to_string(), 32).unwrap();
        assert_eq!(page.lines, ["Vintage Computer Federation Forums\u{2}1\u{3}",
                                "Vintage Computer Federation Forums\u{2}2\u{3}"]);
        // A narrow table cell keeps its word and number together: HN's header,
        // where the cell was sized for `login` and the number was cut in two.
        let hn = br#"<table><tr><td><a href="/"><img src="y.svg"></a></td><td><a href="news">Hacker News</a><a href="newest">new</a> | <a href="front">past</a> | <a href="c">comments</a> | <a href="ask">ask</a> | <a href="show">show</a> | <a href="jobs">jobs</a> | <a href="submit">submit</a></td><td><a href="login">login</a></td></tr></table>"#;
        let hn_page = [&b"<center><table width=85%><tr><td>"[..], hn, b"</td></tr></table></center>"].concat();
        for (width, html) in [(32, &hn[..]), (72, &hn[..]), (32, &hn_page[..])] {
            let (page, _) = render_html_body(html, "http://x.test/".to_string(), width).unwrap();
            let text = page.lines.join("\n");
            assert!(text.contains("login\u{2}9\u{3}"), "{width}: {text:?}");
            // On the header's first row, as the layout put it: a space added
            // after the layout pushed it to a row of its own.
            assert!(page.lines[0].starts_with("Hacker News\u{2}1\u{3} new") && page.lines[0].contains("login"),
                "{width}: {text:?}");
            // The icon-only link has nothing to show and gets no number.
            assert_eq!(page.links.len(), 9, "{:?}", page.links);
            for line in &page.lines {
                assert_eq!(line.matches('\u{2}').count(), line.matches('\u{3}').count(), "{width}: split {line:?}");
            }
        }
    }

    /// Two elements that touch get a space where lower case meets a capital
    /// (GitHub's `CopilotWrite better code`), and nowhere else.
    #[test]
    fn test_touching_elements_are_spaced_only_at_a_case_change() {
        let render = |body: &str| {
            let html = format!("<html><body>{body}</body></html>");
            let (page, _) = render_html_body(html.as_bytes(), "http://x.test/".into(), 73).unwrap();
            page.lines.join("\n")
        };
        let gh = render(r#"<a href="/c"><span><svg><path d="M0"/></svg>GitHub Copilot</span><span>Write better code</span></a>"#);
        assert!(gh.contains("GitHub Copilot Write better code"), "{gh:?}");
        // Nested deeper on each side, and with an icon between.
        let deep = render("<p><b><i>alpha</i></b><img src=x.png alt=''><em><span>Beta</span></em></p>");
        assert!(deep.contains("alpha Beta"), "{deep:?}");
        // Lower meeting lower is a word split for styling; leave it.
        assert!(render("<p><b>Wiki</b><span>pedia</span></p>").contains("Wikipedia"));
        // Already spaced: not doubled.
        assert!(render("<p><span>one </span><span>Two</span></p>").contains("one Two"));
        // Code is every byte meant, however deep the spans.
        let code = render("<pre><span><span>let</span><span>X</span></span></pre><code><span>a</span><span>B</span></code>");
        assert!(code.contains("letX") && code.contains("aB"), "{code:?}");
    }

    #[test]
    fn test_json_and_plain_text_keep_their_lines() {
        for t in ["text/plain", "application/json", "application/ld+json; charset=utf-8", "text/csv"] {
            assert!(is_plain_text(t), "{t}");
        }
        for t in ["text/html", "application/xhtml+xml", "text/xml", "", "application/octet-stream"] {
            assert!(!is_plain_text(t), "{t}");
        }
        // Served: httpbin's pretty-printed reply arrived as one paragraph.
        let json = b"{\n  \"args\": {},\n  \"url\": \"https://httpbin.org/get\"\n}\n";
        let page = fetch_and_render(&serve_raw(response("200 OK", Some("application/json"), json)), 73).unwrap();
        assert_eq!(page.lines, ["{", "  \"args\": {},", "  \"url\": \"https://httpbin.org/get\"", "}"]);
    }

    /// A word wider than a whole row is cut by html2text at the row edge,
    /// which knows nothing of link numbers; the number is put back whole.
    #[test]
    fn test_a_number_is_never_cut_by_an_overlong_word() {
        for len in 30..=40 {
            let word = "x".repeat(len);
            let html = format!("<p><a href=\"/{len}\">{word}</a> after</p>");
            let (page, _) = render_html_body(html.as_bytes(), "http://x.test/".into(), 32).unwrap();
            for line in &page.lines {
                assert_eq!(line.matches('\u{2}').count(), line.matches('\u{3}').count(), "{len}: {:?}", page.lines);
            }
            assert!(page.lines.join("").contains("x\u{2}1\u{3}"), "{len}: {:?}", page.lines);
        }
    }

    /// Nesting deep enough that the narrowest column html2text is allowed
    /// cannot fit renders anyway, as it did before that minimum was raised.
    #[test]
    fn test_deep_nesting_still_renders() {
        for depth in [12, 14, 16] {
            let html = format!("{}<a href=\"/d\">deep</a>{}", "<blockquote>".repeat(depth), "</blockquote>".repeat(depth));
            let (page, _) = render_html_body(html.as_bytes(), "http://x.test/".into(), 32)
                .unwrap_or_else(|e| panic!("{depth}: {e}"));
            for line in &page.lines {
                assert_eq!(line.matches('\u{2}').count(), line.matches('\u{3}').count(), "{depth}: {:?}", page.lines);
            }
            // Where the preferred minimum still fits, the number stays on its
            // word; past it there may be no room for both on one row.
            let joined = page.lines.join("");
            assert!(joined.contains("deep") && joined.contains("\u{2}1\u{3}"), "{depth}: {:?}", page.lines);
            if depth == 12 {
                assert!(joined.contains("deep\u{2}1\u{3}"), "{:?}", page.lines);
            }
        }
    }

    /// Inside an inline SVG, html2text draws only a leading `<title>`; a
    /// number placed anywhere else is never seen, and a link it never draws
    /// takes a number from the ones it does.
    #[test]
    fn test_svg_contents_take_no_number() {
        let html = br#"<p><a href="/x">Docs<svg><path d="M0"/><text>ext</text></svg></a>
            <svg><a href="/hidden"><text>hidden</text></a></svg>
            <a href="/y">Next</a> <a href="/z"><svg><title>Logo</title><path/></svg></a></p>"#;
        let (page, _) = render_html_body(html, "http://x.test/".into(), 73).unwrap();
        let text = page.lines.join("\n");
        assert!(text.contains("Docs\u{2}1\u{3}") && text.contains("Next\u{2}2\u{3}"), "{text:?}");
        assert!(text.contains("Logo\u{2}3\u{3}"), "{text:?}");
        assert_eq!(page.links, ["/x", "/y", "/z"]);
        // An image with no `src` is not drawn, so its link takes no number.
        let (page, _) = render_html_body(br#"<p><a href="/n"><img alt="Logo"></a> <a href="/m">More</a></p>"#,
            "http://x.test/".into(), 73).unwrap();
        assert_eq!(page.links, ["/m"]);
        assert!(!page.lines.join("").starts_with('\u{2}'), "{:?}", page.lines);
    }

    /// The characters that carry a number through the layout cannot be
    /// typed into a page to forge one.
    #[test]
    fn test_a_page_cannot_forge_a_link_number() {
        // An image's alt text is drawn when the image is a link.
        let html = "<p>Click here&#xFDD0;1&#xFDD1; or \u{FDD0}2\u{FDD1}</p><p><a href=\"/i\"><img src=\"p.png\" alt=\"pic&#xFDD0;3&#xFDD1;\"></a> <a href=\"/real\">real</a></p>";
        let (page, _) = render_html_body(html.as_bytes(), "http://x.test/".into(), 73).unwrap();
        let text = page.lines.join("\n");
        assert_eq!(text.matches('\u{2}').count(), 2, "{text:?}");
        assert!(text.contains("pic3\u{2}1\u{3} real\u{2}2\u{3}") && text.contains("Click here1 or 2"), "{text:?}");
    }

    #[test]
    fn test_place_link_markers() {
        let m = |s: &str| s.replace('<', &MARK_OPEN.to_string()).replace('>', &MARK_CLOSE.to_string());
        assert_eq!(place_link_markers(&m("Log in<22> Register<23>")), "Log in\u{2}22\u{3} Register\u{2}23\u{3}");
        // A page's own sentinel bytes are not markers.
        assert_eq!(place_link_markers("a\u{2}9\u{3}b"), "a9b");
    }

    /// A field the page gave no label is named for what it is, not by its
    /// internal identifier.
    #[test]
    fn test_unlabelled_form_fields_get_readable_names() {
        let html = br#"<html><body><form action="/f">
            <input name="q"><select name="kl"><option>All</option></select>
            <label>Region <select name="rg"><option>Europe</option><option>Asia</option></select></label>
            <input type="email" name="e1"><input name="first_name"><input name="zz">
            <input name="x" placeholder="Your town">
            <input name="y" placeholder="" aria-label="Search the site">
            </form></body></html>"#;
        let (page, _) = render_html_body(html, "http://x.test/".to_string(), 73).unwrap();
        let labels: Vec<String> = page.forms[0]
            .fields
            .iter()
            .filter_map(|f| match f {
                FormField::Text { label, .. } | FormField::Select { label, .. } => Some(label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            ["Search", "Choose", "Region", "Email", "First name", "Text", "Your town", "Search the site"]
        );
        // A radio button inside a label is named by it, not by its value.
        let radio = br#"<form><label><input type="radio" name="r" value="1"> Yes</label></form>"#;
        let (page, _) = render_html_body(radio, "http://x.test/".into(), 73).unwrap();
        assert!(matches!(&page.forms[0].fields[0], FormField::Radio { label, .. } if label == "Yes"));
        // Nested labels: each takes its own words, not everything below it.
        let nested = br#"<form><label>Outer <label>Inner <input name="n1"></label> <input name="n2"></label></form>"#;
        let (page, _) = render_html_body(nested, "http://x.test/".into(), 73).unwrap();
        let got: Vec<&str> = page.forms[0]
            .fields
            .iter()
            .filter_map(|f| match f {
                FormField::Text { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(got, ["Inner", "Outer"], "the outer label's own words, not the inner's too");
    }

    #[test]
    fn test_e2e_gopher_text_file() {
        let port = spawn_oneshot_server(|mut stream| {
            use std::io::{Read, Write};
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf); // selector + CRLF
            let body = "Line one of the text file.\r\n\
                        Line two has more content.\r\n\
                        The third line ends here.\r\n";
            stream.write_all(body.as_bytes()).unwrap();
        });
        let url = format!("gopher://127.0.0.1:{}/0/about.txt", port);
        let page = fetch_gopher(&url, 73).unwrap();
        assert_eq!(page.title.as_deref(), Some("about.txt"));
        assert!(page.lines.iter().any(|l| l.contains("Line one")));
        assert!(page.lines.iter().any(|l| l.contains("third line")));
        assert!(page.links.is_empty());
        assert_eq!(page.url, url);
    }

    #[test]
    fn test_e2e_gopher_directory() {
        // Hand-rolled gopher menu: itype + display + \t + selector +
        // \t + host + \t + port + CRLF.  Mix of informational ('i'),
        // text file ('0'), directory ('1'), and search ('7') items.
        let port = spawn_oneshot_server(|mut stream| {
            use std::io::{Read, Write};
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf);
            let port = stream.local_addr().unwrap().port();
            let body = format!(
                "iWelcome to the test server.\t\terror.host\t1\r\n\
                 0Read the README\t/0/readme.txt\t127.0.0.1\t{port}\r\n\
                 1Subdirectory\t/1/sub\t127.0.0.1\t{port}\r\n\
                 7Search the index\t/7/search\t127.0.0.1\t{port}\r\n\
                 .\r\n"
            );
            stream.write_all(body.as_bytes()).unwrap();
        });
        let url = format!("gopher://127.0.0.1:{}/", port);
        let page = fetch_gopher(&url, 73).unwrap();
        assert_eq!(page.links.len(), 3, "expected 3 actionable links");
        assert!(page.links[0].contains("/0/readme.txt"));
        assert!(page.links[1].contains("/1/sub"));
        assert!(
            page.links[2].ends_with("?search"),
            "type-7 search link should end with ?search marker, got {}",
            page.links[2]
        );
        assert!(page.lines.iter().any(|l| l.contains("Welcome")));
    }

    #[test]
    fn test_e2e_http_basic_page() {
        let port = spawn_oneshot_server(|mut stream| {
            use std::io::Write;
            let _ = read_request_blob(&stream);
            let body = "<!DOCTYPE html><html><head><title>Test Page</title></head>\
                        <body>\
                        <p>This is a paragraph.</p>\
                        <p>Visit <a href=\"http://example.org/\">example.org</a> for info.</p>\
                        </body></html>";
            stream
                .write_all(http_200("text/html; charset=utf-8", body).as_bytes())
                .unwrap();
        });
        let url = format!("http://127.0.0.1:{}/", port);
        let page = fetch_and_render(&url, 73).unwrap();
        assert_eq!(page.title.as_deref(), Some("Test Page"));
        assert!(page.lines.iter().any(|l| l.contains("paragraph")));
        assert_eq!(page.links.len(), 1);
        assert_eq!(page.links[0], "http://example.org/");
    }

    #[test]
    fn test_e2e_http_plain_text() {
        let port = spawn_oneshot_server(|mut stream| {
            use std::io::Write;
            let _ = read_request_blob(&stream);
            let body = "Plain text line 1.\n\
                        Plain text line 2.\n\
                        Plain text line 3.\n";
            stream
                .write_all(http_200("text/plain; charset=utf-8", body).as_bytes())
                .unwrap();
        });
        let url = format!("http://127.0.0.1:{}/", port);
        let page = fetch_and_render(&url, 73).unwrap();
        // text/plain bypasses HTML parsing — no <title>, no link
        // extraction, no form discovery.
        assert!(page.title.is_none());
        assert!(page.lines.iter().any(|l| l.contains("Plain text line 1")));
        assert!(page.lines.iter().any(|l| l.contains("Plain text line 3")));
        assert!(page.links.is_empty());
        assert!(page.forms.is_empty());
    }

    #[test]
    fn test_e2e_http_form_submit_post() {
        // Two-connection flow: GET returns a form page; POST returns a
        // confirmation page and we capture the request body to verify
        // the form-encoded payload.
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let captured_handler = Arc::clone(&captured);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // First conn: serve the form page.
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_request_blob(&stream);
            let body = "<html><head><title>Form Page</title></head><body>\
                        <form method=\"post\" action=\"/submit\">\
                        <label for=\"q\">Query:</label>\
                        <input type=\"text\" name=\"q\" id=\"q\" value=\"initial\">\
                        <input type=\"submit\" value=\"Go\">\
                        </form></body></html>";
            stream
                .write_all(http_200("text/html", body).as_bytes())
                .unwrap();
            drop(stream);

            // Second conn: capture POST body, return confirmation.
            let (mut stream, _) = listener.accept().unwrap();
            let req = read_request_blob(&stream);
            captured_handler.lock().unwrap().extend_from_slice(&req);
            let body =
                "<html><head><title>Submitted</title></head><body>OK</body></html>";
            stream
                .write_all(http_200("text/html", body).as_bytes())
                .unwrap();
        });

        let base = format!("http://127.0.0.1:{}/", port);

        // Round 1: fetch form, mutate the text field, submit.
        let page = fetch_and_render(&base, 73).unwrap();
        assert_eq!(page.forms.len(), 1, "expected 1 form on the page");
        let mut form = page.forms[0].clone();
        assert_eq!(form.method, "post");
        for f in form.fields.iter_mut() {
            if let FormField::Text { name, value, .. } = f {
                if name == "q" {
                    *value = "hello world".to_string();
                }
            }
        }
        let confirm = submit_form(&base, &form, 73).unwrap();
        assert_eq!(confirm.title.as_deref(), Some("Submitted"));

        // Round 2: verify what the server received.  url-form-encoded
        // pairs use '+' for space; a standards-friendly URL-encoder
        // could also emit %20 — accept either.
        let req = captured.lock().unwrap();
        let s = std::str::from_utf8(&req).unwrap();
        assert!(
            s.starts_with("POST /submit "),
            "expected POST /submit, got: {}",
            s.lines().next().unwrap_or("(empty)")
        );
        assert!(
            s.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "expected form-urlencoded Content-Type"
        );
        let body_start = s
            .find("\r\n\r\n")
            .map(|i| i + 4)
            .expect("request had no body");
        let body = &s[body_start..];
        assert!(
            body.contains("q=hello+world") || body.contains("q=hello%20world"),
            "expected q=hello world (URL-encoded), got body: {:?}",
            body
        );
    }
}
