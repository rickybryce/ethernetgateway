//! Outbound gateways: SSH proxy, telnet proxy, and serial gateway,
//! plus the shared gateway protocol plumbing (IAC-aware event reader,
//! Q-method telnet option state machine, ANSI-strip output filter,
//! russh client handler).
//!
//! Split out of `telnet/mod.rs`; behaviour unchanged.

use super::*;

/// Events surfaced by the outgoing Telnet Gateway's local-side reader.
///
/// Unlike [`read_byte_iac_filtered`] (which drops every IAC sequence
/// silently), this reader surfaces `SB NAWS <w><h> IAC SE` as a structured
/// resize event so the gateway can forward it to the remote server while
/// a session is already live.  All other IAC framing — 2-byte commands,
/// option negotiations, non-NAWS subnegotiations — is still consumed.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::telnet) enum GatewayInboundEvent {
    /// A plain data byte from the local user.  `IAC IAC` is unescaped.
    Data(u8),
    /// The local client sent `IAC SB NAWS <cols16><rows16> IAC SE`.
    NawsResize(u16, u16),
    /// Connection closed.
    Eof,
}

/// Read one event from the local user's side of a Telnet Gateway session.
pub(in crate::telnet) async fn read_gateway_event(
    reader: &mut (dyn tokio::io::AsyncRead + Unpin + Send),
) -> std::io::Result<GatewayInboundEvent> {
    let mut buf = [0u8; 1];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(GatewayInboundEvent::Eof),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let byte = buf[0];
        if byte != IAC {
            return Ok(GatewayInboundEvent::Data(byte));
        }
        // Read the command byte.
        match reader.read(&mut buf).await {
            Ok(0) => return Ok(GatewayInboundEvent::Eof),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let cmd = buf[0];
        match cmd {
            IAC => return Ok(GatewayInboundEvent::Data(IAC)),
            SB => {
                // Read the option code.
                match reader.read(&mut buf).await {
                    Ok(0) => return Ok(GatewayInboundEvent::Eof),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
                let opt = buf[0];
                // Read body until IAC SE, unescaping IAC IAC → single
                // IAC.  Cap accumulated size so a malicious peer cannot
                // drive memory unbounded by sending a giant SB without
                // a terminating IAC SE; bytes past the cap are dropped
                // but the loop still scans for IAC SE to stay in sync.
                let mut body: Vec<u8> = Vec::new();
                let mut in_iac = false;
                loop {
                    // Bound in-SB reads (slowloris guard); a stalled
                    // subnegotiation is treated as a closed connection.
                    match tokio::time::timeout(SB_DRAIN_TIMEOUT, reader.read(&mut buf)).await {
                        Err(_) => return Ok(GatewayInboundEvent::Eof),
                        Ok(Ok(0)) => return Ok(GatewayInboundEvent::Eof),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => return Err(e),
                    }
                    let b = buf[0];
                    if in_iac {
                        if b == SE {
                            break;
                        } else if b == IAC {
                            if body.len() < MAX_SB_BODY_BYTES {
                                body.push(IAC);
                            }
                            in_iac = false;
                        } else {
                            in_iac = false;
                        }
                    } else if b == IAC {
                        in_iac = true;
                    } else if body.len() < MAX_SB_BODY_BYTES {
                        body.push(b);
                    }
                }
                if opt == OPT_NAWS && body.len() == 4 {
                    let w = u16::from_be_bytes([body[0], body[1]]);
                    let h = u16::from_be_bytes([body[2], body[3]]);
                    return Ok(GatewayInboundEvent::NawsResize(w, h));
                }
                // Non-NAWS subnegotiation: drop and keep reading.
            }
            WILL | WONT | DO | DONT => {
                // Consume the option byte; drop the negotiation.
                match reader.read(&mut buf).await {
                    Ok(0) => return Ok(GatewayInboundEvent::Eof),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            _ => {
                // 2-byte command (NOP, DM, BRK, IP, AO, AYT, EC, EL, GA)
                // — already fully consumed.
            }
        }
    }
}

// ─── SSH Gateway helpers ────────────────────────────────────

/// True when gateway byte-tracing is enabled — either by the
/// `gateway_debug` config flag (toggleable from the GUI, web console, and
/// the in-session Serial Configuration menu) or forced on by the
/// `EGATEWAY_GATEWAY_DEBUG` environment variable (any non-empty value).
/// Gates the chatty per-byte diagnostics in the SSH and Telnet gateway
/// proxy loops so they cost nothing when off.  `cfg_flag` is the caller's
/// already-read `cfg.gateway_debug`, avoiding a second config lock.
pub(in crate::telnet) fn gw_debug_enabled(cfg_flag: bool) -> bool {
    cfg_flag || std::env::var_os("EGATEWAY_GATEWAY_DEBUG").is_some_and(|v| !v.is_empty())
}

/// Maximum bytes the gateway-debug `dbg_in` line buffer will accumulate
/// before being force-flushed.  Prevents a no-newline stream (a TUI editor,
/// a binary paste, a remote program doing its own line editing) from growing
/// the trace buffer without bound while gateway_debug is enabled.
const GW_DBG_IN_CAP: usize = 4096;

/// Format a byte slice as a compact hex + printable-ASCII dump for the
/// gateway diagnostics log, e.g. `73 75 64 6f | "sudo"`.  Non-printable
/// bytes render as `.` in the ASCII column.
fn gw_hexdump(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| if (0x20..=0x7E).contains(&b) { b as char } else { '.' })
        .collect();
    format!("{} | \"{}\"", hex.join(" "), ascii)
}

/// Filter SSH gateway output for non-ANSI terminals.
///
/// Strips all ANSI escape sequences (CSI, OSC, DCS, PM, APC, SOS) from the
/// byte stream.  For PETSCII terminals, plain-text bytes are also case-swapped.
/// `state` is the ANSI parser state carried across calls (start at 0):
///   0=normal, 1=ESC seen, 2=CSI sequence, 3=string sequence, 4=ESC in string
pub(in crate::telnet) fn filter_gateway_output(input: &[u8], state: &mut u8, is_petscii: bool, out: &mut Vec<u8>) {
    for &b in input {
        match *state {
            0 => {
                if b == 0x1B {
                    *state = 1;
                } else if is_petscii {
                    match b {
                        b'~' => {}  // tilde has no PETSCII equivalent
                        0x08 | 0x7F => out.push(0x14),  // backspace/DEL → PETSCII DEL
                        b'A'..=b'Z' => out.push(b + 32),
                        b'a'..=b'z' => out.push(b - 32),
                        _ => out.push(b),
                    }
                } else {
                    out.push(b);
                }
            }
            1 => {
                *state = match b {
                    b'[' => 2,                                   // CSI
                    b']' | b'P' | b'^' | b'_' | b'X' => 3,      // OSC/DCS/PM/APC/SOS
                    0x1B => 1,                                   // Another ESC
                    _ => 0,                                      // 2-char sequence done
                };
            }
            2 => {
                // CSI: parameter/intermediate bytes stay in state 2.
                // Final byte (0x40-0x7E) ends the sequence.
                if (0x40..=0x7E).contains(&b) {
                    *state = 0;
                } else if b == 0x1B {
                    *state = 1;
                } else if b < 0x20 || b == 0x7F {
                    *state = 0;
                }
            }
            3 => {
                // String sequence: consume until BEL or ESC
                if b == 0x07 {
                    *state = 0;
                } else if b == 0x1B {
                    *state = 4;
                }
            }
            _ => {
                // ESC inside string: '\' = ST (end), else resume string
                *state = if b == b'\\' { 0 } else { 3 };
            }
        }
    }
}

/// Per-option Q-method state — full RFC 1143 six-state variant.
///
/// Each option tracks two independent state machines: one for our side
/// (what we've declared via WILL/WONT) and one for the peer's side (what
/// they've declared via WILL/WONT).
///
/// The "Opposite" variants handle the race where we change our mind
/// about an option while a prior request is still in flight.  Example:
/// we send `WILL TTYPE` (entering WantYes), then before the peer's reply
/// arrives we decide we no longer want TTYPE, so we send `WONT TTYPE`
/// — we cannot simply go to WantNo because our WILL is still on the wire
/// and the peer will eventually respond to it.  Instead we enter
/// `WantYesOpposite`, meaning "we're still waiting for the WILL reply,
/// but our current intent is Off."  When the peer finally replies, the
/// state machine resolves cleanly.
///
/// See RFC 1143 §7 for the full transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telnet) enum OptState {
    /// Option is off and no negotiation is in flight.
    No,
    /// Option is on.
    Yes,
    /// We have asked to enable the option; awaiting peer's reply.
    WantYes,
    /// Same as WantYes, but since sending the request we've changed our
    /// mind and now want the option off.  On the peer's reply we will
    /// send the opposite verb.
    WantYesOpposite,
    /// We have asked to disable the option; awaiting peer's reply.
    WantNo,
    /// Same as WantNo, but since sending the request we've changed our
    /// mind and now want the option on.  On the peer's reply we will
    /// send the opposite verb.
    WantNoOpposite,
}

/// Telnet-client IAC parser + Q-method state machine for the outgoing
/// gateway.  Handles the remote→local direction: parses IAC, unescapes
/// `IAC IAC` to a single data byte, consumes 2-byte commands, and
/// performs option negotiation.
///
/// Negotiation policy:
///
/// - **ECHO** (RFC 857) — always cooperative: peer's `WILL ECHO` is
///   accepted with `DO ECHO`.  Raw-TCP services never send WILL ECHO so
///   this is always safe.
/// - **TTYPE** (RFC 1091) and **NAWS** (RFC 1073) — cooperative only
///   when `cooperate == true`.  Gated because cooperation implies
///   proactive `WILL TTYPE` / `WILL NAWS` at connect, which raw-TCP
///   services would see as garbage.
/// - **Everything else** — refused: `WILL → DONT`, `DO → WONT`.
///
/// The parser never initiates a TTYPE/NAWS request from the peer side;
/// we don't care about the server's own terminal type or window size.
pub(in crate::telnet) struct GatewayTelnetIac {
    pub(in crate::telnet) state: GatewayIacState,
    /// Cooperate on TTYPE / NAWS (from the config toggle).
    pub(in crate::telnet) cooperate: bool,
    /// Terminal name reported in `SB TTYPE IS`.  Chosen to match the
    /// local user's detected terminal type.
    pub(in crate::telnet) terminal_name: String,
    /// Width to report in `SB NAWS`.
    pub(in crate::telnet) window_cols: u16,
    /// Height to report in `SB NAWS`.
    pub(in crate::telnet) window_rows: u16,
    /// Per-option state: what we've said about our own side.
    pub(in crate::telnet) us_state: Box<[OptState; 256]>,
    /// Per-option state: what the peer has said about their side.
    pub(in crate::telnet) him_state: Box<[OptState; 256]>,
    /// Whether we've already sent a `DONT <opt>` refusal for this option.
    /// Cleared when the peer finally sends `WONT <opt>` to ack the refusal.
    /// Prevents a chattery peer from getting repeated DONTs for the same
    /// unwanted WILL.
    pub(in crate::telnet) sent_dont: Box<[bool; 256]>,
    /// Whether we've already sent a `WONT <opt>` refusal.  Cleared when the
    /// peer sends `DONT <opt>` to ack.
    pub(in crate::telnet) sent_wont: Box<[bool; 256]>,
    /// Subnegotiation buffer.  `sb_option` is set when we enter the SB
    /// body (just after `IAC SB <opt>`); `sb_body` accumulates bytes
    /// with `IAC IAC` already unescaped to single 0xFF.
    pub(in crate::telnet) sb_option: u8,
    pub(in crate::telnet) sb_body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::telnet) enum GatewayIacState {
    /// Either a plain data byte or the start of a new IAC sequence.
    Normal,
    /// Previous byte was IAC; waiting for the command byte.
    SawIac,
    /// Previous bytes were IAC + WILL/WONT/DO/DONT; waiting for the option.
    SawVerb(u8),
    /// Just saw `IAC SB`; the next byte is the option code.
    SawSbOption,
    /// Inside an SB subnegotiation body; scanning for IAC SE.
    InSb,
    /// Inside an SB body, just saw an IAC; next byte decides whether it was
    /// IAC SE (end of SB) or IAC IAC (escaped data byte, stay in SB).
    InSbIac,
}

impl GatewayTelnetIac {
    /// Build a fresh parser.  Returns `(parser, initial_offers)` — any
    /// bytes that must be written to the remote before we start reading,
    /// to advertise our cooperative options.  Empty when `cooperate` is
    /// off (reactive-only mode).
    pub(in crate::telnet) fn new(
        cooperate: bool,
        terminal_name: String,
        window_cols: u16,
        window_rows: u16,
    ) -> (Self, Vec<u8>) {
        let mut parser = Self {
            state: GatewayIacState::Normal,
            cooperate,
            terminal_name,
            window_cols,
            window_rows,
            us_state: Box::new([OptState::No; 256]),
            him_state: Box::new([OptState::No; 256]),
            sent_dont: Box::new([false; 256]),
            sent_wont: Box::new([false; 256]),
            sb_option: 0,
            sb_body: Vec::new(),
        };
        let mut initial = Vec::new();
        if cooperate {
            // Proactively offer WILL TTYPE and WILL NAWS; proactively
            // request DO ECHO so we don't need to wait for the peer to
            // offer echo (some BBSes wait for the client to ask first).
            // Set the matching WantYes states so peer acks are recognised.
            parser.us_state[OPT_TTYPE as usize] = OptState::WantYes;
            parser.us_state[OPT_NAWS as usize] = OptState::WantYes;
            parser.him_state[OPT_ECHO as usize] = OptState::WantYes;
            initial.extend_from_slice(&[IAC, WILL, OPT_TTYPE]);
            initial.extend_from_slice(&[IAC, WILL, OPT_NAWS]);
            initial.extend_from_slice(&[IAC, DO, OPT_ECHO]);
        }
        (parser, initial)
    }

    /// True if we should answer the peer's `WILL <opt>` with `DO <opt>`.
    pub(in crate::telnet) fn cooperate_with_his_will(&self, opt: u8) -> bool {
        // ECHO from the server is always welcome — it means "I'll echo
        // your input," which for a retro user is what makes typing
        // visible.  Everything else (WILL TTYPE / WILL NAWS from the
        // server is unusual) we decline.
        opt == OPT_ECHO
    }

    /// True if we should answer the peer's `DO <opt>` with `WILL <opt>`.
    pub(in crate::telnet) fn cooperate_with_his_do(&self, opt: u8) -> bool {
        self.cooperate && (opt == OPT_TTYPE || opt == OPT_NAWS)
    }

    pub(in crate::telnet) fn feed(&mut self, byte: u8, data: &mut Vec<u8>, replies: &mut Vec<u8>) {
        match self.state {
            GatewayIacState::Normal => {
                if byte == IAC {
                    self.state = GatewayIacState::SawIac;
                } else {
                    data.push(byte);
                }
            }
            GatewayIacState::SawIac => {
                match byte {
                    IAC => {
                        data.push(IAC);
                        self.state = GatewayIacState::Normal;
                    }
                    SB => {
                        self.state = GatewayIacState::SawSbOption;
                    }
                    WILL | WONT | DO | DONT => {
                        self.state = GatewayIacState::SawVerb(byte);
                    }
                    _ => {
                        // 2-byte command (NOP, DM, BRK, IP, AO, AYT, EC,
                        // EL, GA, SE-out-of-context) — consumed.
                        self.state = GatewayIacState::Normal;
                    }
                }
            }
            GatewayIacState::SawVerb(verb) => {
                let opt = byte;
                match verb {
                    WILL => self.handle_recv_will(opt, replies),
                    WONT => self.handle_recv_wont(opt, replies),
                    DO => self.handle_recv_do(opt, replies),
                    DONT => self.handle_recv_dont(opt, replies),
                    _ => {}
                }
                self.state = GatewayIacState::Normal;
            }
            GatewayIacState::SawSbOption => {
                self.sb_option = byte;
                self.sb_body.clear();
                self.state = GatewayIacState::InSb;
            }
            GatewayIacState::InSb => {
                if byte == IAC {
                    self.state = GatewayIacState::InSbIac;
                } else if self.sb_body.len() < MAX_SB_BODY_BYTES {
                    self.sb_body.push(byte);
                }
                // Bytes beyond MAX_SB_BODY_BYTES are dropped; we stay in
                // InSb so an eventual IAC SE still terminates the SB.
            }
            GatewayIacState::InSbIac => {
                match byte {
                    SE => {
                        self.process_subneg(replies);
                        self.state = GatewayIacState::Normal;
                    }
                    IAC => {
                        // Escaped IAC inside SB — keep as single 0xFF
                        // (subject to the body-size cap).
                        if self.sb_body.len() < MAX_SB_BODY_BYTES {
                            self.sb_body.push(IAC);
                        }
                        self.state = GatewayIacState::InSb;
                    }
                    _ => {
                        // Malformed; resume scanning for IAC SE.
                        self.state = GatewayIacState::InSb;
                    }
                }
            }
        }
    }

    // ─── Q-method handlers (his side) ─────────────────────

    pub(in crate::telnet) fn handle_recv_will(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.him_state[idx] {
            OptState::No => {
                if self.cooperate_with_his_will(opt) {
                    self.him_state[idx] = OptState::Yes;
                    self.sent_dont[idx] = false; // contradicts any prior refusal
                    replies.extend_from_slice(&[IAC, DO, opt]);
                } else if !self.sent_dont[idx] {
                    // Refuse, but only once per cycle.  Q-method keeps
                    // him at No because we do not want it on.
                    self.sent_dont[idx] = true;
                    replies.extend_from_slice(&[IAC, DONT, opt]);
                }
            }
            OptState::Yes => {
                // Already on — spec says ignore.
            }
            OptState::WantYes => {
                // Peer acks our DO.
                self.him_state[idx] = OptState::Yes;
            }
            OptState::WantYesOpposite => {
                // Peer acked our original DO, but we've since changed to
                // wanting No; send DONT and enter WantNo.  Mark the
                // refusal so a misbehaving peer that re-sends WILL from
                // the subsequent WantNo state doesn't get a duplicate.
                self.him_state[idx] = OptState::WantNo;
                self.sent_dont[idx] = true;
                replies.extend_from_slice(&[IAC, DONT, opt]);
            }
            OptState::WantNo => {
                // Error: peer sent WILL in response to our DONT.  Log
                // by dropping back to No and, if we haven't already,
                // refuse again.
                self.him_state[idx] = OptState::No;
                if !self.sent_dont[idx] {
                    self.sent_dont[idx] = true;
                    replies.extend_from_slice(&[IAC, DONT, opt]);
                }
            }
            OptState::WantNoOpposite => {
                // Error but harmless: we wanted Yes again anyway.  The
                // stale DONT we sent on the way in is now contradicted
                // by our accepting Yes — clear the refusal flag.
                self.him_state[idx] = OptState::Yes;
                self.sent_dont[idx] = false;
            }
        }
    }

    pub(in crate::telnet) fn handle_recv_wont(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        // Peer is acking our refusal or withdrawing — reset refusal-sent
        // so a future fresh cycle can issue a DONT again.
        self.sent_dont[idx] = false;
        match self.him_state[idx] {
            OptState::No => {
                // Already off — ignore.
            }
            OptState::Yes => {
                self.him_state[idx] = OptState::No;
                replies.extend_from_slice(&[IAC, DONT, opt]);
            }
            OptState::WantNo => {
                self.him_state[idx] = OptState::No;
            }
            OptState::WantNoOpposite => {
                // Peer confirmed our DONT, but we changed to WantYes;
                // send a fresh DO.
                self.him_state[idx] = OptState::WantYes;
                self.sent_dont[idx] = false;
                replies.extend_from_slice(&[IAC, DO, opt]);
            }
            OptState::WantYes => {
                // Peer refused our DO.
                self.him_state[idx] = OptState::No;
            }
            OptState::WantYesOpposite => {
                // Peer refused our DO, but we already swung back to No,
                // so we're exactly where we wanted.
                self.him_state[idx] = OptState::No;
            }
        }
    }

    pub(in crate::telnet) fn handle_recv_do(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::No => {
                if self.cooperate_with_his_do(opt) {
                    self.us_state[idx] = OptState::Yes;
                    self.sent_wont[idx] = false; // contradicts any prior refusal
                    replies.extend_from_slice(&[IAC, WILL, opt]);
                    if opt == OPT_NAWS {
                        self.emit_naws_sb(replies);
                    }
                } else if !self.sent_wont[idx] {
                    self.sent_wont[idx] = true;
                    replies.extend_from_slice(&[IAC, WONT, opt]);
                }
            }
            OptState::Yes => {
                // Already on — ignore.
            }
            OptState::WantYes => {
                self.us_state[idx] = OptState::Yes;
                if opt == OPT_NAWS {
                    self.emit_naws_sb(replies);
                }
            }
            OptState::WantYesOpposite => {
                // Peer acked our WILL but we want No; send WONT.  Mark
                // the refusal so a misbehaving peer that re-sends DO
                // from the subsequent WantNo state doesn't get a dup.
                self.us_state[idx] = OptState::WantNo;
                self.sent_wont[idx] = true;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::WantNo => {
                // Error: peer DO after our WONT.  Bounce to No.
                self.us_state[idx] = OptState::No;
                if !self.sent_wont[idx] {
                    self.sent_wont[idx] = true;
                    replies.extend_from_slice(&[IAC, WONT, opt]);
                }
            }
            OptState::WantNoOpposite => {
                // Error but harmless — we wanted Yes.  The stale WONT
                // we sent on the way in is contradicted by accepting
                // Yes; clear the refusal flag.
                self.us_state[idx] = OptState::Yes;
                self.sent_wont[idx] = false;
                if opt == OPT_NAWS {
                    self.emit_naws_sb(replies);
                }
            }
        }
    }

    pub(in crate::telnet) fn handle_recv_dont(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        self.sent_wont[idx] = false;
        match self.us_state[idx] {
            OptState::No => {
                // Already off.
            }
            OptState::Yes => {
                self.us_state[idx] = OptState::No;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::WantNo => {
                self.us_state[idx] = OptState::No;
            }
            OptState::WantNoOpposite => {
                // Peer confirmed DONT, but we changed to WantYes — send WILL.
                self.us_state[idx] = OptState::WantYes;
                self.sent_wont[idx] = false;
                replies.extend_from_slice(&[IAC, WILL, opt]);
            }
            OptState::WantYes => {
                // Peer refused our WILL.
                self.us_state[idx] = OptState::No;
            }
            OptState::WantYesOpposite => {
                // Peer refused our WILL, and we already swung back to No —
                // exactly where we wanted.
                self.us_state[idx] = OptState::No;
            }
        }
    }

    // ─── Active-change helpers (for mind-changes mid-flight) ──

    /// Ask for our side of `opt` to be enabled (send `WILL`).  Advances
    /// the Q-method state for `us_state[opt]` per RFC 1143 §7.
    ///
    /// Currently unused by `gateway_telnet` — we only enter `WantYes` via
    /// the proactive offers in `new()` — but kept for symmetry and so
    /// future active-change flows compile cleanly.
    #[allow(dead_code)]
    pub(in crate::telnet) fn request_local_enable(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::No => {
                self.us_state[idx] = OptState::WantYes;
                self.sent_wont[idx] = false; // contradicts any prior refusal
                replies.extend_from_slice(&[IAC, WILL, opt]);
            }
            OptState::Yes => {} // already on
            OptState::WantNo => {
                // Changed mind mid-flight.
                self.us_state[idx] = OptState::WantNoOpposite;
            }
            OptState::WantNoOpposite => {} // already queued to enable
            OptState::WantYes => {}
            OptState::WantYesOpposite => {
                // Reverting to original intent.
                self.us_state[idx] = OptState::WantYes;
            }
        }
    }

    /// Ask for our side of `opt` to be disabled (send `WONT`).
    #[allow(dead_code)]
    pub(in crate::telnet) fn request_local_disable(&mut self, opt: u8, replies: &mut Vec<u8>) {
        let idx = opt as usize;
        match self.us_state[idx] {
            OptState::Yes => {
                self.us_state[idx] = OptState::WantNo;
                replies.extend_from_slice(&[IAC, WONT, opt]);
            }
            OptState::No => {} // already off
            OptState::WantYes => {
                self.us_state[idx] = OptState::WantYesOpposite;
            }
            OptState::WantYesOpposite => {}
            OptState::WantNo => {}
            OptState::WantNoOpposite => {
                self.us_state[idx] = OptState::WantNo;
            }
        }
    }

    // ─── Subnegotiation ───────────────────────────────────

    pub(in crate::telnet) fn process_subneg(&mut self, replies: &mut Vec<u8>) {
        if self.sb_option == OPT_TTYPE
            && self.us_state[OPT_TTYPE as usize] == OptState::Yes
            && self.sb_body.first().copied() == Some(TTYPE_SEND)
        {
            // Respond with our terminal name.  Any 0xFF in the name
            // (shouldn't happen for our controlled values) would need
            // IAC-doubling; we check explicitly.
            let mut body = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
            for &b in self.terminal_name.as_bytes() {
                if b == IAC {
                    body.push(IAC);
                }
                body.push(b);
            }
            body.extend_from_slice(&[IAC, SE]);
            replies.extend_from_slice(&body);
        }
        // All other SB bodies are informational only — we silently drop.
    }

    /// Record an updated window size from the local user and, if NAWS is
    /// currently enabled on our side, emit an `IAC SB NAWS <w><h> IAC SE`
    /// update to the remote.  Called from the gateway loop when the user
    /// resizes their terminal mid-session.
    pub(in crate::telnet) fn send_naws_update(&mut self, cols: u16, rows: u16, replies: &mut Vec<u8>) {
        self.window_cols = cols;
        self.window_rows = rows;
        if self.us_state[OPT_NAWS as usize] == OptState::Yes {
            self.emit_naws_sb(replies);
        }
    }

    pub(in crate::telnet) fn emit_naws_sb(&self, replies: &mut Vec<u8>) {
        // `IAC SB NAWS <w16_BE> <h16_BE> IAC SE`, with any byte equal to
        // IAC doubled per RFC 854.
        let w = self.window_cols.to_be_bytes();
        let h = self.window_rows.to_be_bytes();
        let size_bytes = [w[0], w[1], h[0], h[1]];
        let mut body = vec![IAC, SB, OPT_NAWS];
        for &b in &size_bytes {
            if b == IAC {
                body.push(IAC);
            }
            body.push(b);
        }
        body.extend_from_slice(&[IAC, SE]);
        replies.extend_from_slice(&body);
    }
}

/// Default terminal name reported via `SB TTYPE IS`.  Chosen to be
/// informative to modern BBSes and still truthful.
pub(in crate::telnet) fn gateway_terminal_name(tt: TerminalType) -> &'static str {
    match tt {
        TerminalType::Petscii => "PETSCII",
        TerminalType::Ansi => "ANSI",
        TerminalType::Ascii => "DUMB",
    }
}

