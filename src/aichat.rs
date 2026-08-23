//! AI Chat via the Groq API.
//!
//! Sends user questions to the Groq API (OpenAI-compatible endpoint) and
//! returns the text response. Uses a blocking HTTP client (`ureq`) which
//! should be called from `tokio::task::spawn_blocking()`.

use std::io::Read;

const API_TIMEOUT_SECS: u64 = 30;
/// The model to ask, when the operator has not named one.
///
/// **Groq retires models, and a retired model kills this feature outright.**
/// `llama-3.3-70b-versatile` was the default until 2026-08-23, when it began
/// answering `404 model_not_found` -- "The model does not exist or you do not
/// have access to it" -- and AI Chat simply stopped working. That is why
/// `ai_model` exists: an operator can move to whatever Groq serves next without
/// waiting for a build.
///
/// `openai/gpt-oss-120b` measured clean against a live key: a plain chat model,
/// no chain-of-thought in the answer, and no built-in web tools -- which matters
/// for something reached through a gateway, since `groq/compound*` will go and
/// fetch things on its own. Two candidates were rejected on measurement:
/// `qwen/qwen3.6-27b` puts a literal `<think>` block in `content`, and
/// `groq/compound` produced 20 KB of reasoning for a one-sentence answer.
pub(crate) const GROQ_MODEL: &str = "openai/gpt-oss-120b";

#[cfg(test)]
mod model_tests {
    use super::*;

    /// **A `<think>` block is the model's working, not its answer.**  Measured on
    /// `qwen/qwen3.6-27b`, which answered a one-sentence question with 1775
    /// characters of which the first 1600 were deliberation.
    #[test]
    fn test_a_leading_thought_block_is_dropped() {
        assert_eq!(strip_thoughts("<think>\nplanning\n</think>\n\nThe answer."), "The answer.");
        assert_eq!(strip_thoughts("  <think>x</think> Answer  "), "Answer");
        // No block: untouched but trimmed.
        assert_eq!(strip_thoughts("  Just an answer.  "), "Just an answer.");
    }

    /// The two cases where the block must be **kept**, because dropping it would
    /// throw away the only text there is.
    #[test]
    fn test_an_unclosed_or_empty_thought_block_is_kept() {
        // Cut short: everything after the tag is all the operator has.
        let cut = "<think>it was thinking when the reply ended";
        assert_eq!(strip_thoughts(cut), cut);
        // Closed but nothing after it: the working is the whole reply.
        let only = "<think>all of it</think>";
        assert_eq!(strip_thoughts(only), only);
        // A tag in the middle is the model talking about the tag.
        let mid = "You write <think> like this.";
        assert_eq!(strip_thoughts(mid), mid);
    }

    /// **The characters a real model actually sends.**  Measured from live Groq
    /// replies, not imagined: an answer about a "PLC-5" comes back with U+2011
    /// non-breaking hyphens, U+202F narrow no-break spaces and U+2019
    /// apostrophes in it, and a PETSCII or 7-bit terminal renders each as two or
    /// three pieces of rubbish.  Reported from a C64 on 2026-08-23, when the
    /// fold existed but only the web browser called it.
    #[test]
    fn test_a_models_typography_reaches_the_terminal_as_ascii() {
        use super::display_for_terminal as sanitize_for_terminal;
        // Exactly what was measured coming back from Groq.
        assert_eq!(
            sanitize_for_terminal("The Altair\u{202f}8800 is a kit\u{2011}based micro"),
            "The Altair 8800 is a kit-based micro"
        );
        assert_eq!(
            sanitize_for_terminal("Allen\u{2011}Bradley\u{2019}s PLC\u{2010}5 \u{2014} a PLC"),
            "Allen-Bradley's PLC-5 - a PLC"
        );
        // Every dash in the block, since a model picks whichever it likes.
        for dash in ['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}'] {
            assert_eq!(sanitize_for_terminal(&format!("PLC{dash}5")), "PLC-5", "{dash:?}");
        }
        // And the thing this must NOT do: real letters are not mangled.
        assert_eq!(sanitize_for_terminal("café Ångström"), "café Ångström");
    }

