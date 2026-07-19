//! Session I/O primitives: telnet IAC-aware read/write, output helpers
//! (send/send_line/flush/clear), option negotiation + subnegotiation,
//! line/menu/password input loops, drain helpers, ZMODEM autostart
//! detection, and shared help/error paging.
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

/// Read a single byte, filtering out telnet IAC protocol sequences.
pub(crate) async fn read_byte_iac_filtered(
    reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
    filter_iac: bool,
) -> Result<Option<u8>, std::io::Error> {
    let mut buf = [0u8; 1];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(None),
            Ok(_) => {
                let byte = buf[0];
                if filter_iac && byte == 0xFF {
                    match reader.read(&mut buf).await {
                        Ok(0) => return Ok(None),
                        Ok(_) => {
                            let cmd = buf[0];
                            if cmd == 0xFF {
                                return Ok(Some(0xFF));
                            }
                            if cmd == 0xFA {
                                // Subnegotiation — consume until IAC SE.  Bound
                                // each in-SB read so a peer can't pin us by
                                // dribbling an SB that never terminates; a
                                // stalled SB is treated as a closed connection.
                                let mut in_iac = false;
                                loop {
                                    match tokio::time::timeout(
                                        SB_DRAIN_TIMEOUT,
                                        reader.read(&mut buf),
                                    )
                                    .await
                                    {
                                        Err(_) => return Ok(None),
                                        Ok(Ok(0)) => return Ok(None),
                                        Ok(Ok(_)) => {
                                            if in_iac {
                                                if buf[0] == 0xF0 {
                                                    break;
                                                }
                                                in_iac = false;
                                            } else if buf[0] == 0xFF {
                                                in_iac = true;
                                            }
                                        }
                                        Ok(Err(e)) => return Err(e),
                                    }
                                }
                                continue;
                            }
                            // WILL/WONT/DO/DONT — consume the option byte
                            if (0xFB..=0xFE).contains(&cmd) {
                                match reader.read(&mut buf).await {
                                    Ok(0) => return Ok(None),
                                    Err(e) => return Err(e),
                                    _ => {}
                                }
                            }
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                return Ok(Some(byte));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write `bytes` to `w`, doubling any 0xFF as IAC IAC per RFC 854.  Used
/// by the outgoing telnet gateway in both directions so that literal 0xFF
/// data bytes survive the wire without being mistaken for IAC.
pub(crate) async fn write_telnet_data<W>(w: &mut W, bytes: &[u8]) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin + ?Sized,
{
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == IAC {
            if last < i {
                w.write_all(&bytes[last..i]).await?;
            }
            w.write_all(&[IAC, IAC]).await?;
            last = i + 1;
        }
    }
    if last < bytes.len() {
        w.write_all(&bytes[last..]).await?;
    }
    Ok(())
}

impl TelnetSession {
    // ─── I/O helpers ───────────────────────────────────────

    pub(in crate::telnet) async fn send(&mut self, text: &str) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => {
                let swapped = swap_case_for_petscii(text);
                let bytes = to_latin1_bytes(&swapped);
                self.send_raw(&bytes).await
            }
            _ => self.send_raw(text.as_bytes()).await,
        }
    }

    pub(in crate::telnet) async fn send_line(&mut self, text: &str) -> Result<(), std::io::Error> {
        let line = format!("{}\r\n", text);
        self.send(&line).await
    }

    /// Write user-data bytes to the session. In telnet mode, any 0xFF
    /// data byte is escaped as IAC IAC (0xFF 0xFF) per RFC 854 so the
    /// peer doesn't misinterpret it as the start of a protocol command.
    /// Serial and SSH sessions don't speak the IAC protocol, so bytes
    /// pass through unchanged there.
    pub(in crate::telnet) async fn send_raw(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        let needs_escape = !self.is_serial && !self.is_ssh;
        if !needs_escape || !bytes.contains(&IAC) {
            return self.writer.lock().await.write_all(bytes).await;
        }
        let mut escaped = Vec::with_capacity(bytes.len() + 1);
        for &b in bytes {
            escaped.push(b);
            if b == IAC {
                escaped.push(IAC);
            }
        }
        self.writer.lock().await.write_all(&escaped).await
    }

    /// Write raw telnet-protocol bytes (IAC sequences) without any data
    /// escaping. Use only for sending IAC commands and option
    /// negotiation where 0xFF bytes are intentional.
    pub(in crate::telnet) async fn send_telnet_protocol(&mut self, bytes: &[u8]) -> Result<(), std::io::Error> {
        self.writer.lock().await.write_all(bytes).await
    }

    pub(in crate::telnet) async fn flush(&mut self) -> Result<(), std::io::Error> {
        self.writer.lock().await.flush().await
    }

    pub(in crate::telnet) async fn clear_screen(&mut self) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => self.send_raw(&[PETSCII_CLEAR]).await,
            TerminalType::Ansi => self.send_raw(ANSI_CLEAR.as_bytes()).await,
            TerminalType::Ascii => self.send_raw(b"\r\n\r\n\r\n").await,
        }
    }

    pub(in crate::telnet) async fn read_byte_filtered(&mut self) -> Result<Option<u8>, std::io::Error> {
        if self.idle_timeout.is_zero() {
            self.session_read_byte().await
        } else {
            match tokio::time::timeout(self.idle_timeout, self.session_read_byte()).await {
                Ok(result) => result,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "idle timeout",
                )),
            }
        }
    }

    /// Read a single data byte from the session. In telnet mode, IAC
    /// sequences are consumed transparently. DO/WILL option requests
    /// get WONT/DONT replies (RFC 855) except for options we support
    /// (ECHO, SGA, TTYPE, NAWS). AYT (Are You There) gets a visible
    /// reply. IP (Interrupt Process) and BRK (Break) surface as the
    /// terminal's ESC byte so callers treat them like a Ctrl+C / ESC.
    pub(in crate::telnet) async fn session_read_byte(&mut self) -> Result<Option<u8>, std::io::Error> {
        if let Some(b) = self.pushback.take() {
            return Ok(Some(b));
        }
        let filter_iac = !self.is_serial && !self.is_ssh;
        let mut buf = [0u8; 1];
        loop {
            // `mid_iac_cmd` is a resume point: if a previous call was cancelled
            // (e.g. the CP/M out-of-band drain's zero-timeout) after consuming
            // an IAC but before its command byte, skip re-reading the data byte
            // and read the command directly — so the IAC isn't lost and telnet
            // parsing stays in sync.  Normal (uncancelled) callers see the flag
            // toggle within one call, so behavior is unchanged.
            if !self.mid_iac_cmd {
                if self.reader.read(&mut buf).await? == 0 {
                    return Ok(None);
                }
                let byte = buf[0];
                if !filter_iac || byte != IAC {
                    return Ok(Some(byte));
                }
                // Committed to an IAC sequence: mark the resume point before
                // the (cancellable) command-byte read.
                self.mid_iac_cmd = true;
            }
            if self.reader.read(&mut buf).await? == 0 {
                self.mid_iac_cmd = false;
                return Ok(None);
            }
            self.mid_iac_cmd = false;
            let cmd = buf[0];
            match cmd {
                IAC => return Ok(Some(IAC)), // escaped data 0xFF
                SB => {
                    self.telnet_negotiated = true;
                    let Some(payload) = self.read_subneg_payload().await? else {
                        return Ok(None);
                    };
                    if let Some((opt, body)) = payload.split_first() {
                        self.handle_subnegotiation(*opt, body).await?;
                    }
                }
                WILL | WONT | DO | DONT => {
                    self.telnet_negotiated = true;
                    if self.reader.read(&mut buf).await? == 0 {
                        return Ok(None);
                    }
                    let opt = buf[0];
                    self.handle_telnet_option(cmd, opt).await?;
                }
                AYT => {
                    // Through send_line so PETSCII case-swap applies if the
                    // terminal type is known.
                    self.send_line("[Yes]").await?;
                    self.flush().await?;
                }
                IP | BRK => {
                    let esc = if self.terminal_type == TerminalType::Petscii {
                        0x5F
                    } else {
                        0x1B
                    };
                    return Ok(Some(esc));
                }
                EC => {
                    // RFC 854: delete the last received character.  Our
                    // architecture has no low-level input buffer, so
                    // translate to DEL (0x7F); the line-input layer
                    // already handles this as backspace.
                    return Ok(Some(0x7F));
                }
                EL => {
                    // RFC 854: delete everything on the current line.
                    // Translate to NAK (0x15); the line-input loop
                    // treats this as "erase-line."
                    return Ok(Some(LINE_ERASE_BYTE));
                }
                _ => {
                    // NOP (241), DM (242), AO (245), GA (249) — consumed.
                    //
                    // DM is the SYNCH marker (RFC 854 §3).  Proper SYNCH
                    // requires reading TCP urgent-mode data; we do not
                    // implement that, so DM is informational only.
                }
            }
        }
    }

    /// Consume a subnegotiation payload up to (and including) the
    /// terminating IAC SE. Returns the payload bytes with any escaped
    /// `IAC IAC` unescaped. First byte is the option code. Returns
    /// Ok(None) if the connection closes mid-sequence.
    ///
    /// Each read is bounded by `SB_DRAIN_TIMEOUT` (slowloris guard): a peer
    /// that sends `IAC SB <opt>` and then stalls without `IAC SE` must not
    /// pin the session task and its `max_sessions` slot indefinitely.  This
    /// guard is independent of `idle_timeout_secs` (which can be 0 = off),
    /// matching the two gateway-path SB readers, which bound the identical
    /// loop the same way regardless of idle config.  A stalled drain is
    /// treated as a closed connection (Ok(None)).
    pub(in crate::telnet) async fn read_subneg_payload(&mut self) -> Result<Option<Vec<u8>>, std::io::Error> {
        let mut payload = Vec::with_capacity(32);
        let mut buf = [0u8; 1];
        loop {
            match tokio::time::timeout(SB_DRAIN_TIMEOUT, self.reader.read(&mut buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
            }
            if buf[0] != IAC {
                if payload.len() < 512 {
                    payload.push(buf[0]);
                }
                continue;
            }
            match tokio::time::timeout(SB_DRAIN_TIMEOUT, self.reader.read(&mut buf)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
            }
            match buf[0] {
                SE => return Ok(Some(payload)),
                IAC => {
                    if payload.len() < 512 {
                        payload.push(IAC);
                    }
                }
                _ => {
                    // Malformed — skip and keep scanning for IAC SE.
                }
            }
        }
    }

    /// Reply to peer WILL/WONT/DO/DONT per RFC 855. Options we want
    /// enabled (ECHO, SGA on our side; SGA, TTYPE, NAWS on peer's side)
    /// treat the matching ack as a no-op. Everything else is refused
    /// once. DONT/WONT get a matching ack only if we had actually
    /// advertised the corresponding WILL/DO.
    pub(in crate::telnet) async fn handle_telnet_option(
        &mut self,
        cmd: u8,
        opt: u8,
    ) -> Result<(), std::io::Error> {
        match cmd {
            DO if opt == OPT_TIMING_MARK => {
                // RFC 860: DO TIMING-MARK is a one-shot synchronization
                // request — reply with WILL TIMING-MARK *after* we have
                // flushed whatever output was queued when the DO arrived.
                // The WILL response is itself the mark; no persistent
                // state (so we don't set neg_sent_will).
                self.flush().await?;
                self.send_telnet_protocol(&[IAC, WILL, OPT_TIMING_MARK]).await?;
                self.flush().await?;
            }
            DONT if opt == OPT_TIMING_MARK => {
                // RFC 860: DONT TIMING-MARK has no action to ack since
                // we never maintain the option as enabled.
            }
            DO if opt == OPT_STATUS => {
                // RFC 859: agree to act as the status sender.  Mark
                // neg_sent_will so the peer's future DOs are treated as
                // acks and we don't loop.  A later SB STATUS SEND will
                // trigger the actual state dump.
                if !self.neg_sent_will[OPT_STATUS as usize] {
                    self.neg_sent_will[OPT_STATUS as usize] = true;
                    self.send_telnet_protocol(&[IAC, WILL, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            DONT if opt == OPT_STATUS => {
                // Peer withdraws the status-sender role.  Ack with WONT
                // only if we had asserted WILL.
                if self.neg_sent_will[OPT_STATUS as usize] {
                    self.neg_sent_will[OPT_STATUS as usize] = false;
                    self.send_telnet_protocol(&[IAC, WONT, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            WILL if opt == OPT_STATUS => {
                // We don't request status from clients — refuse.
                if !self.neg_sent_dont[OPT_STATUS as usize] {
                    self.neg_sent_dont[OPT_STATUS as usize] = true;
                    self.send_telnet_protocol(&[IAC, DONT, OPT_STATUS]).await?;
                    self.flush().await?;
                }
            }
            DO => {
                // If we already advertised WILL for opt, peer's DO is an
                // acknowledgement — no reply needed.
                if self.neg_sent_will[opt as usize] {
                    return Ok(());
                }
                if self.neg_sent_wont[opt as usize] {
                    return Ok(());
                }
                self.neg_sent_wont[opt as usize] = true;
                self.send_telnet_protocol(&[IAC, WONT, opt]).await?;
                self.flush().await?;
            }
            WILL => {
                // If we already advertised DO for opt, peer's WILL is an
                // acknowledgement — no reply needed.
                if self.neg_sent_do[opt as usize] && opt != OPT_TTYPE {
                    // TTYPE still needs SB SEND on first WILL so we can
                    // request the name; handled below.
                    return Ok(());
                }
                if opt == OPT_TTYPE {
                    if !self.neg_sent_do[opt as usize] {
                        self.neg_sent_do[opt as usize] = true;
                        self.send_telnet_protocol(&[IAC, DO, OPT_TTYPE]).await?;
                    }
                    if !self.ttype_matched {
                        self.send_telnet_protocol(&[
                            IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE,
                        ])
                        .await?;
                    }
                    self.flush().await?;
                    return Ok(());
                }
                if opt == OPT_NAWS {
                    if !self.neg_sent_do[opt as usize] {
                        self.neg_sent_do[opt as usize] = true;
                        self.send_telnet_protocol(&[IAC, DO, OPT_NAWS]).await?;
                        self.flush().await?;
                    }
                    return Ok(());
                }
                if self.neg_sent_dont[opt as usize] {
                    return Ok(());
                }
                self.neg_sent_dont[opt as usize] = true;
                self.send_telnet_protocol(&[IAC, DONT, opt]).await?;
                self.flush().await?;
            }
            DONT => {
                // Acknowledge with WONT only if we had previously
                // advertised WILL for opt.
                if self.neg_sent_will[opt as usize]
                    && !self.neg_sent_wont[opt as usize]
                {
                    self.neg_sent_wont[opt as usize] = true;
                    self.send_telnet_protocol(&[IAC, WONT, opt]).await?;
                    self.flush().await?;
                }
            }
            WONT => {
                // Acknowledge with DONT only if we had previously
                // advertised DO for opt.
                if self.neg_sent_do[opt as usize]
                    && !self.neg_sent_dont[opt as usize]
                {
                    self.neg_sent_dont[opt as usize] = true;
                    self.send_telnet_protocol(&[IAC, DONT, opt]).await?;
                    self.flush().await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Process a completed subnegotiation. `body` is the payload after
    /// the option code. TTYPE IS sets terminal_type if the reported
    /// name is recognized; NAWS stores the reported window dimensions.
    pub(in crate::telnet) async fn handle_subnegotiation(
        &mut self,
        opt: u8,
        body: &[u8],
    ) -> Result<(), std::io::Error> {
        match opt {
            OPT_TTYPE => {
                if body.first().copied() == Some(TTYPE_IS) && !self.ttype_matched {
                    let name_bytes = &body[1..];
                    let name: String = name_bytes
                        .iter()
                        .map(|&b| b as char)
                        .filter(|c| !c.is_control())
                        .collect();
                    // Record what the client announced even when we don't
                    // recognize it, so the gateway-debug terminal diagnostic
                    // can show the exact name that failed to match.
                    self.ttype_raw = Some(name.clone());
                    if let Some(tt) = match_terminal_name(&name) {
                        self.terminal_type = tt;
                        self.ttype_matched = true;
                    }
                }
            }
            OPT_STATUS => {
                // RFC 859: only the SEND request needs a response.  The
                // IS variant (a peer dumping its state to us) is ignored
                // — we don't maintain a model of peer options.
                if body.first().copied() == Some(STATUS_SEND)
                    && self.neg_sent_will[OPT_STATUS as usize]
                {
                    self.send_status_is().await?;
                }
            }
            OPT_NAWS => {
                if body.len() >= 4 {
                    let w = u16::from_be_bytes([body[0], body[1]]);
                    let h = u16::from_be_bytes([body[2], body[3]]);
                    if w > 0 {
                        self.window_width = Some(w);
                    }
                    if h > 0 {
                        self.window_height = Some(h);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Emit `IAC SB STATUS IS <state> IAC SE` in response to a peer's
    /// `IAC SB STATUS SEND IAC SE` (RFC 859).
    ///
    /// The state body is a concatenation of `WILL opt` and `DO opt`
    /// triplets for every option we have advertised and not had denied.
    /// Any 0xFF byte inside the body (none of our opts are 0xFF, but the
    /// RFC requires it) is doubled per IAC escaping rules.
    pub(in crate::telnet) async fn send_status_is(&mut self) -> Result<(), std::io::Error> {
        let mut body = vec![IAC, SB, OPT_STATUS, STATUS_IS];
        for opt in 0u8..=255u8 {
            let idx = opt as usize;
            if self.neg_sent_will[idx] && !self.neg_sent_wont[idx] {
                body.push(WILL);
                if opt == IAC {
                    body.push(IAC);
                }
                body.push(opt);
            }
            if self.neg_sent_do[idx] && !self.neg_sent_dont[idx] {
                body.push(DO);
                if opt == IAC {
                    body.push(IAC);
                }
                body.push(opt);
            }
            if opt == 255 {
                break;
            }
        }
        body.push(IAC);
        body.push(SE);
        self.send_telnet_protocol(&body).await?;
        self.flush().await
    }

    /// Consume up to `max` immediately-queued CR/LF/NUL bytes left behind by a
    /// linemode telnet client (e.g. the `\n` of a CRLF pair, or the `\0` of the
    /// NVT `CR NUL` that a telnet client sends for a bare Enter, after a menu
    /// selection or line submit). Uses a short read timeout so nothing is eaten
    /// in char-at-a-time mode. Any other byte seen is pushed back for the next
    /// real input call, so no keystrokes are lost.
    ///
    /// The NUL matters for the CP/M emulator: without draining it, the `\0` of
    /// a `CR NUL` Enter lingered as `pushback` past the command-line read and
    /// was then consumed by the launched program's first console read (e.g. a
    /// `Y/N` prompt read via BDOS 1/6), skipping the prompt.
    pub(in crate::telnet) async fn drain_trailing_eol(&mut self, max: usize) {
        if self.pushback.is_some() {
            return;
        }
        for _ in 0..max {
            let res = tokio::time::timeout(
                std::time::Duration::from_millis(20),
                self.session_read_byte(),
            )
            .await;
            match res {
                Ok(Ok(Some(b))) if b == b'\r' || b == b'\n' || b == 0 => continue,
                Ok(Ok(Some(b))) => {
                    self.pushback = Some(b);
                    return;
                }
                _ => return,
            }
        }
    }

    pub(in crate::telnet) async fn echo_backspace(&mut self) -> Result<(), std::io::Error> {
        match self.terminal_type {
            TerminalType::Petscii => self.send_raw(&[0x9D, 0x20, 0x9D]).await,
            _ => self.send_raw(&[0x08, 0x20, 0x08]).await,
        }
    }

    pub(in crate::telnet) async fn get_line_input(&mut self) -> Result<Option<String>, std::io::Error> {
        self.read_input_loop(&mut Vec::new(), InputMode::Normal).await
    }

    pub(in crate::telnet) async fn get_password_input(&mut self) -> Result<Option<String>, std::io::Error> {
        self.read_input_loop(&mut Vec::new(), InputMode::Password).await
    }

    /// Core input loop shared by `get_line_input` and `get_password_input`.
    /// In `Normal` mode, typed characters are echoed
    /// and the result is trimmed. In `Password` mode, `*` is echoed instead and
    /// the result is returned untrimmed.
    pub(in crate::telnet) async fn read_input_loop(
        &mut self,
        buf: &mut Vec<u8>,
        mode: InputMode,
    ) -> Result<Option<String>, std::io::Error> {
        let is_password = matches!(mode, InputMode::Password);
        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };

            if byte == b'\r' || byte == b'\n' {
                self.send_raw(b"\r\n").await?;
                self.flush().await?;
                // Drain the paired byte of a CRLF (or LFCR) so the next
                // prompt isn't silently satisfied by a leftover newline.
                self.drain_trailing_eol(1).await;
                let result: String = if self.terminal_type == TerminalType::Petscii {
                    buf.iter()
                        .map(|&b| petscii_to_ascii_byte(b) as char)
                        .collect()
                } else {
                    buf.iter().map(|&b| b as char).collect()
                };
                return Ok(Some(if is_password {
                    result
                } else {
                    result.trim().to_string()
                }));
            }

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                self.drain_input().await;
                return Ok(None);
            }

            if is_backspace_key(byte, self.erase_char) {
                if !buf.is_empty() {
                    buf.pop();
                    self.echo_backspace().await?;
                    self.flush().await?;
                }
                continue;
            }

            if byte == LINE_ERASE_BYTE {
                // RFC 854 EL (delivered by session_read_byte as 0x15).
                // Erase the current line both in the buffer and on the
                // user's terminal.
                while !buf.is_empty() {
                    buf.pop();
                    self.echo_backspace().await?;
                }
                self.flush().await?;
                continue;
            }

            if byte < 0x20 {
                continue;
            }

            if buf.len() >= MAX_INPUT_LENGTH {
                self.send_raw(b"\r\n").await?;
                self.show_error("Input too long.").await?;
                return Ok(None);
            }

            if is_password {
                self.send_raw(b"*").await?;
            } else {
                self.send_raw(&[byte]).await?;
            }
            self.flush().await?;
            buf.push(byte);
        }
    }

    pub(in crate::telnet) async fn get_menu_input(
        &mut self,
        instant_digits: bool,
    ) -> Result<Option<String>, std::io::Error> {
        // ZMODEM autostart detection state.  A compliant ZMODEM sender
        // opens a transfer with `** ZDLE <header-type>` where
        // `<header-type>` is one of `A` (binary/CRC-16), `B` (hex),
        // or `C` (binary/CRC-32).  Reading the full four-byte prefix
        // off the menu input loop is an unambiguous "the user's
        // terminal just tried to auto-start a ZMODEM transfer" signal
        // — bridge directly into the ZMODEM receive flow so the upload
        // succeeds without the user having to navigate the menu first.
        let mut zmodem_state: u8 = 0;
        loop {
            let byte = match self.read_byte_filtered().await? {
                Some(b) => b,
                None => return Ok(None),
            };

            // ZMODEM autostart: **\x18[ABC].
            match (zmodem_state, byte) {
                (0, b'*') => {
                    zmodem_state = 1;
                    continue;
                }
                (1, b'*') => {
                    zmodem_state = 2;
                    continue;
                }
                (2, 0x18) => {
                    zmodem_state = 3;
                    continue;
                }
                (3, b'A') | (3, b'B') | (3, b'C') => {
                    self.handle_zmodem_autostart().await?;
                    // Bounce back to the caller so the menu redraws.
                    return Ok(None);
                }
                _ => {
                    zmodem_state = 0;
                    // Fall through and process `byte` normally.
                }
            }

            if is_esc_key(byte, self.terminal_type == TerminalType::Petscii) {
                self.drain_input().await;
                return Ok(None);
            }

            if byte == b'\r' || byte == b'\n' {
                continue;
            }
            if is_backspace_key(byte, self.erase_char) {
                continue;
            }
            if byte < 0x20 {
                continue;
            }

            let ch = if self.terminal_type == TerminalType::Petscii {
                (petscii_to_ascii_byte(byte) as char).to_ascii_lowercase()
            } else {
                (byte as char).to_ascii_lowercase()
            };

            if ch.is_ascii_alphabetic() {
                self.send_raw(&[byte]).await?;
                self.send_raw(b"\r\n").await?;
                self.flush().await?;
                // Linemode clients send `letter\r\n`; drop the trailing
                // CRLF so a follow-up prompt isn't auto-submitted.
                self.drain_trailing_eol(2).await;
                return Ok(Some(ch.to_string()));
            }

            if ch.is_ascii_digit() {
                if instant_digits {
                    self.send_raw(&[byte]).await?;
                    self.send_raw(b"\r\n").await?;
                    self.flush().await?;
                    self.drain_trailing_eol(2).await;
                    return Ok(Some(ch.to_string()));
                }

                self.send_raw(&[byte]).await?;
                self.flush().await?;
                let mut input = String::new();
                input.push(ch);

                loop {
                    let b2 = match self.read_byte_filtered().await? {
                        Some(b) => b,
                        None => return Ok(None),
                    };

                    if b2 == b'\r' || b2 == b'\n' {
                        self.send_raw(b"\r\n").await?;
                        self.flush().await?;
                        self.drain_trailing_eol(1).await;
                        return Ok(Some(input));
                    }

                    if is_esc_key(b2, self.terminal_type == TerminalType::Petscii) {
                        self.drain_input().await;
                        return Ok(None);
                    }

                    if is_backspace_key(b2, self.erase_char) {
                        if !input.is_empty() {
                            input.pop();
                            self.echo_backspace().await?;
                            self.flush().await?;
                        }
                        continue;
                    }

                    if b2 < 0x20 {
                        continue;
                    }

                    let ch2 = if self.terminal_type == TerminalType::Petscii {
                        petscii_to_ascii_byte(b2) as char
                    } else {
                        b2 as char
                    };

                    if ch2.is_ascii_digit() && input.len() < MAX_INPUT_LENGTH {
                        self.send_raw(&[b2]).await?;
                        self.flush().await?;
                        input.push(ch2);
                    }
                }
            }

            self.send_raw(&[byte]).await?;
            self.send_raw(b"\r\n").await?;
            self.flush().await?;
            self.drain_trailing_eol(2).await;
            return Ok(Some(ch.to_string()));
        }
    }

    pub(in crate::telnet) async fn wait_for_key(&mut self) -> Result<(), std::io::Error> {
        loop {
            match self.read_byte_filtered().await? {
                Some(b)
                    if b >= 0x20
                        || b == b'\r'
                        || b == b'\n'
                        || is_esc_key(b, self.terminal_type == TerminalType::Petscii) =>
                {
                    return Ok(());
                }
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "disconnected",
                    ));
                }
                _ => continue,
            }
        }
    }

    pub(in crate::telnet) async fn drain_input(&mut self) {
        self.drain_input_until_quiet(50, None).await;
    }

    /// Read and discard pending input until the line is quiet for `quiet_ms`,
    /// optionally capped at `max_ms` total wall-clock so a peer that is still
    /// actively streaming can't stall us forever.
    ///
    /// The default `drain_input` uses a short 50ms gap, fine for clearing the
    /// dribble left by a menu keystroke.  Before a *file transfer* we drain
    /// with a longer gap (see the transfer-start call sites): at 1200 baud a
    /// 50ms gap is only ~6 char-times, so a peer flushing its serial buffer in
    /// a late burst — e.g. CCGMS after a silent Punter cancel, which sends no
    /// wire byte to signal the abort — can dribble stale bytes past a 50ms
    /// drain and have them mistaken for the next transfer's opening handshake.
    /// This drain runs after we print "start within N seconds" but before the
    /// human has started their sender, so a longer gap never eats a legitimate
    /// opening code.
    pub(in crate::telnet) async fn drain_input_until_quiet(&mut self, quiet_ms: u64, max_ms: Option<u64>) {
        let deadline = max_ms
            .map(|m| std::time::Instant::now() + std::time::Duration::from_millis(m));
        while let Ok(Ok(Some(_))) = tokio::time::timeout(
            std::time::Duration::from_millis(quiet_ms),
            self.session_read_byte(),
        )
        .await
        {
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                break;
            }
        }
    }

    /// Handle a detected ZMODEM autostart prefix (`**\x18[ABC]`) on the
    /// menu input stream.  The four leading bytes have already been
    /// consumed by the menu state machine; the rest of the partial
    /// ZRQINIT header (and any retransmits) sit on the wire.  We drain
    /// them, set up the transfer directory, and hand off to the regular
    /// `zmodem_receive` flow — once we emit our `rz\r` + ZRINIT the
    /// sender's protocol retry will resync regardless of what we just
    /// drained.  Files are saved using the sender's filename (after
    /// path validation), with `apply_ymodem_meta` applied for mtime /
    /// mode so the upload behaves identically to a menu-initiated
    /// `Z` upload.
    pub(in crate::telnet) async fn handle_zmodem_autostart(&mut self) -> Result<(), std::io::Error> {
        glog!("File transfer: ZMODEM autostart detected; switching to receive");
        // Drain residual ZRQINIT bytes the sender already pushed before
        // we got a chance to start the receiver — the sender will
        // retransmit once it sees our ZRINIT below.
        self.drain_input().await;

        self.ensure_transfer_dir().await?;
        if Self::is_disk_full() {
            self.show_error("Disk space is low. Uploads disabled.")
                .await?;
            return Ok(());
        }

        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.green("ZMODEM upload detected — receiving...")
        ))
        .await?;
        self.flush().await?;

        let verbose = config::get_config().verbose;
        let target_dir = self.transfer_path();
        let target_dir_for_decide = target_dir.clone();
        // Auto-accept anything with a valid filename that doesn't
        // already exist.  Same sanitation as the interactive batch
        // upload's "subsequent files" path.
        let decide = move |_idx: usize, sender_name: &str, _size: Option<u64>| -> bool {
            if Self::validate_filename(sender_name).is_err() {
                return false;
            }
            !target_dir_for_decide.join(sender_name).exists()
        };

        let start = std::time::Instant::now();
        let result = {
            let mut writer_guard = self.writer.lock().await;
            crate::zmodem::zmodem_receive(
                &mut self.reader,
                &mut *writer_guard,
                self.xmodem_iac,
                verbose,
                decide,
            )
            .await
        };
        let elapsed = start.elapsed();

        let received = match result {
            Ok(rxs) => rxs,
            Err(e) => {
                self.post_transfer_settle().await;
                self.show_error(&format!("ZMODEM receive failed: {}", e))
                    .await?;
                return Ok(());
            }
        };

        // Save each accepted file with the sender's name + metadata.
        // Files the decide closure rejected aren't in `received` at all
        // — the sender saw a ZSKIP and moved on — so any skips here
        // are post-receive failures (write error, race on existence).
        let mut saved: Vec<(String, usize)> = Vec::new();
        let mut skipped: Vec<(String, &'static str)> = Vec::new();
        for rx in &received {
            if Self::validate_filename(&rx.filename).is_err() {
                // Sanitize the sender-supplied name before it can reach the
                // terminal in the skipped summary (it may carry ANSI escapes).
                skipped.push((crate::aichat::sanitize_for_terminal(&rx.filename), "invalid filename"));
                continue;
            }
            let filepath = target_dir.join(&rx.filename);
            // Atomic create-only open — closes the TOCTOU window
            // between an `exists()` check and the write that
            // `std::fs::write` would leave open, and async lets the
            // 8 MB cap not block the tokio executor.
            let meta = (rx.modtime.is_some() || rx.mode.is_some())
                .then_some(crate::xmodem::YmodemReceiveMeta {
                    size: None,
                    modtime: rx.modtime,
                    mode: rx.mode,
                });
            match Self::save_received_file(&filepath, &rx.data, meta.as_ref()).await {
                Ok(()) => saved.push((rx.filename.clone(), rx.data.len())),
                Err(SaveError::AlreadyExists) => {
                    skipped.push((rx.filename.clone(), "already exists"));
                }
                Err(SaveError::WriteFailed) => {
                    skipped.push((rx.filename.clone(), "write failed"));
                }
            }
        }

        self.post_transfer_settle().await;
        self.send_line("").await?;
        self.send_line(&format!(
            "  ZMODEM upload completed in {:.1}s.",
            elapsed.as_secs_f64()
        ))
        .await?;
        self.send_line(&format!(
            "  Received: {} file(s), saved: {}, skipped: {}.",
            received.len(),
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
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        let _ = self.wait_for_key().await;
        Ok(())
    }

    pub(in crate::telnet) async fn show_error(&mut self, msg: &str) -> Result<(), std::io::Error> {
        self.send_line(&format!("  {}", self.red(msg))).await?;
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Option 4 (`punter_hangup_on_failure`): C1 has no in-band abort, so when
    /// a Punter transfer gives up the C64 is left spinning in its own retry
    /// loop until its (long) internal timeout.  Dropping the connection makes
    /// the modem bridge signal loss-of-carrier so the C64 exits its transfer
    /// at once.  Sends a short notice — no `wait_for_key`, since the peer is
    /// mid-protocol and won't press a key — and returns `ConnectionAborted`.
    /// `handle_file_transfer_command` propagates that kind up through `run()`,
    /// whose caller unconditionally shuts down the writer (telnet TCP socket /
    /// SSH channel), which is the carrier drop.
    pub(in crate::telnet) async fn punter_hangup(&mut self) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        self.send_line(&format!(
            "  {}",
            self.amber("Dropping carrier to release the C64.")
        ))
        .await?;
        self.flush().await?;
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "Punter transfer failed; hanging up to release the peer",
        ))
    }

    /// Pause after an XMODEM/YMODEM transfer so the client's own
    /// transfer dialog finishes closing and the underlying terminal is
    /// visible again before we print status.  Drains trailing bytes
    /// from the client's post-transfer chatter (NAWS updates, stray
    /// CR/LF from a dialog-dismiss keypress, etc.) so the subsequent
    /// `wait_for_key` actually waits for a human keypress instead of
    /// being satisfied by leftover noise.
    pub(in crate::telnet) async fn post_transfer_settle(&mut self) {
        self.drain_input().await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        self.drain_input().await;
    }

    /// Show a multi-line informational message and wait for a keypress.
    pub(in crate::telnet) async fn show_error_lines(&mut self, lines: &[&str]) -> Result<(), std::io::Error> {
        self.send_line("").await?;
        for line in lines {
            self.send_line(&format!("  {}", line)).await?;
        }
        self.send_line("").await?;
        self.send("  Press any key to continue.").await?;
        self.flush().await?;
        self.wait_for_key().await?;
        Ok(())
    }

    /// Show a full-screen help page with a header and wait for a keypress.
    /// Split help content into pages that each fit within `max_per_page`
    /// lines.  Prefers breaking at **blank lines** so a logical group —
    /// a section header plus its continuation lines, a letter-command
    /// plus its description — stays together on a single page.  Falls
    /// back to a hard split at `max_per_page` only if no blank exists
    /// within the range; authors avoid that path by separating groups
    /// with a blank line in the help content.
    ///
    /// The returned pages have trailing blanks stripped and leading
    /// blanks skipped so each page renders cleanly without drifting
    /// chrome.
    pub(in crate::telnet) fn paginate_help<'a>(
        lines: &'a [&'a str],
        max_per_page: usize,
    ) -> Vec<Vec<&'a str>> {
        assert!(max_per_page >= 1, "max_per_page must be ≥ 1");
        fn is_blank(s: &str) -> bool {
            s.trim().is_empty()
        }
        let mut pages: Vec<Vec<&'a str>> = Vec::new();
        let mut remaining: &[&str] = lines;
        while !remaining.is_empty() {
            let take = remaining.len().min(max_per_page);
            // Prefer splitting at the last blank line within `take`.
            // Falling back to `take` only when no blank exists in range
            // — authors should avoid this by separating groups with
            // blanks, but we don't want to loop forever on malformed
            // input.
            let mut split = take;
            for i in (1..=take).rev() {
                if is_blank(remaining[i - 1]) {
                    split = i;
                    break;
                }
            }
            // Emit the page with trailing blanks trimmed.
            let mut page: Vec<&str> = remaining[..split].to_vec();
            while page.last().is_some_and(|s| is_blank(s)) {
                page.pop();
            }
            if !page.is_empty() {
                pages.push(page);
            }
            // Skip leading blanks on the next page so the header isn't
            // followed by an awkward empty line.
            remaining = &remaining[split..];
            while !remaining.is_empty() && is_blank(remaining[0]) {
                remaining = &remaining[1..];
            }
        }
        pages
    }

    pub(in crate::telnet) async fn show_help_page(
        &mut self,
        title: &str,
        lines: &[&str],
    ) -> Result<(), std::io::Error> {
        // Chrome is 6 rows: sep(1) + title(1) + sep(1) + blank(1) +
        // blank(1) + footer(1).  PETSCII renders 22 usable rows on a
        // 25-line Commodore 64, so 22 - 6 = 16 content rows.  We use 15
        // to leave a little breathing room for terminals that occasionally
        // push an extra line at the bottom.
        const MAX_CONTENT_LINES: usize = 15;

        let pages = Self::paginate_help(lines, MAX_CONTENT_LINES);
        // Empty content is rare but possible; treat it as one blank page
        // so the caller still gets the usual "Press any key" affordance.
        let pages: Vec<Vec<&str>> = if pages.is_empty() {
            vec![Vec::new()]
        } else {
            pages
        };
        let total = pages.len();

        for (idx, page_lines) in pages.iter().enumerate() {
            self.clear_screen().await?;
            let sep = self.separator();
            self.send_line(&sep).await?;
            self.send_line(&format!("  {}", self.yellow(title))).await?;
            self.send_line(&sep).await?;
            self.send_line("").await?;
            for line in page_lines {
                self.send_line(line).await?;
            }
            self.send_line("").await?;

            let is_last = idx + 1 == total;
            let prompt = if total == 1 {
                "  Press any key to continue.".to_string()
            } else if is_last {
                format!("  Page {}/{} - Press any key.", idx + 1, total)
            } else {
                format!("  Page {}/{} - next key, Q to quit", idx + 1, total)
            };
            self.send(&prompt).await?;
            self.flush().await?;

            let key = self.wait_for_key_returning().await?;
            // Early-exit on Q between pages.  ESC also bails out so the
            // existing "escape twice means leave this screen" reflex
            // works on help screens too.
            if !is_last
                && (matches!(key, b'q' | b'Q')
                    || is_esc_key(key, self.terminal_type == TerminalType::Petscii))
            {
                break;
            }
        }
        // The last "Press any key" prompt was dismissed with the cursor still
        // on that line; advance to a fresh line so whatever renders next (a
        // menu that clears the screen, or the Gateway Shell's bare `A>` prompt)
        // doesn't appear glued to the end of the prompt text.
        self.send_line("").await?;
        Ok(())
    }

    /// Variant of `wait_for_key` that returns the byte that unblocked
    /// it.  Used by paginated help screens so they can react to `Q`
    /// (quit) or ESC during multi-page navigation.
    pub(in crate::telnet) async fn wait_for_key_returning(&mut self) -> Result<u8, std::io::Error> {
        loop {
            match self.read_byte_filtered().await? {
                Some(b)
                    if b >= 0x20
                        || b == b'\r'
                        || b == b'\n'
                        || is_esc_key(b, self.terminal_type == TerminalType::Petscii) =>
                {
                    return Ok(b);
                }
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionAborted,
                        "disconnected",
                    ));
                }
                _ => continue,
            }
        }
    }
}