/// Default window dimensions to report via `SB NAWS` when the local
/// client hasn't supplied any via its own NAWS.
fn gateway_default_window(tt: TerminalType) -> (u16, u16) {
    match tt {
        TerminalType::Petscii => (PETSCII_WIDTH as u16, 25),
        TerminalType::Ansi | TerminalType::Ascii => (80, 24),
    }
}

/// Normalize a client input byte for SSH gateway forwarding.
///
/// Telnet clients send CR+LF or CR+NUL for Enter; SSH expects bare CR.
/// Returns `Some(byte)` if the byte should be forwarded, `None` to suppress.
pub(in crate::telnet) fn normalize_gateway_input(b: u8, last_cr: &mut bool) -> Option<u8> {
    if (b == b'\n' || b == 0x00) && *last_cr {
        *last_cr = false;
        return None;
    }
    *last_cr = b == b'\r';
    Some(b)
}

/// SSH client handler for the gateway feature. Captures the server's host key
/// so it can be verified against the known-hosts file after connection.
struct GatewayHandler {
    server_key: Arc<std::sync::Mutex<Option<russh::keys::PublicKey>>>,
}

impl russh::client::Handler for GatewayHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        if let Ok(mut key) = self.server_key.lock() {
            *key = Some(server_public_key.clone());
        }
        Ok(true)
    }
}

/// A Serial Gateway pick: either a local port (A/B) or a registered
/// remote console port on a slave (§9 #12), keyed by the slave's IP and
/// port label.
pub(in crate::telnet) enum GatewayPick {
    Local(crate::config::SerialPortId),
    Remote { ip: IpAddr, label: String },
}

/// Max remote console ports shown in the Serial Gateway picker.  §9 #12
/// allows "paging OR a cap"; a cap (like `SERVER_ADDR_DISPLAY_CAP`) keeps
/// the picker inside the 22-row PETSCII budget without paging state.
pub(in crate::telnet) const REMOTE_PORT_DISPLAY_CAP: usize = 6;

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