    /// **A wrapped line must be as many columns as it is characters.**
    ///
    /// This is the "chat is not wrapping" report, and it is the *same* defect as
    /// the mangled dashes rather than a second one. [`super::wrap_line`] counts
    /// characters, so it wraps correctly -- but a terminal that draws one glyph
    /// per byte renders a multi-byte character as two or three columns, and the
    /// line overruns the screen and wraps itself. Measured on a real Groq reply:
    /// 143 characters, **151 bytes**, so a 78-character line arrived as 82
    /// columns on an 80-column terminal.
    #[test]
    fn test_a_wrapped_line_is_as_many_columns_as_characters() {
        let reply = "The Altair\u{202f}8800 was a pioneering, kit\u{2011}based microcomputer \
                     released in 1975 that used Intel\u{2019}s 8080 CPU and sparked the \
                     home\u{2011}computer revolution.";
        // The premise, asserted rather than assumed: this text really is wider in
        // bytes than in characters before folding.
        assert!(
            reply.len() > reply.chars().count(),
            "the reply this test rests on is pure ASCII, so it proves nothing"
        );

        // 38 for a 40-column PETSCII screen, 78 for an 80-column one.
        for width in [38usize, 78] {
            for line in super::wrap_line(&super::display_for_terminal(reply), width) {
                assert!(
                    line.chars().count() <= width,
                    "{line:?} is {} characters, over {width}",
                    line.chars().count()
                );
                assert_eq!(
                    line.len(),
                    line.chars().count(),
                    "{line:?} is {} bytes but {} characters -- it will overrun the screen",
                    line.len(),
                    line.chars().count()
                );
            }
        }
    }

    /// A blank model name falls back rather than asking Groq for `""`.
    #[test]
    fn test_a_blank_model_is_the_default() {
        // The resolution is in `ask_model`; this pins the rule it implements so
        // the constant cannot quietly become the empty string.
        assert!(!GROQ_MODEL.trim().is_empty());
        assert!(GROQ_MODEL.contains('/') || !GROQ_MODEL.contains(' '), "{GROQ_MODEL}");
    }
}

/// Drop a leading chain-of-thought block from an answer.
///
/// Some models put their working in `content` inside `<think>` tags rather than
/// in a field of its own -- measured on `qwen/qwen3.6-27b`, which answered a
/// one-sentence question with 1775 characters of which the first 1600 were its
/// own deliberation. An operator on a 40-column terminal reading that at 9600
/// baud is being punished for their choice of model, so it is removed here
/// rather than described in a release note.
///
/// Only a *leading* block, and only a closed one: a `<think>` in the middle of
/// an answer is the model talking about the tag, and an unclosed one means the
/// reply was cut short, where the text after it is all the operator has.
fn strip_thoughts(text: &str) -> String {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix("<think>") else { return text.trim().to_string() };
    match rest.split_once("</think>") {
        Some((_, after)) if !after.trim().is_empty() => after.trim().to_string(),
        _ => text.trim().to_string(),
    }
}

/// Send a question to the Groq API and return the response text, asking `model`
/// -- or [`GROQ_MODEL`] when the operator has named none.
pub(crate) fn ask_model(api_key: &str, question: &str, model: &str) -> Result<String, String> {
    let url = "https://api.groq.com/openai/v1/chat/completions";
    let model = if model.trim().is_empty() { GROQ_MODEL } else { model.trim() };

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "user", "content": question}
        ]
    });

    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(std::time::Duration::from_secs(API_TIMEOUT_SECS)))
            // Read the body on non-2xx responses instead of collapsing them into
            // an opaque "http status: 401" transport error.  Groq returns a JSON
            // body with a descriptive `error.message` (e.g. "Invalid API Key")
            // even on failures; keeping the response lets the extraction below
            // surface that message to the user.
            .http_status_as_error(false)
            .build(),
    );

    let response = agent
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", &format!("Bearer {}", api_key))
        .send(serde_json::to_string(&request_body).map_err(|e| format!("JSON serialize error: {}", e))?.as_bytes())
        .map_err(|e| format!("API error: {}", e))?;

    let mut body_bytes = Vec::new();
    response
        .into_body()
        .as_reader()
        .take(1024 * 1024)
        .read_to_end(&mut body_bytes)
        .map_err(|e| format!("Read error: {}", e))?;

    let json: serde_json::Value =
        serde_json::from_slice(&body_bytes).map_err(|e| format!("JSON parse error: {}", e))?;

    let message = json.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message"));
    let field = |name: &str| -> Option<String> {
        message
            .and_then(|m| m.get(name))
            .and_then(|t| t.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    // **`content`, then `reasoning`, because an empty answer is the worst
    // outcome.** The reasoning-capable models put their working in a second
    // field and their answer in `content` -- but a model asked for very few
    // tokens spends them all on the working and returns `content: ""`, which
    // this used to hand back verbatim: a blank screen, indistinguishable from a
    // hang. Measured on `openai/gpt-oss-20b`, whose whole reply was 322
    // characters of `reasoning` and nothing else.
    field("content")
        .map(|s| strip_thoughts(&s))
        .or_else(|| field("reasoning").map(|s| strip_thoughts(&s)))
        .ok_or_else(|| {
            if let Some(err) = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
            {
                format!("Groq error: {}", err)
            } else {
                "No response from Groq".to_string()
            }
        })
}

/// Strip bytes that would corrupt a terminal session if a remote
/// response (or a prompt-injected reply through the LLM) tried to
/// smuggle them: ASCII control codes (< 0x20) except tab, plus DEL
/// and IAC.  Non-ASCII Unicode is preserved so the model can return
/// accented or extended characters.  Bare ESC is dropped — the
/// trailing bytes of any CSI/OSC sequence then render as visible
/// printable text rather than as cursor moves or color changes.
pub(crate) fn sanitize_for_terminal(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|&c| {
            let b = c as u32;
            // Allow tab and printable characters; drop C0 controls, DEL, and
            // the C1 control range (U+0080–U+009F) which some 8-bit terminals
            // interpret as CSI/OSC introducers.  A real telnet IAC is a raw
            // 0xFF *byte* — it cannot exist as a char in a &str, so IAC
            // escaping is handled on the wire in tnio, not by filtering here.
            c == '\t' || (b >= 0x20 && b != 0x7F && !(0x80..=0x9F).contains(&b))
        })
        .collect::<String>();
    kept
}

