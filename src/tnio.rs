//! Shared raw-I/O layer for the file-transfer protocol modules.
//!
//! Centralizes the byte-stream details that are identical across
//! XMODEM/YMODEM, ZMODEM, and Kermit:
//!
//! - **Telnet IAC unescaping.**  On TCP connections the peer doubles
//!   any 0xFF data byte as `IAC IAC`; we collapse it back.  Telnet
//!   command sequences (WILL/WONT/DO/DONT and SB ... SE blocks) are
//!   silently consumed so option-negotiation traffic doesn't show up
//!   as data.
//!
//! - **8-bit transparency (RFC 856 binary semantics).**  File-transfer
//!   payloads are 8-bit binary (or, for Kermit, self-quoting), so the
//!   transport must pass every non-IAC byte — *including CR (0x0D)* —
//!   through literally.  We deliberately do NOT apply RFC 854 NVT CR-NUL
//!   stuffing/stripping: that is a text-mode rule, and inserting (or
//!   swallowing) a 0x00 around a 0x0D corrupts binary data for any peer
//!   that doesn't mirror it — which manifested as endless mid-transfer
//!   checksum failures against real Punter/XMODEM peers and telnet↔serial
//!   bridges (e.g. tcpser).
//!
//! - **Forsberg's CAN×2 abort rule.**  Two consecutive 0x18 bytes mean
//!   "user pressed Ctrl-X to bail."  XMODEM and Kermit both honor it
//!   (ZMODEM has its own ZCAN frame, so it doesn't use this helper).
//!
//! Each protocol module imports what it needs from here rather than
//! redefining the same ~140 lines.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ─── Telnet protocol constants ───────────────────────────────

/// Telnet IAC ("Interpret As Command", 0xFF) — start of every option
/// negotiation or sub-negotiation, doubled when transmitting a literal
/// 0xFF data byte.
pub(crate) const IAC: u8 = 0xFF;
/// Subnegotiation begin — followed by option-specific bytes until SE.
pub(crate) const SB: u8 = 250;
/// Subnegotiation end.
pub(crate) const SE: u8 = 240;
/// Option-negotiation verbs.
pub(crate) const WILL: u8 = 251;
pub(crate) const WONT: u8 = 252;
pub(crate) const DO_CMD: u8 = 253;
pub(crate) const DONT: u8 = 254;

/// CAN (0x18) — bytewise abort signal.  XMODEM and Kermit both adopt
/// Forsberg's "two consecutive CANs = abort" rule; a single CAN is
/// considered line noise.
pub(crate) const CAN: u8 = 0x18;

/// Hard cap on any single file we'll send or receive across XMODEM /
/// YMODEM / ZMODEM / Kermit.  8 MiB matches the historical cap from
/// when each protocol module owned its own copy of the constant; lifted
/// here so the protocols agree on a single value rather than each
/// importing their own slightly-typed copy.  Protocol modules can cast
/// to their preferred integer width — kermit/zmodem use this as `u64`
/// directly, xmodem treats it as `usize` since its frame counter is
/// already `usize`-shaped.
pub(crate) const MAX_FILE_SIZE: u64 = 8 * 1024 * 1024;

/// The error a read returns when the peer has hung up.
///
/// A distinct value because a closed connection is not line noise: a
/// protocol that treats it as one retries into a socket that answers every
/// read at once, and a ZMODEM receiver still negotiating spun a core for the
/// whole 45 s negotiation budget that way.  `read_error` is the one place it
/// is made, so a caller can test for it by value.
pub(crate) const PEER_CLOSED_ERR: &str = "connection closed by the peer";

/// A read error as the protocols report it, with a hang-up made recognisable.
pub(crate) fn read_error(e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        PEER_CLOSED_ERR.into()
    } else {
        e.to_string()
    }
}

// ─── Retry budget ────────────────────────────────────────────

/// **What `*_max_retries = N` means, in every protocol here: N retries.**
///
/// The first attempt is not a retry.  A block, frame or packet is therefore
/// sent -- or, on the receiving side, re-prompted for with a NAK / ZRPOS --
/// up to N more times after it first fails, and the transfer gives up on the
/// (N + 1)th consecutive failure.  `failures` is how many attempts have failed
/// so far.
///
/// One statement of the rule because there were two in the tree.  Every
/// receiver and Punter already counted this way; every *sender* in XMODEM,
/// ZMODEM and Kermit, and Kermit's receiver, compared with `>=` and so gave up
/// one attempt early -- `max_retries = 10` resent a block nine times, while
/// the manual and the reference pages say "resends of one block".  The
/// difference matters most at the small values an operator sets to fail
/// fast: `1` meant *no* resend at all on those paths.
pub(crate) fn retries_exhausted<T: PartialOrd>(failures: T, max_retries: T) -> bool {
    failures > max_retries
}

