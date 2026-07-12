//! Outbound gateways: SSH proxy, telnet proxy, and serial gateway
//! (local port + remote console picker + console bridge loop).
//!
//! The gateway protocol plumbing (GatewayTelnetIac, read_gateway_event,
//! filter_gateway_output, GatewayHandler, ...) lives in `telnet/mod.rs`
//! and is reached here via `use super::*`. Behaviour unchanged.

use super::*;

impl TelnetSession {
    // ─── SSH GATEWAY ────────────────────────────────────────

    /// Gateway timeout for SSH connection attempts.
    const GATEWAY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// Prompt for the remote SSH host, port, and username.  Password is
    /// collected separately (`gateway_password_prompt`) so we can skip
    /// it entirely when public-key authentication succeeds.
    pub(in crate::telnet) async fn gateway_host_prompts(
        &mut self,
    ) -> Result<Option<(String, u16, String)>, std::io::Error> {
        self.send(&format!("  {} ", self.cyan("Host:")))
            .await?;
        self.flush().await?;
        let host = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        self.send(&format!("  {} ", self.cyan("Port (22):")))
            .await?;
        self.flush().await?;
        let port: u16 = match self.get_line_input().await? {
            Some(s) if s.is_empty() => 22,
            Some(s) => match s.parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    self.show_error("Invalid port number.").await?;
                    return Ok(None);
                }
            },
            None => return Ok(None),
        };

        self.send(&format!("  {} ", self.cyan("Username:")))
            .await?;
        self.flush().await?;
        let username = match self.get_line_input().await? {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };

        Ok(Some((host, port, username)))
    }

    /// Prompt for the remote SSH password.  Called only after public-key
    /// authentication is rejected by the remote so users who have set up
    /// the gateway's key in the remote's `authorized_keys` never see
    /// this prompt at all.
    pub(in crate::telnet) async fn gateway_password_prompt(
        &mut self,
    ) -> Result<Option<String>, std::io::Error> {
        self.send(&format!("  {} ", self.cyan("Password:")))
            .await?;
        self.flush().await?;
        match self.get_password_input().await? {
            Some(s) => Ok(Some(s)),
            None => Ok(None),
        }
    }

    /// SSH gateway: connect to a remote server and proxy the session.
    pub(in crate::telnet) async fn gateway_ssh(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("SSH GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Connect to a remote SSH server.")
            .await?;
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  Press {} at any prompt to cancel.",
            self.cyan(esc_label)
        ))
        .await?;
        let auth_label = if cfg.ssh_gateway_auth == "password" {
            self.yellow("password")
        } else {
            self.green("gateway key")
        };
        self.send_line(&format!("  Auth: {}", auth_label)).await?;
        self.send_line("").await?;

        let (host, port, username) = if idle_timeout.is_zero() {
            match self.gateway_host_prompts().await {
                Ok(Some(v)) => v,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match tokio::time::timeout(
                idle_timeout,
                self.gateway_host_prompts(),
            )
            .await
            {
                Ok(Ok(Some(v))) => v,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout in gateway prompts",
                    ));
                }
            }
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Connecting to {}:{}...",
            self.amber(&host),
            port
        ))
        .await?;
        self.flush().await?;

        // Connect to remote SSH server
        let ssh_config = std::sync::Arc::new(russh::client::Config {
            inactivity_timeout: Some(std::time::Duration::from_secs(600)),
            ..Default::default()
        });
        let server_key_slot: Arc<std::sync::Mutex<Option<russh::keys::PublicKey>>> =
            Arc::new(std::sync::Mutex::new(None));
        let handler = GatewayHandler {
            server_key: server_key_slot.clone(),
        };

        let mut session = match tokio::time::timeout(
            Self::GATEWAY_CONNECT_TIMEOUT,
            russh::client::connect(ssh_config, (host.as_str(), port), handler),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                self.show_error(&format!("Connection failed: {}", e))
                    .await?;
                return Ok(());
            }
            Err(_) => {
                self.show_error("Connection timed out.").await?;
                return Ok(());
            }
        };

        // Verify server host key against known-hosts file
        let server_key = server_key_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let Some(ref key) = server_key else {
            self.show_error("Could not verify server host key.").await?;
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "no host key", "")
                .await;
            return Ok(());
        };
        {
            match check_known_host(&host, port, key) {
                HostKeyStatus::Known => {}
                HostKeyStatus::Unknown => {
                    let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
                    let algo = key.algorithm();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.yellow("Host key not recognized.")
                    ))
                    .await?;
                    let algo_str = algo.to_string();
                    let fp_str = fingerprint.to_string();
                    self.send_line(&format!("  Type: {}", self.cyan(&algo_str)))
                        .await?;
                    self.send_line(&format!(
                        "  Fingerprint: {}",
                        self.cyan(&fp_str)
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send(&format!(
                        "  {} ",
                        self.cyan("Trust this host? (Y/N):")
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
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "host key rejected", "")
                            .await;
                        self.show_error("Connection aborted.").await?;
                        return Ok(());
                    }
                    save_known_host(&host, port, key);
                    glog!(
                        "SSH gateway: TOFU-accepted host key for {}:{} ({} {})",
                        host,
                        port,
                        key.algorithm(),
                        key.fingerprint(russh::keys::HashAlg::Sha256),
                    );
                    self.send_line(&format!(
                        "  {}",
                        self.green("Host key saved.")
                    ))
                    .await?;
                }
                HostKeyStatus::Changed => {
                    let fingerprint = key.fingerprint(russh::keys::HashAlg::Sha256);
                    let algo_str = key.algorithm().to_string();
                    let fp_str = fingerprint.to_string();
                    self.send_line("").await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("WARNING: HOST KEY HAS CHANGED!")
                    ))
                    .await?;
                    self.send_line(&format!(
                        "  {}",
                        self.red("This could indicate a security threat.")
                    ))
                    .await?;
                    self.send_line(&format!("  New type: {}", self.cyan(&algo_str)))
                        .await?;
                    self.send_line(&format!(
                        "  New fingerprint: {}",
                        self.cyan(&fp_str)
                    ))
                    .await?;
                    self.send_line("").await?;
                    self.send(&format!(
                        "  {} ",
                        self.cyan("Update key? (Y/N):")
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
                        save_known_host(&host, port, key);
                        glog!(
                            "SSH gateway: operator UPDATED changed host key for {}:{} (new {} {})",
                            host,
                            port,
                            key.algorithm(),
                            key.fingerprint(russh::keys::HashAlg::Sha256),
                        );
                        self.send_line(&format!(
                            "  {}",
                            self.green("Host key updated.")
                        ))
                        .await?;
                    } else {
                        glog!(
                            "SSH gateway: operator REJECTED changed host key for {}:{} (presented {} {})",
                            host,
                            port,
                            key.algorithm(),
                            key.fingerprint(russh::keys::HashAlg::Sha256),
                        );
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "host key rejected", "")
                            .await;
                        self.show_error("Connection aborted.").await?;
                        return Ok(());
                    }
                }
            }
        }

        // Authenticate using the configured mode.  The server-config
        // `ssh_gateway_auth` key dictates the method: "key" uses the
        // gateway's own auto-generated Ed25519 client key (copy the
        // public half printed by `cat gateway_client_key.pub` into the
        // remote's `~/.ssh/authorized_keys` first); "password" prompts
        // the operator each time.  No silent fallback — the remote sees
        // exactly one auth method, so failures are unambiguous.
        let mut authed = false;
        if cfg.ssh_gateway_auth == "password" {
            let password = if idle_timeout.is_zero() {
                match self.gateway_password_prompt().await {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Ok(());
                    }
                    Err(e) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Err(e);
                    }
                }
            } else {
                match tokio::time::timeout(
                    idle_timeout,
                    self.gateway_password_prompt(),
                )
                .await
                {
                    Ok(Ok(Some(p))) => p,
                    Ok(Ok(None)) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "cancelled", "")
                            .await;
                        return Err(e);
                    }
                    Err(_) => {
                        let _ = session
                            .disconnect(russh::Disconnect::ByApplication, "idle timeout", "")
                            .await;
                        let _ = self
                            .send_line("\r\nDisconnected: idle timeout.")
                            .await;
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "idle timeout at password prompt",
                        ));
                    }
                }
            };
            match session.authenticate_password(&username, &password).await {
                Ok(russh::client::AuthResult::Success) => {
                    authed = true;
                    glog!(
                        "SSH gateway: authenticated to {}:{} as {} via password",
                        host, port, username,
                    );
                }
                Ok(russh::client::AuthResult::Failure { .. }) => {}
                Err(e) => {
                    let _ = session
                        .disconnect(russh::Disconnect::ByApplication, "auth error", "")
                        .await;
                    self.show_error(&format!("Auth error: {}", e)).await?;
                    return Ok(());
                }
            }
        } else {
            // "key" mode — gateway's Ed25519 client key, no password fallback.
            match crate::ssh::load_or_generate_client_key() {
                Ok(key) => {
                    // best_supported_rsa_hash returns Result<Option<Option<HashAlg>>>:
                    //   outer Option = "server doesn't specify a preference",
                    //   inner Option = "preference is 'no hash' (i.e., not RSA)".
                    // Two flattens collapse both to Option<HashAlg>.
                    let hash_alg = session
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    match session
                        .authenticate_publickey(
                            &username,
                            russh::keys::PrivateKeyWithHashAlg::new(
                                std::sync::Arc::new(key),
                                hash_alg,
                            ),
                        )
                        .await
                    {
                        Ok(russh::client::AuthResult::Success) => {
                            authed = true;
                            glog!(
                                "SSH gateway: authenticated to {}:{} as {} via pubkey",
                                host, port, username,
                            );
                            self.send_line(&format!(
                                "  {}",
                                self.green("Authenticated (gateway key).")
                            ))
                            .await?;
                        }
                        Ok(russh::client::AuthResult::Failure { .. }) => {}
                        Err(e) => {
                            glog!("SSH gateway: pubkey auth error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    glog!("SSH gateway: client key unavailable: {}", e);
                }
            }
        }
        if !authed {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "auth failed", "")
                .await;
            if cfg.ssh_gateway_auth == "password" {
                self.show_error("Authentication failed.").await?;
            } else {
                self.show_error(
                    "Key authentication failed. Copy the gateway's public \
                     key (shown in the GUI Server > More popup) into the \
                     remote's ~/.ssh/authorized_keys, or switch to Password \
                     mode from Configuration > Gateway Configuration.",
                )
                .await?;
            }
            return Ok(());
        }

        // Open channel and request PTY + shell.  Every error path from
        // here forward must call `session.disconnect` before returning
        // — otherwise the remote sees an orphaned, still-authenticated
        // session and its connection slot stays occupied until a TCP
        // timeout eventually reaps it.
        let channel = match session.channel_open_session().await {
            Ok(ch) => ch,
            Err(e) => {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "channel open failed", "")
                    .await;
                self.show_error(&format!("Channel error: {}", e))
                    .await?;
                return Ok(());
            }
        };

        let (cols, rows, term) = match self.terminal_type {
            TerminalType::Petscii => (40, 25, "dumb"),
            TerminalType::Ascii => (80, 24, "dumb"),
            TerminalType::Ansi => (80, 24, "xterm"),
        };

        if let Err(e) = channel
            .request_pty(false, term, cols, rows, 0, 0, &[])
            .await
        {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "pty request failed", "")
                .await;
            self.show_error(&format!("PTY error: {}", e)).await?;
            return Ok(());
        }
        if let Err(e) = channel.request_shell(false).await {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "shell request failed", "")
                .await;
            self.show_error(&format!("Shell error: {}", e)).await?;
            return Ok(());
        }

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line(&format!(
            "  Press {} twice to disconnect.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        // Proxy I/O between telnet client and SSH channel
        let stream = channel.into_stream();
        let (mut ssh_reader, mut ssh_writer) = tokio::io::split(stream);

        let reader = &mut self.reader;
        let writer = &self.writer;
        let erase_char = self.erase_char;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let is_ascii = self.terminal_type == TerminalType::Ascii;
        // Idle bound for the live bridge: if neither side sends a byte
        // within this window, tear the session down so a half-open client
        // (laptop asleep, NAT drop) can't pin it — and its max_sessions
        // slot — forever.  Copied out before the reader borrow below; zero
        // disables it, matching the rest of the session's idle policy.
        let idle_timeout = self.idle_timeout;

        let mut ssh_buf = [0u8; 4096];
        let mut filter_buf: Vec<u8> = Vec::new();
        let mut ansi_state: u8 = 0;
        let mut last_cr = false;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        // Gateway byte-tracing (EGATEWAY_GATEWAY_DEBUG).  `dbg_in` accumulates
        // every byte we forward to the remote shell and is flushed to the log
        // on each CR/LF — so the log shows exactly the line bash receives on
        // RETURN, which is the crux of the c64sshwrap long-line truncation
        // investigation.  A no-newline stream (binary paste, TUI input editor)
        // is capped at GW_DBG_IN_CAP bytes so a long-running debug session
        // doesn't grow the buffer without bound.
        let gw_debug = gw_debug_enabled(cfg.gateway_debug);
        let mut dbg_in: Vec<u8> = Vec::new();
        // Per-byte timing: `+Δms` is the gap since the previous input byte and
        // `t=…` is elapsed since trace start.  Large gaps = bytes typed live
        // (character-mode terminal); a near-zero burst = a line dumped at once
        // (screen-memory walk).  This is what tells the two mechanisms apart.
        let gw_start = std::time::Instant::now();
        let mut gw_last = gw_start;
        if gw_debug {
            glog!(
                "[gw] SSH gateway trace ON — term={:?} pty=({}x{},{})",
                self.terminal_type, cols, rows, term
            );
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                byte = read_byte_iac_filtered(reader, true) => {
                    match byte {
                        Ok(Some(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // Two consecutive ESC presses — disconnect
                            }
                            last_was_esc = true;
                        }
                        Ok(Some(b)) => {
                            // Forward the previously held ESC before this byte
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii { petscii_to_ascii_byte(esc_byte) } else { esc_byte };
                                if let Some(e) = normalize_gateway_input(e, &mut last_cr)
                                    && ssh_writer.write_all(&[e]).await.is_err() { break; }
                            }
                            let raw = b;
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if let Some(b) = normalize_gateway_input(b, &mut last_cr) {
                                if gw_debug {
                                    let now = std::time::Instant::now();
                                    let dt = now.duration_since(gw_last).as_millis();
                                    let t = now.duration_since(gw_start).as_millis();
                                    gw_last = now;
                                    let swap = if raw != b { format!(" (petscii 0x{:02x})", raw) } else { String::new() };
                                    glog!("[gw-in] +{:>5}ms t={:>6}ms  byte=0x{:02x} '{}'{}",
                                        dt, t, b,
                                        if (0x20..=0x7E).contains(&b) { b as char } else { '.' },
                                        swap);
                                    if b == b'\r' || b == b'\n' {
                                        glog!("[gw-in] line ({} bytes) -> {}", dbg_in.len(), gw_hexdump(&dbg_in));
                                        dbg_in.clear();
                                    } else {
                                        dbg_in.push(b);
                                        if dbg_in.len() >= GW_DBG_IN_CAP {
                                            glog!("[gw-in] line (no CR/LF, {} bytes cap) -> {}",
                                                dbg_in.len(), gw_hexdump(&dbg_in));
                                            dbg_in.clear();
                                        }
                                    }
                                }
                                if ssh_writer.write_all(&[b]).await.is_err() { break; }
                                if ssh_writer.flush().await.is_err() { break; }
                            }
                        }
                        _ => break,
                    }
                }
                n = ssh_reader.read(&mut ssh_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = if is_petscii || is_ascii {
                                filter_buf.clear();
                                filter_gateway_output(&ssh_buf[..n], &mut ansi_state, is_petscii, &mut filter_buf);
                                &filter_buf[..]
                            } else {
                                &ssh_buf[..n]
                            };
                            if gw_debug {
                                glog!("[gw-out] raw {} bytes -> {}", n, gw_hexdump(&ssh_buf[..n]));
                                if is_petscii || is_ascii {
                                    glog!("[gw-out] filtered {} bytes -> {}", data.len(), gw_hexdump(data));
                                }
                            }
                            if !data.is_empty() {
                                let mut w = writer.lock().await;
                                if w.write_all(data).await.is_err() { break; }
                                if w.flush().await.is_err() { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Clean up SSH channel and session
        let _ = ssh_writer.shutdown().await;
        drop(ssh_writer);
        drop(ssh_reader);
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "bye", "")
            .await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Connection closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        if idle_timeout.is_zero() {
            self.wait_for_key().await?;
        } else {
            match tokio::time::timeout(idle_timeout, self.wait_for_key()).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                }
            }
        }
        Ok(())
    }

    // ─── TELNET GATEWAY ──────────────────────────────────────

    /// Telnet gateway: connect to a remote telnet server and proxy the session.
    pub(in crate::telnet) async fn gateway_telnet(&mut self) -> Result<(), std::io::Error> {
        let cfg = config::get_config();
        let idle_timeout = std::time::Duration::from_secs(cfg.idle_timeout_secs);
        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!("  {}", self.yellow("TELNET GATEWAY")))
            .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line("  Connect to a remote telnet server.")
            .await?;
        self.send_line(&format!(
            "  Press {} at any prompt to cancel.",
            self.cyan(esc_label)
        ))
        .await?;
        let mode_label = if cfg.telnet_gateway_raw {
            self.red("Raw TCP (no IAC parsing)")
        } else {
            self.green("Telnet protocol")
        };
        self.send_line(&format!("  Mode: {}", mode_label)).await?;
        self.send_line("").await?;

        // Gather host and port
        let get_host_port = async {
            self.send(&format!("  {} ", self.cyan("Host:")))
                .await?;
            self.flush().await?;
            let host = match self.get_line_input().await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(None),
            };

            self.send(&format!("  {} ", self.cyan("Port (23):")))
                .await?;
            self.flush().await?;
            let port: u16 = match self.get_line_input().await? {
                Some(s) if s.is_empty() => 23,
                Some(s) => match s.parse::<u16>() {
                    Ok(p) if p > 0 => p,
                    _ => {
                        self.show_error("Invalid port number.").await?;
                        return Ok(None);
                    }
                },
                None => return Ok(None),
            };

            Ok::<Option<(String, u16)>, std::io::Error>(Some((host, port)))
        };

        let (host, port) = if idle_timeout.is_zero() {
            match get_host_port.await {
                Ok(Some(hp)) => hp,
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            }
        } else {
            match tokio::time::timeout(idle_timeout, get_host_port).await {
                Ok(Ok(Some(hp))) => hp,
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "idle timeout in telnet gateway prompts",
                    ));
                }
            }
        };

        self.send_line("").await?;
        self.send_line(&format!(
            "  Connecting to {}:{}...",
            self.amber(&host),
            port
        ))
        .await?;
        self.flush().await?;

        // Connect to remote telnet server
        let addr = format!("{}:{}", host, port);
        let remote = match tokio::time::timeout(
            Self::GATEWAY_CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                self.show_error(&format!("Connection failed: {}", e))
                    .await?;
                return Ok(());
            }
            Err(_) => {
                self.show_error("Connection timed out.").await?;
                return Ok(());
            }
        };
        let _ = remote.set_nodelay(true);

        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line(&format!(
            "  Press {} twice to disconnect.",
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        // Proxy I/O between local telnet client and remote telnet server
        let (mut remote_reader, mut remote_writer) = remote.into_split();

        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let is_ascii = self.terminal_type == TerminalType::Ascii;

        let erase_char = self.erase_char;
        let mut remote_buf = [0u8; 4096];
        // Idle bound for the live bridge (see gateway_ssh): disconnect a
        // half-open client so it can't pin the session's max_sessions slot.
        // Zero disables it, matching the session's idle policy.
        let idle_timeout = self.idle_timeout;
        let mut filter_buf: Vec<u8> = Vec::new();
        let mut ansi_state: u8 = 0;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        // Telnet-client IAC state machine + option negotiator.  Whether
        // we offer TTYPE / NAWS proactively at connect is gated by the
        // `telnet_gateway_negotiate` config flag.  ECHO cooperation is
        // always on.  In raw mode (`telnet_gateway_raw = true`) the
        // parser is still constructed but its initial offers and
        // negotiation paths are bypassed — see the `raw` checks below.
        let raw = cfg.telnet_gateway_raw;
        let terminal_name = gateway_terminal_name(self.terminal_type).to_string();
        let (cols_default, rows_default) = gateway_default_window(self.terminal_type);
        let cols = self.window_width.unwrap_or(cols_default);
        let rows = self.window_height.unwrap_or(rows_default);
        let (mut iac, initial_offers) = GatewayTelnetIac::new(
            !raw && cfg.telnet_gateway_negotiate,
            terminal_name,
            cols,
            rows,
        );
        if !raw && !initial_offers.is_empty() {
            if remote_writer.write_all(&initial_offers).await.is_err() {
                let _ = remote_writer.shutdown().await;
                return Ok(());
            }
            let _ = remote_writer.flush().await;
        }
        let mut data_from_remote: Vec<u8> = Vec::with_capacity(4096);
        let mut replies_to_remote: Vec<u8> = Vec::new();

        // Gateway byte-tracing (EGATEWAY_GATEWAY_DEBUG) — mirrors the SSH
        // gateway path so the Telnet Gateway can be checked for the same
        // c64sshwrap long-line truncation.
        let gw_debug = gw_debug_enabled(cfg.gateway_debug);
        let mut dbg_in: Vec<u8> = Vec::new();
        let gw_start = std::time::Instant::now();
        let mut gw_last = gw_start;
        if gw_debug {
            glog!(
                "[gw] Telnet gateway trace ON — term={:?} raw={} window=({}x{})",
                self.terminal_type, raw, cols, rows
            );
        }

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                event = read_gateway_event(reader) => {
                    match event {
                        Ok(GatewayInboundEvent::Data(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // Two consecutive ESC presses — disconnect
                            }
                            last_was_esc = true;
                        }
                        Ok(GatewayInboundEvent::Data(b)) => {
                            // Forward the previously held ESC before this byte
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii { petscii_to_ascii_byte(esc_byte) } else { esc_byte };
                                let write_ok = if raw {
                                    remote_writer.write_all(&[e]).await.is_ok()
                                } else {
                                    write_telnet_data(&mut remote_writer, &[e]).await.is_ok()
                                };
                                if !write_ok { break; }
                            }
                            let raw_in = b;
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if gw_debug {
                                let now = std::time::Instant::now();
                                let dt = now.duration_since(gw_last).as_millis();
                                let t = now.duration_since(gw_start).as_millis();
                                gw_last = now;
                                let swap = if raw_in != b { format!(" (petscii 0x{:02x})", raw_in) } else { String::new() };
                                glog!("[gw-in] +{:>5}ms t={:>6}ms  byte=0x{:02x} '{}'{}",
                                    dt, t, b,
                                    if (0x20..=0x7E).contains(&b) { b as char } else { '.' },
                                    swap);
                                if b == b'\r' || b == b'\n' {
                                    glog!("[gw-in] line ({} bytes) -> {}", dbg_in.len(), gw_hexdump(&dbg_in));
                                    dbg_in.clear();
                                } else {
                                    dbg_in.push(b);
                                    if dbg_in.len() >= GW_DBG_IN_CAP {
                                        glog!("[gw-in] line (no CR/LF, {} bytes cap) -> {}",
                                            dbg_in.len(), gw_hexdump(&dbg_in));
                                        dbg_in.clear();
                                    }
                                }
                            }
                            let write_ok = if raw {
                                remote_writer.write_all(&[b]).await.is_ok()
                            } else {
                                write_telnet_data(&mut remote_writer, &[b]).await.is_ok()
                            };
                            if !write_ok { break; }
                            if remote_writer.flush().await.is_err() { break; }
                        }
                        Ok(GatewayInboundEvent::NawsResize(cols, rows)) => {
                            if !raw {
                                let mut naws_update = Vec::new();
                                iac.send_naws_update(cols, rows, &mut naws_update);
                                if !naws_update.is_empty() {
                                    if remote_writer.write_all(&naws_update).await.is_err() { break; }
                                    if remote_writer.flush().await.is_err() { break; }
                                }
                            }
                            // In raw mode we swallow the resize — the
                            // destination isn't speaking telnet so there's
                            // nowhere to forward it to.
                        }
                        Ok(GatewayInboundEvent::Eof) => break,
                        Err(_) => break,
                    }
                }
                n = remote_reader.read(&mut remote_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let raw_slice: &[u8];
                            if raw {
                                // No IAC parsing — bytes are user data straight through.
                                raw_slice = &remote_buf[..n];
                            } else {
                                data_from_remote.clear();
                                replies_to_remote.clear();
                                for &b in &remote_buf[..n] {
                                    iac.feed(b, &mut data_from_remote, &mut replies_to_remote);
                                }
                                if !replies_to_remote.is_empty() {
                                    if remote_writer.write_all(&replies_to_remote).await.is_err() { break; }
                                    if remote_writer.flush().await.is_err() { break; }
                                }
                                raw_slice = &data_from_remote[..];
                            }
                            let data: &[u8] = if is_petscii || is_ascii {
                                filter_buf.clear();
                                filter_gateway_output(raw_slice, &mut ansi_state, is_petscii, &mut filter_buf);
                                &filter_buf[..]
                            } else {
                                raw_slice
                            };
                            if gw_debug {
                                glog!("[gw-out] raw {} bytes -> {}", raw_slice.len(), gw_hexdump(raw_slice));
                                if is_petscii || is_ascii {
                                    glog!("[gw-out] filtered {} bytes -> {}", data.len(), gw_hexdump(data));
                                }
                            }
                            if !data.is_empty() {
                                let mut w = writer.lock().await;
                                // Always IAC-escape when writing to the
                                // local user — their client is a real
                                // telnet peer and a literal 0xFF would
                                // be misinterpreted as IAC.
                                if write_telnet_data(&mut **w, data).await.is_err() { break; }
                                if w.flush().await.is_err() { break; }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Clean up
        let _ = remote_writer.shutdown().await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Connection closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        if idle_timeout.is_zero() {
            self.wait_for_key().await?;
        } else {
            match tokio::time::timeout(idle_timeout, self.wait_for_key()).await {
                Ok(result) => result?,
                Err(_) => {
                    let _ = self
                        .send_line("\r\nDisconnected: idle timeout.")
                        .await;
                }
            }
        }
        Ok(())
    }

    // ─── SERIAL GATEWAY ─────────────────────────────────────

    /// True when `id` is the very port this session arrived on.
    /// Bridging that port back into itself would loop the user's
    /// terminal, so the picker marks it ineligible and `gateway_serial`
    /// rejects a stale pick of it.  A non-serial session (telnet/SSH)
    /// never owns a serial port, so this is always false for them — they
    /// may bridge to any eligible port.
    pub(in crate::telnet) fn is_own_arrival_port(&self, id: crate::config::SerialPortId) -> bool {
        self.is_serial && self.serial_port_id == Some(id)
    }

    /// Render the Serial Gateway port picker.  Returns the user's pick
    /// (a local port or a registered remote console port, §9 #12), or
    /// `Ok(None)` if they backed out.  Always shows both local ports'
    /// status — even when only one is eligible — so the menu structure
    /// stays consistent and the user can see *why* a port is unavailable.
    pub(in crate::telnet) async fn gateway_serial_picker(
        &mut self,
    ) -> Result<Option<GatewayPick>, std::io::Error> {
        use crate::config::{SerialPortId, SERIAL_PORT_IDS};

        loop {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow("SERIAL GATEWAY")))
                .await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;

            let cfg = config::get_config();
            // When peer-dial is on, show this gateway's address so a caller
            // knows the phone-book form to dial from a modem port:
            // `ATD <Port>@<ip>`.  One line, and only when the feature is on,
            // so the default layout is unchanged.
            if cfg.allow_peer_dial {
                let ip = crate::serial::primary_local_ip();
                // Keep <=40 cols: "Dial: <Port>@" (13) + IPv4 (<=15) = 28.
                self.send_line(&format!(
                    "  {}",
                    self.dim(&format!("Dial: <Port>@{}", ip))
                ))
                .await?;
                self.send_line("").await?;
            }
            let mut any_eligible = false;
            for id in SERIAL_PORT_IDS {
                let port = cfg.port(id);
                // A serial-arrived session must not bridge its own
                // arrival port back into itself, so exclude only that
                // port — every other port stays selectable.
                let own_port = self.is_own_arrival_port(id);
                // On a slave, a console port is dedicated to the master
                // (it runs the registration loop, not the local console
                // bridge), so it isn't selectable here — picking it would
                // hang waiting for a local bridge nothing services (§9 #13).
                let relayed_to_master = cfg.gateway_role == "slave"
                    && port.enabled
                    && port.mode == "console"
                    && !port.port.is_empty();
                let console_ok = !own_port
                    && !relayed_to_master
                    && crate::serial::check_console_bridge_eligible(&cfg, id).is_ok();
                // A modem-mode port is selectable when peer-dial is enabled:
                // picking it rings the port (the device answers per its own
                // AT rules), just like `ATD <Port>@<IP>`.
                let peer_ok = cfg.allow_peer_dial
                    && !own_port
                    && !relayed_to_master
                    && port.enabled
                    && port.mode != "console"
                    && !port.port.is_empty();
                let ok = console_ok || peer_ok;
                any_eligible |= ok;
                // Two-line per-port entry so the device path + baud
                // never overflow the 40-col PETSCII budget.  Line 1 is
                // the role label; line 2 (when there is a device set)
                // shows the path/baud indented to align under the
                // role label.  ASCII-only — no em-dash so .len() and
                // display width agree.
                let label = format!("[{}] Port {}", id.label(), id.label());
                let role = if own_port {
                    "Your port"
                } else if relayed_to_master {
                    "-> master"
                } else if !port.enabled {
                    "Disabled"
                } else if port.mode != "console" {
                    // Modem port: selectable (rings) only when peer-dial is on.
                    if peer_ok { "Modem (rings)" } else { "Modem mode" }
                } else if port.port.is_empty() {
                    "No device"
                } else {
                    "Console mode"
                };
                let role_colored = if own_port {
                    self.amber(role)
                } else if relayed_to_master {
                    self.dim(role)
                } else if !port.enabled {
                    self.red(role)
                } else if port.mode != "console" {
                    if peer_ok { self.green(role) } else { self.amber(role) }
                } else if port.port.is_empty() {
                    self.red(role)
                } else {
                    self.green(role)
                };
                self.send_line(&format!(
                    "  {} - {}",
                    if ok { self.cyan(&label) } else { self.dim(&label) },
                    role_colored
                ))
                .await?;
                if !port.port.is_empty() {
                    // Indent under "[A] " on line 1 (6 spaces).  Path
                    // truncated so the worst-case line stays under
                    // 40 cols: 6 indent + path(<=23) + " " + baud(<=6) = 36.
                    self.send_line(&format!(
                        "      {} {}",
                        self.amber(&truncate_to_width(&port.port, 23)),
                        port.baud
                    ))
                    .await?;
                }
            }
            self.send_line("").await?;

            // Registered remote console ports (§9 #12), capped.  Each is a
            // single line keyed by a digit; the captured Vec maps the digit
            // back to its (slave IP, label) on selection.
            let remotes = crate::relay::list_remote_ports();
            let shown: Vec<(std::net::IpAddr, String)> =
                remotes.iter().take(REMOTE_PORT_DISPLAY_CAP).cloned().collect();
            any_eligible |= !shown.is_empty();
            if !remotes.is_empty() {
                self.send_line(&format!("  {}", self.dim("Remote (slave) ports:")))
                    .await?;
                for (i, (ip, label)) in shown.iter().enumerate() {
                    // No spaces around '@' — the entry is exactly the string the
                    // user types to dial it (`ATDT <Port>@<ip>`).
                    let entry = truncate_to_width(&format!("{}@{}", label, ip), 30);
                    self.send_line(&format!(
                        "  {} {}",
                        self.cyan(&format!("[{}]", i + 1)),
                        self.green(&entry)
                    ))
                    .await?;
                }
                if remotes.len() > shown.len() {
                    self.send_line(&format!(
                        "  {}",
                        self.dim(&format!("+{} more not shown", remotes.len() - shown.len()))
                    ))
                    .await?;
                }
                self.send_line("").await?;
            }

            if !any_eligible {
                self.send_line(&format!(
                    "  {}",
                    self.red("No port is available to bridge.")
                ))
                .await?;
                self.send_line(&format!(
                    "  {}",
                    self.dim("Enable console mode via Config > M.")
                ))
                .await?;
                self.send_line("").await?;
            }
            if any_eligible {
                // A picked port is a transparent, direct link (no host echoing
                // keystrokes back), so the caller needs their terminal's local
                // echo to see what they type. 38 cols — fits the PETSCII width.
                self.send_line(&format!(
                    "  {}",
                    self.dim("Tip: enable local echo to see typing")
                ))
                .await?;
            }
            self.send_line(&format!("  {}", self.action_prompt("Q", "Back")))
                .await?;
            let prompt = format!("{}> ", self.cyan("ethernet/gateway"));
            self.send(&prompt).await?;
            self.flush().await?;

            let input = match self.get_menu_input(false).await? {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(None),
            };
            match input.as_str() {
                "a" => return Ok(Some(GatewayPick::Local(SerialPortId::A))),
                "b" => return Ok(Some(GatewayPick::Local(SerialPortId::B))),
                "q" => return Ok(None),
                s => {
                    // A digit selects a remote port from the shown list.
                    if let Ok(n) = s.parse::<usize>()
                        && n >= 1
                        && n <= shown.len()
                    {
                        let (ip, label) = shown[n - 1].clone();
                        return Ok(Some(GatewayPick::Remote { ip, label }));
                    }
                    self.show_error("Press A, B, a number, or Q.").await?;
                    continue;
                }
            }
            // Final eligibility for a LOCAL pick is re-checked by the
            // caller, so a dim port still returns (the user gets a
            // specific reason rather than a generic rejection).
        }
    }

    /// Bridge the telnet session directly to one of the configured
    /// serial ports.  Always presents an A/B picker first; the chosen
    /// port must be `enabled = true` with `mode = "console"`.
    ///
    /// The escape sequence is two consecutive ESC presses (PETSCII `<-`
    /// on Commodore terminals).  A single ESC is forwarded to the wire
    /// after one read cycle, so editors that need ESC (vi, ed) keep
    /// working as long as the user types a normal key after each ESC.
    pub(in crate::telnet) async fn gateway_serial(&mut self) -> Result<(), std::io::Error> {
        // Always render a picker — even if only one port is eligible
        // — so the user can see both ports' status side-by-side and
        // the menu structure stays consistent regardless of config.
        match self.gateway_serial_picker().await? {
            None => Ok(()),
            Some(GatewayPick::Local(id)) => self.gateway_serial_local(id).await,
            Some(GatewayPick::Remote { ip, label }) => {
                self.gateway_serial_remote(ip, label).await
            }
        }
    }

    /// Bridge to a local serial port (the original Serial Gateway path).
    pub(in crate::telnet) async fn gateway_serial_local(
        &mut self,
        id: crate::config::SerialPortId,
    ) -> Result<(), std::io::Error> {
        // Bridging the modem-emulator's own port back into the session
        // that arrived over that very port would loop the user's
        // terminal to itself — a footgun.  Reject *only* that case: a
        // serial-arrived user may still bridge to a different port.
        // (The picker already marks the arrival port ineligible, so this
        // is the belt-and-braces guard against a stale pick.)
        if self.is_own_arrival_port(id) {
            self.show_error_lines(&[
                "Cannot bridge a serial port to",
                "itself.  Pick a different port.",
            ])
            .await?;
            return Ok(());
        }

        let cfg = config::get_config();
        let port_cfg = cfg.port(id).clone();
        // A console-mode target connects directly; a modem-mode target is
        // rung (peer-dial) and answers per its own AT rules.  Re-validate
        // under the picked id — mode/eligibility might have changed since
        // the picker rendered (operator could have toggled it elsewhere).
        let is_console = port_cfg.mode == "console";
        if is_console {
            if let Err(e) = crate::serial::check_console_bridge_eligible(&cfg, id) {
                self.show_error_lines(&["Could not acquire serial port:", "", e.as_str()])
                    .await?;
                return Ok(());
            }
        } else if !cfg.allow_peer_dial || !port_cfg.enabled || port_cfg.port.is_empty() {
            // Modem-mode target requires the peer-dial opt-in and a live port.
            self.show_error_lines(&[
                "That port can't be dialed.",
                "",
                "Enable peer-dial (Serial Config > P)",
                "and give the modem port a device.",
            ])
            .await?;
            return Ok(());
        }

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };

        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        self.send_line(&format!(
            "  {}",
            self.yellow(&format!("SERIAL GATEWAY (PORT {})", id.label()))
        ))
        .await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        // Stack the port info so a long device path
        // (e.g. /dev/ttyUSB10) can never overflow the 40-col
        // PETSCII width.
        self.send_line(&format!(
            "  Port: {}",
            self.amber(&port_cfg.port)
        ))
        .await?;
        self.send_line(&format!(
            "  Baud: {}",
            self.amber(&port_cfg.baud.to_string())
        ))
        .await?;
        self.send_line(&format!(
            "  Data: {}{}{} flow={}",
            port_cfg.databits,
            port_cfg.parity.chars().next().unwrap_or('N').to_uppercase(),
            port_cfg.stopbits,
            port_cfg.flowcontrol,
        ))
        .await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Press {} {} to disconnect.",
            self.cyan(esc_label),
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("  Single ESC passes through on the").await?;
        self.send_line("  next keystroke.").await?;
        self.send_line("").await?;
        self.send(&format!(
            "  {} ",
            self.cyan("Connect now? (Y/N):")
        ))
        .await?;
        self.flush().await?;

        let confirm = match self.read_byte_filtered().await? {
            Some(b) => b,
            None => return Ok(()),
        };
        // Terminate the prompt line.  The user's terminal supplies
        // its own echo of `Y` (or its absence) — the gateway only
        // emits a CRLF here so subsequent output starts cleanly,
        // matching the convention used by `modem_apply_settings`.
        self.send_line("").await?;
        if confirm != b'Y' && confirm != b'y' {
            return Ok(());
        }

        // Acquire the bridge BEFORE printing "Connected." so the
        // user doesn't see a confusing "Connected." followed
        // immediately by an acquisition error.  The request returns
        // quickly when the serial-manager loop is healthy (it polls
        // the slot every 150 ms).
        self.send_line(&format!(
            "  {}",
            self.dim(if is_console { "Acquiring serial port..." } else { "Ringing port..." })
        ))
        .await?;
        self.flush().await?;
        let bridge = if is_console {
            match crate::serial::request_console_bridge(id).await {
                Ok(b) => b,
                Err(e) => {
                    self.show_error_lines(&["Could not acquire serial port:", "", e.as_str()])
                        .await?;
                    return Ok(());
                }
            }
        } else {
            // Ring the modem-mode target; it answers per its own AT rules
            // (S0 auto-answer / manual ATA).  ~30 s covers the default
            // S0=5 at the 6 s ring cadence, plus a manual answer.
            use crate::serial::PeerCallOutcome;
            match crate::serial::request_peer_call(id, std::time::Duration::from_secs(30)).await {
                Ok(b) => b,
                Err(outcome) => {
                    let why = match outcome {
                        PeerCallOutcome::Busy => "That port is busy (in a call).",
                        PeerCallOutcome::NoAnswer => "No answer.",
                        _ => "The call could not be completed.",
                    };
                    self.show_error_lines(&["Could not connect:", "", why]).await?;
                    return Ok(());
                }
            }
        };

        self.send_line(&format!(
            "  {}",
            self.green("Connected.")
        ))
        .await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.run_serial_console_loop(bridge).await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Serial bridge closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let idle_timeout = std::time::Duration::from_secs(
            config::get_config().idle_timeout_secs,
        );
        if idle_timeout.is_zero() {
            let _ = self.wait_for_key().await;
        } else {
            let _ = tokio::time::timeout(idle_timeout, self.wait_for_key()).await;
        }
        result
    }

    /// Bridge to a registered **remote** console port on a slave (§9 #12).
    /// The master reaches inward: claim the slave's idle registration
    /// channel, send the one-byte activate signal so the slave starts
    /// bridging its UART, then run the same console pump against the
    /// channel.  Dropping the stream at the end closes the channel, which
    /// the slave sees as end-of-bridge (it re-registers).
    pub(in crate::telnet) async fn gateway_serial_remote(
        &mut self,
        ip: IpAddr,
        label: String,
    ) -> Result<(), std::io::Error> {
        use tokio::io::AsyncWriteExt;

        let esc_label = match self.terminal_type {
            TerminalType::Petscii => "<-",
            _ => "ESC",
        };
        self.clear_screen().await?;
        let sep = self.separator();
        self.send_line(&sep).await?;
        let title = truncate_to_width(&format!("REMOTE: {}@{}", label, ip), 36);
        self.send_line(&format!("  {}", self.yellow(&title))).await?;
        self.send_line(&sep).await?;
        self.send_line("").await?;
        self.send_line(&format!(
            "  Press {} {} to disconnect.",
            self.cyan(esc_label),
            self.cyan(esc_label)
        ))
        .await?;
        self.send_line("").await?;
        self.send(&format!("  {} ", self.cyan("Connect now? (Y/N):")))
            .await?;
        self.flush().await?;
        let confirm = match self.read_byte_filtered().await? {
            Some(b) => b,
            None => return Ok(()),
        };
        self.send_line("").await?;
        if confirm != b'Y' && confirm != b'y' {
            return Ok(());
        }

        // Claim the registration channel (removes it from the registry so
        // no other master user can grab the same port).
        let Some(mut stream) = crate::relay::remove_remote_port(ip, &label) else {
            self.show_error_lines(&[
                "That remote port is no longer",
                "available (slave disconnected).",
            ])
            .await?;
            return Ok(());
        };
        // Signal the slave that a user attached so it starts bridging its
        // UART (the byte is consumed by the slave, never reaches the user).
        if stream
            .write_all(&[crate::relay::RELAY_ACTIVATE_BYTE])
            .await
            .is_err()
            || stream.flush().await.is_err()
        {
            self.show_error_lines(&[
                "Remote port went away before",
                "the bridge could start.",
            ])
            .await?;
            return Ok(());
        }

        self.send_line(&format!("  {}", self.green("Connected."))).await?;
        self.send_line("").await?;
        self.flush().await?;

        let result = self.run_serial_console_loop(stream).await;

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.yellow("Remote serial bridge closed.")
        ))
        .await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let idle_timeout =
            std::time::Duration::from_secs(config::get_config().idle_timeout_secs);
        if idle_timeout.is_zero() {
            let _ = self.wait_for_key().await;
        } else {
            let _ = tokio::time::timeout(idle_timeout, self.wait_for_key()).await;
        }
        result
    }

    /// Inner pump loop for the Serial Gateway.  Reads bytes from the
    /// telnet session and writes them to the serial bridge; reads
    /// bytes from the bridge and writes them back to the session.
    /// Exits cleanly on double-ESC or when either side closes.
    pub(in crate::telnet) async fn run_serial_console_loop(
        &mut self,
        bridge: tokio::io::DuplexStream,
    ) -> Result<(), std::io::Error> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge);

        let reader = &mut self.reader;
        let writer = &self.writer;
        let is_petscii = self.terminal_type == TerminalType::Petscii;
        let erase_char = self.erase_char;
        // Idle bound for the bridge (see gateway_ssh): disconnect a
        // half-open client so it can't pin the session's max_sessions
        // slot.  Zero disables it.
        let idle_timeout = self.idle_timeout;
        let mut last_was_esc = false;
        let esc_byte: u8 = if is_petscii { 0x5F } else { 0x1B };

        let mut bridge_buf = [0u8; 4096];

        loop {
            tokio::select! {
                _ = tokio::time::sleep(idle_timeout), if !idle_timeout.is_zero() => {
                    // Idle in both directions past the timeout window —
                    // disconnect so a half-open client can't pin the
                    // session and leak its max_sessions slot.
                    break;
                }
                event = read_gateway_event(reader) => {
                    match event {
                        Ok(GatewayInboundEvent::Data(b)) if is_esc_key(b, is_petscii) => {
                            if last_was_esc {
                                break; // double-ESC — exit bridge
                            }
                            last_was_esc = true;
                        }
                        Ok(GatewayInboundEvent::Data(b)) => {
                            if last_was_esc {
                                last_was_esc = false;
                                let e = if is_petscii {
                                    petscii_to_ascii_byte(esc_byte)
                                } else {
                                    esc_byte
                                };
                                if bridge_write.write_all(&[e]).await.is_err() {
                                    break;
                                }
                            }
                            let b = if is_petscii { petscii_to_ascii_byte(b) } else { b };
                            // Map an unusual erase byte (e.g. PETSCII
                            // 0x14) back to ASCII DEL so editors that
                            // expect 0x7F see what they expect.
                            let b = if b == erase_char && erase_char != 0x7F { 0x7F } else { b };
                            if bridge_write.write_all(&[b]).await.is_err() {
                                break;
                            }
                            if bridge_write.flush().await.is_err() {
                                break;
                            }
                        }
                        Ok(GatewayInboundEvent::NawsResize(_, _)) => {
                            // No way to tell the wire about a window
                            // resize; ignore.
                        }
                        Ok(GatewayInboundEvent::Eof) => break,
                        Err(_) => break,
                    }
                }
                n = bridge_read.read(&mut bridge_buf) => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = &bridge_buf[..n];
                            let mut w = writer.lock().await;
                            // Always IAC-escape on the wire to the
                            // local user — they're a real telnet peer
                            // and a literal 0xFF would be misread as
                            // IAC.
                            if write_telnet_data(&mut **w, data).await.is_err() {
                                break;
                            }
                            if w.flush().await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        let _ = bridge_write.shutdown().await;
        Ok(())
    }
}