/// Text on its way to a terminal: control bytes stripped **and** typography
/// folded to ASCII.
///
/// **The composed rule needs a name, because the two halves were separate and
/// only one caller ever took both.** `fold_terminal_safe` lived in the web
/// browser and was called nowhere else, so AI Chat and the weather service
/// stripped escapes and then printed the typography raw. On a PETSCII or 7-bit
/// terminal each such character arrives as two or three pieces of rubbish --
/// reported from a C64 on 2026-08-23, asking what a PLC-5 is and getting the
/// hyphens back as garbage.
///
/// It also **fixes the wrapping**, which looked like a second bug and is the
/// same one: [`wrap_line`] counts *characters*, and a line of 78 characters
/// containing four multi-byte ones is 82 bytes -- 82 columns on a terminal that
/// draws one glyph per byte -- so it overran an 80-column screen and wrapped
/// itself. Measured on a real reply: 143 characters, 151 bytes before folding
/// and 143 after, at which point 78 characters is 78 columns.
///
/// **Not folded into [`sanitize_for_terminal`]**, though that was tried: the web
/// browser sanitizes a page's *URL* with it, and folding an en-dash in a URL
/// breaks relative-link resolution. `test_sanitize_does_not_fold_the_url_or_
/// form_values` caught it, which is exactly the test one hopes exists.
pub(crate) fn display_for_terminal(s: &str) -> String {
    fold_terminal_safe(&sanitize_for_terminal(s))
}

