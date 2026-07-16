//! Text-based web browser for telnet sessions.
//!
//! Fetches HTTP(S) pages, converts HTML to wrapped plain text with numbered
//! link references, and supports page-by-page navigation. Designed to work
//! within the 40-column PETSCII constraint as well as wider ANSI/ASCII terminals.

use html2text::render::{RichAnnotation, TaggedLine, TaggedLineElement};
use html2text::{config, Element, Handle, RcDom};
use std::io::Read;

use crate::logger::glog;

/// Bookmarks file, stored next to the binary.
const BOOKMARKS_FILE: &str = "bookmarks.txt";
/// Maximum number of bookmarks.
const MAX_BOOKMARKS: usize = 100;

/// Maximum HTTP response body size (1 MB).
const MAX_BODY_SIZE: usize = 1024 * 1024;
/// Maximum rendered lines to keep (prevents memory bloat on huge pages).
const MAX_RENDERED_LINES: usize = 5000;
/// HTTP request timeout in seconds.
const HTTP_TIMEOUT_SECS: u64 = 15;
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
    /// Page title extracted from <title>, if any.
    pub title: Option<String>,
    /// Rendered text lines (plain text, already wrapped to target width).
    pub lines: Vec<String>,
    /// Extracted link URLs, indexed starting at 1 (links[0] = link [1]).
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
            *title = sanitize_for_terminal(title);
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
        return crate::aichat::sanitize_for_terminal(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut segment = String::new();
    for c in line.chars() {
        if c == '\u{02}' || c == '\u{03}' {
            out.push_str(&crate::aichat::sanitize_for_terminal(&segment));
            segment.clear();
            out.push(c);
        } else {
            segment.push(c);
        }
    }
    out.push_str(&crate::aichat::sanitize_for_terminal(&segment));
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
        }
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
/// `width` is the target column count for word-wrapping (33 for PETSCII, 73 for ANSI).
pub(crate) fn fetch_and_render(url: &str, width: usize) -> Result<WebPage, String> {
    // Follow redirects manually (auto-follow disabled below) so EVERY hop
    // is SSRF-checked before we connect — otherwise ureq would follow a
    // public→internal redirect and dial the internal host before our guard
    // saw it.  (DNS rebinding between this check and ureq's own connect-time
    // resolution remains theoretically possible; it only matters for an
    // untrusted caller with a hostile resolver — documented on is_internal_ip.)
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS)))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .build(),
    );

    let mut current = url.to_string();
    let mut tls_downgraded = false;
    let mut hops = 0usize;
    let (response, final_url) = loop {
        guard_public_url(&current)?;
        let resp = match agent
            .get(&current)
            .header("User-Agent", "EthernetGateway/1.0 (text-mode browser)")
            .header("Accept", "text/html, text/plain;q=0.9, */*;q=0.1")
            .call()
        {
            Ok(r) => r,
            // HTTPS TLS failure: retry the same resource over HTTP
            // (re-guarded at the top of the next iteration).
            Err(e) if current.starts_with("https://") && is_tls_error(&e) => {
                tls_downgraded = true;
                current = format!("http://{}", &current["https://".len()..]);
                continue;
            }
            Err(e) => return Err(format!("{}", e)),
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

    // Check content type
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    // Read body with size limit
    let mut body_bytes = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(MAX_BODY_SIZE as u64)
        .read_to_end(&mut body_bytes)
        .map_err(|e| format!("Read error: {}", e))?;

    let mut page = if content_type.contains("text/plain") {
        // Plain text: just split into lines and wrap
        let text = String::from_utf8_lossy(&body_bytes);
        let lines: Vec<String> = text
            .lines()
            .flat_map(|line| wrap_line(line, width))
            .take(MAX_RENDERED_LINES)
            .collect();
        WebPage {
            title: None,
            lines,
            links: Vec::new(),
            url: final_url,
            forms: Vec::new(),
        }
    } else {
        render_html_body(&body_bytes, final_url, width)?
    };

    page.sanitize();

    if tls_downgraded {
        prepend_tls_downgrade_notice(&mut page, width);
    }
    Ok(page)
}

/// Insert a visible warning at the top of the page when we silently
/// fell back from HTTPS to HTTP because of a TLS error.  Without this,
/// the user has no signal that their request is now in the clear —
/// dangerous for any page that reads cookies, form data, or
/// authentication.
fn prepend_tls_downgrade_notice(page: &mut WebPage, width: usize) {
    let notice = "[!] HTTPS failed (TLS error) — page fetched over plain HTTP.";
    // Use char count (not byte length) so the separator width matches
    // the rendered notice on terminals that count visual columns —
    // the notice contains a 3-byte UTF-8 em-dash.
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
            Err(e) => return Err(format!("{}", e)),
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
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let mut body_bytes = Vec::new();
        response
            .into_body()
            .as_reader()
            .take(MAX_BODY_SIZE as u64)
            .read_to_end(&mut body_bytes)
            .map_err(|e| format!("Read error: {}", e))?;

        let mut page = if content_type.contains("text/plain") {
            let text = String::from_utf8_lossy(&body_bytes);
            let lines: Vec<String> = text
                .lines()
                .flat_map(|line| wrap_line(line, width))
                .take(MAX_RENDERED_LINES)
                .collect();
            WebPage {
                title: None,
                lines,
                links: Vec::new(),
                url: final_url,
                forms: Vec::new(),
            }
        } else {
            render_html_body(&body_bytes, final_url, width)?
        };
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

/// Parse an HTML body into a rendered WebPage with title, links, and forms.
fn render_html_body(body_bytes: &[u8], final_url: String, width: usize) -> Result<WebPage, String> {
    let cfg = config::rich();
    let dom = cfg.parse_html(body_bytes)
        .map_err(|e| format!("Parse error: {}", e))?;

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

    let render_tree = cfg.dom_to_render_tree(&dom)
        .map_err(|e| format!("Render error: {}", e))?;
    let tagged_lines: Vec<TaggedLine<Vec<RichAnnotation>>> = cfg.render_to_lines(render_tree, width)
        .map_err(|e| format!("Render error: {}", e))?;

    // Extract links and build numbered text.
    let mut links: Vec<String> = Vec::new();
    let mut rendered_lines: Vec<String> = Vec::new();

    for tagged_line in &tagged_lines {
        let mut line_text = String::new();
        let elements: Vec<_> = tagged_line.iter().collect();
        for (idx, element) in elements.iter().enumerate() {
            if let TaggedLineElement::Str(tagged_str) = *element as &TaggedLineElement<Vec<RichAnnotation>> {
                let seg_link = tagged_str.tag.iter().find_map(|ann| {
                    if let RichAnnotation::Link(url) = ann { Some(url.clone()) } else { None }
                });

                line_text.push_str(&tagged_str.s);

                if let Some(ref href) = seg_link {
                    let next_link = elements.get(idx + 1).and_then(|next| {
                        if let TaggedLineElement::Str(ns) = *next as &TaggedLineElement<Vec<RichAnnotation>> {
                            ns.tag.iter().find_map(|ann| {
                                if let RichAnnotation::Link(u) = ann { Some(u.clone()) } else { None }
                            })
                        } else {
                            None
                        }
                    });

                    let link_ending = match &next_link {
                        Some(next_href) => next_href != href,
                        None => true,
                    };

                    if link_ending && !href.is_empty() && !href.starts_with('#') {
                        let link_num = if let Some(pos) = links.iter().position(|l| l == href) {
                            pos + 1
                        } else {
                            links.push(href.clone());
                            links.len()
                        };
                        line_text.push_str(&format!("\x02{}\x03", link_num));
                    }
                }
            }
        }
        rendered_lines.push(line_text);
        if rendered_lines.len() >= MAX_RENDERED_LINES {
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

    Ok(WebPage {
        title,
        lines: cleaned,
        links,
        url: final_url,
        forms,
    })
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
            extract_form_fields(node, &mut fields, &mut submit_label, &labels);

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
/// placeholder, aria-label, title, associated <label> element, then field name.
///
/// `labels` is a pre-built `id → label-text` map for the whole form (see
/// [`collect_field_labels`]).  It replaces a per-field recursive subtree scan
/// for `<label for="id">`: that was O(fields × subtree), so a hostile page of
/// tens of thousands of bare `<input id=…>` (still under `MAX_BODY_SIZE`) cost
/// quadratic CPU on a shared render thread with no time budget — a soft-DoS
/// (round-6 F1).  The one-pass map makes the whole form O(subtree).
fn get_field_label(
    node: &Handle,
    field_name: &str,
    labels: &std::collections::HashMap<String, String>,
) -> String {
    get_attr(node, "placeholder")
        .or_else(|| get_attr(node, "aria-label"))
        .or_else(|| get_attr(node, "title"))
        .or_else(|| get_attr(node, "id").and_then(|id| labels.get(&id).cloned()))
        .unwrap_or_else(|| field_name.to_string())
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

fn extract_form_fields(node: &Handle, fields: &mut Vec<FormField>, submit_label: &mut Option<String>, labels: &std::collections::HashMap<String, String>) {
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
                        if submit_label.is_none() && !value.is_empty() {
                            *submit_label = Some(value.clone());
                        }
                        if !field_name.is_empty() {
                            fields.push(FormField::Hidden { name: field_name, value });
                        }
                    }
                    "checkbox" => {
                        if !field_name.is_empty() {
                            let label = get_field_label(node, &field_name, labels);
                            let val = if value.is_empty() { "on".to_string() } else { value };
                            let checked = get_attr(node, "checked").is_some();
                            fields.push(FormField::Checkbox { name: field_name, value: val, checked, label });
                        }
                    }
                    "radio" => {
                        if !field_name.is_empty() {
                            let label = get_attr(node, "aria-label")
                                .unwrap_or_else(|| value.clone());
                            let checked = get_attr(node, "checked").is_some();
                            fields.push(FormField::Radio { name: field_name, value, checked, label });
                        }
                    }
                    "image" | "button" | "reset" | "file" => {} // skip
                    _ => {
                        if !field_name.is_empty() {
                            let label = get_field_label(node, &field_name, labels);
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
                    let label = get_field_label(node, &field_name, labels);
                    fields.push(FormField::TextArea { name: field_name, value, label });
                }
            }
            "select" => {
                let field_name = get_attr(node, "name").unwrap_or_default();
                if !field_name.is_empty() {
                    let mut options = Vec::new();
                    let mut selected = 0;
                    extract_select_options(node, &mut options, &mut selected);
                    let label = get_field_label(node, &field_name, labels);
                    fields.push(FormField::Select { name: field_name, options, selected, label });
                }
            }
            "button" => {
                let btn_type = get_attr(node, "type").unwrap_or_else(|| "submit".to_string());
                if btn_type == "submit" && submit_label.is_none() {
                    let text = get_text_content(node);
                    if !text.is_empty() {
                        *submit_label = Some(text);
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
        extract_form_fields(child, fields, submit_label, labels);
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

    // Read response
    let mut body = Vec::new();
    stream
        .get_mut()
        .take(GOPHER_MAX_BODY as u64)
        .read_to_end(&mut body)
        .map_err(|e| format!("Read error: {}", e))?;

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
    use super::*;

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
        let page = render_html_body(body.as_bytes(), "http://x/".to_string(), 73)
            .expect("moderately-nested page should render");
        assert!(
            page.lines.iter().any(|l| l.contains("hello world")),
            "rendered text should contain the body content"
        );
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
        ] {
            assert!(
                is_internal_ip(s.parse::<IpAddr>().unwrap()),
                "{} should be classified internal",
                s
            );
        }
        // Public addresses — must be allowed.
        for s in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
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
    /// title) falls back to the field name — unchanged by the refactor.
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
        assert_eq!(label.as_deref(), Some("query"));
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