// ─── Per-stream read state ───────────────────────────────────

/// State threaded through the byte readers so the protocols can implement
/// one-byte lookahead and CAN×2 detection without losing context across
/// calls.
///
/// - `pushback` holds a byte a protocol's own lookahead consumed but wants
///   returned on the next read (XMODEM checksum-under-CRC auto-detect,
///   ZMODEM ZDLE peek, Kermit's pre-MARK hunt); the next `nvt_read_byte`
///   returns it before pulling fresh bytes.
/// - `pending_can` records that the most-recent abort-relevant byte
///   was a CAN.  The next CAN aborts; any non-CAN clears the flag.
///   ZMODEM doesn't use this field (its own CANCAN logic handles
///   abort) but carrying it costs nothing.
#[derive(Default)]
pub(crate) struct ReadState {
    pub(crate) pushback: Option<u8>,
    pub(crate) pending_can: bool,
}

/// Forsberg's CAN×2 abort rule, factored so every read site applies
/// the same state transitions:
///
/// - On CAN: if a previous CAN was already pending, return `true`
///   (caller aborts).  Otherwise set `pending_can` and return `false`
///   so the caller treats the byte as "ignore for now, keep reading."
/// - On any other byte: clear `pending_can` and return `false`.
///
/// Crucially, `pending_can` persists across read calls: a CAN seen
/// during one block followed by a normal byte must NOT abort the
/// session — only **consecutive** CANs do.
pub(crate) fn is_can_abort(byte: u8, state: &mut ReadState) -> bool {
    if byte == CAN {
        if state.pending_can {
            state.pending_can = false;
            return true;
        }
        state.pending_can = true;
        false
    } else {
        state.pending_can = false;
        false
    }
}

// ─── Byte readers ────────────────────────────────────────────

/// Read one logical byte from the transfer stream.  Returns any byte a
/// protocol stashed via `state.pushback` first, otherwise reads from the
/// wire (with telnet IAC unescaping in `raw_read_byte`).
///
/// No RFC 854 CR-NUL stripping: file-transfer payloads are 8-bit binary
/// (RFC 856 binary semantics), so a 0x00 following a 0x0D is real data,
/// not NVT padding — swallowing it would corrupt the stream and desync
/// the block.
pub(crate) async fn nvt_read_byte(
    reader: &mut (impl AsyncRead + Unpin),
    is_tcp: bool,
    state: &mut ReadState,
) -> Result<u8, String> {
    if let Some(b) = state.pushback.take() {
        return Ok(b);
    }
    raw_read_byte(reader, is_tcp).await
}

/// Read one byte from the wire, transparently consuming any telnet
/// IAC sequences encountered.  `IAC IAC` collapses to a literal 0xFF
/// data byte; other commands are passed to `consume_telnet_command`
/// to drain their payload, then the loop continues looking for a real
/// data byte.
pub(crate) async fn raw_read_byte(
    reader: &mut (impl AsyncRead + Unpin),
    is_tcp: bool,
) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    loop {
        // First byte: no timeout — this is the normal "wait for the next
        // data byte" and the caller owns the overall wait (block/negotiation
        // timeout).  Only the bytes *inside* a committed IAC sequence are
        // bounded below (N4).
        reader.read_exact(&mut buf).await.map_err(read_error)?;
        if is_tcp && buf[0] == IAC {
            // We've committed to an IAC sequence — the command byte (and any
            // option payload) must arrive promptly.  Without a bound, a peer
            // that sends a lone 0xFF and then stalls wedges this read_exact
            // forever (N4); mirror the 5 s bound the SB-drain already uses.
            read_iac_continuation(reader, &mut buf).await?;
            if buf[0] == IAC {
                return Ok(IAC);
            }
            consume_telnet_command(reader, buf[0]).await?;
        } else {
            return Ok(buf[0]);
        }
    }
}

/// Time allowed for a byte that is part of an already-started telnet IAC
/// sequence.  The first data byte is deliberately NOT bounded here (the
/// caller times the overall wait); this bounds only mid-sequence bytes so a
/// half-sent IAC command can't block a `read_exact` indefinitely (N4).
const IAC_SEQUENCE_TIMEOUT_SECS: u64 = 5;