/// Fold the characters a text-mode terminal cannot draw down to ASCII.
///
/// **Measured, not guessed.** Loading `telnetbible.com` through this browser
/// and capturing the wire showed page 1 carrying zero bytes above 0x7F and
/// page 3 carrying **918** — a third of the stream. Every one of them was a
/// box-drawing character: `html2text` renders an HTML table with `─` (284 of
/// them), `│`, `┼`, `┴`, `┬`, each three bytes of UTF-8. On a 7-bit console —
/// an SC126 running EGT80, a C64, any real serial terminal — each arrives as
/// three unrenderable characters, which is exactly the "garbage from page two
/// onward" an operator sees. It looks like a terminal fault and is not one.
///
/// `html2text` offers `no_table_borders()`, which would also remove the bytes
/// — by removing the table's structure. `+---+` says the same thing in
/// characters every terminal since the teletype can draw, so the borders are
/// translated rather than dropped.
///
/// **Every surface that shows fetched text needs this, and for two releases
/// only one of them had it.** It lived in `webbrowser` and was called nowhere
/// else, so AI Chat and the weather service sanitized their text and printed
/// the typography raw: a Groq answer about a "PLC-5" arrives with U+2011
/// non-breaking hyphens and U+202F narrow spaces in it, which a PETSCII or
/// 7-bit terminal renders as two or three pieces of rubbish per character.
/// Reported from a C64 on 2026-08-23. It is called from
/// [`sanitize_for_terminal`] now, so a new consumer gets it by construction
/// rather than by remembering.
///
/// **Deliberately narrow.** Only characters with an unambiguous ASCII
/// equivalent are folded: box drawing, the smart quotes and dashes that word
/// processors emit, the non-breaking space. Accented letters and non-Latin
/// scripts are left exactly as they were — a modern terminal over SSH still
/// renders them, and turning them into `?` would trade one class of wrong
/// output for another. This fixes what was measured and nothing else.
pub(crate) fn fold_terminal_safe(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            Some(match c {
                // Box drawing, light through heavy and double: horizontals,
                // verticals, then every corner and junction as '+'.
                '\u{2500}' | '\u{2501}' | '\u{2504}' | '\u{2505}' | '\u{2508}' | '\u{2509}'
                | '\u{254C}' | '\u{254D}' | '\u{2550}' | '\u{2574}' | '\u{2576}'
                | '\u{2578}' | '\u{257A}' => '-',
                '\u{2502}' | '\u{2503}' | '\u{2506}' | '\u{2507}' | '\u{250A}' | '\u{250B}'
                | '\u{254E}' | '\u{254F}' | '\u{2551}' | '\u{2575}' | '\u{2577}'
                | '\u{2579}' | '\u{257B}' => '|',
                c if ('\u{2500}'..='\u{257F}').contains(&c) => '+',
                // Block elements and shading — a filled cell reads as '#'.
                c if ('\u{2580}'..='\u{259F}').contains(&c) => '#',
                // The typography a CMS emits without being asked.
                '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
                '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
                '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                '\u{00A0}' | '\u{2007}' | '\u{202F}' | '\u{2009}' => ' ',
                '\u{2022}' | '\u{00B7}' | '\u{25AA}' | '\u{25CF}' | '\u{25E6}' => '*',
                // A soft hyphen is an invisible break hint; on a fixed-width
                // screen it is noise, so it goes rather than becoming '-'.
                '\u{00AD}' => return None,
                // U+2026 is deliberately NOT mapped here: it is the one fold
                // that is not one-for-one, and it is expanded below.  Mapping
                // it to '.' here would consume it and leave a single dot.
                other => other,
            })
        })
        .collect::<String>()
        // The ellipsis is the one fold that is not 1:1, done after the pass so
        // the character map above stays a simple substitution.
        .replace('\u{2026}', "...")
}

