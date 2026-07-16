//! CP/M-flavored command shell (flavor A — no CPU emulation).
//!
//! Presents the configured `transfer_dir` as drive **A:** and gives a
//! logged-in telnet/SSH user a CP/M CCP-style `A>` prompt with resident
//! file-management commands (DIR, TYPE, ERA, REN, COPY, MOVE, MKDIR,
//! RMDIR, CD, PWD, STAT, DUMP, HELP).  Everything is implemented purely
//! in Rust over the host filesystem; there is **no** Z80 / BDOS / BIOS
//! emulation and **no** `.COM` execution (that is "flavor B", deferred —
//! see `kernelplan.md` §13).
//!
//! ## Jailing
//! Every command resolves names under `transfer_dir` and can never touch
//! anything above it.  We reuse the transfer subsystem's tested jail
//! primitives rather than inventing new path logic: [`validate_filename`]
//! gates every path component, the current directory is the shared
//! `self.transfer_subdir`, and directory operands are canonicalized and
//! checked with `starts_with(transfer_dir)` for symlink defense — the same
//! belt-and-suspenders `verify_transfer_path` uses.
//!
//! ## Deliberate deviations from stock CP/M (documented for users)
//! - **Paths.** CP/M filenames carry no directory; ours do.  Operands may
//!   be path-qualified with `/` (Unix-style, matching how `transfer_subdir`
//!   is stored): `FILE.TXT` = cwd, `/FILE.TXT` = drive root, `SUB/FILE.TXT`
//!   = into subdir `SUB`, `../FILE.TXT` = parent.  This is what lets COPY /
//!   MOVE / CD move files *between* directories — something the base CP/M
//!   command set cannot express.
//! - **`MOVE` / `MV`** is an invented verb (stock CP/M has no move); `REN`
//!   stays directory-local.
//! - **8.3 is not enforced.**  Files arrive from XMODEM/YMODEM/ZMODEM/Kermit
//!   uploads with names up to 64 chars; forcing 8.3 would hide or mangle
//!   them.  Host names are kept verbatim on disk and shown uppercased in DIR
//!   for CP/M feel; wildcards match case-insensitively against the real name.
//! - **Prompt** shows the cwd (`A:SUB>`) when in a subdirectory, where stock
//!   CP/M only ever shows `A>`.

use super::*;
use std::path::PathBuf;

/// Upper bound on what the interactive `TYPE` / `DUMP` viewers will read into
/// memory before paging.  Files can be up to the 8 MB transfer cap, but
/// pre-formatting a whole 8 MB file into wrapped lines (TYPE) or hex rows
/// (DUMP) would materialize tens of MB of `String`s per session; a 1 MiB
/// viewer cap keeps that bounded (a hex dump or terminal listing of anything
/// larger is not a useful thing to page over a retro link anyway).  Larger
/// files are still transferable — just download them instead of viewing.
const CPM_VIEW_MAX: usize = 1024 * 1024;

/// A parsed CP/M shell command.  Operands are owned so the parser is a
/// pure `&str -> CpmCmd` function with no borrow of the input line.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::telnet) enum CpmCmd {
    /// `DIR [pattern]` / `LS` — list the cwd (or a wildcard/path match).
    Dir(Option<String>),
    /// `TYPE file` — display a text file, paginated.
    Type(String),
    /// `DUMP file` — hex + ASCII dump, paginated.
    Dump(String),
    /// `ERA file` / `DEL` / `RM` — erase file(s) (wildcard allowed).
    Era(String),
    /// `REN new=old` / `REN old new` — rename within one directory.
    Ren { new: String, old: String },
    /// `COPY dst src` / `PIP dst=src` / `CP` — copy a file (no clobber).
    Copy { dst: String, src: String },
    /// `MOVE dst src` / `MV` — relocate a file across directories.
    Move { dst: String, src: String },
    /// `MKDIR name` / `MD` — create a subdirectory.
    Mkdir(String),
    /// `RMDIR name` / `RD` — remove an empty subdirectory.
    Rmdir(String),
    /// `CD [path]` / `CHDIR` — change the working directory.
    Cd(Option<String>),
    /// `PWD` — print the current drive-A path.
    Pwd,
    /// `STAT [file]` — free space / per-file size.
    Stat(Option<String>),
    /// `HELP` / `?` — the command reference.  Any trailing operand is
    /// accepted but ignored (there is no per-command help yet), so
    /// `HELP DIR` shows the full page rather than erroring.
    Help(Option<String>),
    /// `EXIT` / `BYE` / `QUIT` — leave the shell.
    Exit,
    /// `USER` — reported as unsupported (we have no USER areas).
    User,
    /// Blank line — just reprint the prompt.
    Empty,
    /// Unknown verb — CP/M echoes it back with a `?`.
    Unknown(String),
    /// A recognized verb missing its required operand.
    NeedsArg(&'static str),
}

impl TelnetSession {
    // ─── Entry point / REPL ──────────────────────────────────