/// Read one continuation byte of an in-progress IAC sequence into `buf`,
/// bounded by `IAC_SEQUENCE_TIMEOUT_SECS`.
async fn read_iac_continuation(
    reader: &mut (impl AsyncRead + Unpin),
    buf: &mut [u8; 1],
) -> Result<(), String> {
    match tokio::time::timeout(
        tokio::time::Duration::from_secs(IAC_SEQUENCE_TIMEOUT_SECS),
        reader.read_exact(buf),
    )
    .await
    {
        Err(_) => Err("Telnet IAC sequence timed out".into()),
        Ok(r) => r.map(|_| ()).map_err(|e| e.to_string()),
    }
}

/// Drain a telnet command sequence after an IAC and command byte have
/// already been read.  WILL/WONT/DO/DONT each take one option byte;
/// SB ... SE blocks are read until the closing IAC SE pair (with a
/// 5-second timeout so a buggy peer can't wedge us).
pub(crate) async fn consume_telnet_command(
    reader: &mut (impl AsyncRead + Unpin),
    command: u8,
) -> Result<(), String> {
    let mut buf = [0u8; 1];
    match command {
        SB => {
            let sb_result = tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
                loop {
                    reader
                        .read_exact(&mut buf)
                        .await
                        .map_err(|e| e.to_string())?;
                    if buf[0] == IAC {
                        reader
                            .read_exact(&mut buf)
                            .await
                            .map_err(|e| e.to_string())?;
                        if buf[0] == SE {
                            break;
                        }
                    }
                }
                Ok::<(), String>(())
            })
            .await;
            match sb_result {
                Err(_) => return Err("Telnet subnegotiation timed out".into()),
                Ok(r) => r?,
            }
        }
        WILL | WONT | DO_CMD | DONT => {
            // Bounded like the SB drain (N4): the single option byte must
            // arrive promptly so a `IAC WILL` + stall can't wedge us.
            read_iac_continuation(reader, &mut buf).await?;
        }
        _ => {}
    }
    Ok(())
}

// ─── Byte writer ─────────────────────────────────────────────

