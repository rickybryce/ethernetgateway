//! The main menu's second page (`2`): Add-Ons, and restarting or shutting
//! down the computer the gateway runs on.
//!
//! **Every platform, unlike the power items on it.**  Restart and shutdown
//! are Unix only (`power.rs`), and while they were all the page carried the
//! page was Unix only too -- so a Windows gateway had no second page at all.
//! Add-Ons work everywhere, so the page now does, and the power rows, keys
//! and help are the parts gated.

use super::*;

/// Whether this installation can change the computer's power state: always
/// `false` off Unix, where the power items do not exist.
pub(in crate::telnet) fn power_available() -> bool {
    #[cfg(unix)]
    {
        super::power::available()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether the second page has anything on it.
///
/// **The page outlives its contents.**  It is where the next thing that does
/// not fit the main menu goes; Add-Ons were the first, which made this
/// `power || add-ons` rather than the power setting alone.
///
/// Offering `2` to an empty page is worse than not offering it: the operator
/// spends a keypress to learn there was nothing there.
pub(in crate::telnet) fn second_page_has_items() -> bool {
    power_available() || crate::addons::any()
}

/// The page's valid-key hint, for what the page is showing.
///
/// A function rather than a literal at the `_` arm, so the test that checks
/// it fits and names every key reads the string the operator is shown.
///
/// **Both flags are parameters**, for the reason `more_menu_rows` takes them:
/// power comes from the kernel and add-ons from the disk, so a version
/// reading them itself could only ever be exercised in whichever state the
/// machine running the suite happens to be in.
///
/// Budgeted against `PETSCII_WIDTH - 2`: `show_error` prints a two-space
/// indent in front of whatever it is given.
pub(in crate::telnet) fn more_menu_hint(powered: bool, addons: bool) -> &'static str {
    match (addons, powered) {
        (true, true) => "Press A, R, S, H, or Q.",
        (true, false) => "Press A, H, or Q.",
        (false, true) => "Press R, S, H, or Q.",
        (false, false) => "Press H or Q.",
    }
}

impl TelnetSession {
    /// The main menu's second page.
    ///
    /// Returns `false` when the session must end — the same convention as
    /// [`TelnetSession::handle_main_command`], and the only way out of here
    /// that isn't `Q`: once the machine has accepted a restart or a shutdown
    /// there is nothing left to return to.
    pub(in crate::telnet) async fn more_menu(&mut self) -> Result<bool, std::io::Error> {
        loop {
            // Asked on every draw: an add-on copied in (or the last one
            // removed) shows here at the next look, as it does on the list.
            let powered = power_available();
            let addons = crate::addons::any();
            self.clear_screen().await?;
            for row in self.more_menu_rows(powered, addons) {
                self.send_line(&row).await?;
            }
            // Named for the page, like every other prompt in the product
            // (`gateway/config`, `gateway/serial`): "more" was the old
            // item's word and stopped naming anything when it became
            // "Second Menu".  Found by reading the live screen, not the code.
            let prompt = format!("{}> ", self.cyan("gateway/second"));
            self.send(&prompt).await?;
            self.flush().await?;

            // ESC and a dropped session both arrive as `None`, and both mean
            // the same thing here: go back.  `get_menu_input` has already
            // lowercased whatever was typed.
            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(true),
            };

            match input.as_str() {
                "q" => return Ok(true),
                "h" => {
                    // ASCII only: `to_latin1_bytes` turns anything above
                    // U+00FF into `?`, and a PETSCII screen has no dash of
                    // its own to spare.
                    self.show_help_page("SECOND MENU HELP", Self::more_help_lines(powered, addons))
                        .await?;
                }
                // Gated with the rows and the hint, so a key that is not on
                // the screen is not accepted either.
                "a" if addons => {
                    self.addons_page().await?;
                }
                // `power_action` keeps its own `Elevate::Blocked` arm behind
                // these: a UI that hides an action and a routine that refuses
                // to perform one are different jobs, and the safety one
                // should not depend on the cosmetic one being right.
                #[cfg(unix)]
                "r" if powered => {
                    if !self.power_action(super::power::PowerAction::Restart).await? {
                        return Ok(false);
                    }
                }
                #[cfg(unix)]
                "s" if powered => {
                    if !self.power_action(super::power::PowerAction::Shutdown).await? {
                        return Ok(false);
                    }
                }
                _ => {
                    self.show_error(more_menu_hint(powered, addons)).await?;
                }
            }
        }
    }

    /// Every row the page draws, in order.
    ///
    /// Split out of the renderer so `test_more_menu_rows_fit_the_screen` can
    /// count and measure the **real** page.  The row-count guard it replaced
    /// was arithmetic over literals, which reads nothing from this function
    /// and so could never have noticed a row being added.
    pub(in crate::telnet) fn more_menu_rows(&self, powered: bool, addons: bool) -> Vec<String> {
        let sep = self.separator();
        let mut rows = vec![
            sep.clone(),
            // Named for the menu item that reaches it: an operator who chose
            // "2  Second Menu" must not land on a page calling itself
            // something else.
            format!("  {}", self.yellow("SECOND MENU")),
            sep,
            String::new(),
        ];
        if addons {
            rows.push(format!("  {}  Add-Ons", self.cyan("A")));
            rows.push(String::new());
        }
        // Hidden, not shown-and-refused: an item an operator cannot use
        // teaches them to distrust the menu.
        #[cfg(unix)]
        if powered {
            // Which computer these two keys act on.  A telnet user is by
            // definition somewhere else, and the name is the only thing on
            // this page that says where "the computer" is -- so it goes with
            // the two keys, not with Add-Ons.
            if let Some(row) = self.computer_row() {
                rows.push(row);
                rows.push(String::new());
            }
            rows.push(format!("  {}  Restart the computer", self.cyan("R")));
            rows.push(format!("  {}  Shut down the computer", self.cyan("S")));
            rows.push(String::new());
        }
        #[cfg(not(unix))]
        let _ = powered;
        rows.push(format!(
            "  {}  {}",
            self.action_prompt("Q", "Back"),
            self.action_prompt("H", "Help")
        ));
        rows
    }
}