    /// Run the CP/M shell as a blocking sub-loop, exactly like
    /// `weather()` / `ai_chat()`.  Returns to the File Transfer menu on
    /// `EXIT`/ESC; a disconnect surfaces as `Ok(())` here and is detected
    /// by the caller's next write (same model as the other sub-loops).
    pub(in crate::telnet) async fn cpm_shell(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;
        // A subdir left over from the File Transfer menu might have been
        // removed out from under us; snap back to root if it no longer
        // resolves inside the jail.
        self.verify_transfer_path();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("CP/M SHELL  (drive A:)")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Type HELP for commands, EXIT to")
        ))
        .await?;
        self.send_line(&format!("  {}", self.dim("return to the transfer menu.")))
            .await?;
        self.send_line("").await?;

        loop {
            let prompt = self.cpm_prompt();
            self.send(&prompt).await?;
            self.flush().await?;

            let line = match self.get_line_input().await? {
                Some(s) => s,
                None => break, // ESC or disconnect
            };

            match Self::cpm_parse(&line) {
                CpmCmd::Empty => continue,
                CpmCmd::Exit => break,
                CpmCmd::Pwd => self.cpm_pwd().await?,
                CpmCmd::Dir(pat) => self.cpm_dir(pat.as_deref()).await?,
                CpmCmd::Cd(path) => self.cpm_cd(path.as_deref()).await?,
                CpmCmd::Stat(f) => self.cpm_stat(f.as_deref()).await?,
                CpmCmd::Type(f) => self.cpm_type(&f).await?,
                CpmCmd::Dump(f) => self.cpm_dump(&f).await?,
                CpmCmd::Mkdir(n) => self.cpm_mkdir(&n).await?,
                CpmCmd::Rmdir(n) => self.cpm_rmdir(&n).await?,
                CpmCmd::Era(p) => self.cpm_era(&p).await?,
                CpmCmd::Ren { new, old } => self.cpm_ren(&new, &old).await?,
                CpmCmd::Copy { dst, src } => self.cpm_copy(&dst, &src).await?,
                CpmCmd::Move { dst, src } => self.cpm_move(&dst, &src).await?,
                CpmCmd::Help(topic) => self.cpm_help(topic.as_deref()).await?,
                CpmCmd::User => {
                    self.cpm_err("USER areas are not supported.").await?;
                }
                CpmCmd::NeedsArg(usage) => {
                    self.cpm_err(&format!("Usage: {}", usage)).await?;
                }
                CpmCmd::Unknown(verb) => {
                    // CP/M's classic "echo the bad verb + ?" error.
                    self.send_line(&format!("  {}?", verb.to_uppercase()))
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// The prompt string: `A>` at the root, `A:SUB>` (uppercased) in a
    /// subdirectory so the user always sees where they are.
    fn cpm_prompt(&self) -> String {
        if self.transfer_subdir.is_empty() {
            "A>".to_string()
        } else {
            format!("A:{}>", self.transfer_subdir.to_uppercase())
        }
    }

    /// Print an inline (red) error line and return.  Unlike `show_error`,
    /// this does not clear the screen or wait for a keypress — a shell shows
    /// the error and the REPL loop immediately re-prints the prompt.
    async fn cpm_err(&mut self, msg: &str) -> Result<(), std::io::Error> {
        let colored = self.red(msg);
        self.send_line(&format!("  {}", colored)).await
    }

    // ─── Parsing (pure) ──────────────────────────────────────

    /// Parse a raw input line into a [`CpmCmd`].  Case-insensitive verb,
    /// whitespace-tokenized operands.  Pure — unit-tested directly.
    pub(in crate::telnet) fn cpm_parse(line: &str) -> CpmCmd {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return CpmCmd::Empty;
        }
        // Split verb from the rest (operands keep their internal spacing
        // trimmed at the ends).
        let (verb, rest) = match trimmed.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (trimmed, ""),
        };
        let verb_lc = verb.to_ascii_lowercase();
        let arg = if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        };

        match verb_lc.as_str() {
            "dir" | "ls" => CpmCmd::Dir(arg),
            "type" => match arg {
                Some(a) => CpmCmd::Type(a),
                None => CpmCmd::NeedsArg("TYPE file"),
            },
            "dump" => match arg {
                Some(a) => CpmCmd::Dump(a),
                None => CpmCmd::NeedsArg("DUMP file"),
            },
            "era" | "del" | "rm" => match arg {
                Some(a) => CpmCmd::Era(a),
                None => CpmCmd::NeedsArg("ERA file"),
            },
            "ren" | "rename" => Self::parse_ren(rest),
            "copy" | "pip" | "cp" => Self::parse_two_operand(rest, /*is_move=*/ false),
            "move" | "mv" => Self::parse_two_operand(rest, /*is_move=*/ true),
            "mkdir" | "md" => match arg {
                Some(a) => CpmCmd::Mkdir(a),
                None => CpmCmd::NeedsArg("MKDIR name"),
            },
            "rmdir" | "rd" => match arg {
                Some(a) => CpmCmd::Rmdir(a),
                None => CpmCmd::NeedsArg("RMDIR name"),
            },
            "cd" | "chdir" => CpmCmd::Cd(arg),
            "pwd" => CpmCmd::Pwd,
            "stat" => CpmCmd::Stat(arg),
            "help" | "?" => CpmCmd::Help(arg),
            "exit" | "bye" | "quit" => CpmCmd::Exit,
            "user" => CpmCmd::User,
            _ => CpmCmd::Unknown(verb.to_string()),
        }
    }

    /// `REN` accepts both the CP/M `REN new=old` form and the DOS-style
    /// `REN old new` space form.  The `=` form is unambiguous; the space
    /// form follows DOS order (source first, destination second).
    fn parse_ren(rest: &str) -> CpmCmd {
        if rest.is_empty() {
            return CpmCmd::NeedsArg("REN new=old");
        }
        if let Some((new, old)) = rest.split_once('=') {
            let new = new.trim();
            let old = old.trim();
            if new.is_empty() || old.is_empty() {
                return CpmCmd::NeedsArg("REN new=old");
            }
            return CpmCmd::Ren {
                new: new.to_string(),
                old: old.to_string(),
            };
        }
        let mut toks = rest.split_whitespace();
        match (toks.next(), toks.next()) {
            // Space form is DOS order: OLD then NEW.
            (Some(old), Some(new)) => CpmCmd::Ren {
                new: new.to_string(),
                old: old.to_string(),
            },
            _ => CpmCmd::NeedsArg("REN new=old  (or REN old new)"),
        }
    }

    /// COPY / MOVE share `dst src` / `dst=src` operand parsing.  The
    /// destination is first (CP/M `PIP dest=source` order).
    fn parse_two_operand(rest: &str, is_move: bool) -> CpmCmd {
        let usage: &'static str = if is_move { "MOVE dst src" } else { "COPY dst src" };
        if rest.is_empty() {
            return CpmCmd::NeedsArg(usage);
        }
        let (dst, src) = if let Some((d, s)) = rest.split_once('=') {
            (d.trim().to_string(), s.trim().to_string())
        } else {
            let mut toks = rest.split_whitespace();
            match (toks.next(), toks.next()) {
                (Some(d), Some(s)) => (d.to_string(), s.to_string()),
                _ => return CpmCmd::NeedsArg(usage),
            }
        };
        if dst.is_empty() || src.is_empty() {
            return CpmCmd::NeedsArg(usage);
        }
        if is_move {
            CpmCmd::Move { dst, src }
        } else {
            CpmCmd::Copy { dst, src }
        }
    }

    // ─── Path resolution / jail (pure component logic) ───────

    /// Normalize a path operand into a component vector relative to the
    /// drive-A root, purely from string logic.
    ///
    /// - A leading `/` resolves from the root; otherwise from `cwd`.
    /// - `.` is skipped, `..` pops one component (popping past the root is
    ///   an `Access denied.` error — the jail can never be escaped).
    /// - Every real component is gated by [`validate_filename`], so illegal
    ///   characters, over-length names, leading dots, and `..`-embedding are
    ///   all rejected here, before any disk access.
    ///
    /// `cwd` is `self.transfer_subdir` (a `/`-separated relative path, `""`
    /// at root).  Pure — unit-tested directly.
    pub(in crate::telnet) fn cpm_normalize(
        cwd: &str,
        operand: &str,
    ) -> Result<Vec<String>, &'static str> {
        let mut comps: Vec<String> = if operand.starts_with('/') {
            Vec::new()
        } else {
            cwd.split('/')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        };
        for part in operand.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                if comps.pop().is_none() {
                    return Err("Access denied.");
                }
                continue;
            }
            Self::validate_filename(part)?;
            comps.push(part.to_string());
        }
        Ok(comps)
    }

    /// Split an operand into its directory portion and its leaf (the part
    /// after the last `/`).  The leaf may be empty (a trailing `/` means a
    /// directory operand) or a wildcard pattern.
    fn cpm_split_leaf(operand: &str) -> (&str, &str) {
        match operand.rfind('/') {
            Some(i) => (&operand[..=i], &operand[i + 1..]),
            None => ("", operand),
        }
    }

    /// Resolve a directory operand to a canonicalized absolute path that is
    /// verified to exist inside the jail.  Used by CD and as the parent
    /// resolver for file operands.
    fn cpm_dir_abs(&self, dir_operand: &str) -> Result<PathBuf, &'static str> {
        let comps = Self::cpm_normalize(&self.transfer_subdir, dir_operand)?;
        let cfg = config::get_config();
        let base = std::fs::canonicalize(&cfg.transfer_dir).map_err(|_| "Access denied.")?;
        let mut abs = base.clone();
        for c in &comps {
            abs.push(c);
        }
        let canon = std::fs::canonicalize(&abs).map_err(|_| "No such directory.")?;
        if !canon.starts_with(&base) {
            return Err("Access denied.");
        }
        if !canon.is_dir() {
            return Err("Not a directory.");
        }
        Ok(canon)
    }

    /// Resolve a file operand that must already exist, to a canonicalized
    /// absolute path verified inside the jail and confirmed to be a regular
    /// file.  Symlink-safe (the full path is canonicalized).
    fn cpm_existing_file(&self, operand: &str) -> Result<PathBuf, &'static str> {
        let (dir_part, leaf) = Self::cpm_split_leaf(operand);
        if leaf.is_empty() {
            return Err("Not a file.");
        }
        Self::validate_filename(leaf)?;
        let dir = self.cpm_dir_abs(dir_part)?;
        let path = dir.join(leaf);
        let cfg = config::get_config();
        let base = std::fs::canonicalize(&cfg.transfer_dir).map_err(|_| "Access denied.")?;
        let canon = std::fs::canonicalize(&path).map_err(|_| "File not found.")?;
        if !canon.starts_with(&base) {
            return Err("Access denied.");
        }
        if !canon.is_file() {
            return Err("Not a file.");
        }
        Ok(canon)
    }

    /// Resolve a *destination* file operand into `(parent_dir, filename)`.
    /// The parent directory must exist inside the jail; the filename itself
    /// need not exist (it is being created).  When `src_name` is given and
    /// the operand names an existing directory (or ends with `/`), the
    /// destination becomes that directory plus the source's own filename —
    /// the "COPY into a dir" convenience.
    fn cpm_dest_file(
        &self,
        operand: &str,
        src_name: &str,
    ) -> Result<(PathBuf, String), &'static str> {
        // A trailing slash forces a directory target; otherwise, if the
        // operand resolves to an existing directory, treat it as one too.
        // Both mean "into that directory, keeping the source name".  Resolve
        // once per branch (no double canonicalize).
        if operand.ends_with('/') {
            return Ok((self.cpm_dir_abs(operand)?, src_name.to_string()));
        }
        if let Ok(dir) = self.cpm_dir_abs(operand) {
            return Ok((dir, src_name.to_string()));
        }
        let (dir_part, leaf) = Self::cpm_split_leaf(operand);
        if leaf.is_empty() {
            return Err("Bad destination.");
        }
        Self::validate_filename(leaf)?;
        let dir = self.cpm_dir_abs(dir_part)?;
        Ok((dir, leaf.to_string()))
    }

    // ─── Wildcard glob (pure) ────────────────────────────────

    /// CP/M / DOS wildcard match: `*` matches any run of characters
    /// (including none), `?` matches exactly one character.  Matching is
    /// case-insensitive and applies to the whole filename (we do not split
    /// on the `8.3` dot, because our names are not 8.3).  Pure —
    /// unit-tested directly.
    pub(in crate::telnet) fn cpm_glob_match(pattern: &str, name: &str) -> bool {
        // Classic two-pointer glob with backtracking on `*`.
        let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
        let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
        let (mut pi, mut ni) = (0usize, 0usize);
        let (mut star, mut mark) = (None::<usize>, 0usize);
        while ni < n.len() {
            if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
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
        while pi < p.len() && p[pi] == '*' {
            pi += 1;
        }
        pi == p.len()
    }

    /// True if an operand contains a wildcard metacharacter.
    fn cpm_has_wildcard(s: &str) -> bool {
        s.contains('*') || s.contains('?')
    }

    // ─── Commands: read-only ─────────────────────────────────

    /// `PWD` — print the current drive-A path.
    async fn cpm_pwd(&mut self) -> Result<(), std::io::Error> {
        let path = if self.transfer_subdir.is_empty() {
            "A:/".to_string()
        } else {
            format!("A:/{}", self.transfer_subdir.to_uppercase())
        };
        self.send_line(&format!("  {}", self.amber(&path))).await
    }

    /// `DIR [pattern]` — list the cwd (or a path-qualified / wildcard set).
    async fn cpm_dir(&mut self, pattern: Option<&str>) -> Result<(), std::io::Error> {
        // Resolve the directory to list + the name pattern to match.
        let (dir_operand, name_pat) = match pattern {
            None => (String::new(), "*".to_string()),
            Some(p) => {
                let (dir_part, leaf) = Self::cpm_split_leaf(p);
                let leaf = if leaf.is_empty() { "*" } else { leaf };
                (dir_part.to_string(), leaf.to_string())
            }
        };

        let dir = match self.cpm_dir_abs(&dir_operand) {
            Ok(d) => d,
            Err(e) => return self.cpm_err(e).await,
        };

        let entries = Self::list_transfer_entries_in(&dir).await?;
        let mut lines: Vec<String> = Vec::new();
        let mut files = 0u64;
        let mut dirs = 0u64;
        let mut total_bytes = 0u64;
        for (name, size, is_dir) in &entries {
            if !Self::cpm_glob_match(&name_pat, name) {
                continue;
            }
            let disp = truncate_to_width(&name.to_uppercase(), 24);
            if *is_dir {
                dirs += 1;
                lines.push(format!("  {:<24} {}", disp, self.cyan("<DIR>")));
            } else {
                files += 1;
                total_bytes += *size;
                lines.push(format!("  {:<24} {:>9}", disp, Self::format_file_size(*size)));
            }
        }

        if lines.is_empty() {
            return self.cpm_err("No file").await;
        }
        lines.push(String::new());
        lines.push(format!(
            "  {} file(s), {} dir(s), {}",
            files,
            dirs,
            Self::format_file_size(total_bytes)
        ));
        self.cpm_page_lines(&lines).await
    }

    /// `CD [path]` — change the working directory (jailed).
    async fn cpm_cd(&mut self, path: Option<&str>) -> Result<(), std::io::Error> {
        // Bare CD, or CD / — go to the drive root.
        let operand = match path {
            None | Some("/") => {
                self.transfer_subdir.clear();
                return Ok(());
            }
            Some(p) => p,
        };

        let comps = match Self::cpm_normalize(&self.transfer_subdir, operand) {
            Ok(c) => c,
            Err(e) => return self.cpm_err(e).await,
        };
        let new_subdir = comps.join("/");
        // Apply, then verify on disk (existence + symlink jail).  Revert on
        // failure so a bad CD never leaves us in a phantom directory.
        let prev = std::mem::replace(&mut self.transfer_subdir, new_subdir);
        if self.verify_transfer_path() && self.transfer_path().is_dir() {
            Ok(())
        } else {
            self.transfer_subdir = prev;
            self.cpm_err("No such directory.").await
        }
    }

    /// `STAT [file]` — per-file size (with CP/M 128-byte record count) or a
    /// directory summary plus a disk-space indicator.
    async fn cpm_stat(&mut self, file: Option<&str>) -> Result<(), std::io::Error> {
        if let Some(f) = file {
            let path = match self.cpm_existing_file(f) {
                Ok(p) => p,
                Err(e) => return self.cpm_err(e).await,
            };
            let size = match tokio::fs::metadata(&path).await {
                Ok(m) => m.len(),
                Err(_) => return self.cpm_err("File not found.").await,
            };
            let records = size.div_ceil(128);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_uppercase();
            // Keep the line within the 40-column PETSCII budget: a narrower
            // name column and no record-count suffix there (a big file's
            // "8.0 MB (65536 recs)" tail would otherwise wrap the C64 row).
            let line = if self.terminal_type == TerminalType::Petscii {
                format!(
                    "  {:<20} {}",
                    truncate_to_width(&name, 20),
                    Self::format_file_size(size)
                )
            } else {
                format!(
                    "  {:<24} {} ({} recs)",
                    truncate_to_width(&name, 24),
                    Self::format_file_size(size),
                    records
                )
            };
            self.send_line(&line).await?;
            return Ok(());
        }

        // Summary of the current directory.
        let entries = Self::list_transfer_entries_in(&self.transfer_path()).await?;
        let mut files = 0u64;
        let mut total = 0u64;
        for (_, size, is_dir) in &entries {
            if !is_dir {
                files += 1;
                total += *size;
            }
        }
        self.send_line(&format!(
            "  A: {} file(s), {} used",
            files,
            Self::format_file_size(total)
        ))
        .await?;
        let disk = if Self::is_disk_full() {
            self.red("DISK NEARLY FULL")
        } else {
            self.green("disk space OK")
        };
        self.send_line(&format!("  {}", disk)).await
    }

    // ─── Commands: TYPE / DUMP (paginated viewers) ───────────

    /// `TYPE file` — display a text file, paginated.  Refuses binary files
    /// (NUL bytes or a high proportion of control bytes) so a C64 isn't fed
    /// terminal-hostile garbage, and is bounded by the 8 MB transfer cap.
    async fn cpm_type(&mut self, file: &str) -> Result<(), std::io::Error> {
        if Self::cpm_has_wildcard(file) {
            return self.cpm_err("TYPE takes one file (no wildcards).").await;
        }
        let path = match self.cpm_existing_file(file) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let data = match Self::cpm_read_capped(&path, CPM_VIEW_MAX).await {
            Ok(d) => d,
            Err(msg) => return self.cpm_err(msg).await,
        };
        if Self::looks_binary(&data) {
            return self
                .cpm_err("Not a text file - download it instead.")
                .await;
        }

        let width = if self.terminal_type == TerminalType::Petscii {
            PETSCII_WIDTH - 2
        } else {
            78
        };
        let text = String::from_utf8_lossy(&data);
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let expanded = raw.replace('\t', "    ");
            if expanded.is_empty() {
                lines.push(String::new());
                continue;
            }
            // Wrap long lines to the terminal width.  `chunks` avoids the
            // O(n^2) cost a `drain`-in-loop would incur on a very long line.
            let chars: Vec<char> = expanded.chars().collect();
            for chunk in chars.chunks(width) {
                lines.push(chunk.iter().collect());
            }
        }
        self.cpm_page_lines(&lines).await
    }

    /// `DUMP file` — hex + ASCII dump, paginated (8 bytes/row on PETSCII,
    /// 16 on wider terminals).
    async fn cpm_dump(&mut self, file: &str) -> Result<(), std::io::Error> {
        if Self::cpm_has_wildcard(file) {
            return self.cpm_err("DUMP takes one file (no wildcards).").await;
        }
        let path = match self.cpm_existing_file(file) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let data = match Self::cpm_read_capped(&path, CPM_VIEW_MAX).await {
            Ok(d) => d,
            Err(msg) => return self.cpm_err(msg).await,
        };
        use std::fmt::Write as _;
        // Fit the row to the terminal: on a 40-column PETSCII screen use a
        // tight 8-byte row (5-digit offset, single gaps → 38 cols) so it
        // doesn't wrap; on wider ANSI/ASCII use a roomy 16-byte row.
        let petscii = self.terminal_type == TerminalType::Petscii;
        let per_row = if petscii { 8 } else { 16 };
        let hex_width = per_row * 3; // "XX " per byte, incl. trailing space
        let mut lines: Vec<String> = Vec::new();
        for (row, chunk) in data.chunks(per_row).enumerate() {
            let mut hex = String::new();
            let mut ascii = String::new();
            for b in chunk {
                let _ = write!(hex, "{:02X} ", b);
                ascii.push(if (0x20..0x7F).contains(b) {
                    *b as char
                } else {
                    '.'
                });
            }
            let off = row * per_row;
            // Pad the hex column so the ASCII gutter lines up on short rows.
            if petscii {
                lines.push(format!("{:05X} {:<hex_width$}{}", off, hex, ascii));
            } else {
                lines.push(format!("  {:06X}  {:<hex_width$}{}", off, hex, ascii));
            }
        }
        if lines.is_empty() {
            lines.push("  (empty file)".to_string());
        }
        self.cpm_page_lines(&lines).await
    }

    /// Read a file, refusing anything over `max` bytes.  Callers pass
    /// `MAX_FILE_SIZE` for copy/move (the 8 MB transfer cap) or
    /// `CPM_VIEW_MAX` for the TYPE/DUMP viewers.  Takes no `&self` so the
    /// returned future stays `Send` (a shared `&TelnetSession` held across an
    /// await would not be, since the session isn't `Sync`).
    async fn cpm_read_capped(
        path: &std::path::Path,
        max: usize,
    ) -> Result<Vec<u8>, &'static str> {
        let meta = tokio::fs::metadata(path).await.map_err(|_| "File not found.")?;
        if meta.len() as usize > max {
            return Err(if max >= Self::MAX_FILE_SIZE {
                "File too big (over 8 MB)."
            } else {
                "Too big to view - download it."
            });
        }
        tokio::fs::read(path).await.map_err(|_| "Read failed.")
    }

    /// Heuristic binary sniff: any NUL byte, or more than 30% control bytes
    /// (outside tab/newline/carriage-return), means "not text".
    pub(in crate::telnet) fn looks_binary(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        let mut ctrl = 0usize;
        for &b in data {
            if b == 0 {
                return true;
            }
            let printable = b == b'\t' || b == b'\n' || b == b'\r' || (0x20..=0x7E).contains(&b);
            // High-bit bytes are common in PETSCII / Latin-1 text, so they
            // are not counted as "control" here — only C0 control bytes are.
            if !printable && b < 0x80 {
                ctrl += 1;
            }
        }
        ctrl * 100 / data.len() > 30
    }

    /// Paginate a list of pre-formatted lines with a `--More--` pause.
    /// SPACE (or any non-special key) shows the next screenful, RETURN
    /// advances a single line, Q or ESC stops.
    async fn cpm_page_lines(&mut self, lines: &[String]) -> Result<(), std::io::Error> {
        let full = if self.terminal_type == TerminalType::Petscii {
            20
        } else {
            22
        };
        let mut idx = 0usize;
        let mut step = full;
        while idx < lines.len() {
            let end = (idx + step).min(lines.len());
            for line in &lines[idx..end] {
                self.send_line(line).await?;
            }
            idx = end;
            if idx >= lines.len() {
                break;
            }
            self.send(&format!("  {}", self.dim("--More-- (SPACE, RET, Q)")))
                .await?;
            self.flush().await?;
            let key = self.wait_for_key_returning().await?;
            // Erase the --More-- prompt line before continuing.
            self.send_raw(b"\r").await?;
            let translated = if self.terminal_type == TerminalType::Petscii {
                petscii_to_ascii_byte(key)
            } else {
                key
            };
            if translated == b'q'
                || translated == b'Q'
                || is_esc_key(key, self.terminal_type == TerminalType::Petscii)
            {
                self.send_line("").await?;
                break;
            }
            step = if translated == b'\r' || translated == b'\n' {
                1
            } else {
                full
            };
        }
        Ok(())
    }

    // ─── Commands: directory management ──────────────────────

    /// `MKDIR name` — create a subdirectory relative to the cwd.
    async fn cpm_mkdir(&mut self, name: &str) -> Result<(), std::io::Error> {
        let comps = match Self::cpm_normalize(&self.transfer_subdir, name) {
            Ok(c) => c,
            Err(e) => return self.cpm_err(e).await,
        };
        if comps.is_empty() {
            return self.cpm_err("Bad directory name.").await;
        }
        // `comps` is normalized relative to the drive root, so the parent
        // chain is always addressable from the root ("/" = the root itself).
        let (last, parents) = comps.split_last().unwrap();
        let parent_operand = format!("/{}", parents.join("/"));
        let parent = match self.cpm_dir_abs(&parent_operand) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let target = parent.join(last);
        match tokio::fs::create_dir(&target).await {
            Ok(()) => {
                self.send_line(&format!("  {}", self.green(&format!("Created {}/", last.to_uppercase()))))
                    .await
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.cpm_err("That name already exists.").await
            }
            Err(_) => self.cpm_err("Could not create directory.").await,
        }
    }

    /// `RMDIR name` — remove an **empty** subdirectory.  Refuses a
    /// non-empty directory, the cwd itself, and any ancestor of the cwd.
    async fn cpm_rmdir(&mut self, name: &str) -> Result<(), std::io::Error> {
        let dir = match self.cpm_dir_abs(name) {
            Ok(d) => d,
            Err(e) => return self.cpm_err(e).await,
        };
        // Never remove the current directory or an ancestor of it.
        let cwd = match std::fs::canonicalize(self.transfer_path()) {
            Ok(p) => p,
            Err(_) => return self.cpm_err("Access denied.").await,
        };
        if cwd.starts_with(&dir) {
            return self.cpm_err("Cannot remove the current directory.").await;
        }
        // Refuse the drive root.
        let cfg = config::get_config();
        if let Ok(base) = std::fs::canonicalize(&cfg.transfer_dir)
            && dir == base
        {
            return self.cpm_err("Cannot remove the drive root.").await;
        }
        match tokio::fs::remove_dir(&dir).await {
            Ok(()) => {
                self.send_line(&format!("  {}", self.green("Directory removed.")))
                    .await
            }
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                self.cpm_err("Directory is not empty.").await
            }
            Err(_) => self.cpm_err("Could not remove directory.").await,
        }
    }

    // ─── Commands: erase / rename ────────────────────────────

    /// `ERA file` — erase file(s).  A wildcard pattern erases every match
    /// after a `(Y/N)` confirmation; a directory is refused.
    async fn cpm_era(&mut self, operand: &str) -> Result<(), std::io::Error> {
        let (dir_part, leaf) = Self::cpm_split_leaf(operand);
        if leaf.is_empty() {
            return self.cpm_err("ERA takes a filename.").await;
        }
        let dir = match self.cpm_dir_abs(dir_part) {
            Ok(d) => d,
            Err(e) => return self.cpm_err(e).await,
        };

        if Self::cpm_has_wildcard(leaf) {
            let entries = Self::list_transfer_entries_in(&dir).await?;
            let matches: Vec<String> = entries
                .into_iter()
                .filter(|(name, _, is_dir)| !is_dir && Self::cpm_glob_match(leaf, name))
                .map(|(name, _, _)| name)
                .collect();
            if matches.is_empty() {
                return self.cpm_err("No file").await;
            }
            self.send(&format!(
                "  Erase {} file(s) (Y/N)? ",
                matches.len()
            ))
            .await?;
            self.flush().await?;
            if !self.cpm_confirm().await? {
                return self.send_line(&format!("  {}", self.dim("Cancelled."))).await;
            }
            let mut erased = 0u64;
            for name in &matches {
                if tokio::fs::remove_file(dir.join(name)).await.is_ok() {
                    erased += 1;
                }
            }
            return self
                .send_line(&format!("  {}", self.green(&format!("{} erased.", erased))))
                .await;
        }

        // Single named file.
        let path = match self.cpm_existing_file(operand) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        match tokio::fs::remove_file(&path).await {
            Ok(()) => self.send_line(&format!("  {}", self.green("Erased."))).await,
            Err(_) => self.cpm_err("Could not erase.").await,
        }
    }

    /// `REN new=old` — rename within one directory.  Both operands must be
    /// bare filenames (no path); cross-directory relocation is `MOVE`.
    async fn cpm_ren(&mut self, new: &str, old: &str) -> Result<(), std::io::Error> {
        if new.contains('/') || old.contains('/') {
            return self
                .cpm_err("REN is in-place; use MOVE across dirs.")
                .await;
        }
        if let Err(e) = Self::validate_filename(new) {
            return self.cpm_err(e).await;
        }
        let src = match self.cpm_existing_file(old) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let dst = self.transfer_path().join(new);
        if dst.exists() {
            return self.cpm_err("Target name already exists.").await;
        }
        match tokio::fs::rename(&src, &dst).await {
            Ok(()) => self.send_line(&format!("  {}", self.green("Renamed."))).await,
            Err(_) => self.cpm_err("Could not rename.").await,
        }
    }

    // ─── Commands: copy / move ───────────────────────────────

    /// `COPY dst src` — copy a file (create-new, never clobbers).  Both
    /// operands may be path-qualified; a wildcard source copies each match
    /// into a directory destination.
    async fn cpm_copy(&mut self, dst: &str, src: &str) -> Result<(), std::io::Error> {
        let (src_dir_part, src_leaf) = Self::cpm_split_leaf(src);

        if Self::cpm_has_wildcard(src_leaf) {
            // Wildcard source requires a directory destination.
            let src_dir = match self.cpm_dir_abs(src_dir_part) {
                Ok(d) => d,
                Err(e) => return self.cpm_err(e).await,
            };
            let dst_dir = match self.cpm_dir_abs(dst) {
                Ok(d) => d,
                Err(_) => {
                    return self
                        .cpm_err("Wildcard COPY needs a directory dest.")
                        .await
                }
            };
            let entries = Self::list_transfer_entries_in(&src_dir).await?;
            let matches: Vec<(String, u64)> = entries
                .into_iter()
                .filter(|(name, _, is_dir)| !is_dir && Self::cpm_glob_match(src_leaf, name))
                .map(|(name, size, _)| (name, size))
                .collect();
            if matches.is_empty() {
                return self.cpm_err("No file").await;
            }
            let mut copied = 0u64;
            for (name, _) in &matches {
                let Ok(data) = tokio::fs::read(src_dir.join(name)).await else {
                    continue;
                };
                if Self::save_received_file_sync(&dst_dir.join(name), &data, None, false).is_ok() {
                    copied += 1;
                }
            }
            return self
                .send_line(&format!("  {}", self.green(&format!("{} copied.", copied))))
                .await;
        }

        // Single-file copy.
        let src_path = match self.cpm_existing_file(src) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let src_name = src_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let (dst_dir, dst_name) = match self.cpm_dest_file(dst, &src_name) {
            Ok(v) => v,
            Err(e) => return self.cpm_err(e).await,
        };
        let dst_path = dst_dir.join(&dst_name);
        if dst_path == src_path {
            return self.cpm_err("Source and destination are the same.").await;
        }
        let data = match Self::cpm_read_capped(&src_path, Self::MAX_FILE_SIZE).await {
            Ok(d) => d,
            Err(msg) => return self.cpm_err(msg).await,
        };
        match Self::save_received_file_sync(&dst_path, &data, None, false) {
            Ok(()) => self.send_line(&format!("  {}", self.green("Copied."))).await,
            Err(SaveError::AlreadyExists) => self.cpm_err("Destination exists.").await,
            Err(SaveError::WriteFailed) => self.cpm_err("Copy failed.").await,
        }
    }

    /// `MOVE dst src` — relocate a file across directories via a jailed
    /// rename, falling back to copy-then-erase when rename can't cross a
    /// boundary.  Never clobbers an existing destination.
    async fn cpm_move(&mut self, dst: &str, src: &str) -> Result<(), std::io::Error> {
        if Self::cpm_has_wildcard(src) || Self::cpm_has_wildcard(dst) {
            return self.cpm_err("MOVE takes one source file.").await;
        }
        let src_path = match self.cpm_existing_file(src) {
            Ok(p) => p,
            Err(e) => return self.cpm_err(e).await,
        };
        let src_name = src_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let (dst_dir, dst_name) = match self.cpm_dest_file(dst, &src_name) {
            Ok(v) => v,
            Err(e) => return self.cpm_err(e).await,
        };
        let dst_path = dst_dir.join(&dst_name);
        if dst_path == src_path {
            return self.cpm_err("Source and destination are the same.").await;
        }
        if dst_path.exists() {
            return self.cpm_err("Destination exists.").await;
        }
        // Try a straight rename first.
        if tokio::fs::rename(&src_path, &dst_path).await.is_ok() {
            return self.send_line(&format!("  {}", self.green("Moved."))).await;
        }
        // Fallback: copy (create-new) then erase the original.
        let data = match Self::cpm_read_capped(&src_path, Self::MAX_FILE_SIZE).await {
            Ok(d) => d,
            Err(msg) => return self.cpm_err(msg).await,
        };
        match Self::save_received_file_sync(&dst_path, &data, None, false) {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&src_path).await;
                self.send_line(&format!("  {}", self.green("Moved."))).await
            }
            Err(SaveError::AlreadyExists) => self.cpm_err("Destination exists.").await,
            Err(SaveError::WriteFailed) => self.cpm_err("Move failed.").await,
        }
    }

    /// Read a single Y/N answer for confirmations.  Returns true only on
    /// an explicit `Y`.
    async fn cpm_confirm(&mut self) -> Result<bool, std::io::Error> {
        self.drain_input().await;
        let key = match self.read_byte_filtered().await? {
            Some(b) => b,
            None => return Ok(false),
        };
        let b = if self.terminal_type == TerminalType::Petscii {
            petscii_to_ascii_byte(key)
        } else {
            key
        };
        self.send_line("").await?;
        Ok(b == b'y' || b == b'Y')
    }

    // ─── Help ────────────────────────────────────────────────

    /// `HELP` — paged command reference (fit-tested for 40 cols).  A topic
    /// operand is currently ignored; the whole reference is shown.
    async fn cpm_help(&mut self, _topic: Option<&str>) -> Result<(), std::io::Error> {
        self.show_help_page("CP/M SHELL HELP", Self::cpm_help_lines())
            .await
    }

    /// Static help text for the CP/M shell.  Kept ≤ 40 cols so it fits a
    /// PETSCII screen; registered in the aggregate help-fit test.
    pub(in crate::telnet) fn cpm_help_lines() -> &'static [&'static str] {
        &[
            "  Commands (case-insensitive):",
            "  DIR [pat]   List files (LS)",
            "  TYPE file   Show a text file",
            "  DUMP file   Hex dump a file",
            "  ERA file    Erase files (DEL,RM)",
            "  REN new=old Rename in place",
            "  COPY d s    Copy file (CP,PIP)",
            "  MOVE d s    Move file (MV)",
            "  MKDIR name  New directory (MD)",
            "  RMDIR name  Remove empty dir (RD)",
            "  CD [path]   Change dir (CHDIR)",
            "  PWD         Show current dir",
            "  STAT [file] Space / file size",
            "  HELP        This help (?)",
            "  EXIT        Back to menu (BYE)",
            "",
            "  Paths use / with A: as the",
            "  transfer dir:",
            "    FILE.TXT   file in this dir",
            "    SUB/F.TXT  inside subdir SUB",
            "    /F.TXT     at the A: root",
            "    ../F.TXT   in the parent dir",
            "",
            "  COPY/MOVE take DEST first:",
            "    COPY SUB/ FILE.TXT",
            "    MOVE /DONE/ OLD.DAT",
            "",
            "  Wildcards * and ? work in",
            "  DIR, ERA and COPY source:",
            "    DIR *.TXT    ERA *.BAK",
            "",
            "  All commands are jailed to the",
            "  transfer dir - nothing above",
            "  the A: root can be reached.",
        ]
    }
}