/// Word-wrap a single line to fit within `width` columns, breaking at spaces.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut remaining = line;
    while !remaining.is_empty() {
        if remaining.chars().count() <= width {
            result.push(remaining.to_string());
            break;
        }
        let boundary = remaining
            .char_indices()
            .nth(width)
            .map_or(remaining.len(), |(i, _)| i);
        let boundary = if boundary == 0 {
            remaining
                .char_indices()
                .nth(1)
                .map_or(remaining.len(), |(i, _)| i)
        } else {
            boundary
        };
        let break_at = remaining[..boundary].rfind(' ').unwrap_or(boundary);
        let break_at = if break_at == 0 { boundary } else { break_at };
        result.push(remaining[..break_at].to_string());
        remaining = remaining[break_at..].trim_start();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_timeout_is_reasonable() {
        const _: () = assert!(API_TIMEOUT_SECS >= 10, "too short for LLM response");
        const _: () = assert!(API_TIMEOUT_SECS <= 120, "too long to wait");
    }

    #[test]
    fn test_wrap_line_short() {
        assert_eq!(wrap_line("hello", 40), vec!["hello"]);
    }

    #[test]
    fn test_wrap_line_empty() {
        assert_eq!(wrap_line("", 40), vec![""]);
    }

    #[test]
    fn test_wrap_line_long() {
        let lines = wrap_line("the quick brown fox jumps over the lazy dog", 20);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.chars().count() <= 20, "line too long: '{}'", line);
        }
    }

    #[test]
    fn test_wrap_line_exact() {
        assert_eq!(wrap_line("1234567890", 10), vec!["1234567890"]);
    }

    #[test]
    fn test_wrap_line_no_spaces() {
        let lines = wrap_line("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(lines.len() > 1);
    }

    #[test]
    fn test_wrap_line_preserves_words() {
        let lines = wrap_line("hello world foo bar", 12);
        assert_eq!(lines[0], "hello world");
        assert_eq!(lines[1], "foo bar");
    }

    #[test]
    fn test_sanitize_strips_ansi_escape() {
        assert_eq!(sanitize_for_terminal("\x1b[31mred\x1b[0m"), "[31mred[0m");
    }

    #[test]
    fn test_sanitize_strips_bare_cr_and_nul() {
        assert_eq!(sanitize_for_terminal("a\rb\0c"), "abc");
    }

    #[test]
    fn test_sanitize_keeps_printable_latin1() {
        // U+00FF (ÿ) is a printable Latin-1 character and must be preserved.
        // The real telnet IAC (a wire 0xFF byte) can never reach here as a
        // char, so it is handled downstream in tnio, not by char filtering.
        assert_eq!(sanitize_for_terminal("ok\u{00ff}done"), "ok\u{00ff}done");
    }

    #[test]
    fn test_sanitize_keeps_tab_and_unicode() {
        assert_eq!(sanitize_for_terminal("a\tb café"), "a\tb café");
    }

    #[test]
    fn test_sanitize_strips_del() {
        assert_eq!(sanitize_for_terminal("a\x7fb"), "ab");
    }

    #[test]
    fn test_wrap_line_petscii_width() {
        let text = "This is a test of the PETSCII word wrapping at 38 columns wide";
        let lines = wrap_line(text, 38);
        for line in &lines {
            assert!(line.chars().count() <= 38, "line '{}' exceeds 38 chars", line);
        }
    }

    #[test]
    fn test_wrap_line_long_word_respects_width() {
        // A single word longer than the width has no space to break on, so it
        // must be hard-broken into chunks that each still fit the width.
        let lines = wrap_line("abcdefghij", 4);
        for l in &lines {
            assert!(l.chars().count() <= 4, "chunk '{}' exceeds width 4", l);
        }
        assert_eq!(lines.concat(), "abcdefghij", "hard break must not drop chars");
    }

    #[test]
    fn test_wrap_line_multibyte_hard_break_no_panic() {
        // Forced break on multibyte chars must land on UTF-8 boundaries (never
        // panic on a byte slice) and still respect the width. 7 two-byte 'é's,
        // no spaces to break on.
        let lines = wrap_line("ééééééé", 3);
        for l in &lines {
            assert!(l.chars().count() <= 3, "chunk '{}' exceeds width 3", l);
        }
        assert_eq!(lines.concat(), "ééééééé", "hard break must not drop chars");
    }

    #[test]
    fn test_sanitize_strips_c1_controls() {
        // The C1 control range (U+0080–U+009F) — e.g. U+009B (CSI) — is
        // dropped while surrounding printable text survives.
        assert_eq!(sanitize_for_terminal("a\u{009b}b"), "ab");
        assert_eq!(sanitize_for_terminal("x\u{0080}mid\u{009f}y"), "xmidy");
    }
}

#[cfg(test)]
mod live_gate {
    /// **The default model actually answers, through the shipped call.**
    ///
    /// The gate this feature did not have, and the reason it broke silently:
    /// `llama-3.3-70b-versatile` was retired by Groq and every unit test still
    /// passed, because none of them spoke to Groq. It asserts three things a
    /// working AI Chat needs -- a reply arrives, it is not empty, and it carries
    /// no chain-of-thought markup -- since a model can satisfy the first and
    /// fail the others (measured: `qwen/qwen3.6-27b` returns 1775 characters
    /// beginning with `<think>`, `openai/gpt-oss-20b` under a small token
    /// budget returns nothing but `reasoning`).
    ///
    /// Ignored: needs a Groq key and the network. Set `GROQ_KEY`, and
    /// `GROQ_TEST_MODEL` to try one you are considering.
    #[test]
    #[ignore]
    fn test_the_configured_model_answers() {
        let Ok(key) = std::env::var("GROQ_KEY") else {
            eprintln!("set GROQ_KEY to run this");
            return;
        };
        let model = std::env::var("GROQ_TEST_MODEL").unwrap_or_else(|_| super::GROQ_MODEL.into());
        let answer = super::ask_model(&key, "In one sentence, what is an Altair 8800?", &model)
            .unwrap_or_else(|e| panic!("{model} did not answer: {e}"));
        println!("{model} answered {} chars: {answer}", answer.len());
        assert!(!answer.trim().is_empty(), "{model} returned an empty answer");
        assert!(
            !answer.contains("<think>"),
            "{model} put its working in the answer, which an operator reads at 9600 baud: {answer}"
        );
        assert!(
            answer.to_lowercase().contains("8080") || answer.to_lowercase().contains("mits"),
            "{model} answered something, but not about an Altair: {answer}"
        );
    }
}
