//! File-transfer UI: the transfer menu, upload/download flows,
//! protocol prompts, received-file save (+ YMODEM metadata), Kermit
//! server mode, and directory operations (list/delete/chdir/mkdir).
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

impl TelnetSession {
    // ─── File Transfer menu ──────────────────────────────────

    pub(in crate::telnet) async fn render_file_transfer(&mut self) -> Result<(), std::io::Error> {
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("FILE TRANSFER")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        let max_dir = if self.terminal_type == TerminalType::Petscii {
            30
        } else {
            60
        };
        let dir_str = truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!("  Dir: {}", self.amber(&dir_str)))
            .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}  Upload a file",
            self.cyan("U")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Download a file",
            self.cyan("D")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Delete a file",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Change directory",
            self.cyan("C")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Make directory",
            self.cyan("M")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  Kermit server mode",
            self.cyan("K")
        ))
        .await?;
        let iac_status = if self.xmodem_iac {
            self.green("ON")
        } else {
            self.red("OFF")
        };
        self.send_line(&format!(
            "  {}  IAC escaping [{}]",
            self.cyan("I"),
            iac_status
        ))
        .await?;
        self.send_line("").await?;
        let footer = self.nav_footer();
        self.send_line(&footer).await?;
        Ok(())
    }

    pub(in crate::telnet) async fn handle_file_transfer_command(
        &mut self,
        input: &str,
    ) -> Result<bool, std::io::Error> {
        match input {
            "u" => {
                if let Err(e) = self.file_transfer_upload().await {
                    // ConnectionAborted means the session should end: either a
                    // deliberate Punter hangup-on-failure (`punter_hangup`) or
                    // the client already dropped (`wait_for_key`/reads surface
                    // EOF as ConnectionAborted).  Propagate so `run()` tears
                    // down and the writer is shut (carrier drop) instead of
                    // writing a doomed "Press any key" to a dead socket.
                    if e.kind() == std::io::ErrorKind::ConnectionAborted {
                        return Err(e);
                    }
                    self.show_error(&format!("Transfer error: {}", e))
                        .await?;
                }
            }
            "d" => {
                if let Err(e) = self.file_transfer_download().await {
                    if e.kind() == std::io::ErrorKind::ConnectionAborted {
                        return Err(e);
                    }
                    self.show_error(&format!("Transfer error: {}", e))
                        .await?;
                }
            }
            "x" => {
                if let Err(e) = self.file_transfer_delete().await {
                    self.show_error(&format!("Error: {}", e)).await?;
                }
            }
            "c" => {
                self.file_transfer_chdir().await?;
            }
            "m" => {
                self.file_transfer_mkdir().await?;
            }
            "k" => {
                match self.file_transfer_kermit_server().await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        // Kermit idle-timeout: propagate up so the
                        // session ends and the peer's TCP socket gets
                        // an immediate EOF on top of the E-packet we
                        // just sent.  Without this the peer's next
                        // `remote ...` lands on the file-transfer
                        // menu and surfaces as "too many retries".
                        return Err(e);
                    }
                    Err(e) => {
                        self.show_error(&format!("Server error: {}", e)).await?;
                    }
                }
            }
            "i" => {
                self.xmodem_iac = !self.xmodem_iac;
            }
            "q" => {
                self.current_menu = Menu::Main;
            }
            "h" => {
                self.show_help_page("FILE TRANSFER HELP", Self::file_transfer_menu_help_lines())
                    .await?;
            }
            "r" => {} // Refresh — just re-render
            _ => {
                self.show_error("Press U, D, X, C, M, K, I, R, Q, or H.")
                    .await?;
            }
        }
        Ok(true)
    }

    pub(in crate::telnet) fn transfer_dir_display(&self) -> String {
        let cfg = config::get_config();
        if self.transfer_subdir.is_empty() {
            format!("{}/", cfg.transfer_dir)
        } else {
            format!("{}/{}/", cfg.transfer_dir, self.transfer_subdir)
        }
    }

    pub(in crate::telnet) fn transfer_path(&self) -> std::path::PathBuf {
        let cfg = config::get_config();
        let mut p = std::path::PathBuf::from(&cfg.transfer_dir);
        if !self.transfer_subdir.is_empty() {
            p.push(&self.transfer_subdir);
        }
        p
    }

    /// Verify that the current transfer_subdir resolves to a path inside the
    /// transfer base directory. Resets to root if it escapes (e.g. via symlink).
    pub(in crate::telnet) fn verify_transfer_path(&mut self) -> bool {
        let cfg = config::get_config();
        let base = match std::fs::canonicalize(&cfg.transfer_dir) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let full = match std::fs::canonicalize(self.transfer_path()) {
            Ok(p) => p,
            Err(_) => {
                self.transfer_subdir.clear();
                return false;
            }
        };
        if full.starts_with(&base) {
            true
        } else {
            self.transfer_subdir.clear();
            false
        }
    }

    pub(in crate::telnet) async fn ensure_transfer_dir(&mut self) -> Result<(), std::io::Error> {
        tokio::fs::create_dir_all(self.transfer_path()).await
    }

    /// Atomic write of a freshly received file to the transfer dir,
    /// shared by every batch-receive save site (ZMODEM autostart,
    /// ZMODEM/Kermit menu-initiated upload's per-batch-file path,
    /// Kermit server-mode dispatch).  `create_new` closes the TOCTOU
    /// window between an `exists()` check and the actual write that a
    /// plain `std::fs::write` would leave open; `tokio::fs` keeps the
    /// 8 MB cap from blocking the executor.
    ///
    /// Returns `SaveError::AlreadyExists` if a file with the same name
    /// is already present (caller decides whether that's "skip" or a
    /// fatal upload error), or `SaveError::WriteFailed` for any other
    /// I/O failure.
    pub(in crate::telnet) async fn save_received_file(
        path: &std::path::Path,
        data: &[u8],
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
    ) -> Result<(), SaveError> {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                if file.write_all(data).await.is_err() {
                    return Err(SaveError::WriteFailed);
                }
                if file.flush().await.is_err() {
                    return Err(SaveError::WriteFailed);
                }
                drop(file);
                Self::apply_ymodem_meta(path, meta);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SaveError::AlreadyExists)
            }
            Err(_) => Err(SaveError::WriteFailed),
        }
    }

    /// Sync sibling of `save_received_file` for callers that can't
    /// `.await` (e.g. the Kermit server's on-file callback, which runs
    /// inside a non-async closure).  Same `SaveError` discrimination
    /// as the async sibling — only the I/O backend differs.  At the
    /// file sizes we deal with (≤8 MB) the blocking write is sub-
    /// millisecond on SSD and a few ms on spinning disk; briefly
    /// stalling the runtime is preferable to plumbing async closures
    /// through `kermit_server`'s generic boundary.
    ///
    /// `replace_existing=true` is the resume case: the caller has
    /// already merged the on-disk partial bytes into `data`, so we
    /// must atomically replace whatever's at `path` with the merged
    /// full file.  Done via tmp-file + rename so a process death
    /// mid-write leaves the original partial intact rather than
    /// corrupting both versions.  `false` keeps the create-new
    /// "refuse to clobber" semantics that every other save site
    /// uses.
    pub(crate) fn save_received_file_sync(
        path: &std::path::Path,
        data: &[u8],
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
        replace_existing: bool,
    ) -> Result<(), SaveError> {
        use std::io::Write;
        if replace_existing {
            // Resume: write to <name>.kermit-resume.tmp, fsync,
            // rename over the partial.  POSIX rename is atomic
            // within a filesystem; on failure we leave .tmp behind
            // but the original partial is untouched.
            let mut tmp_path = path.to_path_buf();
            let mut tmp_name = tmp_path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            tmp_name.push(".kermit-resume.tmp");
            tmp_path.set_file_name(tmp_name);
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
            {
                Ok(f) => f,
                Err(_) => return Err(SaveError::WriteFailed),
            };
            if file.write_all(data).is_err() || file.flush().is_err() {
                drop(file);
                let _ = std::fs::remove_file(&tmp_path);
                return Err(SaveError::WriteFailed);
            }
            drop(file);
            if std::fs::rename(&tmp_path, path).is_err() {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(SaveError::WriteFailed);
            }
            Self::apply_ymodem_meta(path, meta);
            return Ok(());
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                if file.write_all(data).is_err() {
                    return Err(SaveError::WriteFailed);
                }
                if file.flush().is_err() {
                    return Err(SaveError::WriteFailed);
                }
                drop(file);
                Self::apply_ymodem_meta(path, meta);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SaveError::AlreadyExists)
            }
            Err(_) => Err(SaveError::WriteFailed),
        }
    }

    /// Apply YMODEM block-0 metadata to a freshly saved file.  Both
    /// modtime and mode are best-effort — failures are ignored because
    /// they don't affect data integrity.  Mode is masked to `0o777` so
    /// a misbehaving sender can't set setuid/setgid/sticky bits on our
    /// saved files; mode application is a no-op on non-Unix platforms.
    /// Sync std::fs calls are deliberate — these are microsecond-level
    /// operations that run once per saved file, so the cost of routing
    /// through `spawn_blocking` would exceed the operations themselves.
    pub(in crate::telnet) fn apply_ymodem_meta(
        path: &std::path::Path,
        meta: Option<&crate::xmodem::YmodemReceiveMeta>,
    ) {
        let Some(m) = meta else { return };
        if let Some(secs) = m.modtime
            && let Ok(file) = std::fs::OpenOptions::new().write(true).open(path)
        {
            let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            let _ = file.set_modified(when);
        }
        #[cfg(unix)]
        if let Some(mode) = m.mode {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode & 0o777);
            let _ = std::fs::set_permissions(path, perms);
        }
    }

    pub(crate) fn validate_filename(name: &str) -> Result<(), &'static str> {
        if name.is_empty() {
            return Err("Filename cannot be empty");
        }
        if name.len() > Self::MAX_FILENAME_LEN {
            return Err("Filename too long (max 64 chars)");
        }
        if name.starts_with('.') {
            return Err("Filename cannot start with a dot");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            return Err("Only letters, numbers, dots, hyphens, underscores");
        }
        if !name.chars().any(|c| c.is_ascii_alphanumeric()) {
            return Err("Filename must contain a letter or number");
        }
        if name.contains("..") {
            return Err("Invalid filename");
        }
        Ok(())
    }

    pub(in crate::telnet) async fn list_transfer_entries_in(
        path: &std::path::Path,
    ) -> Result<Vec<(String, u64, bool)>, std::io::Error> {
        let mut dir = match tokio::fs::read_dir(&path).await {
            Ok(d) => d,
            Err(_) => return Ok(Vec::new()),
        };
        let mut entries: Vec<(String, u64, bool)> = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let Some(name) = entry.file_name().to_str() {
                if metadata.is_dir() {
                    entries.push((name.to_string(), 0, true));
                } else if metadata.is_file() {
                    entries.push((name.to_string(), metadata.len(), false));
                }
            }
        }
        entries.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        Ok(entries)
    }

    pub(in crate::telnet) fn format_file_size(size: u64) -> String {
        if size < 1024 {
            format!("{} B", size)
        } else if size < 1024 * 1024 {
            format!("{:.1} KB", size as f64 / 1024.0)
        } else {
            format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
        }
    }

    /// Returns true if disk usage exceeds 90%.
    pub(in crate::telnet) fn is_disk_full() -> bool {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::mem::MaybeUninit;
            let cfg = config::get_config();
            let dir = if std::path::Path::new(&cfg.transfer_dir).exists() {
                cfg.transfer_dir.clone()
            } else {
                ".".to_string()
            };
            // "." never contains a nul byte, so the fallback CString is
            // always constructable.
            let path = CString::new(dir.as_str())
                .unwrap_or_else(|_| c".".to_owned());
            let mut stat = MaybeUninit::<libc::statvfs>::uninit();
            let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
            if rc != 0 {
                return true;
            }
            let stat = unsafe { stat.assume_init() };
            // `f_frsize` / `f_blocks` / `f_bavail` are u64 on Linux but
            // u32 on macOS/BSD — cast all three to u64 explicitly so
            // the multiplication is portable across the Unix targets
            // our release workflow builds (Linux x86_64 + macOS aarch64).
            // The casts are no-ops on Linux; clippy flags them because
            // it only sees the host target.
            #[allow(clippy::unnecessary_cast)]
            let frsize = stat.f_frsize as u64;
            #[allow(clippy::unnecessary_cast)]
            let total = stat.f_blocks as u64 * frsize;
            #[allow(clippy::unnecessary_cast)]
            let avail = stat.f_bavail as u64 * frsize;
            if total == 0 || avail >= total {
                return total == 0;
            }
            let used_pct = 100 - (avail * 100 / total);
            used_pct > 90
        }
        #[cfg(windows)]
        {
            use std::ffi::OsStr;
            use std::os::windows::ffi::OsStrExt;

            #[repr(C)]
            #[allow(non_snake_case)]
            struct ULARGE_INTEGER {
                QuadPart: u64,
            }

            unsafe extern "system" {
                fn GetDiskFreeSpaceExW(
                    lpDirectoryName: *const u16,
                    lpFreeBytesAvailableToCaller: *mut ULARGE_INTEGER,
                    lpTotalNumberOfBytes: *mut ULARGE_INTEGER,
                    lpTotalNumberOfFreeBytes: *mut ULARGE_INTEGER,
                ) -> i32;
            }

            let cfg = config::get_config();
            let dir = if std::path::Path::new(&cfg.transfer_dir).exists() {
                cfg.transfer_dir.clone()
            } else {
                ".".to_string()
            };
            let wide: Vec<u16> = OsStr::new(&dir).encode_wide().chain(std::iter::once(0)).collect();
            let mut avail = ULARGE_INTEGER { QuadPart: 0 };
            let mut total = ULARGE_INTEGER { QuadPart: 0 };
            let mut _free = ULARGE_INTEGER { QuadPart: 0 };
            let rc = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut _free) };
            if rc == 0 || total.QuadPart == 0 {
                return total.QuadPart == 0;
            }
            let used_pct = 100 - (avail.QuadPart * 100 / total.QuadPart);
            used_pct > 90
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    // ─── UPLOAD ─────────────────────────────────────────────

    /// Prompt the user to pick the upload protocol on its own screen.
    /// Returns `None` if the user pressed ESC / PETSCII `<-` to cancel
    /// back to the file-transfer menu.  Parallel to
    /// [`Self::prompt_download_protocol`] — same screen layout,
    /// navigation keys, and petscii/ANSI handling.
    pub(in crate::telnet) async fn prompt_upload_protocol(
        &mut self,
    ) -> Result<Option<UploadProtocol>, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let esc_label = if is_petscii { "<-" } else { "ESC" };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("SELECT UPLOAD PROTOCOL")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Keep each line <= 39 columns so it doesn't wrap on a 40-column
        // PETSCII (C64) screen.
        self.send_line(&format!(
            "  {}  XMODEM/YMODEM  128/1K, auto",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  ZMODEM         1K, autostart",
            self.cyan("Z")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  KERMIT         any flavor, auto",
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  PUNTER         C1 CCGMS/Novaterm",
            self.cyan("P")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Pick one, or {} to cancel: ",
            self.cyan(esc_label)
        ))
        .await?;
        self.flush().await?;

        loop {
            let b = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };
            if is_esc_key(b, is_petscii) {
                self.send_line("").await?;
                return Ok(None);
            }
            let ch = if is_petscii {
                (petscii_to_ascii_byte(b) as char).to_ascii_lowercase()
            } else {
                (b as char).to_ascii_lowercase()
            };
            // Accept 'Y' as a synonym for 'X' so a user thinking
            // "YMODEM" doesn't have to hunt for the right key — the
            // XMODEM/YMODEM receive path handles both.
            let chosen = match ch {
                'x' | 'y' => Some(UploadProtocol::XmodemYmodem),
                'z' => Some(UploadProtocol::Zmodem),
                'k' => Some(UploadProtocol::Kermit),
                'p' => Some(UploadProtocol::Punter),
                _ => None,
            };
            if let Some(p) = chosen {
                self.send_raw(&[b]).await?;
                self.send_line("").await?;
                self.flush().await?;
                return Ok(Some(p));
            }
            // Invalid key — stay at the prompt.
        }
    }

    pub(in crate::telnet) async fn file_transfer_upload(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        if Self::is_disk_full() {
            self.show_error("Disk space is low. Uploads disabled.")
                .await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("UPLOAD FILE")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let p = format!("  {} ", self.cyan("Filename:"));
        self.send(&p).await?;
        self.flush().await?;

        let filename = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if let Err(msg) = Self::validate_filename(&filename) {
            self.show_error(msg).await?;
            return Ok(());
        }

        let filepath = self.transfer_path().join(&filename);

        // Detect duplicates up-front so the user doesn't sit through a
        // whole transfer only to have the save-step fail.  Prompt to
        // overwrite; if declined, cancel cleanly.
        let overwrite = if tokio::fs::try_exists(&filepath).await.unwrap_or(false) {
            self.send_line("").await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&format!("File '{}' already exists.", filename))
            ))
            .await?;
            self.send(&format!(
                "  {} ",
                self.cyan("Overwrite? (Y/N):")
            ))
            .await?;
            self.flush().await?;
            self.drain_input().await;
            let answer = match self.read_byte_filtered().await? {
                Some(b) => {
                    if self.terminal_type == TerminalType::Petscii {
                        petscii_to_ascii_byte(b)
                    } else {
                        b
                    }
                }
                None => return Ok(()),
            };
            self.send_line("").await?;
            if answer != b'y' && answer != b'Y' {
                return Ok(());
            }
            true
        } else {
            false
        };

        // Ask the user which protocol their sender will use.  Putting
        // this on its own screen after the filename + overwrite prompts
        // mirrors the download flow (file → protocol → transfer) and
        // gives the user as long as they need to browse menus on their
        // terminal before committing to the transfer window.  ESC /
        // PETSCII `<-` at the protocol prompt cancels cleanly.
        let protocol = match self.prompt_upload_protocol().await? {
            Some(p) => p,
            None => return Ok(()),
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Ready to receive: {}",
            self.amber(&filename)
        ))
        .await?;
        self.send_line(&format!(
            "  Max file size: {} MB",
            Self::MAX_FILE_SIZE / (1024 * 1024)
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(match protocol {
                UploadProtocol::XmodemYmodem =>
                    "Start XMODEM/YMODEM send from your terminal now.",
                UploadProtocol::Zmodem =>
                    "Start ZMODEM send from your terminal now.",
                UploadProtocol::Kermit =>
                    "Start KERMIT send from your terminal now.",
                UploadProtocol::Punter =>
                    "Start PUNTER send from your terminal now.",
            })
        ))
        .await?;
        // Make it explicit that the action happens on the user's side.
        // For ExtraPutty it's File Transfer → Zmodem → Send; other
        // terminals have similar menu items.  Users who know the drill
        // can ignore this — it's here for the first-timer path.
        if matches!(protocol, UploadProtocol::Zmodem) {
            self.send_line(
                "  (ExtraPutty: File Transfer > Zmodem > Send. Other clients vary.)",
            )
            .await?;
        }
        let neg_timeout = {
            let cfg = config::get_config();
            match protocol {
                UploadProtocol::Zmodem => cfg.zmodem_negotiation_timeout,
                UploadProtocol::Kermit => cfg.kermit_negotiation_timeout,
                UploadProtocol::Punter => cfg.punter_negotiation_timeout,
                UploadProtocol::XmodemYmodem => cfg.xmodem_negotiation_timeout,
            }
        };
        self.send_line(&format!("  Start transfer within {} seconds.", neg_timeout))
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!("  {} to cancel", self.cyan(esc_label)))
            .await?;
        self.send_line("").await?;
        self.flush().await?;

        if config::get_config().verbose {
            glog!("Upload: IAC escaping={} protocol={:?}", self.xmodem_iac, protocol);
        }
        // See the download path: Punter's silent cancel can leave stale bytes
        // in the pipe, so drain with a longer quiet gap before receiving.
        if matches!(protocol, UploadProtocol::Punter) {
            self.drain_input_until_quiet(250, Some(2000)).await;
        } else {
            self.drain_input().await;
        }

        let verbose = config::get_config().verbose;
        let start = std::time::Instant::now();
        let mut writer_guard = self.writer.lock().await;
        // Normalize both receive paths to a Vec of (sender-proposed
        // filename, data).  XMODEM/YMODEM never carries a filename in
        // the protocol, so we mark it as None and the user-entered
        // name wins.  ZMODEM carries a filename per file; we keep it
        // so batches can save files 2..N under their sender names.
        // The third tuple slot carries optional YMODEM metadata
        // (modtime/mode/sno) parsed from block 0; ZMODEM doesn't surface
        // file attributes through this path so its entries are always
        // `None`.  The save-side applies modtime + mode after writing.
        type Received = Vec<(Option<String>, Vec<u8>, Option<crate::xmodem::YmodemReceiveMeta>)>;
        // Decide callback for the ZMODEM receiver.  The first file
        // (idx 0) is always accepted — the user typed a destination
        // filename in the upload prompt, so they want this one saved
        // regardless of what the sender called it.  Later files in a
        // batch use the sender's name, which we sanitize through the
        // same `validate_filename` rules as user input and reject with
        // ZSKIP if they fail or collide with an existing file.  The
        // path-existence check is a sync std::fs call — fast, no
        // runtime-blocking concern.
        let transfer_path = self.transfer_path();
        let decide = |idx: usize,
                      sender_name: &str,
                      _size: Option<u64>|
         -> bool {
            if idx == 0 {
                return true;
            }
            if Self::validate_filename(sender_name).is_err() {
                return false;
            }
            !transfer_path.join(sender_name).exists()
        };
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        // Captured by the Kermit branch's mapping closure when the
        // peer's flavor is detected.  Surfaced in the post-transfer
        // summary so the user sees who they talked to.
        let mut kermit_flavor: Option<String> = None;
        let result: Result<Received, String> = match protocol {
            UploadProtocol::Zmodem => crate::zmodem::zmodem_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                verbose,
                decide,
            )
            .await
            .map(|rxs| {
                rxs.into_iter()
                    .map(|rx| {
                        // ZFILE info per Forsberg §11 carries length / mtime
                        // / mode — feed them into apply_ymodem_meta so the
                        // saved file gets the sender's mtime + permissions
                        // (matching YMODEM and Kermit behavior).
                        let meta = (rx.modtime.is_some() || rx.mode.is_some())
                            .then_some(crate::xmodem::YmodemReceiveMeta {
                                size: None,
                                modtime: rx.modtime,
                                mode: rx.mode,
                            });
                        (Some(rx.filename), rx.data, meta)
                    })
                    .collect()
            }),
            UploadProtocol::XmodemYmodem => crate::xmodem::xmodem_receive_batch(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
            )
            .await
            // A YMODEM batch yields multiple files.  The first keeps the
            // user-entered name (matching plain XMODEM / ZMODEM / Kermit); files
            // 2..N take the sender's block-0 filename (the save path sanitizes
            // it against path traversal, as it does for ZMODEM/Kermit names).
            .map(|files| {
                files
                    .into_iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let name = if i == 0 { None } else { f.filename };
                        (name, f.data, f.meta)
                    })
                    .collect()
            }),
            UploadProtocol::Kermit => crate::kermit::kermit_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
            )
            .await
            .map(|rxs| {
                // Capture flavor (per-session, identical across files
                // in a batch).
                kermit_flavor = rxs.first().map(|r| r.flavor.display());
                // Map KermitReceive list to (Option<filename>, data, None).
                // First file gets None for filename so user-entered name
                // wins (matches XMODEM/YMODEM behavior); subsequent files
                // in the batch use the sender's name like ZMODEM does.
                rxs.into_iter()
                    .enumerate()
                    .map(|(i, rx)| {
                        let name = if i == 0 { None } else { Some(rx.filename) };
                        let meta = crate::xmodem::YmodemReceiveMeta {
                            size: rx.declared_size,
                            modtime: rx.modtime,
                            mode: rx.mode,
                        };
                        (name, rx.data, Some(meta))
                    })
                    .collect()
            }),
            UploadProtocol::Punter => crate::punter::punter_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
            )
            .await
            // C1 carries no filename, so the user-entered name normally
            // wins (matching XMODEM/YMODEM).  Novaterm preserves the
            // declared PRG/SEQ/USR type via the CBM directory entry; on
            // Linux we don't have that, so we append the matching
            // extension when the user's filename has none — the same
            // suffix `PunterFileType::autodetect` will read on the way
            // back out.  Anything the user typed with an explicit
            // extension is honored verbatim, and `Unknown` skips the
            // suffix entirely.
            .map(|(data, file_type)| {
                let has_extension = filename
                    .find('.')
                    .map(|i| i > 0)
                    .unwrap_or(false);
                let chosen_name = match file_type.extension() {
                    Some(ext) if !has_extension => {
                        Some(format!("{}.{}", filename, ext))
                    }
                    _ => None,
                };
                vec![(chosen_name, data, None)]
            }),
        };
        drop(writer_guard);
        let elapsed = start.elapsed();

        let uploads = match result {
            Ok(v) => v,
            Err(e) => {
                self.post_transfer_settle().await;
                // Option 4: with no in-band abort, a Punter give-up otherwise
                // strands the C64.  Drop carrier instead of waiting on a
                // keypress the hung peer will never send — but only on a
                // genuine give-up, NOT a user-initiated cancel (ESC →
                // "Transfer cancelled"), which must return to the menu.
                if matches!(protocol, UploadProtocol::Punter)
                    && config::get_config().punter_hangup_on_failure
                    && !e.contains("cancelled")
                {
                    self.send_line(&format!(
                        "  {}",
                        self.red(&format!("Transfer failed: {}", e))
                    ))
                    .await?;
                    return self.punter_hangup().await;
                }
                self.show_error(&format!("Transfer failed: {}", e))
                    .await?;
                return Ok(());
            }
        };

        // Save each file.  The first file goes to the user-entered
        // path with the user-chosen overwrite behavior.  Any additional
        // files (ZMODEM batch mode per Forsberg §4) go to the sender's
        // own filename after the same `validate_filename` sanitation
        // we apply to user input — and if the name collides with an
        // existing file we skip rather than clobber.  Batch files
        // share the transfer-complete window with the first file; we
        // don't prompt per-file.
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();

        for (idx, (sender_name, data, ymeta)) in uploads.iter().enumerate() {
            if idx == 0 {
                // First file: user-entered filename, honor overwrite.
                // A codec may refine the name — Punter appends the
                // .prg/.seq extension matching the declared CBM type when
                // the user's filename had none (the same suffix
                // `PunterFileType::autodetect` reads on the way back out).
                // The user's overwrite choice for the base name carries to
                // the suffixed name; a late collision still surfaces via
                // create_new below.
                let (save_name, save_path) = match sender_name {
                    Some(n) if Self::validate_filename(n).is_ok() => {
                        (n.clone(), self.transfer_path().join(n))
                    }
                    _ => (filename.clone(), filepath.clone()),
                };
                let mut opts = tokio::fs::OpenOptions::new();
                opts.write(true);
                if overwrite {
                    opts.create(true).truncate(true);
                } else {
                    opts.create_new(true);
                }
                match opts.open(&save_path).await {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(data).await {
                            self.post_transfer_settle().await;
                            self.show_error(&format!("Failed to save: {}", e))
                                .await?;
                            return Ok(());
                        }
                        let _ = file.flush().await;
                        drop(file);
                        Self::apply_ymodem_meta(&save_path, ymeta.as_ref());
                        saved.push((save_name, data.len()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        self.post_transfer_settle().await;
                        self.show_error("File already exists.").await?;
                        return Ok(());
                    }
                    Err(e) => {
                        self.post_transfer_settle().await;
                        self.show_error(&format!("Failed to save: {}", e))
                            .await?;
                        return Ok(());
                    }
                }
            } else {
                // Batch file 2..N: save under sender's name.  ZMODEM, Kermit,
                // and a YMODEM batch (`sb file1 file2 …`) all produce these, so
                // `sender_name` is Some here.  Routes through the same atomic
                // save_received_file helper as the autostart and Kermit-server
                // batch paths so the create_new + tokio::fs guarantees stay
                // symmetric.
                let name = match sender_name {
                    Some(n) => n.clone(),
                    // A YMODEM batch file whose block-0 name wasn't valid UTF-8
                    // arrives nameless — save it under a generated name rather
                    // than silently dropping it (ZMODEM/Kermit always name theirs).
                    None => format!("ymodem_file_{}", idx + 1),
                };
                if Self::validate_filename(&name).is_err() {
                    // Sanitize the sender-supplied name before it reaches the
                    // terminal (a rejected name can carry ANSI escapes).
                    let safe = crate::aichat::sanitize_for_terminal(&name);
                    skipped.push((safe, "invalid filename"));
                    continue;
                }
                let batch_path = self.transfer_path().join(&name);
                match Self::save_received_file(&batch_path, data, ymeta.as_ref()).await {
                    Ok(()) => saved.push((name, data.len())),
                    Err(SaveError::AlreadyExists) => {
                        skipped.push((name, "already exists"));
                    }
                    Err(SaveError::WriteFailed) => {
                        skipped.push((name, "write failed"));
                    }
                }
            }
        }

        self.post_transfer_settle().await;

        // Transfer-complete summary.  Preserve the classic single-file
        // "N bytes, M blocks, T seconds" format when exactly one file
        // was transferred (by far the common case); expand to a
        // per-file list only when we actually saw a batch.
        self.send_line("").await?;
        if uploads.len() == 1 {
            let bytes = saved.first().map(|(_, n)| *n).unwrap_or(0);
            let blocks = bytes.div_ceil(crate::xmodem::XMODEM_BLOCK_SIZE);
            self.send_line(&format!(
                "  {}",
                self.green("Upload complete!")
            ))
            .await?;
            self.send_line(&format!(
                "  {} bytes, {} blocks, {:.1}s",
                bytes,
                blocks,
                elapsed.as_secs_f64()
            ))
            .await?;
        } else {
            self.send_line(&format!(
                "  {}",
                self.green(&format!(
                    "Upload complete: {} saved, {} skipped, {:.1}s",
                    saved.len(),
                    skipped.len(),
                    elapsed.as_secs_f64()
                ))
            ))
            .await?;
            for (name, bytes) in &saved {
                self.send_line(&format!(
                    "  {} {} ({} bytes)",
                    self.green("*"),
                    name,
                    bytes
                ))
                .await?;
            }
            for (name, reason) in &skipped {
                self.send_line(&format!(
                    "  {} {} ({})",
                    self.yellow("-"),
                    name,
                    reason
                ))
                .await?;
            }
        }
        // Surface detected Kermit flavor (auto-classified from the
        // peer's Send-Init / peer_id) so users see whom they talked to.
        if let Some(flavor) = &kermit_flavor {
            self.send_line(&format!("  {} {}", self.dim("Peer:"), flavor))
                .await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── DOWNLOAD ───────────────────────────────────────────

    pub(in crate::telnet) async fn file_transfer_download(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;
        let mut page: usize = 0;

        loop {
            let files = Self::list_transfer_entries_in(&self.transfer_path())
                .await?
                .into_iter()
                .filter(|(_, _, is_dir)| !is_dir)
                .map(|(name, size, _)| (name, size))
                .collect::<Vec<_>>();

            if files.is_empty() {
                self.show_error("No files available.").await?;
                return Ok(());
            }

            let total_pages = files.len().div_ceil(Self::TRANSFER_PAGE_SIZE);
            if page >= total_pages {
                page = total_pages - 1;
            }
            let offset = page * Self::TRANSFER_PAGE_SIZE;
            let end = (offset + Self::TRANSFER_PAGE_SIZE).min(files.len());
            let page_files = &files[offset..end];

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("DOWNLOAD FILE")
            ))
            .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "   {} {:<22} {}",
                self.cyan("#."),
                "Filename",
                "Size"
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&"-".repeat(36))
            ))
            .await?;

            for (i, (name, size)) in page_files.iter().enumerate() {
                let num = i + 1;
                let display_name = if name.chars().count() > 22 {
                    let truncated: String = name.chars().take(19).collect();
                    format!("{}...", truncated)
                } else {
                    name.clone()
                };
                let size_display = Self::format_file_size(*size);
                self.send_line(&format!(
                    "  {:>2}. {:<22} {}",
                    num, display_name, size_display
                ))
                .await?;
            }

            self.send_line("").await?;
            self.send_line(&format!(
                "  Page {} of {}",
                page + 1,
                total_pages
            ))
            .await?;
            self.send_line("").await?;

            let mut nav = Vec::new();
            if page > 0 {
                nav.push(self.action_prompt("P", "Prev"));
            }
            if page + 1 < total_pages {
                nav.push(self.action_prompt("N", "Next"));
            }
            nav.push(self.action_prompt("Q", "Back"));
            nav.push(self.action_prompt("H", "Help"));
            let esc_label = match self.terminal_type {
                TerminalType::Petscii => "<-",
                _ => "ESC",
            };
            nav.push(self.action_prompt(esc_label, "Main"));
            self.send_line(&format!("  {}", nav.join(" | ")))
                .await?;
            self.send_line("").await?;
            self.send(&format!("  {} ", self.cyan("Select #:")))
                .await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "p" => {
                    page = page.saturating_sub(1);
                }
                "n" => {
                    if page + 1 < total_pages {
                        page += 1;
                    }
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("DOWNLOAD HELP", Self::download_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if num >= 1 && num <= page_files.len() {
                            let (ref filename, file_size) = page_files[num - 1];
                            self.initiate_download(filename, file_size).await?;
                        } else {
                            self.show_error("Invalid selection.").await?;
                        }
                    } else {
                        self.show_error("Enter a number, P, N, Q, or H.")
                            .await?;
                    }
                }
            }
        }
    }

    /// Prompt the user for which XMODEM-family protocol to use for this
    /// download.  Shows the file being downloaded (name + size) so the user
    /// can confirm they picked the right one before starting.  Returns `None`
    /// if the user presses ESC to cancel.
    pub(in crate::telnet) async fn prompt_download_protocol(
        &mut self,
        filename: &str,
        file_size: u64,
    ) -> Result<Option<DownloadProtocol>, std::io::Error> {
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let esc_label = if is_petscii { "<-" } else { "ESC" };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("SELECT PROTOCOL")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Show what's being downloaded so the user can verify they picked the
        // right file before choosing a protocol.
        let max_name = if is_petscii { 31 } else { 60 };
        self.send_line(&format!(
            "  File: {}",
            self.amber(&truncate_to_width(filename, max_name))
        ))
        .await?;
        self.send_line(&format!("  Size: {} bytes", file_size))
            .await?;
        self.send_line("").await?;
        // Keep each line <= 39 columns so it doesn't wrap on a 40-column
        // PETSCII (C64) screen.
        self.send_line(&format!(
            "  {}  XMODEM     128-byte blocks",
            self.cyan("X")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  XMODEM-1K  1024-byte blocks",
            self.cyan("1")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  YMODEM     name+size hdr, 1K",
            self.cyan("Y")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  ZMODEM     autostart, 1K",
            self.cyan("Z")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  KERMIT     any flavor, auto",
            self.cyan("K")
        ))
        .await?;
        self.send_line(&format!(
            "  {}  PUNTER     C1 CCGMS/Novaterm",
            self.cyan("P")
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!(
            "  Pick one, or {} to cancel: ",
            self.cyan(esc_label)
        ))
        .await?;
        self.flush().await?;

        loop {
            let b = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };
            if is_esc_key(b, is_petscii) {
                self.send_line("").await?;
                return Ok(None);
            }
            let ch = if is_petscii {
                (petscii_to_ascii_byte(b) as char).to_ascii_lowercase()
            } else {
                (b as char).to_ascii_lowercase()
            };
            let chosen = match ch {
                'x' => Some(DownloadProtocol::Xmodem),
                '1' => Some(DownloadProtocol::Xmodem1k),
                'y' => Some(DownloadProtocol::Ymodem),
                'z' => Some(DownloadProtocol::Zmodem),
                'k' => Some(DownloadProtocol::Kermit),
                'p' => Some(DownloadProtocol::Punter),
                _ => None,
            };
            if let Some(p) = chosen {
                self.send_raw(&[b]).await?;
                self.send_line("").await?;
                self.flush().await?;
                return Ok(Some(p));
            }
            // Invalid key — stay at the prompt.
        }
    }

    pub(in crate::telnet) async fn initiate_download(
        &mut self,
        filename: &str,
        file_size: u64,
    ) -> Result<(), std::io::Error> {
        let blocks = (file_size as usize).div_ceil(crate::xmodem::XMODEM_BLOCK_SIZE);

        self.send_line("").await?;
        self.send_line(&format!(
            "  Sending: {}",
            self.amber(filename)
        ))
        .await?;
        self.send_line(&format!(
            "  {} bytes, {} blocks",
            file_size, blocks
        ))
        .await?;

        if file_size as usize > Self::MAX_FILE_SIZE {
            self.show_error("File too large.").await?;
            return Ok(());
        }

        let filepath = self.transfer_path().join(filename);
        let data = match tokio::fs::read(&filepath).await {
            Ok(d) => d,
            Err(e) => {
                self.show_error(&format!("Failed to read: {}", e))
                    .await?;
                return Ok(());
            }
        };
        // Best-effort fs metadata for the YMODEM block-0 modtime/mode
        // fields (Forsberg §6.1).  Both are informational — if metadata
        // lookup fails or the platform doesn't expose UNIX mode bits we
        // pass `None` and the sender emits octal `0` in that slot.
        let (file_modtime, file_mode) = match tokio::fs::metadata(&filepath).await {
            Ok(m) => {
                let modtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::MetadataExt;
                    Some(m.mode())
                };
                #[cfg(not(unix))]
                let mode: Option<u32> = None;
                (modtime, mode)
            }
            Err(_) => (None, None),
        };

        // Prompt the user to pick the transfer protocol for this download.
        // ESC at the prompt cancels the transfer.
        let protocol = match self.prompt_download_protocol(filename, file_size).await? {
            Some(p) => p,
            None => return Ok(()),
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green(match protocol {
                DownloadProtocol::Xmodem => "Start XMODEM receive now.",
                DownloadProtocol::Xmodem1k => "Start XMODEM-1K receive now.",
                DownloadProtocol::Ymodem => "Start YMODEM receive now.",
                DownloadProtocol::Zmodem => "Start ZMODEM receive now.",
                DownloadProtocol::Kermit => "Start KERMIT receive now.",
                DownloadProtocol::Punter => "Start PUNTER receive now.",
            })
        ))
        .await?;
        let neg_timeout = {
            let cfg = config::get_config();
            match protocol {
                DownloadProtocol::Zmodem => cfg.zmodem_negotiation_timeout,
                DownloadProtocol::Kermit => cfg.kermit_negotiation_timeout,
                DownloadProtocol::Punter => cfg.punter_negotiation_timeout,
                DownloadProtocol::Xmodem
                | DownloadProtocol::Xmodem1k
                | DownloadProtocol::Ymodem => cfg.xmodem_negotiation_timeout,
            }
        };
        self.send_line(&format!("  Start transfer within {} seconds.", neg_timeout))
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!("  {} to cancel", self.cyan(esc_label)))
            .await?;
        self.send_line("").await?;
        self.flush().await?;

        if config::get_config().verbose {
            glog!("Download: IAC escaping={} protocol={:?}", self.xmodem_iac, protocol);
        }
        // Punter has no in-band cancel, so a restart after a C64-side abort can
        // strand stale bytes in the pipe; drain with a longer quiet gap to
        // clear them before this transfer's handshake (capped so a peer still
        // streaming can't stall the start).  Other protocols keep the short gap.
        if matches!(protocol, DownloadProtocol::Punter) {
            self.drain_input_until_quiet(250, Some(2000)).await;
        } else {
            self.drain_input().await;
        }

        let start = std::time::Instant::now();
        let cfg = config::get_config();
        let verbose = cfg.verbose;
        let mut writer_guard = self.writer.lock().await;
        let result = if matches!(protocol, DownloadProtocol::Zmodem) {
            // zmodem_send is batch-capable; download always sends
            // exactly one file, so we pass a single-element slice.
            let batch: [(&str, &[u8]); 1] = [(filename, &data)];
            crate::zmodem::zmodem_send(
                &mut self.reader,
                &mut *writer_guard,
                &batch,
                self.xmodem_iac,
                verbose,
            )
            .await
        } else if matches!(protocol, DownloadProtocol::Kermit) {
            let is_petscii = self.terminal_type == TerminalType::Petscii;
            let files = vec![crate::kermit::KermitSendFile {
                name: filename,
                data: &data,
                modtime: file_modtime,
                mode: file_mode,
            }];
            // Interactive download: hold the Send-Init until the receiver's
            // initiating NAK arrives (gated by `kermit_wait_for_receiver`) so
            // the S packet doesn't paint as garbage on a vintage client (e.g.
            // QTerm) that isn't yet in receive mode when the menu selection
            // is made.  Server mode never takes this path.
            crate::kermit::kermit_send_with_starting_seq(
                &mut self.reader,
                &mut *writer_guard,
                &files,
                self.xmodem_iac,
                is_petscii,
                verbose,
                0,
                false,
                cfg.kermit_wait_for_receiver,
            )
            .await
        } else if matches!(protocol, DownloadProtocol::Punter) {
            // C1 declares a PRG/SEQ type in its Phase-A block; auto-detect it
            // from the filename (text extensions → SEQ, else PRG).
            let file_type = crate::punter::PunterFileType::autodetect(filename);
            crate::punter::punter_send(
                &mut self.reader,
                &mut *writer_guard,
                &data,
                file_type,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
            )
            .await
        } else {
            // YMODEM always uses 1K data blocks; XMODEM-1K uses 1K
            // blocks without the filename header; classic XMODEM uses
            // 128-byte blocks only.
            let use_1k = matches!(
                protocol,
                DownloadProtocol::Xmodem1k | DownloadProtocol::Ymodem,
            );
            let ymodem = if matches!(protocol, DownloadProtocol::Ymodem) {
                Some(crate::xmodem::YmodemHeader {
                    filename: filename.to_string(),
                    size: file_size,
                    modtime: file_modtime,
                    mode: file_mode,
                })
            } else {
                None
            };
            crate::xmodem::xmodem_send(
                &mut self.reader,
                &mut *writer_guard,
                &data,
                self.xmodem_iac,
                self.terminal_type == TerminalType::Petscii,
                verbose,
                use_1k,
                ymodem,
            )
            .await
        };
        drop(writer_guard);
        let elapsed = start.elapsed();

        match result {
            Ok(()) => {
                // Brief pause so the remote terminal can switch back from
                // XMODEM mode to text display.
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.green("Download complete!")
                ))
                .await?;
                self.send_line(&format!(
                    "  {} bytes, {} blocks, {:.1}s",
                    data.len(),
                    blocks,
                    elapsed.as_secs_f64()
                ))
                .await?;
            }
            Err(e) => {
                self.send_line("").await?;
                self.send_line(&format!(
                    "  {}",
                    self.red(&format!("Transfer failed: {}", e))
                ))
                .await?;
                // Option 4: with no in-band abort, a Punter give-up otherwise
                // strands the C64.  Drop carrier so it sees loss-of-carrier —
                // but only on a genuine give-up, NOT a user-initiated cancel
                // (ESC → "Transfer cancelled"), which must return to the menu
                // like every other protocol rather than drop the whole session.
                if matches!(protocol, DownloadProtocol::Punter)
                    && config::get_config().punter_hangup_on_failure
                    && !e.contains("cancelled")
                {
                    return self.punter_hangup().await;
                }
            }
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── KERMIT SERVER MODE ─────────────────────────────────

    /// Idle as a Kermit server: peer drives the session by sending
    /// Kermit commands (`send`, `get`, `dir`, `finish`, `bye`, etc.).
    /// On exit, any files received during the session are written to
    /// the current transfer subdir using the same `validate_filename`
    /// rules as the interactive upload path.  Files whose sender-
    /// supplied names fail validation or collide with an existing
    /// path are skipped rather than clobbered, mirroring ZMODEM batch
    /// behavior.
    pub(in crate::telnet) async fn file_transfer_kermit_server(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        if Self::is_disk_full() {
            self.show_error("Disk space is low. Server mode disabled.")
                .await?;
            return Ok(());
        }

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("KERMIT SERVER MODE"))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green("Listening for Kermit packets.")
        ))
        .await?;
        self.send_line(&format!(
            "  {}",
            self.dim("Your screen will be quiet — that's normal.")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("Needs a Kermit-aware client at your end.")
        ))
        .await?;
        self.send_line("  A plain telnet client cannot drive this.").await?;
        self.send_line("").await?;
        self.send_line("  Compatible clients:").await?;
        self.send_line(&format!(
            "    {} use the built-in Kermit menu",
            self.cyan("Tera Term / Kermit-95 —")
        ))
        .await?;
        self.send_line(&format!(
            "    {} run from a separate shell:",
            self.cyan("C-Kermit / G-Kermit —")
        ))
        .await?;
        self.send_line(&format!(
            "      {}",
            self.amber("kermit -j host:port -g file")
        ))
        .await?;
        self.send_line("").await?;
        self.send_line("  Remote commands once your client is talking:").await?;
        self.send_line(&format!(
            "    {}  upload to us",
            self.cyan("send <file>")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  download from us",
            self.cyan("get <file>")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  list / change dir / show help",
            self.cyan("remote dir / cwd / help")
        ))
        .await?;
        self.send_line(&format!(
            "    {}  end the session",
            self.cyan("finish | bye")
        ))
        .await?;
        self.send_line("").await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        let idle_secs = config::get_config().kermit_idle_timeout;
        self.send_line(&format!(
            "  {} returns to the File Transfer menu.",
            self.cyan(esc_label)
        ))
        .await?;
        if idle_secs == 0 {
            self.send_line(
                "  Idle timeout disabled — server holds the session",
            )
            .await?;
            self.send_line("  open until the peer sends finish/bye.").await?;
        } else {
            let idle_display = if idle_secs >= 60 && idle_secs.is_multiple_of(60) {
                format!("{} min", idle_secs / 60)
            } else {
                format!("{}s", idle_secs)
            };
            self.send_line(&format!(
                "  After {} idle, we send the client an error packet",
                self.amber(&idle_display)
            ))
            .await?;
            self.send_line("  and disconnect.").await?;
        }
        self.send_line("  See kermit.html for full client setup.").await?;
        self.send_line("").await?;
        self.flush().await?;

        let verbose = config::get_config().verbose;
        let is_petscii = self.terminal_type == TerminalType::Petscii;

        // Saved/skipped lists are populated by the on-file callback as
        // each S-dispatch completes — see the `kermit_server` doc
        // comment.  Hoisting them out here keeps the summary render
        // below independent of what kermit_server returns.
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
        let target_dir = self.transfer_path();

        let start = std::time::Instant::now();
        let result = {
            let mut writer_guard = self.writer.lock().await;
            crate::kermit::kermit_server_with_outcome(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                is_petscii,
                verbose,
                |rx| {
                    // Filename strictness now enforced at F-packet
                    // receipt (see kermit.rs F-packet handler), so any
                    // KermitReceive that reaches this callback already
                    // has a saver-acceptable name.  The defensive check
                    // stays because validate_filename is cheap and
                    // closes the door on any future kermit-side bypass.
                    if Self::validate_filename(&rx.filename).is_err() {
                        // Sanitize before the name can reach the terminal summary.
                        skipped.push((crate::aichat::sanitize_for_terminal(&rx.filename), "invalid filename"));
                        return;
                    }
                    // Defense-in-depth: re-validate the subdir before joining
                    // it.  rx.subdir is only ever set after kermit's own
                    // is_safe_relative_subdir today, but re-checking here
                    // closes the door on any future kermit-side bypass — the
                    // same belt-and-suspenders rationale as the filename
                    // re-check above.
                    if !crate::kermit::is_safe_relative_subdir(&rx.subdir) {
                        skipped.push((rx.filename.clone(), "unsafe subdir"));
                        return;
                    }
                    // Honor any `remote cwd <subdir>` the peer set —
                    // server-mode stamps `rx.subdir` with its current
                    // working subdir at the moment of receipt.  Without
                    // this, `remote cd assembly` followed by `put hello.txt`
                    // silently landed hello.txt in the base transfer_dir
                    // instead of transfer_dir/assembly, and a follow-up
                    // `remote dir` would show an empty assembly directory.
                    let dir = if rx.subdir.is_empty() {
                        target_dir.clone()
                    } else {
                        target_dir.join(&rx.subdir)
                    };
                    let filepath = dir.join(&rx.filename);
                    let meta = crate::xmodem::YmodemReceiveMeta {
                        size: rx.declared_size,
                        modtime: rx.modtime,
                        mode: rx.mode,
                    };
                    match Self::save_received_file_sync(
                        &filepath,
                        &rx.data,
                        Some(&meta),
                        rx.resumed,
                    ) {
                        Ok(()) => saved.push((rx.filename.clone(), rx.data.len())),
                        Err(SaveError::AlreadyExists) => {
                            skipped.push((rx.filename.clone(), "already exists"));
                        }
                        Err(SaveError::WriteFailed) => {
                            skipped.push((rx.filename.clone(), "write failed"));
                        }
                    }
                },
            )
            .await
        };
        let elapsed = start.elapsed();

        // On Err the closure may have already committed files to
        // disk before the failure — fall through to the summary so
        // the user sees which ones landed, with the error shown
        // alongside.  Early-returning here would silently drop
        // saved/skipped, which is the bug the audit caught.
        let (error_msg, idle_timeout) = match &result {
            Ok(outcome) => (None, outcome.idle_timeout),
            Err(e) => (Some(format!("Server session failed: {}", e)), false),
        };
        let total = saved.len() + skipped.len();

        // On idle-timeout the gateway has just written an E-packet
        // ("Server idle timeout") to the socket and we MUST return
        // before sending any more bytes.  The peer's protocol parser
        // is queued to read that E-packet on its next request — if we
        // mix in summary text first, the peer reads the text as
        // garbage, doesn't surface the E-packet message, and the
        // operator sees "too many retries" instead of a clean
        // "connection closed" with the timeout reason.
        // Returning ErrorKind::TimedOut here propagates up through
        // `?` in the menu loop and ends the telnet session, which
        // is what gives the peer a clean EOF on its socket.
        if idle_timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Kermit server idle timeout — disconnecting",
            ));
        }

        // Summary screen.
        self.post_transfer_settle().await;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Server session ended in {:.1}s.",
            elapsed.as_secs_f64()
        ))
        .await?;
        if let Some(msg) = &error_msg {
            self.send_line(&format!("  {}", self.red(msg))).await?;
        }
        self.send_line(&format!(
            "  Received: {} file(s), saved: {}, skipped: {}.",
            total,
            saved.len(),
            skipped.len()
        ))
        .await?;
        for (name, size) in &saved {
            self.send_line(&format!(
                "    {} {} ({} bytes)",
                self.green("✓"),
                self.amber(name),
                size
            ))
            .await?;
        }
        for (name, reason) in &skipped {
            self.send_line(&format!(
                "    {} {} ({})",
                self.red("✗"),
                self.amber(name),
                reason
            ))
            .await?;
        }
        self.send_line("").await?;
        self.wait_for_key().await?;
        Ok(())
    }

    // ─── DELETE ─────────────────────────────────────────────

    pub(in crate::telnet) async fn file_transfer_delete(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;
        let mut page: usize = 0;

        loop {
            let files = Self::list_transfer_entries_in(&self.transfer_path())
                .await?
                .into_iter()
                .filter(|(_, _, is_dir)| !is_dir)
                .map(|(name, size, _)| (name, size))
                .collect::<Vec<_>>();

            if files.is_empty() {
                self.show_error("No files to delete.").await?;
                return Ok(());
            }

            let total_pages = files.len().div_ceil(Self::TRANSFER_PAGE_SIZE);
            if page >= total_pages {
                page = total_pages - 1;
            }
            let offset = page * Self::TRANSFER_PAGE_SIZE;
            let end = (offset + Self::TRANSFER_PAGE_SIZE).min(files.len());
            let page_files = &files[offset..end];

            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!(
                "  {}",
                self.yellow("DELETE FILE")
            ))
            .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            self.send_line(&format!(
                "   {} {:<22} {}",
                self.cyan("#."),
                "Filename",
                "Size"
            ))
            .await?;
            self.send_line(&format!(
                "  {}",
                self.yellow(&"-".repeat(36))
            ))
            .await?;

            for (i, (name, size)) in page_files.iter().enumerate() {
                let num = i + 1;
                let display_name = if name.chars().count() > 22 {
                    let truncated: String = name.chars().take(19).collect();
                    format!("{}...", truncated)
                } else {
                    name.clone()
                };
                let size_display = Self::format_file_size(*size);
                self.send_line(&format!(
                    "  {:>2}. {:<22} {}",
                    num, display_name, size_display
                ))
                .await?;
            }

            self.send_line("").await?;
            self.send_line(&format!(
                "  Page {} of {}",
                page + 1,
                total_pages
            ))
            .await?;
            self.send_line("").await?;

            let mut nav = Vec::new();
            if page > 0 {
                nav.push(self.action_prompt("P", "Prev"));
            }
            if page + 1 < total_pages {
                nav.push(self.action_prompt("N", "Next"));
            }
            nav.push(self.action_prompt("Q", "Back"));
            nav.push(self.action_prompt("H", "Help"));
            let esc_label = match self.terminal_type {
                TerminalType::Petscii => "<-",
                _ => "ESC",
            };
            nav.push(self.action_prompt(esc_label, "Main"));
            self.send_line(&format!("  {}", nav.join(" | ")))
                .await?;
            self.send_line("").await?;
            self.send(&format!("  {} ", self.cyan("Select #:")))
                .await?;
            self.flush().await?;

            let input = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            };

            match input.as_str() {
                "p" => {
                    page = page.saturating_sub(1);
                }
                "n" => {
                    if page + 1 < total_pages {
                        page += 1;
                    }
                }
                "q" => return Ok(()),
                "h" => {
                    self.show_help_page("DELETE HELP", Self::delete_help_lines())
                        .await?;
                }
                other => {
                    if let Ok(num) = other.parse::<usize>() {
                        if num >= 1 && num <= page_files.len() {
                            let (ref filename, _) = page_files[num - 1];
                            self.send_line("").await?;
                            let p = format!(
                                "  Delete {}? ({}/{}) ",
                                self.amber(filename),
                                self.green("Y"),
                                self.red("N"),
                            );
                            self.send(&p).await?;
                            self.flush().await?;

                            match self.read_byte_filtered().await? {
                                Some(b)
                                    if {
                                        let ch =
                                            if self.terminal_type == TerminalType::Petscii {
                                                petscii_to_ascii_byte(b)
                                            } else {
                                                b
                                            };
                                        ch == b'y' || ch == b'Y'
                                    } =>
                                {
                                    self.send_line("").await?;
                                    let path = self.transfer_path().join(filename);
                                    match tokio::fs::remove_file(&path).await {
                                        Ok(()) => {
                                            self.send_line(&format!(
                                                "  {}",
                                                self.green("File deleted.")
                                            ))
                                            .await?;
                                            self.send_line("").await?;
                                            self.send(
                                                "  Press any key to continue.",
                                            )
                                            .await?;
                                            self.flush().await?;
                                            self.wait_for_key().await?;
                                        }
                                        Err(e) => {
                                            self.show_error(&format!(
                                                "Delete failed: {}",
                                                e
                                            ))
                                            .await?;
                                        }
                                    }
                                }
                                _ => {
                                    self.send_line("").await?;
                                    self.send_line("  Cancelled.").await?;
                                    self.send_line("").await?;
                                    self.send("  Press any key to continue.")
                                        .await?;
                                    self.flush().await?;
                                    self.wait_for_key().await?;
                                }
                            }
                        } else {
                            self.show_error("Invalid selection.").await?;
                        }
                    } else {
                        self.show_error("Enter a number, P, N, Q, or H.")
                            .await?;
                    }
                }
            }
        }
    }

    // ─── CHANGE DIRECTORY ───────────────────────────────────

    pub(in crate::telnet) async fn file_transfer_chdir(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        let entries =
            Self::list_transfer_entries_in(&self.transfer_path()).await?;
        let dirs: Vec<&str> = entries
            .iter()
            .filter(|(_, _, is_dir)| *is_dir)
            .map(|(name, _, _)| name.as_str())
            .collect();

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("CHANGE DIRECTORY")
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_dir = if self.terminal_type == TerminalType::Petscii {
            26
        } else {
            56
        };
        let dir_str =
            truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!(
            "  Current: {}",
            self.amber(&dir_str)
        ))
        .await?;
        self.send_line("").await?;

        let mut num = 0usize;
        if !self.transfer_subdir.is_empty() {
            num += 1;
            self.send_line(&format!(
                "  {:>2}. {}",
                num,
                self.cyan("..")
            ))
            .await?;
        }

        for name in &dirs {
            num += 1;
            let display = if name.chars().count() > 30 {
                let t: String = name.chars().take(27).collect();
                format!("{}...", t)
            } else {
                name.to_string()
            };
            self.send_line(&format!(
                "  {:>2}. {}/",
                num,
                self.cyan(&display)
            ))
            .await?;
        }

        if num == 0 {
            self.show_error("No subdirectories.").await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("Select #:")))
            .await?;
        self.flush().await?;

        let input = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        if input == "q" {
            return Ok(());
        }

        if let Ok(n) = input.parse::<usize>() {
            if n == 0 {
                self.show_error("Invalid selection.").await?;
                return Ok(());
            }
            let has_parent = !self.transfer_subdir.is_empty();
            if has_parent && n == 1 {
                if let Some(pos) = self.transfer_subdir.rfind('/') {
                    self.transfer_subdir.truncate(pos);
                } else {
                    self.transfer_subdir.clear();
                }
            } else {
                let dir_idx = if has_parent { n - 2 } else { n - 1 };
                if dir_idx < dirs.len() {
                    let name = dirs[dir_idx];
                    let prev = self.transfer_subdir.clone();
                    if self.transfer_subdir.is_empty() {
                        self.transfer_subdir = name.to_string();
                    } else {
                        self.transfer_subdir =
                            format!("{}/{}", self.transfer_subdir, name);
                    }
                    if !self.verify_transfer_path() {
                        self.transfer_subdir = prev;
                        self.show_error("Access denied.").await?;
                    }
                } else {
                    self.show_error("Invalid selection.").await?;
                }
            }
        } else {
            self.show_error("Enter a number or Q.").await?;
        }
        Ok(())
    }

    /// Create a new subdirectory inside the current transfer working directory,
    /// then offer to make it the working directory.  The name goes through
    /// `validate_filename` (a single component — no `..`, `/`, or leading dot),
    /// so the new path can't escape the transfer base; the optional switch is
    /// still re-checked with `verify_transfer_path` for defense in depth.
    pub(in crate::telnet) async fn file_transfer_mkdir(&mut self) -> Result<(), std::io::Error> {
        self.ensure_transfer_dir().await?;

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("MAKE DIRECTORY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;

        let max_dir = if self.terminal_type == TerminalType::Petscii {
            26
        } else {
            56
        };
        let dir_str = truncate_to_width(&self.transfer_dir_display(), max_dir);
        self.send_line(&format!("  In: {}", self.amber(&dir_str)))
            .await?;
        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("New directory name:")))
            .await?;
        self.flush().await?;

        let name = match self.get_line_input().await? {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return Ok(()), // empty / cancel
        };

        if let Err(msg) = Self::validate_filename(&name) {
            self.show_error(msg).await?;
            return Ok(());
        }

        let target = self.transfer_path().join(&name);
        match tokio::fs::create_dir(&target).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.show_error("That name already exists.").await?;
                return Ok(());
            }
            Err(e) => {
                self.show_error(&format!("Could not create: {}", e)).await?;
                return Ok(());
            }
        }

        let created = truncate_to_width(&format!("Created {}/", name), max_dir);
        self.send_line(&format!("  {}", self.green(&created))).await?;
        self.send_line("").await?;

        // Offer to switch into the new directory.
        self.send(&format!(
            "  {} ",
            self.cyan("Make this the working dir? (Y/N):")
        ))
        .await?;
        self.flush().await?;
        self.drain_input().await;
        let answer = match self.read_byte_filtered().await? {
            Some(b) => {
                if self.terminal_type == TerminalType::Petscii {
                    petscii_to_ascii_byte(b)
                } else {
                    b
                }
            }
            None => return Ok(()),
        };
        self.send_line("").await?;

        if answer == b'y' || answer == b'Y' {
            let prev = self.transfer_subdir.clone();
            if self.transfer_subdir.is_empty() {
                self.transfer_subdir = name.clone();
            } else {
                self.transfer_subdir = format!("{}/{}", self.transfer_subdir, name);
            }
            if self.verify_transfer_path() {
                let disp = truncate_to_width(&self.transfer_dir_display(), max_dir);
                self.send_line(&format!("  {} {}", self.dim("Now in:"), self.amber(&disp)))
                    .await?;
            } else {
                // Should not happen (validate_filename bars escape), but revert
                // and report rather than leave a bad subdir set.
                self.transfer_subdir = prev;
                self.show_error("Access denied.").await?;
                return Ok(());
            }
        }

        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }
}