/// Write a buffer of bytes to the wire, doubling telnet IAC (`0xFF` →
/// `IAC IAC`) when `is_tcp` is true so a literal 0xFF data byte isn't
/// mistaken for a telnet command.  This is the RFC 856 (Telnet Binary
/// Transmission) transport model: IAC is the only reserved byte, and every
/// other byte — *including CR (0x0D)* — passes through literally so the
/// 8-bit file-transfer stream stays transparent.
///
/// We deliberately do NOT apply RFC 854 CR-NUL stuffing.  That is an NVT
/// *text-mode* rule; inserting a 0x00 after every 0x0D corrupts binary
/// payloads for any peer that doesn't strip it back out (real C64/CP-M
/// Punter/XMODEM peers and telnet↔serial bridges like tcpser do not), which
/// surfaced as endless mid-transfer checksum failures.
pub(crate) async fn raw_write_bytes(
    writer: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    is_tcp: bool,
) -> Result<(), String> {
    if is_tcp {
        let mut buf = Vec::with_capacity(data.len() + 8);
        for &b in data {
            if b == IAC {
                buf.push(IAC);
                buf.push(IAC);
            } else {
                buf.push(b);
            }
        }
        writer.write_all(&buf).await.map_err(|e| e.to_string())?;
    } else {
        writer.write_all(data).await.map_err(|e| e.to_string())?;
    }
    writer.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N4: once an IAC sequence has started, a peer that sends the lead
    /// 0xFF (or IAC + command) and then stalls must not block the reader
    /// forever — the mid-sequence read is bounded.  Paused clock so the
    /// window elapses at once; the write half stays open so the missing
    /// continuation byte *pends* (a real stall) rather than hitting EOF.
    #[tokio::test(start_paused = true)]
    async fn test_raw_read_byte_times_out_on_truncated_iac_command() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(&[IAC]).await.unwrap(); // lead IAC, no command byte
        let r = raw_read_byte(&mut reader, true).await;
        assert!(
            r.is_err(),
            "a lone IAC followed by a stall must time out, not block forever"
        );
        drop(writer);
    }

    /// The truncated-IAC bound also covers a WILL/WONT/DO/DONT whose option
    /// byte never arrives (drained via consume_telnet_command).
    #[tokio::test(start_paused = true)]
    async fn test_raw_read_byte_times_out_on_truncated_will() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(&[IAC, WILL]).await.unwrap(); // no option byte follows
        let r = raw_read_byte(&mut reader, true).await;
        assert!(
            r.is_err(),
            "an IAC WILL with no option byte must time out, not block forever"
        );
        drop(writer);
    }

    /// A subnegotiation that starts and never reaches `IAC SE` is bounded
    /// too, not only the single continuation byte after an IAC.  The outer
    /// timeout is this test's own, so a regression fails rather than hangs.
    #[tokio::test(start_paused = true)]
    async fn test_raw_read_byte_times_out_on_unterminated_subnegotiation() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(&[IAC, SB, 0x18, 0x01]).await.unwrap(); // no IAC SE
        let r = tokio::time::timeout(
            tokio::time::Duration::from_secs(60),
            raw_read_byte(&mut reader, true),
        )
        .await
        .expect("the reader itself must give up on a stalled subnegotiation");
        assert!(r.is_err(), "an unterminated IAC SB must time out, not block forever");
        drop(writer);
    }

    /// The retry rule, at its boundary: N failures still allow a retry, the
    /// (N + 1)th does not.  Zero means one attempt and no retry.
    #[test]
    fn test_max_retries_counts_retries_not_attempts() {
        for n in [0u32, 1, 5, 10] {
            for failures in 0..=n {
                assert!(!retries_exhausted(failures, n), "{failures} of {n}: still a retry left");
            }
            assert!(retries_exhausted(n + 1, n), "{} failures exhaust {n} retries", n + 1);
        }
    }

    /// **No protocol may count its retry budget the other way.**  The `>=`
    /// form gave up one attempt early and sat in every sender for as long as
    /// the receivers used `>`; a scan, because the next counter is written in
    /// whatever file the next protocol change touches.  Flags a comparison
    /// that makes `N` a count of *attempts* -- `>= max_retries`,
    /// `< max_retries`, `0..max_retries` -- anywhere outside test code.
    /// (`0..=max_retries` and `> max_retries` are the rule, and pass.)
    #[test]
    fn test_no_protocol_counts_max_retries_as_attempts() {
        let mut offenders = Vec::new();
        for file in ["xmodem.rs", "zmodem.rs", "kermit.rs", "punter.rs"] {
            let path = format!("{}/src/{}", env!("CARGO_MANIFEST_DIR"), file);
            let src = std::fs::read_to_string(&path).unwrap();
            for (no, line) in src.lines().enumerate() {
                if line.starts_with("mod tests") {
                    break;
                }
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                let mut from = 0;
                while let Some(at) = compact[from..].find("max_retries") {
                    let at = from + at;
                    // Back over the name and any `t.` / `cfg.` before it.
                    let start = compact[..at]
                        .rfind(|c: char| !(c.is_ascii_lowercase() || c == '_' || c == '.'))
                        .map_or(0, |i| i + 1);
                    let before = &compact[..start];
                    let is_count_of_attempts = before.ends_with(">=")
                        || (before.ends_with('<') && !before.ends_with("<<"))
                        || before.ends_with("..");
                    if is_count_of_attempts {
                        offenders.push(format!("{}:{}: {}", file, no + 1, line.trim()));
                    }
                    from = at + "max_retries".len();
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "max_retries counted as attempts (use `tnio::retries_exhausted`):\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn test_can_abort_state_machine() {
        let mut s = ReadState::default();
        // First CAN sets pending; doesn't abort.
        assert!(!is_can_abort(CAN, &mut s));
        assert!(s.pending_can);
        // Second consecutive CAN aborts.
        assert!(is_can_abort(CAN, &mut s));
        assert!(!s.pending_can);
        // Non-CAN clears pending.
        assert!(!is_can_abort(CAN, &mut s));
        assert!(s.pending_can);
        assert!(!is_can_abort(b'X', &mut s));
        assert!(!s.pending_can);
    }

    #[tokio::test]
    async fn test_raw_write_bytes_iac_escapes() {
        let mut buf: Vec<u8> = Vec::new();
        raw_write_bytes(&mut buf, &[0x41, IAC, 0x42], true).await.unwrap();
        assert_eq!(buf, &[0x41, IAC, IAC, 0x42]);
    }

    #[tokio::test]
    async fn test_raw_write_bytes_passes_cr_through_literally() {
        // CR (0x0D) is binary data here, NOT NVT text: it must pass through
        // untouched (no RFC 854 CR-NUL stuffing) so the 8-bit stream stays
        // transparent for the file-transfer protocols.
        let mut buf: Vec<u8> = Vec::new();
        raw_write_bytes(&mut buf, &[0x41, 0x0D, 0x42], true).await.unwrap();
        assert_eq!(buf, &[0x41, 0x0D, 0x42]);
        // A CR immediately followed by a real NUL data byte is preserved too.
        let mut buf2: Vec<u8> = Vec::new();
        raw_write_bytes(&mut buf2, &[0x0D, 0x00, 0x0D, 0x0A], true).await.unwrap();
        assert_eq!(buf2, &[0x0D, 0x00, 0x0D, 0x0A]);
    }

    #[tokio::test]
    async fn test_raw_write_bytes_passthrough_when_not_tcp() {
        let mut buf: Vec<u8> = Vec::new();
        raw_write_bytes(&mut buf, &[0x41, IAC, 0x0D, 0x42], false).await.unwrap();
        assert_eq!(buf, &[0x41, IAC, 0x0D, 0x42]);
    }

    #[tokio::test]
    async fn test_nvt_read_byte_keeps_cr_null_as_data() {
        // The 0x00 after a 0x0D is real binary data, not NVT padding, so it
        // must be returned, not swallowed.
        let data = vec![0x41, 0x0D, 0x00, 0x42];
        let mut cur = std::io::Cursor::new(data);
        let mut s = ReadState::default();
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x41);
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x0D);
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x00);
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_nvt_read_byte_returns_pushback_first() {
        // A protocol's own lookahead (xmodem/zmodem/kermit) stashes a byte in
        // state.pushback; the next read must return it before touching the wire.
        let data = vec![0x42];
        let mut cur = std::io::Cursor::new(data);
        let mut s = ReadState { pushback: Some(0x99), pending_can: false };
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x99);
        assert_eq!(nvt_read_byte(&mut cur, true, &mut s).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_raw_read_byte_unescapes_iac_iac() {
        let data = vec![IAC, IAC, 0x42];
        let mut cur = std::io::Cursor::new(data);
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), IAC);
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_raw_read_byte_skips_option_negotiation() {
        // IAC WILL <opt> is consumed transparently; the next data byte wins.
        let data = vec![IAC, WILL, 0x18, 0x42];
        let mut cur = std::io::Cursor::new(data);
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_raw_read_byte_skips_subnegotiation_block() {
        // A full IAC SB ... IAC SE block is drained, leaving the data byte.
        let data = vec![IAC, SB, 0x18, 0x01, 0x02, IAC, SE, 0x42];
        let mut cur = std::io::Cursor::new(data);
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_raw_read_byte_keeps_iac_as_data_when_not_tcp() {
        // Off TCP there is no IAC layer, so 0xFF is ordinary data.
        let data = vec![IAC, 0x42];
        let mut cur = std::io::Cursor::new(data);
        assert_eq!(raw_read_byte(&mut cur, false).await.unwrap(), IAC);
        assert_eq!(raw_read_byte(&mut cur, false).await.unwrap(), 0x42);
    }

    #[tokio::test]
    async fn test_consume_telnet_command_will_takes_one_option_byte() {
        // WILL/WONT/DO/DONT each consume exactly one following option byte.
        let data = vec![0x18, 0x99];
        let mut cur = std::io::Cursor::new(data);
        consume_telnet_command(&mut cur, WILL).await.unwrap();
        // Only the option byte (0x18) was drained; 0x99 remains readable.
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x99);
    }

    #[tokio::test]
    async fn test_consume_telnet_command_sb_drains_to_se() {
        // SB ... IAC SE is drained in full; the trailing data byte remains.
        let data = vec![0x01, 0x02, 0x03, IAC, SE, 0x99];
        let mut cur = std::io::Cursor::new(data);
        consume_telnet_command(&mut cur, SB).await.unwrap();
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x99);
    }

    #[tokio::test]
    async fn test_consume_telnet_command_sb_ignores_doubled_iac() {
        // An IAC IAC inside the SB body is escaped data, not the SE terminator,
        // so the block only ends at the real IAC SE.
        let data = vec![0x01, IAC, IAC, 0x02, IAC, SE, 0x99];
        let mut cur = std::io::Cursor::new(data);
        consume_telnet_command(&mut cur, SB).await.unwrap();
        assert_eq!(raw_read_byte(&mut cur, true).await.unwrap(), 0x99);
    }
}
