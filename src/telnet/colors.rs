//! Color output and PETSCII text-formatting helpers for the telnet session.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

// ─── PETSCII encoding helpers ───────────────────────────────

pub(crate) fn swap_case_for_petscii(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'A'..='Z' => ((c as u8) + 32) as char,
            'a'..='z' => ((c as u8) - 32) as char,
            _ => c,
        })
        .collect()
}

pub(crate) fn petscii_to_ascii_byte(byte: u8) -> u8 {
    match byte {
        0x41..=0x5A => byte + 32,
        0xC1..=0xDA => byte - 0x80,
        _ => byte,
    }
}

pub(crate) fn to_latin1_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
        .collect()
}

impl TelnetSession {
    // ─── Color helpers ─────────────────────────────────────

    pub(in crate::telnet) fn petscii_color(code: u8, text: &str) -> String {
        format!(
            "{}{}{}",
            char::from(code),
            text,
            char::from(PETSCII_DEFAULT),
        )
    }

    pub(in crate::telnet) fn green(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_GREEN, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_GREEN, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn red(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_RED, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_RED, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn cyan(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_CYAN, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_CYAN, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn yellow(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_YELLOW, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_YELLOW, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn amber(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_AMBER, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_YELLOW, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn dim(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_DIM, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_LIGHT_GRAY, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn blue(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_BLUE, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_LIGHT_BLUE, text),
            TerminalType::Ascii => text.to_string(),
        }
    }
    pub(in crate::telnet) fn white(&self, text: &str) -> String {
        match self.terminal_type {
            TerminalType::Ansi => format!("{}{}{}", ANSI_WHITE, text, ANSI_RESET),
            TerminalType::Petscii => Self::petscii_color(PETSCII_WHITE, text),
            TerminalType::Ascii => text.to_string(),
        }
    }

    /// Convert link-marker sentinels (\x02N\x03) to visible `[N]`, colorized
    /// in blue for ANSI/PETSCII terminals. Applied after truncation so that
    /// invisible escape bytes don't affect width calculations.
    pub(in crate::telnet) fn colorize_link_markers(&self, text: &str) -> String {
        let mut result = String::with_capacity(text.len() + 64);
        let mut rest = text;
        while let Some(open) = rest.find('\x02') {
            result.push_str(&rest[..open]);
            let after_open = &rest[open + 1..];
            if let Some(close) = after_open.find('\x03') {
                let inner = &after_open[..close];
                let marker = format!("[{}]", inner);
                if self.terminal_type == TerminalType::Ascii {
                    result.push_str(&marker);
                } else {
                    result.push_str(&self.blue(&marker));
                }
                rest = &after_open[close + 1..];
            } else {
                // Malformed sentinel (e.g. truncated) — silently drop it
                rest = after_open;
            }
        }
        result.push_str(rest);
        result
    }

    pub(in crate::telnet) fn separator(&self) -> String {
        // PETSCII terminals are auto-wrap: a 40-char separator on a
        // 40-col C64 fills the row, auto-wraps to col 0 of the next
        // row, *then* the trailing CR/LF emits another newline — so
        // we end up two rows below the separator with an empty row
        // in between, wasting a precious line on a 25-row screen.
        // Shrinking the bar by one column keeps the cursor inside
        // the row so CR/LF moves down exactly once.
        let width = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 1
        } else {
            56
        };
        self.yellow(&"=".repeat(width))
    }

    pub(in crate::telnet) fn action_prompt(&self, key: &str, description: &str) -> String {
        format!("{}={}", self.cyan(key), description)
    }

    pub(in crate::telnet) fn nav_footer(&self) -> String {
        format!(
            "  {} {} {}",
            self.action_prompt("R", "Refresh"),
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help"),
        )
    }

    pub(in crate::telnet) fn prompt_str(&self) -> String {
        let mut path = self.current_menu.path().to_string();
        if self.current_menu == Menu::FileTransfer && !self.transfer_subdir.is_empty() {
            path = format!("{}/{}", path, self.transfer_subdir);
        }
        format!("{}> ", self.cyan(&path))
    }
}
