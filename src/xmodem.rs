//! XMODEM Protocol Module
//!
//! Implements the XMODEM file transfer protocol with CRC-16 and checksum modes:
//! - xmodem_receive: receive file data from a sender (upload)
//! - xmodem_send: send file data to a receiver (download)
//! - Raw I/O helpers with telnet IAC escaping
//! - CRC-16 (CCITT polynomial 0x1021) computation

use tokio::io::{AsyncRead, AsyncWrite};

use crate::config;
use crate::logger::glog;
use crate::telnet::is_esc_key;
use crate::tnio::{is_can_abort, nvt_read_byte, raw_write_bytes, ReadState, CAN};

// XMODEM protocol constants
const SOH: u8 = 0x01;
/// XMODEM-1K block header: the next block is 1024 bytes of payload.
const STX: u8 = 0x02;
const EOT: u8 = 0x04;
const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const SUB: u8 = 0x1A;
const CRC_REQUEST: u8 = b'C';

// Telnet IAC + raw I/O now live in `crate::tnio` (shared with kermit
// and zmodem).  Local-only XMODEM bytes stay above.

pub(crate) const XMODEM_BLOCK_SIZE: usize = 128;
/// XMODEM-1K block size.  The sender chooses per-block; the receiver
/// branches on the `SOH` / `STX` header byte to know which one arrived.
pub(crate) const XMODEM_1K_BLOCK_SIZE: usize = 1024;

/// Hard cap on file size — sourced from `tnio::MAX_FILE_SIZE` so all
/// four protocols agree on a single value.  Cast to `usize` once here
/// because XMODEM's frame counter is already `usize`.
const MAX_FILE_SIZE: usize = crate::tnio::MAX_FILE_SIZE as usize;
/// Hard cap on the number of files accepted in one YMODEM batch, so a sender
/// that never sends the end-of-batch terminator (or a hostile peer streaming
/// endless files) can't accumulate files without bound.  This bounds the file
/// *count*; each file is separately capped at `MAX_FILE_SIZE`, so the batch's
/// aggregate memory is bounded by `MAX_BATCH_FILES × MAX_FILE_SIZE`.  Mirrors
/// Kermit's `MAX_BATCH_FILES`.
const MAX_BATCH_FILES: usize = 1000;
/// Time allowed for the full 131-byte block body (after SOH) to arrive.
const BLOCK_BODY_TIMEOUT_SECS: u64 = 60;
/// Auto-detect (first block only) grace window for the SECOND CRC trailer
/// byte.  A genuine CRC sender writes the low byte back-to-back with the
/// high byte, so it always arrives well within this window.  A strict
/// lock-step checksum-only sender (vintage Christensen 1977 / CP/M MODEM7 /
/// C64 BBS uploader that ignored our 'C') instead emits ONE trailer byte and
/// waits for our ACK/NAK — an unconditional second read would then block for
/// the full `BLOCK_BODY_TIMEOUT_SECS` (X1).  When the low byte does not
/// arrive in this window we conclude there is no second trailer byte and fall
/// back to 1-byte-checksum validation.  Kept generous enough that inter-byte
/// jitter on a real CRC trailer can never be mistaken for its absence.
const AUTO_DETECT_TRAILER_TIMEOUT_SECS: u64 = 3;

/// Classification of the block read between YMODEM batch files (after an EOT).
enum InterFileBlock0 {
    /// A block 0 carrying a filename — the next file in the batch.  The name is
    /// `None` when it wasn't valid UTF-8 (the file is still received; the caller
    /// generates a fallback name).
    File(Option<String>, Option<YmodemReceiveMeta>),
    /// The end-of-batch terminator (block 0 whose filename field starts NUL).
    Terminator,
    /// A block 0 that failed CRC / header validation — retryable via NAK.
    Invalid,
    /// The peer sent something other than a block 0 (or nothing) — end the batch.
    NotBlock0,
}

#[derive(Clone, Copy)]
enum TransferMode {
    Checksum,
    Crc16,
}

/// YMODEM header metadata supplied by the caller when sending in
/// YMODEM (batch) mode.  Filename and size are mandatory; receivers
/// use the size for exact end-of-file truncation (Forsberg §5).
/// `modtime` (UNIX seconds) and `mode` (UNIX permission bits) are
/// optional informational fields per Forsberg §6.1 — when supplied
/// they're emitted in their respective slots, when `None` they're
/// emitted as octal `0` (the spec-defined "unknown" sentinel).
/// Passing `None` for the whole `Option<YmodemHeader>` parameter to
/// `xmodem_send` selects plain XMODEM mode (no block 0 at all).
#[derive(Clone)]
pub(crate) struct YmodemHeader {
    pub filename: String,
    pub size: u64,
    pub modtime: Option<u64>,
    pub mode: Option<u32>,
}

/// Metadata parsed out of a YMODEM block 0 by the receiver.  All fields
/// are `Option` because the spec allows minimal senders that emit only
/// the filename, or filename + size.  When present, `mode` is masked to
/// `0o7777` by the parser to keep setuid/setgid/sticky bits visible to
/// callers that want them, but the upload-save path masks further to
/// `0o777` before applying.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct YmodemReceiveMeta {
    pub size: Option<u64>,
    pub modtime: Option<u64>,
    pub mode: Option<u32>,
}

/// One file received by `xmodem_receive_batch`.  Plain XMODEM and single-file
/// YMODEM yield a one-element `Vec`; a YMODEM batch (`sb file1 file2 …`) yields
/// one entry per file.  `filename` is the YMODEM block-0 name (needed to name
/// files 2..N in a batch); it is `None` for plain XMODEM, which carries no name.
#[derive(Clone, Debug, Default)]
pub(crate) struct XmodemReceivedFile {
    pub filename: Option<String>,
    pub data: Vec<u8>,
    pub meta: Option<YmodemReceiveMeta>,
}

// =============================================================================
// XMODEM PROTOCOL - RECEIVE (UPLOAD)
// =============================================================================

/// Apply YMODEM end-of-file truncation to one received file's bytes: prefer the
/// exact size reported in block 0 (Forsberg 1988 §5 — preserves files that
/// legitimately end in 0x1A), else strip trailing SUB padding.  Extracted so
/// `xmodem_receive_batch` can finalize every file in a batch, not just the last.
fn finalize_received_file(
    mut data: Vec<u8>,
    meta: &Option<YmodemReceiveMeta>,
    verbose: bool,
) -> Vec<u8> {
    let reported_size = meta.as_ref().and_then(|m| m.size);
    let truncated_by_size = if let Some(size) = reported_size {
        let target = size as usize;
        if target <= data.len() {
            data.truncate(target);
            if verbose { glog!("XMODEM recv: truncated to YMODEM size {} bytes", target); }
            true
        } else {
            if verbose { glog!("XMODEM recv: reported size {} > received {}, falling back to SUB strip", target, data.len()); }
            false
        }
    } else {
        false
    };
    if !truncated_by_size {
        while data.last() == Some(&SUB) {
            data.pop();
        }
    }
    data
}

/// Single-file wrapper around `xmodem_receive_batch` — returns the first (and,
/// for plain XMODEM or a single-file YMODEM transfer, only) received file.
/// Test-only: production code calls `xmodem_receive_batch` directly, but the
/// extensive single-file test suite drives this shape unchanged, which keeps
/// it as the regression guard for the per-file receive path.
#[cfg(test)]
pub(crate) async fn xmodem_receive(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
) -> Result<(Vec<u8>, Option<YmodemReceiveMeta>), String> {
    let mut files = xmodem_receive_batch(reader, writer, is_tcp, is_petscii, verbose).await?;
    if files.is_empty() {
        return Ok((Vec::new(), None));
    }
    let first = files.remove(0);
    Ok((first.data, first.meta))
}

pub(crate) async fn xmodem_receive_batch(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
) -> Result<Vec<XmodemReceivedFile>, String> {
    let cfg = config::get_config();
    let negotiation_timeout = cfg.xmodem_negotiation_timeout;
    let block_timeout = cfg.xmodem_block_timeout;
    let max_retries = cfg.xmodem_max_retries;
    let negotiation_retry_interval = cfg.xmodem_negotiation_retry_interval;

    let mut file_data = Vec::new();
    let mut expected_block: u8 = 1;
    let mut state_owned = ReadState::default();
    let state = &mut state_owned;
    // Set when we successfully handle a YMODEM filename-header block so
    // the EOT handler knows to run the end-of-batch handshake.
    let mut ymodem_mode = false;
    // Parsed metadata from a YMODEM block 0.  Reported file length, when
    // present, drives end-of-transfer truncation (Forsberg §5) instead of
    // SUB-stripping — critical for files that legitimately end in 0x1A
    // bytes.  Modtime and mode are returned to the caller for fs-attribute
    // application after save; we don't apply them ourselves.
    let mut ymodem_meta: Option<YmodemReceiveMeta> = None;
    // Completed files, and the filename of the file currently being received
    // (from its YMODEM block 0).  For a YMODEM batch these accumulate one
    // entry per file; plain XMODEM / single-file YMODEM push exactly one.
    let mut files: Vec<XmodemReceivedFile> = Vec::new();
    let mut current_filename: Option<String> = None;
    let negotiation_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(negotiation_timeout);

    if verbose { glog!("XMODEM recv: starting negotiation (is_tcp={}, is_petscii={})", is_tcp, is_petscii); }

    // Negotiate mode: try CRC first ('C') for 2/3 of the negotiation
    // window, then fall back to checksum (NAK) for the remaining time.
    // With default config (45s window / 7s retry interval) that's ~4
    // CRC requests before the fallback — plenty of time for the user to
    // start a CRC-capable sender.
    let mut mode = TransferMode::Crc16;
    let mut attempt: u32 = 0;

    // `.max(1)` on the divisor guards against a divide-by-zero panic if
    // `negotiation_retry_interval` is ever 0 — today the config layer floors
    // it at ≥1, but this keeps the invariant local rather than relying on it.
    let crc_attempts =
        (negotiation_timeout * 2 / 3 / negotiation_retry_interval.max(1)).max(3) as u32;
    let max_negotiation_attempts = crc_attempts + max_retries as u32;
    loop {
        if tokio::time::Instant::now() >= negotiation_deadline {
            return Err("Negotiation timeout: start your XMODEM sender".into());
        }
        if attempt >= max_negotiation_attempts {
            return Err("Negotiation failed: no response from sender".into());
        }

        let request = if attempt < crc_attempts { CRC_REQUEST } else { NAK };
        if attempt == crc_attempts {
            mode = TransferMode::Checksum;
        }
        if verbose { glog!("XMODEM recv: attempt {} sending 0x{:02X} ({})",
            attempt, request, if request == CRC_REQUEST { "CRC req" } else { "NAK" }); }
        raw_write_byte(writer, request, is_tcp).await?;

        match tokio::time::timeout(
            std::time::Duration::from_secs(negotiation_retry_interval),
            nvt_read_byte(reader, is_tcp, state),
        )
        .await
        {
            Ok(Ok(byte)) => {
                if verbose { glog!("XMODEM recv: got 0x{:02X} during negotiation", byte); }
                if is_esc_key(byte, is_petscii) {
                    return Err("Transfer cancelled".into());
                }
                if is_can_abort(byte, state) {
                    return Err("Transfer cancelled by sender".into());
                }
                if byte == CAN {
                    if verbose { glog!("XMODEM recv: single CAN treated as line noise (waiting for second)"); }
                    continue;
                }
                if byte == SOH || byte == STX {
                    let block_size = if byte == STX {
                        XMODEM_1K_BLOCK_SIZE
                    } else {
                        XMODEM_BLOCK_SIZE
                    };
                    if verbose {
                        glog!(
                            "XMODEM recv: {} received, peeking at block header ({}-byte)",
                            if byte == STX { "STX" } else { "SOH" },
                            block_size,
                        );
                    }
                    // Peek at block_num / complement so we can detect
                    // YMODEM block 0 (filename header) vs. an ordinary
                    // first data block.  Bound both reads so a sender that
                    // emits SOH/STX then stalls can't hang the session at
                    // the negotiation→first-block boundary (the negotiation
                    // timeout above only covered the header byte itself).
                    let block_num = match tokio::time::timeout(
                        std::time::Duration::from_secs(BLOCK_BODY_TIMEOUT_SECS),
                        nvt_read_byte(reader, is_tcp, state),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err("XMODEM: timed out reading first block header".into()),
                    };
                    let block_complement = match tokio::time::timeout(
                        std::time::Duration::from_secs(BLOCK_BODY_TIMEOUT_SECS),
                        nvt_read_byte(reader, is_tcp, state),
                    )
                    .await
                    {
                        Ok(r) => r?,
                        Err(_) => return Err("XMODEM: timed out reading first block header".into()),
                    };
                    if byte == SOH
                        && block_num == 0
                        && block_complement == 0xFF
                    {
                        // YMODEM block 0 — read the 128-byte payload +
                        // trailer under a hard timeout so a stalled
                        // sender can't deadlock the session.  On CRC
                        // success we ACK and send a second 'C' to start
                        // the data phase; on failure we NAK and let the
                        // sender's retry be handled as a duplicate block
                        // by the main loop.
                        if verbose { glog!("XMODEM recv: YMODEM block 0 detected"); }
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(BLOCK_BODY_TIMEOUT_SECS),
                            read_ymodem_block_zero_body(
                                // YMODEM block 0 is always CRC-16; never let
                                // the negotiation's CRC→checksum fallback flip
                                // its validation.  If block 0 took enough
                                // retries to cross the fallback point, reading
                                // a CRC retransmit as a 1-byte checksum would
                                // mismatch and NAK-loop to exhaustion.
                                reader, TransferMode::Crc16, is_tcp, verbose, state,
                            ),
                        )
                        .await
                        {
                            Ok(Ok((true, _is_terminator, filename, meta))) => {
                                raw_write_byte(writer, ACK, is_tcp).await?;
                                // Second 'C' starts the data phase.
                                raw_write_byte(writer, CRC_REQUEST, is_tcp).await?;
                                // YMODEM is CRC-16 throughout; pin the mode so
                                // a negotiation fallback to checksum (if block
                                // 0 took many retries) can't carry into the
                                // data phase and misread CRC blocks as a 1-byte
                                // checksum.
                                mode = TransferMode::Crc16;
                                ymodem_mode = true;
                                ymodem_meta = meta;
                                current_filename = filename;
                                break;
                            }
                            Ok(Ok((false, _, _, _))) => {
                                if verbose { glog!("XMODEM recv: YMODEM block 0 CRC error, NAKing for retransmit"); }
                                raw_write_byte(writer, NAK, is_tcp).await?;
                                // Stay in negotiation: the sender will
                                // retransmit block 0; the next loop
                                // iteration's read picks it up.  Without
                                // this, the retransmit would fall through
                                // to the main loop where expected_block=1
                                // mismatches block_num=0 and the session
                                // NAK-loops to exhaustion.
                                attempt = attempt.saturating_add(1);
                                continue;
                            }
                            Ok(Err(e)) => {
                                if verbose { glog!("XMODEM recv: YMODEM block 0 read error: {}, NAKing for retransmit", e); }
                                raw_write_byte(writer, NAK, is_tcp).await?;
                                attempt = attempt.saturating_add(1);
                                continue;
                            }
                            Err(_) => {
                                if verbose { glog!("XMODEM recv: YMODEM block 0 timeout, NAKing for retransmit"); }
                                raw_write_byte(writer, NAK, is_tcp).await?;
                                attempt = attempt.saturating_add(1);
                                continue;
                            }
                        }
                    }
                    // Not YMODEM block 0 — treat as an ordinary first
                    // data block.  `receive_block_body` takes the
                    // already-read header bytes.  Pass `auto_detect=true`
                    // so a trailer-format mismatch falls back to the
                    // alternate mode and locks the session — closes the
                    // negotiation timing race against vintage senders
                    // (Christensen 1977 / CP/M MODEM7 / C64 BBS clients
                    // that ignore 'C' until NAK'd) and against modern
                    // senders that started in CRC mode but our flip to
                    // checksum landed mid-flight.
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(BLOCK_BODY_TIMEOUT_SECS),
                        receive_block_body(
                            reader,
                            block_num,
                            block_complement,
                            &mut expected_block,
                            &mut mode,
                            is_tcp,
                            verbose,
                            block_size,
                            state,
                            true, // auto_detect on first block
                        ),
                    )
                    .await
                    {
                        Ok(Ok(data)) => {
                            if verbose { glog!("XMODEM recv: block #1 OK"); }
                            file_data.extend_from_slice(&data);
                            raw_write_byte(writer, ACK, is_tcp).await?;
                        }
                        Ok(Err(e)) => {
                            if verbose { glog!("XMODEM recv: block #1 error: {}", e); }
                            raw_write_byte(writer, NAK, is_tcp).await?;
                        }
                        Err(_) => {
                            if verbose { glog!("XMODEM recv: block #1 timeout"); }
                            raw_write_byte(writer, NAK, is_tcp).await?;
                        }
                    }
                    break;
                }
                if byte == EOT {
                    raw_write_byte(writer, ACK, is_tcp).await?;
                    // EOT before any data block: an empty transfer.  Return the
                    // (empty) file so the wrapper's single-file contract holds.
                    files.push(XmodemReceivedFile {
                        filename: current_filename.take(),
                        data: std::mem::take(&mut file_data),
                        meta: ymodem_meta.take(),
                    });
                    return Ok(files);
                }
                // CAN handled above by is_can_abort + single-CAN continue.
                if verbose { glog!("XMODEM recv: ignoring unexpected byte 0x{:02X}", byte); }
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                if verbose { glog!("XMODEM recv: attempt {} timeout, retrying", attempt); }
            }
        }

        attempt = attempt.saturating_add(1);
    }

    // Main receive loop
    let mut error_count: usize = 0;
    // Forsberg receiver EOT verification (plain XMODEM): NAK the first EOT
    // and accept end-of-file only on a resent, confirming EOT.  A lone 0x04
    // from serial line noise in the inter-block gap is then re-prompted
    // instead of silently truncating the file — the failure mode that
    // matters on a real UART (the gateway's primary transport), not just on
    // clean TCP.  Reset ONLY when a new (non-duplicate) block is accepted:
    // a spurious EOT makes the sender resend the in-flight block (a new,
    // expected block → accepted → reset → carry on), whereas a non-standard
    // sender that answers NAK-of-EOT by resending its last block sends a
    // duplicate (flag stays set → the following real EOT is ACKed → no
    // infinite NAK loop).
    let mut eot_naked = false;
    loop {
        let byte = match tokio::time::timeout(
            std::time::Duration::from_secs(block_timeout),
            nvt_read_byte(reader, is_tcp, state),
        )
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // Spec receiver recovery (Forsberg/Christensen): a missing or
                // late block is recovered by NAKing to re-prompt the sender —
                // which retransmits the block it is still awaiting an ACK for —
                // not by an immediate abort.  Share the block-error retry
                // counter so a flapping link is bounded the same way; only
                // after `max_retries` consecutive failures do we cancel.
                error_count += 1;
                if error_count > max_retries {
                    raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                    return Err("Transfer timeout: no data after retries".into());
                }
                if verbose { glog!("XMODEM recv: inter-block timeout, NAK (retry {}/{})", error_count, max_retries); }
                raw_write_byte(writer, NAK, is_tcp).await?;
                continue;
            }
        };

        if is_can_abort(byte, state) {
            return Err("Transfer cancelled by sender".into());
        }

        match byte {
            SOH | STX => {
                let block_size = if byte == STX {
                    XMODEM_1K_BLOCK_SIZE
                } else {
                    XMODEM_BLOCK_SIZE
                };
                match tokio::time::timeout(
                    std::time::Duration::from_secs(BLOCK_BODY_TIMEOUT_SECS),
                    receive_block(
                        reader,
                        &mut expected_block,
                        &mut mode,
                        is_tcp,
                        verbose,
                        block_size,
                        state,
                    ),
                )
                .await
                {
                    Ok(Ok(data)) => {
                        // Per-file cap, enforced BEFORE appending so the
                        // buffer never exceeds MAX_FILE_SIZE even transiently
                        // (X2 — the old top-of-loop `>` check let a file grow
                        // one block past the limit first).  Exactly 8 MB is
                        // still accepted; the block that would push it over is
                        // refused.  In a YMODEM batch this aborts the whole
                        // session (CAN×3 + Err), discarding earlier files too —
                        // an oversize file is a hard error, not skip-and-carry.
                        if file_data.len() + data.len() > MAX_FILE_SIZE {
                            raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                            return Err("File exceeds 8 MB size limit".into());
                        }
                        file_data.extend_from_slice(&data);
                        raw_write_byte(writer, ACK, is_tcp).await?;
                        error_count = 0;
                        // A fresh block arrived after an EOT we NAKed — that
                        // EOT was spurious (noise) and the sender resent the
                        // in-flight block.  Re-arm so the real end-of-file
                        // EOT is verified afresh.
                        eot_naked = false;
                    }
                    Ok(Err(ref e)) if e == "Duplicate block" => {
                        raw_write_byte(writer, ACK, is_tcp).await?;
                    }
                    // Spec: a valid block (good CRC + complement) carrying a
                    // non-duplicate, unexpected sequence number is an
                    // unrecoverable sync loss — XMODEM has no way to request a
                    // specific block, so NAKing would only loop.  Cancel.
                    Ok(Err(ref e)) if e == "Block number mismatch" => {
                        if verbose { glog!("XMODEM recv: block sequence error, cancelling"); }
                        raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                        return Err("Block sequence error".into());
                    }
                    Ok(Err(_)) | Err(_) => {
                        error_count += 1;
                        if error_count > max_retries {
                            raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                            return Err("Too many block errors".into());
                        }
                        raw_write_byte(writer, NAK, is_tcp).await?;
                    }
                }
            }
            EOT => {
                // NAK the first EOT (plain XMODEM) and accept end-of-file
                // only when the sender resends it — Forsberg's spurious-EOT
                // guard, which matters on a noisy serial line.  YMODEM keeps
                // immediate-ACK: its post-EOT 'C' + null-block-0 end-of-batch
                // handshake already confirms completion, and the block-0 size
                // field lets the receiver detect a short file regardless.
                if !ymodem_mode && !eot_naked {
                    eot_naked = true;
                    if verbose { glog!("XMODEM recv: first EOT — NAKing to verify (Forsberg EOT confirmation)"); }
                    raw_write_byte(writer, NAK, is_tcp).await?;
                    continue;
                }
                if verbose { glog!("XMODEM recv: EOT confirmed, ACKing"); }
                raw_write_byte(writer, ACK, is_tcp).await?;

                // Finalize the file that just completed (truncate to block-0
                // size, else strip SUB padding) and record it.
                let meta = ymodem_meta.take();
                let data = finalize_received_file(std::mem::take(&mut file_data), &meta, verbose);
                files.push(XmodemReceivedFile {
                    filename: current_filename.take(),
                    data,
                    meta,
                });

                if !ymodem_mode {
                    // Plain XMODEM: a single file, done.
                    break;
                }

                // YMODEM inter-file / end-of-batch (Forsberg §7.4): after ACKing
                // the EOT, request the next block 0.  A named block 0 is the NEXT
                // file (reset per-file state, continue); the null-terminator block
                // ends the batch.  A corrupt-but-present block 0 is NAK-retried
                // (bounded) so a noisy line can't silently truncate the batch —
                // matching file 1's negotiation.  Nothing coherent arriving (lax
                // sender that skipped the terminator) just ends the batch.
                if verbose { glog!("XMODEM recv: YMODEM inter-file, sending 'C'"); }
                raw_write_byte(writer, CRC_REQUEST, is_tcp).await?;
                let mut b0_attempt: usize = 0;
                let inter = loop {
                    let outcome = tokio::time::timeout(
                        std::time::Duration::from_secs(block_timeout),
                        async {
                            let b = nvt_read_byte(reader, is_tcp, state).await?;
                            if b != SOH {
                                return Ok::<InterFileBlock0, String>(InterFileBlock0::NotBlock0);
                            }
                            let bn = nvt_read_byte(reader, is_tcp, state).await?;
                            let bc = nvt_read_byte(reader, is_tcp, state).await?;
                            if bn != 0 || bc != 0xFF {
                                // Header isn't a block 0 (stray data block / noise).
                                return Ok(InterFileBlock0::Invalid);
                            }
                            let (valid, is_term, filename, meta) = read_ymodem_block_zero_body(
                                reader, TransferMode::Crc16, is_tcp, verbose, state,
                            )
                            .await?;
                            Ok(if !valid {
                                InterFileBlock0::Invalid
                            } else if is_term {
                                InterFileBlock0::Terminator
                            } else {
                                InterFileBlock0::File(filename, meta)
                            })
                        },
                    )
                    .await;
                    match outcome {
                        // A corrupt-but-present block 0 is the only case we
                        // retry: NAK for a retransmit, bounded like the first
                        // file's block-0 retries, then give up as `NotBlock0`.
                        Ok(Ok(InterFileBlock0::Invalid)) => {
                            b0_attempt += 1;
                            if b0_attempt > max_retries {
                                if verbose { glog!("XMODEM recv: inter-file block 0 corrupt past retries, ending batch"); }
                                break InterFileBlock0::NotBlock0;
                            }
                            if verbose { glog!("XMODEM recv: inter-file block 0 CRC/header error, NAKing"); }
                            raw_write_byte(writer, NAK, is_tcp).await?;
                        }
                        // File / Terminator / NotBlock0 pass straight through.
                        Ok(Ok(other)) => break other,
                        // Nothing coherent (read error / timeout) — end the
                        // batch now, keeping already-received files.
                        Ok(Err(e)) => {
                            if verbose { glog!("XMODEM recv: inter-file read error: {}", e); }
                            break InterFileBlock0::NotBlock0;
                        }
                        Err(_) => {
                            if verbose { glog!("XMODEM recv: inter-file timeout — lax sender, ending batch"); }
                            break InterFileBlock0::NotBlock0;
                        }
                    }
                };
                match inter {
                    InterFileBlock0::File(filename, meta) => {
                        // Cap the batch so an unterminated / hostile stream can't
                        // grow the in-memory file list without bound.
                        if files.len() >= MAX_BATCH_FILES {
                            raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                            return Err("YMODEM batch exceeds file-count limit".into());
                        }
                        // Next file: ACK its block 0, send 'C' to start its data
                        // phase, reset per-file state, and loop.  `filename` may be
                        // None (non-UTF-8 name) — the caller assigns a fallback.
                        raw_write_byte(writer, ACK, is_tcp).await?;
                        raw_write_byte(writer, CRC_REQUEST, is_tcp).await?;
                        current_filename = filename;
                        ymodem_meta = meta;
                        expected_block = 1;
                        error_count = 0;
                        if verbose { glog!("XMODEM recv: YMODEM next file in batch"); }
                        continue;
                    }
                    InterFileBlock0::Terminator => {
                        // Null block 0 — end of batch.  ACK and finish.
                        raw_write_byte(writer, ACK, is_tcp).await?;
                        if verbose { glog!("XMODEM recv: YMODEM end-of-batch ACKed"); }
                    }
                    // NotBlock0 (incl. retries-exhausted): end without ACK.
                    // `Invalid` never reaches here — the loop always converts it
                    // to a NAK-retry or a `NotBlock0` break — but the arm is
                    // required for exhaustiveness.
                    InterFileBlock0::Invalid | InterFileBlock0::NotBlock0 => {}
                }
                break;
            }
            CAN => {
                // Single CAN — Forsberg's CAN×2 rule says ignore as
                // possible line noise.  Don't NAK; just keep reading.
                // `is_can_abort` already set `pending_can`; if the
                // very next byte is also CAN we'll abort there.
                if verbose { glog!("XMODEM recv: single CAN treated as line noise"); }
            }
            _ => {
                raw_write_byte(writer, NAK, is_tcp).await?;
            }
        }
    }

    // Every completed file was finalized (size-truncated / SUB-stripped) and
    // pushed at its EOT; nothing to do here but hand back the batch.
    Ok(files)
}

/// Receive and validate a single XMODEM block (after SOH or STX was
/// already read).  `block_size` is 128 for SOH blocks, 1024 for STX
/// (XMODEM-1K) blocks — within a single transfer the sender may mix
/// block sizes, so each call picks up the right size from its header.
#[allow(clippy::too_many_arguments)]
async fn receive_block(
    reader: &mut (impl AsyncRead + Unpin),
    expected_block: &mut u8,
    mode: &mut TransferMode,
    is_tcp: bool,
    verbose: bool,
    block_size: usize,
    state: &mut ReadState,
) -> Result<Vec<u8>, String> {
    let block_num = nvt_read_byte(reader, is_tcp, state).await?;
    let block_complement = nvt_read_byte(reader, is_tcp, state).await?;
    receive_block_body(
        reader,
        block_num,
        block_complement,
        expected_block,
        mode,
        is_tcp,
        verbose,
        block_size,
        state,
        false, // mode locked after first block
    )
    .await
}

/// Read and validate the 128-byte payload + CRC/checksum trailer of a
/// YMODEM block 0, given that `SOH 0x00 0xFF` has already been read.
/// Returns `(valid, meta)` — `valid=false` means CRC/checksum mismatch
/// and `meta` is meaningless; on `valid=true` `meta` carries whatever
/// metadata fields were parsed.  Called under a `tokio::time::timeout`
/// so a stalled sender can't hold the session indefinitely.
///
/// Per Forsberg YMODEM §6.1 the block-0 payload is:
///
///     filename\0length<SP>modtime<SP>mode<SP>sno<SP>...\0<NUL fill>
///
/// where `length` is decimal and `modtime`/`mode`/`sno` are octal.
/// All metadata fields are optional from the receiver's standpoint —
/// minimal senders omit the trailing fields, and we tolerate that.
async fn read_ymodem_block_zero_body(
    reader: &mut (impl AsyncRead + Unpin),
    mode: TransferMode,
    is_tcp: bool,
    verbose: bool,
    state: &mut ReadState,
) -> Result<(bool, bool, Option<String>, Option<YmodemReceiveMeta>), String> {
    let mut payload = [0u8; XMODEM_BLOCK_SIZE];
    for b in payload.iter_mut() {
        *b = nvt_read_byte(reader, is_tcp, state).await?;
    }
    let valid = match mode {
        TransferMode::Crc16 => {
            let hi = nvt_read_byte(reader, is_tcp, state).await?;
            let lo = nvt_read_byte(reader, is_tcp, state).await?;
            let recv = ((hi as u16) << 8) | lo as u16;
            recv == crc16_xmodem(&payload)
        }
        TransferMode::Checksum => {
            let recv = nvt_read_byte(reader, is_tcp, state).await?;
            let calc = payload.iter().fold(0u8, |a, &b| a.wrapping_add(b));
            recv == calc
        }
    };
    if !valid {
        return Ok((false, false, None, None));
    }
    // The end-of-batch terminator is defined by the filename field starting with
    // NUL (Forsberg §7.4) — NOT by the name failing to decode.  Keying off
    // `payload[0] == 0` keeps a legitimately-named but non-UTF-8 file (whose
    // `filename` below is `None`) from being mistaken for the terminator, which
    // would silently drop it and every later file in the batch.
    let is_terminator = payload[0] == 0;
    // Extract the filename (bytes up to the first NUL).  Non-UTF-8 names decode
    // to `None`; the file is still received and the caller generates a name.
    let filename = payload
        .iter()
        .position(|&b| b == 0)
        .filter(|&n| n > 0)
        .and_then(|n| std::str::from_utf8(&payload[..n]).ok())
        .map(|s| s.to_string());
    let parsed = parse_ymodem_block_zero_payload(&payload);
    if verbose {
        let name = payload
            .iter()
            .position(|&b| b == 0)
            .and_then(|n| std::str::from_utf8(&payload[..n]).ok())
            .unwrap_or("<invalid>");
        glog!(
            "XMODEM recv: YMODEM filename='{}' size={} modtime={} mode={}",
            name,
            parsed
                .as_ref()
                .and_then(|m| m.size)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".into()),
            parsed
                .as_ref()
                .and_then(|m| m.modtime)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".into()),
            parsed
                .as_ref()
                .and_then(|m| m.mode)
                .map(|n| format!("{:o}", n))
                .unwrap_or_else(|| "<unknown>".into()),
        );
    }
    Ok((true, is_terminator, filename, parsed))
}

/// Parse the 128-byte block-0 payload into a `YmodemReceiveMeta`.  Returns
/// `None` if the payload is empty (filename starts with NUL — the
/// end-of-batch terminator block).  Otherwise returns `Some(meta)` with
/// whatever fields were present and well-formed.
///
/// Field encoding per Forsberg YMODEM §6.1: `length` is decimal,
/// `modtime`/`mode`/`sno` are octal.  Anything that fails to parse
/// stays `None` rather than poisoning the rest — minimal senders that
/// omit fields, and broken senders that emit junk, are both tolerated
/// the same way.
fn parse_ymodem_block_zero_payload(payload: &[u8]) -> Option<YmodemReceiveMeta> {
    let name_end = payload.iter().position(|&b| b == 0)?;
    if name_end == 0 {
        // End-of-batch null block 0 — no metadata to extract.
        return None;
    }
    let mut meta = YmodemReceiveMeta::default();
    let after = &payload[name_end + 1..];
    let Some(fields_end) = after.iter().position(|&b| b == 0) else {
        return Some(meta);
    };
    let text = match std::str::from_utf8(&after[..fields_end]) {
        Ok(s) => s,
        Err(_) => return Some(meta),
    };
    let mut fields = text.split_ascii_whitespace();
    if let Some(first) = fields.next()
        && let Ok(n) = first.parse::<u64>()
    {
        meta.size = Some(n);
    }
    if let Some(second) = fields.next()
        && let Ok(n) = u64::from_str_radix(second, 8)
        && n != 0
    {
        meta.modtime = Some(n);
    }
    if let Some(third) = fields.next()
        && let Ok(n) = u32::from_str_radix(third, 8)
        && n != 0
    {
        // Mask to permission + setuid/setgid/sticky bits.  The upload
        // path further restricts to `0o777` before applying.
        meta.mode = Some(n & 0o7777);
    }
    Some(meta)
}

/// Same as `receive_block` but the block-number + complement bytes have
/// already been read by the caller.  Used for YMODEM first-block
/// handling where we peek at `block_num` to distinguish block 0
/// (filename header) from block 1 (first data block).
///
/// `auto_detect` is true only for the first data block.  When true, a
/// trailer-format mismatch falls back to the alternate mode and locks
/// `mode` to whatever validates — closes the negotiation timing race
/// where the receiver's mode-flip happened mid-flight against a sender
/// that hadn't yet seen the new request.  After the first block we
/// trust the mode; per-block auto-detect would carry a 1/256 false-
/// positive risk that a coincidental checksum match swallows a CRC
/// block's low byte.
#[allow(clippy::too_many_arguments)]
async fn receive_block_body(
    reader: &mut (impl AsyncRead + Unpin),
    block_num: u8,
    block_complement: u8,
    expected_block: &mut u8,
    mode: &mut TransferMode,
    is_tcp: bool,
    verbose: bool,
    block_size: usize,
    state: &mut ReadState,
    auto_detect: bool,
) -> Result<Vec<u8>, String> {
    if verbose { glog!("XMODEM recv block: num=0x{:02X} complement=0x{:02X} expected=0x{:02X} size={} mode={}{}",
        block_num, block_complement, *expected_block, block_size,
        match *mode { TransferMode::Crc16 => "CRC16", TransferMode::Checksum => "Checksum" },
        if auto_detect { " (auto-detect)" } else { "" }); }

    let mut data = vec![0u8; block_size];
    for byte in data.iter_mut() {
        *byte = nvt_read_byte(reader, is_tcp, state).await?;
    }

    // Auto-detect mutations are deferred until after all post-validation
    // checks pass.  Without this, a complement-mismatch / wrong-block /
    // duplicate Err'd block whose trailer happened to validate under the
    // alternate mode would leave `*mode` flipped and a stray byte in
    // `state.pushback` — wedging the session for the next read.
    enum AutoDetect {
        None,
        LockToCrc,
        // `pushback` is `Some` for a *streaming* checksum sender (the byte
        // read past the 1-byte checksum trailer is the next block's leading
        // byte and must be restored) and `None` for a *lock-step* checksum
        // sender that sent only the single trailer byte (nothing to restore).
        LockToChecksum { pushback: Option<u8> },
    }
    let mut detected = AutoDetect::None;

    let valid = match *mode {
        TransferMode::Checksum => {
            let recv_checksum = nvt_read_byte(reader, is_tcp, state).await?;
            let calc_checksum = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
            if verbose { glog!("XMODEM recv block: checksum recv=0x{:02X} calc=0x{:02X}", recv_checksum, calc_checksum); }
            if recv_checksum == calc_checksum {
                true
            } else if auto_detect {
                // Mismatch on the first block — the sender may actually be
                // in CRC mode despite our checksum NAK.  Read one more byte
                // and try CRC validation; if it matches, lock to CRC mode.
                // Gate that read behind the same short grace window as the
                // CRC-mode branch (X1): a lock-step *checksum* sender that
                // sent one (mismatching) trailer byte and is now waiting for
                // our ACK/NAK sends nothing more, so without the bound this
                // stalls until the 60 s block-body timeout instead of NAKing
                // promptly.  On timeout there is no second byte → reject (NAK).
                match tokio::time::timeout(
                    std::time::Duration::from_secs(AUTO_DETECT_TRAILER_TIMEOUT_SECS),
                    nvt_read_byte(reader, is_tcp, state),
                )
                .await
                {
                    Ok(b) => {
                        let crc_lo = b?;
                        let recv_crc = ((recv_checksum as u16) << 8) | crc_lo as u16;
                        let calc_crc = crc16_xmodem(&data);
                        if recv_crc == calc_crc {
                            if verbose { glog!("XMODEM recv block: auto-detect would lock to CRC16 (CRC=0x{:04X})", calc_crc); }
                            detected = AutoDetect::LockToCrc;
                            true
                        } else {
                            false
                        }
                    }
                    Err(_) => false,
                }
            } else {
                false
            }
        }
        TransferMode::Crc16 => {
            let crc_hi = nvt_read_byte(reader, is_tcp, state).await?;
            // Read the CRC low byte.  On the first block (auto-detect) a
            // strict lock-step checksum-only sender emits just ONE trailer
            // byte and then waits for our ACK/NAK, so an unconditional second
            // read would stall until the 60 s block-body timeout (X1).  Gate
            // it behind a short grace window: a real CRC sender's low byte
            // follows the high byte back-to-back and is already here, so the
            // timeout never fires for it; if nothing arrives we treat the
            // block as having no second trailer byte.  After the first block
            // the mode is locked, so we block unconditionally (a slow link
            // must never be mistaken for a missing byte).
            let crc_lo = if auto_detect {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(AUTO_DETECT_TRAILER_TIMEOUT_SECS),
                    nvt_read_byte(reader, is_tcp, state),
                )
                .await
                {
                    Ok(b) => Some(b?),
                    Err(_) => None,
                }
            } else {
                Some(nvt_read_byte(reader, is_tcp, state).await?)
            };
            let calc_crc = crc16_xmodem(&data);
            match crc_lo {
                Some(crc_lo) => {
                    let recv_crc = ((crc_hi as u16) << 8) | crc_lo as u16;
                    if verbose { glog!("XMODEM recv block: CRC recv=0x{:04X} calc=0x{:04X}", recv_crc, calc_crc); }
                    if recv_crc == calc_crc {
                        true
                    } else if auto_detect {
                        // Two trailer bytes arrived but CRC failed — sender
                        // may be a *streaming* checksum peer (Christensen 1977
                        // / CP/M MODEM7 / C64 BBS uploader that ignored our
                        // 'C' until we NAK'd).  Validate crc_hi as a 1-byte
                        // checksum; if it matches, crc_lo was actually the
                        // next block's leading byte (SOH/EOT) — push it back.
                        // Defer the lock + pushback until the block passes the
                        // remaining checks.
                        let calc_checksum = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
                        if crc_hi == calc_checksum {
                            if verbose { glog!("XMODEM recv block: auto-detect would lock to Checksum (sum=0x{:02X}, would push back trailer byte 0x{:02X})", calc_checksum, crc_lo); }
                            detected = AutoDetect::LockToChecksum { pushback: Some(crc_lo) };
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
                None => {
                    // Auto-detect only: no second trailer byte within the
                    // grace window — a *lock-step* checksum-only sender that
                    // sent just the single checksum byte and is now waiting
                    // for our ACK/NAK.  Validate crc_hi as the whole 1-byte
                    // checksum; there is no byte to push back.
                    let calc_checksum = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
                    if verbose { glog!("XMODEM recv block: auto-detect saw one trailer byte (checksum recv=0x{:02X} calc=0x{:02X})", crc_hi, calc_checksum); }
                    if crc_hi == calc_checksum {
                        detected = AutoDetect::LockToChecksum { pushback: None };
                        true
                    } else {
                        false
                    }
                }
            }
        }
    };

    if block_complement != !(block_num) {
        if verbose { glog!("XMODEM recv block: FAIL complement mismatch 0x{:02X} != !0x{:02X} (0x{:02X})",
            block_complement, block_num, !(block_num)); }
        return Err("Block complement mismatch".into());
    }
    if !valid {
        return Err("Checksum/CRC error".into());
    }
    // Duplicate detection: ACK retransmits of either of the two most
    // recent blocks per Forsberg's recommendation that any already-seen
    // block be acknowledged.  Going beyond two risks racing the 8-bit
    // sequence wraparound on long transfers (>32 KB at SOH, >256 MB at
    // STX); two covers the realistic sender-recovery scenarios.
    if block_num == expected_block.wrapping_sub(1)
        || block_num == expected_block.wrapping_sub(2)
    {
        return Err("Duplicate block".into());
    }
    if block_num != *expected_block {
        if verbose { glog!("XMODEM recv block: FAIL block number 0x{:02X} != expected 0x{:02X}", block_num, *expected_block); }
        return Err("Block number mismatch".into());
    }

    // Block fully accepted — now safe to commit the auto-detect mode
    // flip (and pushback for the checksum-under-CRC case).
    match detected {
        AutoDetect::None => {}
        AutoDetect::LockToCrc => {
            if verbose { glog!("XMODEM recv block: locking session to CRC16"); }
            *mode = TransferMode::Crc16;
        }
        AutoDetect::LockToChecksum { pushback } => {
            *mode = TransferMode::Checksum;
            match pushback {
                Some(b) => {
                    if verbose { glog!("XMODEM recv block: locking session to Checksum, pushing back 0x{:02X}", b); }
                    state.pushback = Some(b);
                }
                None => {
                    if verbose { glog!("XMODEM recv block: locking session to Checksum (lone trailer byte, nothing to push back)"); }
                }
            }
        }
    }

    *expected_block = expected_block.wrapping_add(1);
    Ok(data)
}

// =============================================================================
// XMODEM PROTOCOL - SEND (DOWNLOAD)
// =============================================================================

#[allow(clippy::too_many_arguments)]
pub(crate) async fn xmodem_send(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
    use_1k: bool,
    ymodem: Option<YmodemHeader>,
) -> Result<(), String> {
    let cfg = config::get_config();
    let negotiation_timeout = cfg.xmodem_negotiation_timeout;
    let block_timeout = cfg.xmodem_block_timeout;
    let max_retries = cfg.xmodem_max_retries;
    let mut state_owned = ReadState::default();
    let state = &mut state_owned;

    let negotiation_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(negotiation_timeout);

    if verbose { glog!("XMODEM send: starting negotiation (is_tcp={}, is_petscii={}, data_len={})",
        is_tcp, is_petscii, data.len()); }

    // Wait for receiver's mode request (C = CRC, NAK = checksum)
    let mode = loop {
        let remaining = negotiation_deadline.duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("Negotiation timeout: start your XMODEM receiver".into());
        }

        match tokio::time::timeout(remaining, nvt_read_byte(reader, is_tcp, state)).await {
            Ok(Ok(byte)) => {
                if verbose { glog!("XMODEM send: negotiation got 0x{:02X}", byte); }
                if is_esc_key(byte, is_petscii) {
                    return Err("Transfer cancelled".into());
                }
                if is_can_abort(byte, state) {
                    return Err("Transfer cancelled by receiver".into());
                }
                match byte {
                    CRC_REQUEST => {
                        if verbose { glog!("XMODEM send: receiver requests CRC mode"); }
                        break TransferMode::Crc16;
                    }
                    NAK => {
                        if verbose { glog!("XMODEM send: receiver requests Checksum mode"); }
                        break TransferMode::Checksum;
                    }
                    CAN => {
                        // Single CAN — Forsberg's CAN×2 rule treats it
                        // as possible line noise.  Keep waiting for the
                        // next byte; `is_can_abort` already armed
                        // `pending_can` so a second CAN aborts.
                        if verbose { glog!("XMODEM send: single CAN treated as line noise during negotiation"); }
                        continue;
                    }
                    _ => {
                        if verbose { glog!("XMODEM send: ignoring byte 0x{:02X} during negotiation", byte); }
                        continue;
                    }
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err("Timeout waiting for receiver to start".into());
            }
        }
    };

    // Drain any trailing negotiation bytes (e.g. IMP8 sends 'C' then 'K' for
    // XMODEM-1K; we accepted 'C' but 'K' is still in the buffer).
    // Uses nvt_read_byte to properly handle any IAC sequences on TCP.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    while let Ok(Ok(b)) = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        nvt_read_byte(reader, is_tcp, state),
    )
    .await
    {
        if verbose { glog!("XMODEM send: drained negotiation byte 0x{:02X}", b); }
    }

    // ─── YMODEM block 0 (filename header) ──────────────────
    //
    // When `ymodem` is set, emit an SOH block with block_num=0 carrying
    // `filename\0size mtime\0` followed by NUL padding out to 128 bytes.
    // The receiver ACKs block 0 and then sends a second 'C' byte to
    // signal it's ready for the data phase.
    if let Some(ref hdr) = ymodem {
        send_ymodem_block_zero(
            reader,
            writer,
            hdr,
            is_tcp,
            block_timeout,
            max_retries,
            verbose,
            state,
        )
        .await?;
        // Wait for the receiver's second 'C' (data-phase request).
        match tokio::time::timeout(
            std::time::Duration::from_secs(block_timeout),
            nvt_read_byte(reader, is_tcp, state),
        )
        .await
        {
            Ok(Ok(b)) if b == CRC_REQUEST => {
                if verbose { glog!("XMODEM send: got second 'C' after block 0"); }
            }
            Ok(Ok(b)) => {
                if verbose { glog!("XMODEM send: expected 'C' after block 0 got 0x{:02X}", b); }
            }
            _ => {
                if verbose { glog!("XMODEM send: timed out waiting for second 'C' after block 0"); }
            }
        }
    }

    // Pad data to a 128-byte boundary (the minimum granularity).  When
    // 1K mode is active we consume 1024 bytes per block for full
    // chunks and fall back to 128 for the final partial chunk.
    //
    // For empty input: in plain XMODEM the receiver has no length
    // info, so we must still send at least one block (filled with SUB)
    // so the receiver isn't left waiting for data after the start
    // request.  In YMODEM the block-0 length=0 already tells the
    // receiver to expect zero data bytes, so we skip the data phase
    // entirely and go straight to EOT.
    let mut padded = data.to_vec();
    if padded.is_empty() && ymodem.is_none() {
        padded.push(SUB);
    }
    while !padded.len().is_multiple_of(XMODEM_BLOCK_SIZE) {
        padded.push(SUB);
    }

    let mut block_num: u8 = 1;
    // Tracks the runtime 1K preference.  Starts from the caller's
    // intent and flips to false if the first STX block is rejected by
    // the receiver — from then on we stay with SOH for the rest of
    // the transfer.
    let mut use_1k_runtime = use_1k;
    let mut offset = 0usize;
    let mut block_idx = 0usize;
    if verbose { glog!("XMODEM send: data_len={} padded_len={} use_1k={}",
        data.len(), padded.len(), use_1k); }

    while offset < padded.len() {
        // Choose the block size for this iteration: STX (1024) if the
        // runtime flag still permits and we have a full 1024 bytes
        // left; otherwise SOH (128).  This naturally degrades to a
        // partial final SOH block when the file doesn't divide evenly.
        let use_stx = use_1k_runtime
            && padded.len() - offset >= XMODEM_1K_BLOCK_SIZE;
        let block_size = if use_stx { XMODEM_1K_BLOCK_SIZE } else { XMODEM_BLOCK_SIZE };
        let header = if use_stx { STX } else { SOH };
        let block = &padded[offset..offset + block_size];

        let mut retries = 0;
        loop {
            if retries >= max_retries {
                raw_write_bytes(writer, &[CAN, CAN, CAN], is_tcp).await?;
                return Err("Too many retries, transfer aborted".into());
            }

            let mut packet = Vec::with_capacity(3 + block_size + 2);
            packet.push(header);
            packet.push(block_num);
            packet.push(!block_num);
            packet.extend_from_slice(block);

            match mode {
                TransferMode::Checksum => {
                    let checksum = block.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
                    packet.push(checksum);
                }
                TransferMode::Crc16 => {
                    let crc = crc16_xmodem(block);
                    packet.push((crc >> 8) as u8);
                    packet.push((crc & 0xFF) as u8);
                }
            }

            if block_idx == 0 && retries == 0 && verbose {
                glog!(
                    "XMODEM send: block #1 header=0x{:02X} size={} num=0x{:02X} complement=0x{:02X} packet_len={}",
                    header, block_size, block_num, !block_num, packet.len(),
                );
            }

            raw_write_bytes(writer, &packet, is_tcp).await?;

            // Wait for ACK/NAK, draining single-CAN line noise per
            // Forsberg's CAN×2 abort rule.  The inner loop returns the
            // first non-CAN byte; CAN×2 returns Err immediately via
            // `is_can_abort`.  Read errors and timeouts surface to the
            // outer match for retry handling.
            enum Resp {
                Byte(u8),
                ReadErr(String),
                Timeout,
            }
            let response = loop {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(block_timeout),
                    nvt_read_byte(reader, is_tcp, state),
                )
                .await
                {
                    Ok(Ok(byte)) => {
                        if is_can_abort(byte, state) {
                            if verbose { glog!("XMODEM send: CAN×2 abort at block #{}", block_idx + 1); }
                            return Err("Transfer cancelled by receiver".into());
                        }
                        if byte == CAN {
                            if verbose { glog!("XMODEM send: single CAN at block #{} treated as line noise", block_idx + 1); }
                            continue;
                        }
                        break Resp::Byte(byte);
                    }
                    Ok(Err(e)) => break Resp::ReadErr(e),
                    Err(_) => break Resp::Timeout,
                }
            };
            match response {
                Resp::Byte(ACK) => {
                    if verbose && (block_idx < 3 || retries > 0) {
                        glog!("XMODEM send: block #{} ACK (retries={}, size={})",
                            block_idx + 1, retries, block_size);
                    }
                    break;
                }
                Resp::Byte(NAK) => {
                    if verbose { glog!("XMODEM send: block #{} NAK (retry {})", block_idx + 1, retries + 1); }
                    // Opportunistic fallback: if the very first block
                    // we sent used STX and the receiver rejected it,
                    // the receiver probably doesn't support 1K.  Drop
                    // to SOH for the rest of the transfer and retry
                    // with a 128-byte block from the same offset.
                    if use_stx && block_idx == 0 && retries == 0 {
                        if verbose { glog!(
                            "XMODEM send: STX rejected on first block, \
                             falling back to 128-byte SOH"
                        ); }
                        use_1k_runtime = false;
                        break;
                    }
                    retries += 1;
                    continue;
                }
                Resp::Byte(byte) => {
                    if verbose { glog!("XMODEM send: block #{} unexpected response 0x{:02X} (retry {})",
                        block_idx + 1, byte, retries + 1); }
                    retries += 1;
                    continue;
                }
                Resp::ReadErr(e) => return Err(e),
                Resp::Timeout => {
                    if verbose { glog!("XMODEM send: block #{} timeout (retry {})", block_idx + 1, retries + 1); }
                    retries += 1;
                    continue;
                }
            }
        }

        // Advance.  If we just fell back from STX to SOH we leave the
        // offset alone and the next loop iteration sends the same
        // payload bytes in a 128-byte SOH block.
        if use_1k_runtime || !use_stx {
            offset += block_size;
            block_idx += 1;
            block_num = block_num.wrapping_add(1);
        }
    }

    // Send EOT and wait for ACK.  A Forsberg-compliant receiver NAKs the
    // *first* EOT to verify end-of-file (guarding against a spurious EOT from
    // line noise) and ACKs only the resent one — our own receiver does exactly
    // this.  Completing that handshake therefore requires at least two EOT
    // attempts, so floor the budget at 2 even when `xmodem_max_retries` is 1;
    // otherwise a clean transfer to a verifying receiver would be reported as
    // failed after the single, *expected* verification NAK.  A receiver that
    // ACKs the first EOT still returns on the first pass, so the wire exchange
    // is unchanged in the common case.
    let eot_attempts = max_retries.max(2);
    for _ in 0..eot_attempts {
        raw_write_byte(writer, EOT, is_tcp).await?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(block_timeout),
            nvt_read_byte(reader, is_tcp, state),
        )
        .await
        {
            Ok(Ok(ACK)) => {
                // YMODEM end-of-batch: after EOT is ACKed, the receiver
                // sends one more 'C' and expects an empty block 0
                // (filename starts with NUL) meaning "no more files."
                if ymodem.is_some() {
                    send_ymodem_end_of_batch(
                        reader,
                        writer,
                        is_tcp,
                        block_timeout,
                        verbose,
                        state,
                    )
                    .await?;
                }
                return Ok(());
            }
            Ok(Ok(NAK)) => continue,
            Ok(Ok(b)) => {
                if verbose { glog!("XMODEM send: unexpected EOT response 0x{:02X}, treating as ACK", b); }
                if ymodem.is_some() {
                    // Best-effort: attempt the end-of-batch handshake
                    // but don't hard-fail the transfer if it flakes.
                    let _ = send_ymodem_end_of_batch(
                        reader,
                        writer,
                        is_tcp,
                        block_timeout,
                        verbose,
                        state,
                    )
                    .await;
                }
                return Ok(());
            }
            Ok(Err(e)) => {
                if verbose { glog!("XMODEM send: read error during EOT: {}", e); }
                return Err(format!("Read error during EOT: {}", e));
            }
            Err(_) => continue,
        }
    }
    // EOT exhausted without an ACK — the receiver may not have committed
    // the file.  Surface this so the caller can flag the transfer as
    // failed rather than silently claiming success.
    if verbose { glog!("XMODEM send: EOT not ACKed after {} attempts, returning error", eot_attempts); }
    Err(format!("EOT not ACKed after {} attempts", eot_attempts))
}

/// Build and transmit YMODEM block 0 (filename + size header).
/// Uses a 128-byte SOH block regardless of the sender's 1K preference
/// because the YMODEM spec fixes block 0 at 128 bytes.
///
/// Per Forsberg YMODEM §6.1 the metadata field after the filename NUL
/// is `length<SP>modtime<SP>mode<SP>sno\0` where `length` is decimal
/// and `modtime`/`mode`/`sno` are octal.  We always emit the full
/// quartet — when the caller didn't supply `modtime` or `mode` we
/// substitute octal `0`, the spec-defined "unknown" value, so
/// receivers doing positional parsing always see four fields.  Serial
/// number (`sno`) is always `0` — we don't track per-sender serials.
/// (lrzsz `sb` emits two extra positional fields, `nfiles_left` and
/// `bytes_left`, which the spec lists as optional; we omit them.)
#[allow(clippy::too_many_arguments)]
async fn send_ymodem_block_zero(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    hdr: &YmodemHeader,
    is_tcp: bool,
    block_timeout: u64,
    max_retries: usize,
    verbose: bool,
    state: &mut ReadState,
) -> Result<(), String> {
    // Build the 128-byte payload: "filename\0length modtime mode 0\0"
    // then NUL padding.  Filenames are limited to what fits; anything
    // longer is truncated at 100 bytes so the metadata still fits in
    // 128 alongside the trailing NUL terminator.
    let mut payload = [0u8; XMODEM_BLOCK_SIZE];
    let fn_bytes = hdr.filename.as_bytes();
    let fn_cap = fn_bytes.len().min(100);
    payload[..fn_cap].copy_from_slice(&fn_bytes[..fn_cap]);
    // payload[fn_cap] is already 0 (null-terminator for filename).
    let modtime_oct = hdr.modtime.unwrap_or(0);
    // Mask mode to permission + setuid/setgid/sticky bits before
    // emission — never send anything outside the file-type-independent
    // mode word, regardless of what the caller passed in.
    let mode_oct = hdr.mode.unwrap_or(0) & 0o7777;
    let meta = format!("{} {:o} {:o} 0", hdr.size, modtime_oct, mode_oct);
    let meta_start = fn_cap + 1;
    let meta_end = (meta_start + meta.len()).min(XMODEM_BLOCK_SIZE - 1);
    let meta_len = meta_end - meta_start;
    payload[meta_start..meta_end]
        .copy_from_slice(&meta.as_bytes()[..meta_len]);
    // payload[meta_end] stays 0 as the metadata-block terminator;
    // remaining bytes are NUL padding.

    let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
    packet.push(SOH);
    packet.push(0);       // block_num = 0
    packet.push(0xFF);    // !0
    packet.extend_from_slice(&payload);
    let crc = crc16_xmodem(&payload);
    packet.push((crc >> 8) as u8);
    packet.push((crc & 0xFF) as u8);

    if verbose { glog!(
        "XMODEM send: YMODEM block 0 filename='{}' size={} modtime={:o} mode={:o}",
        hdr.filename, hdr.size, modtime_oct, mode_oct,
    ); }

    let mut retries = 0;
    loop {
        if retries >= max_retries {
            return Err("YMODEM block 0: too many retries".into());
        }
        raw_write_bytes(writer, &packet, is_tcp).await?;
        // Drain single-CAN line noise per Forsberg's CAN×2 abort
        // rule: only two consecutive CANs trigger an abort, all other
        // outcomes (timeout, read error, unexpected byte) feed the
        // retry counter.
        let response = loop {
            match tokio::time::timeout(
                std::time::Duration::from_secs(block_timeout),
                nvt_read_byte(reader, is_tcp, state),
            )
            .await
            {
                Ok(Ok(byte)) => {
                    if is_can_abort(byte, state) {
                        return Err("Transfer cancelled by receiver".into());
                    }
                    if byte == CAN {
                        continue;
                    }
                    break Some(byte);
                }
                Ok(Err(_)) | Err(_) => break None,
            }
        };
        match response {
            Some(ACK) => return Ok(()),
            Some(byte) => {
                if verbose { glog!("XMODEM send: YMODEM block 0 got 0x{:02X} (retry {})", byte, retries + 1); }
                retries += 1;
                continue;
            }
            None => {
                if verbose { glog!("XMODEM send: YMODEM block 0 timeout/read error (retry {})", retries + 1); }
                retries += 1;
                continue;
            }
        }
    }
}

/// Hard-coded short budget for the YMODEM end-of-batch courtesy
/// handshake (`send_ymodem_end_of_batch`).  3 s is plenty for any
/// responsive receiver; 2 attempts is enough to cover a single CRC
/// NAK without burning the user's time.  Hoisted to module scope so
/// the budget contract is visible to tests — the user-visible stall
/// after a failed EOT must stay bounded by these constants.
const EOB_TIMEOUT_SECS: u64 = 3;
const EOB_MAX_RETRIES: usize = 2;

/// After the last data EOT is ACKed, the YMODEM receiver sends one more
/// 'C' and expects an all-zero block 0 meaning "end of batch, no more
/// files."  This keeps single-file YMODEM downloads semantically
/// correct for receivers that enforce the full protocol.
///
/// This is a courtesy handshake — the file's bytes are already committed
/// on the receiver side after the EOT ACK.  Some receivers (AnzioWin
/// observed in practice) send the post-EOT 'C' but then drop straight
/// back to terminal mode without waiting for the null block 0, leaving
/// our retransmits to splatter binary noise (notably the IAC-doubled
/// 0xFF complement byte rendering as `ÿ`) onto the user's terminal.
/// We therefore use a short timeout and a tiny retry budget here, and
/// bail immediately on the first timeout — once a receiver has gone
/// silent on this exchange, further retries can't recover it.
async fn send_ymodem_end_of_batch(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    block_timeout: u64,
    verbose: bool,
    state: &mut ReadState,
) -> Result<(), String> {
    // Wait for the receiver's final 'C'.  Some lax receivers skip this
    // step; don't hard-fail if it never arrives.
    match tokio::time::timeout(
        std::time::Duration::from_secs(block_timeout),
        nvt_read_byte(reader, is_tcp, state),
    )
    .await
    {
        Ok(Ok(b)) if b == CRC_REQUEST => {
            if verbose { glog!("XMODEM send: got end-of-batch 'C'"); }
        }
        other => {
            if verbose { glog!("XMODEM send: no end-of-batch 'C' ({:?}); skipping empty block 0", other); }
            return Ok(());
        }
    }

    let payload = [0u8; XMODEM_BLOCK_SIZE];
    let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
    packet.push(SOH);
    packet.push(0);
    packet.push(0xFF);
    packet.extend_from_slice(&payload);
    let crc = crc16_xmodem(&payload);
    packet.push((crc >> 8) as u8);
    packet.push((crc & 0xFF) as u8);

    let mut retries = 0;
    while retries < EOB_MAX_RETRIES {
        raw_write_bytes(writer, &packet, is_tcp).await?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(EOB_TIMEOUT_SECS),
            nvt_read_byte(reader, is_tcp, state),
        )
        .await
        {
            Ok(Ok(ACK)) => return Ok(()),
            Ok(Ok(byte)) => {
                if verbose { glog!("XMODEM send: end-of-batch got 0x{:02X} (retry {})", byte, retries + 1); }
                retries += 1;
                continue;
            }
            Ok(Err(e)) => {
                if verbose { glog!("XMODEM send: end-of-batch read error: {} — abandoning handshake", e); }
                return Ok(());
            }
            Err(_) => {
                if verbose { glog!("XMODEM send: end-of-batch timeout — receiver not engaging, abandoning handshake"); }
                return Ok(());
            }
        }
    }
    if verbose { glog!("XMODEM send: end-of-batch block 0 not ACKed after {} attempts, continuing", EOB_MAX_RETRIES); }
    Ok(())
}

// =============================================================================
// XMODEM CRC-16 (CCITT polynomial 0x1021)
// =============================================================================

fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

// =============================================================================
// SINGLE-BYTE WRITER (multi-byte raw_write_bytes lives in tnio.rs)
// =============================================================================

/// Write a single raw byte through the telnet IAC-escaping layer (no NVT
/// CR-NUL stuffing).  Thin wrapper over `tnio::raw_write_bytes` for the XMODEM
/// control-byte sites (sending ACK / NAK / CAN / 'C' singletons) so
/// each call site stays at one statement.
async fn raw_write_byte(
    writer: &mut (impl AsyncWrite + Unpin),
    byte: u8,
    is_tcp: bool,
) -> Result<(), String> {
    raw_write_bytes(writer, &[byte], is_tcp).await
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    // Tests reach into the raw I/O layer directly to verify telnet-NVT
    // and IAC handling; pull the helpers and telnet constants in via
    // tnio to keep the test bodies unchanged after the refactor.
    use crate::tnio::{consume_telnet_command, raw_read_byte, IAC, SB, SE, WILL};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_crc16_xmodem() {
        let data = b"123456789";
        assert_eq!(crc16_xmodem(data), 0x31C3);
    }

    #[test]
    fn test_crc16_empty() {
        assert_eq!(crc16_xmodem(&[]), 0x0000);
    }

    #[test]
    fn test_crc16_single_byte() {
        assert_eq!(crc16_xmodem(&[0x00]), 0x0000);
        assert_eq!(crc16_xmodem(&[0xFF]), 0x1EF0);
    }

    /// Run an xmodem_send / xmodem_receive pair over a DuplexStream.
    async fn xmodem_round_trip(original: &[u8]) -> Vec<u8> {
        xmodem_round_trip_mode(original, false).await
    }

    /// Round-trip with the sender's 1K preference controllable.  The
    /// receiver is always prepared to accept both SOH and STX blocks.
    async fn xmodem_round_trip_mode(original: &[u8], use_1k: bool) -> Vec<u8> {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = original.to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                false,
                false,
                false,
                use_1k,
                None, // ymodem disabled
            )
            .await
            .unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false)
                .await
                .unwrap()
        });

        send_task.await.unwrap();
        recv_task.await.unwrap().0
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_small() {
        let original = b"Hello, XModem!";
        let received = xmodem_round_trip(original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_exact_block() {
        let original: Vec<u8> = (0..128).map(|i| (i & 0xFF) as u8).collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_multi_block() {
        let original: Vec<u8> = (0..448).map(|i| (i % 251) as u8).collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_all_byte_values() {
        let original: Vec<u8> = (0..=255).collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_trailing_sub() {
        let mut original = vec![0x41; 100];
        original.push(SUB);
        original.push(SUB);
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, vec![0x41; 100]);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_random_4k() {
        let mut rng: u64 = 0xDEAD_BEEF;
        let original: Vec<u8> = (0..4096)
            .map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                (rng >> 33) as u8
            })
            .collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_block_boundary() {
        let original: Vec<u8> = vec![0x55; 256 * XMODEM_BLOCK_SIZE];
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    // ─── XMODEM-1K (STX) round-trips ──────────────────────

    #[tokio::test]
    async fn test_xmodem_1k_round_trip_exact_1024() {
        let original: Vec<u8> = (0..XMODEM_1K_BLOCK_SIZE).map(|i| (i & 0xFF) as u8).collect();
        let received = xmodem_round_trip_mode(&original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_round_trip_mixed_stx_and_final_soh() {
        // 1024 + 128 partial + few spare bytes to force a mix: one STX
        // block followed by one SOH block.  The receiver transparently
        // handles both headers; the sender degrades to SOH for the
        // sub-1K remainder.
        let original: Vec<u8> = (0..(XMODEM_1K_BLOCK_SIZE + 200))
            .map(|i| ((i * 7) & 0xFF) as u8)
            .collect();
        let received = xmodem_round_trip_mode(&original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_round_trip_multi_1k_blocks() {
        // 3 full 1K blocks, no partial.
        let original: Vec<u8> = (0..(3 * XMODEM_1K_BLOCK_SIZE))
            .map(|i| (i & 0xFF) as u8)
            .collect();
        let received = xmodem_round_trip_mode(&original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_small_file_still_uses_soh() {
        // Under 1024 bytes: even with use_1k=true, the sender must
        // emit an SOH block (one partial) because STX requires a full
        // 1024-byte payload.
        let original = b"Hello, XMODEM-1K on a short file!";
        let received = xmodem_round_trip_mode(original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_round_trip_protocol_bytes_in_data() {
        // Payload contains every protocol byte (SOH/STX/ACK/NAK/CAN/EOT
        // etc.) to verify the 1K path is byte-transparent.
        let mut original: Vec<u8> = Vec::with_capacity(XMODEM_1K_BLOCK_SIZE);
        for i in 0..XMODEM_1K_BLOCK_SIZE {
            original.push((i & 0xFF) as u8);
        }
        let received = xmodem_round_trip_mode(&original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_opportunistic_fallback() {
        // Simulate a receiver that doesn't support STX: it reads the
        // STX header byte and NAKs.  Our sender should fall back to
        // SOH for the same offset and complete the transfer with
        // 128-byte blocks.
        //
        // We drive the sender against a handwritten "minimal receiver"
        // that NAKs on STX and ACKs on SOH.  The test just verifies
        // the sender completes without a Too-Many-Retries error.
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        // 1024-byte file so the sender's first attempt is STX.
        let data: Vec<u8> = (0..XMODEM_1K_BLOCK_SIZE).map(|i| (i & 0xFF) as u8).collect();
        let data_clone = data.clone();

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data_clone,
                false,
                false,
                false,
                true, // use_1k
                None, // ymodem disabled
            )
            .await
        });

        // Fake receiver: request CRC mode ('C'), then:
        //   - on STX: NAK (rejects XMODEM-1K).
        //   - on SOH: read the rest of the 128-byte block + 2-byte CRC,
        //     ACK.
        //   - on EOT: ACK, done.
        let recv_task = tokio::spawn(async move {
            // Kick off with 'C' for CRC mode.
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();

            // Block 1 first try: expect STX.
            let hdr1 = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(hdr1, STX, "sender should try STX first when use_1k=true");
            // Drain the rest of the 1K packet: num + !num + 1024 bytes + 2 CRC.
            for _ in 0..(2 + XMODEM_1K_BLOCK_SIZE + 2) {
                raw_read_byte(&mut recv_read, false).await.unwrap();
            }
            // NAK the STX block → triggers fallback.
            raw_write_byte(&mut recv_write, NAK, false).await.unwrap();

            // All remaining blocks should be SOH (128-byte each).
            // 1024 bytes / 128 = 8 SOH blocks to cover the same payload.
            for _ in 0..8 {
                let hdr = raw_read_byte(&mut recv_read, false).await.unwrap();
                assert_eq!(hdr, SOH, "fallback should use SOH for the rest");
                for _ in 0..(2 + XMODEM_BLOCK_SIZE + 2) {
                    raw_read_byte(&mut recv_read, false).await.unwrap();
                }
                raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
            }

            // EOT
            let eot = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(eot, EOT);
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
        });

        // Both tasks should succeed.
        send_task.await.unwrap().unwrap();
        recv_task.await.unwrap();
        let _ = data; // silence unused warning
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_single_byte() {
        let received = xmodem_round_trip(&[0x42]).await;
        assert_eq!(received, vec![0x42]);
    }

    // ─── YMODEM round-trips ───────────────────────────────

    /// Drive an xmodem_send / xmodem_receive pair with the sender in
    /// YMODEM mode.  The receiver is always prepared to skip a block 0
    /// filename header, so the same xmodem_receive path handles it.
    async fn ymodem_round_trip(filename: &str, original: &[u8]) -> Vec<u8> {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = original.to_vec();
        let hdr = YmodemHeader {
            filename: filename.to_string(),
            size: data.len() as u64,
            modtime: None,
            mode: None,
        };

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                false,
                false,
                false,
                true, // use_1k (YMODEM implies 1K blocks)
                Some(hdr),
            )
            .await
            .unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false)
                .await
                .unwrap()
        });

        send_task.await.unwrap();
        recv_task.await.unwrap().0
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_single_1k_block() {
        let original: Vec<u8> = (0..XMODEM_1K_BLOCK_SIZE).map(|i| (i & 0xFF) as u8).collect();
        let received = ymodem_round_trip("test.bin", &original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_small_file() {
        let original = b"hello YMODEM";
        let received = ymodem_round_trip("hello.txt", original).await;
        // Trailing SUB padding is stripped on receive; the first 12
        // bytes must match exactly.
        assert_eq!(&received[..original.len()], original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_mixed_1k_plus_final_soh() {
        // 1024 + 200 bytes: one STX + one SOH partial.
        let original: Vec<u8> = (0..(XMODEM_1K_BLOCK_SIZE + 200))
            .map(|i| ((i * 13) & 0xFF) as u8)
            .collect();
        let received = ymodem_round_trip("mixed.dat", &original).await;
        assert_eq!(&received[..original.len()], original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_long_filename_truncated_to_100() {
        // The sender truncates filenames to 100 bytes to leave room
        // for the size/metadata trailer inside the 128-byte block 0.
        // A 150-char filename should still round-trip the data OK —
        // the receiver discards the header, so truncation doesn't
        // affect file contents.
        let long_name: String = "a".repeat(150);
        let original = b"payload-for-long-filename-test";
        let received = ymodem_round_trip(&long_name, original).await;
        assert_eq!(&received[..original.len()], original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_protocol_bytes_in_data() {
        // Payload filled with XMODEM-family protocol bytes pushed
        // through the YMODEM send/receive pipeline.  Verifies the
        // data-block path is byte-transparent even when the payload
        // looks like framing bytes.
        let mut original: Vec<u8> = Vec::with_capacity(XMODEM_1K_BLOCK_SIZE);
        for _ in 0..(XMODEM_1K_BLOCK_SIZE / 8) {
            original.extend_from_slice(&[SOH, STX, EOT, ACK, NAK, CAN, SUB, 0xFF]);
        }
        let received = ymodem_round_trip("proto.bin", &original).await;
        assert_eq!(&received[..original.len()], original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_preserves_trailing_sub_bytes() {
        // Regression: a file that legitimately ends in 0x1A bytes must
        // round-trip exactly via YMODEM — the exact size is carried in
        // block 0, and `xmodem_receive` uses it to truncate rather than
        // stripping trailing SUB padding.  An EXE, a compressed archive,
        // or random binary data that ends on 0x1A would be corrupted by
        // the old SUB-stripping path.
        //
        // The payload is 50 bytes of arbitrary data followed by five
        // 0x1A bytes.  After YMODEM round-trip, we must get the full 55
        // bytes back including the trailing 0x1A run.
        let mut original: Vec<u8> = (0u8..50).collect();
        original.extend_from_slice(&[SUB; 5]);
        let received = ymodem_round_trip("ends-in-sub.bin", &original).await;
        assert_eq!(
            received.len(),
            original.len(),
            "length mismatch: YMODEM size-truncation did not preserve trailing SUB bytes",
        );
        assert_eq!(received, original);
    }

    // ─── Checksum-mode round-trip (NAK-initiated) ─────────

    /// Drive `xmodem_send` against a handwritten receiver that starts
    /// negotiation with NAK (checksum mode), verify the sender emits
    /// 1-byte checksum trailers, and confirm the payload round-trips.
    /// The production `xmodem_receive` normally sends 'C' first and
    /// only falls back to NAK after a timeout, so end-to-end checksum
    /// mode wasn't otherwise exercised by the test suite.
    #[tokio::test]
    async fn test_xmodem_checksum_mode_round_trip() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let original: Vec<u8> =
            b"Checksum-mode payload, a few SOHs (\x01\x01\x01) too.".to_vec();
        let original_clone = original.clone();

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &original_clone,
                false, // is_tcp
                false, // is_petscii
                false, // verbose
                false, // use_1k — classic XMODEM only in checksum mode
                None,  // ymodem disabled
            )
            .await
            .unwrap();
        });

        // Fake receiver that forces checksum mode.
        let recv_task = tokio::spawn(async move {
            // Initiate with NAK → sender enters checksum mode.
            raw_write_byte(&mut recv_write, NAK, false).await.unwrap();

            let mut received: Vec<u8> = Vec::new();
            let mut expected_block: u8 = 1;
            loop {
                let header = raw_read_byte(&mut recv_read, false).await.unwrap();
                match header {
                    EOT => {
                        raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
                        break;
                    }
                    SOH => {
                        let block_num =
                            raw_read_byte(&mut recv_read, false).await.unwrap();
                        let block_complement =
                            raw_read_byte(&mut recv_read, false).await.unwrap();
                        assert_eq!(block_complement, !block_num,
                            "complement byte must be bitwise NOT of block_num");
                        assert_eq!(block_num, expected_block,
                            "block numbers must be sequential starting from 1");
                        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
                        for b in payload.iter_mut() {
                            *b = raw_read_byte(&mut recv_read, false).await.unwrap();
                        }
                        // Checksum trailer (1 byte) — NOT CRC-16 (2 bytes).
                        let recv_sum =
                            raw_read_byte(&mut recv_read, false).await.unwrap();
                        let calc_sum =
                            payload.iter().fold(0u8, |a, &b| a.wrapping_add(b));
                        assert_eq!(
                            recv_sum, calc_sum,
                            "checksum-mode sender must emit valid 8-bit sum",
                        );
                        received.extend_from_slice(&payload);
                        raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
                        expected_block = expected_block.wrapping_add(1);
                    }
                    other => panic!(
                        "checksum-mode sender emitted unexpected header 0x{:02X}",
                        other,
                    ),
                }
            }
            received
        });

        send_task.await.unwrap();
        let mut received = recv_task.await.unwrap();
        // Strip trailing SUB padding (sender pads the final block).
        while received.last() == Some(&SUB) {
            received.pop();
        }
        assert_eq!(received, original);
    }

    // ─── IAC-escape round-trips (telnet envelope) ─────────

    /// Round-trip helper for XMODEM/XMODEM-1K with `is_tcp=true`.  The
    /// sender IAC-escapes 0xFF data bytes on the wire; the receiver
    /// unescapes.  Both sides must see the identical original payload
    /// despite the envelope.
    async fn xmodem_round_trip_iac(original: &[u8], use_1k: bool) -> Vec<u8> {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = original.to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                true,  // is_tcp — enable IAC escaping
                false,
                false,
                use_1k,
                None,
            )
            .await
            .unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, true, false, false)
                .await
                .unwrap()
        });
        send_task.await.unwrap();
        recv_task.await.unwrap().0
    }

    async fn ymodem_round_trip_iac(filename: &str, original: &[u8]) -> Vec<u8> {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = original.to_vec();
        let hdr = YmodemHeader {
            filename: filename.to_string(),
            size: data.len() as u64,
            modtime: None,
            mode: None,
        };
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                true, // is_tcp
                false,
                false,
                true, // use_1k (YMODEM implies 1K)
                Some(hdr),
            )
            .await
            .unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, true, false, false)
                .await
                .unwrap()
        });
        send_task.await.unwrap();
        recv_task.await.unwrap().0
    }

    /// 0xFF bytes in the data payload must survive telnet IAC escaping:
    /// sender doubles them on the wire, receiver collapses back.
    #[tokio::test]
    async fn test_xmodem_round_trip_iac_escaping_0xff_in_data() {
        let original: Vec<u8> = vec![0xFF; 128];
        let received = xmodem_round_trip_iac(&original, false).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_iac_escaping_all_bytes() {
        // Every byte value 0..=255 across two 128-byte blocks, with
        // IAC escaping active.  This is the strictest byte-transparency
        // check for classic XMODEM over a telnet-style transport.
        let original: Vec<u8> = (0..=255u8).collect();
        let received = xmodem_round_trip_iac(&original, false).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_1k_round_trip_iac_escaping_all_bytes() {
        // Same stress test forced into XMODEM-1K mode.  Tests the
        // 1024-byte-block path over a telnet envelope.
        let mut original: Vec<u8> = Vec::with_capacity(XMODEM_1K_BLOCK_SIZE);
        for b in 0..=255u8 {
            original.extend_from_slice(&[b; 4]);
        }
        assert_eq!(original.len(), XMODEM_1K_BLOCK_SIZE);
        let received = xmodem_round_trip_iac(&original, true).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_ymodem_round_trip_iac_escaping() {
        // YMODEM block 0 filename header + data blocks over a telnet
        // envelope.  0xFF bytes in the data must survive; block 0
        // payload is short enough to contain no 0xFF itself.
        let original: Vec<u8> = (0..=255u8).collect();
        let received = ymodem_round_trip_iac("iac.bin", &original).await;
        assert_eq!(&received[..original.len()], original);
    }

    // ─── Error-path & edge-case tests ─────────────────────

    /// Read one 128-byte XMODEM-CRC block from a stream and return its
    /// payload.  Frame: `SOH | num | !num | 128 data bytes | CRC-hi | CRC-lo`.
    /// Used by the fake-receiver tests below.
    async fn read_soh_crc_block(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> Vec<u8> {
        let soh = raw_read_byte(reader, false).await.unwrap();
        assert_eq!(soh, SOH, "expected SOH header");
        let _block_num = raw_read_byte(reader, false).await.unwrap();
        let _block_complement = raw_read_byte(reader, false).await.unwrap();
        let mut payload = vec![0u8; XMODEM_BLOCK_SIZE];
        for b in payload.iter_mut() {
            *b = raw_read_byte(reader, false).await.unwrap();
        }
        // CRC trailer (2 bytes).
        let _ = raw_read_byte(reader, false).await.unwrap();
        let _ = raw_read_byte(reader, false).await.unwrap();
        payload
    }

    /// Drive the sender side of a plain-XMODEM end-of-transfer against the
    /// NAK-first-EOT receiver: send EOT, expect the verification NAK, resend
    /// EOT, expect the final ACK.  Used by the hand-rolled-sender tests so
    /// they exercise (and pin) Forsberg's spurious-EOT guard rather than the
    /// old immediate-ACK behavior.
    async fn finish_plain_eot(
        send_read: &mut (impl AsyncRead + Unpin),
        send_write: &mut (impl AsyncWrite + Unpin),
    ) {
        raw_write_byte(send_write, EOT, false).await.unwrap();
        assert_eq!(
            raw_read_byte(send_read, false).await.unwrap(),
            NAK,
            "receiver must NAK the first EOT (Forsberg verification)",
        );
        raw_write_byte(send_write, EOT, false).await.unwrap();
        assert_eq!(
            raw_read_byte(send_read, false).await.unwrap(),
            ACK,
            "receiver must ACK the confirming (resent) EOT",
        );
    }

    /// Test 1: sender must retry a block when NAK'd, and complete
    /// successfully when the receiver eventually ACKs.
    #[tokio::test]
    async fn test_xmodem_send_nak_retry_then_success() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let original = b"NAK-retry test payload".to_vec();
        let orig = original.clone();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &orig,
                false, false, false, false, None,
            ).await.unwrap();
        });

        let recv_task = tokio::spawn(async move {
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();
            // NAK block 1 twice.
            for _ in 0..2 {
                let _ = read_soh_crc_block(&mut recv_read).await;
                raw_write_byte(&mut recv_write, NAK, false).await.unwrap();
            }
            // Third attempt: ACK with actual payload verification.
            let payload = read_soh_crc_block(&mut recv_read).await;
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
            // EOT.
            let eot = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(eot, EOT);
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
            payload
        });

        send_task.await.unwrap();
        let payload = recv_task.await.unwrap();
        // The third (accepted) attempt must carry the original data.
        assert_eq!(&payload[..original.len()], original);
    }

    /// Test 2: corrupted-block recovery end-to-end with the REAL
    /// receiver.  A middle task flips one byte in block 1 on the way
    /// to the receiver for the first attempt, then forwards verbatim.
    /// The real `xmodem_receive` CRC-validates, NAKs, the sender
    /// retries, and the transfer completes with correct data.
    #[tokio::test]
    async fn test_xmodem_corrupted_block_recovery() {
        // Two duplex channels chained through a middle forwarder.
        // duplex1: sender_half  ↔ peer_a
        // duplex2: peer_b       ↔ receiver_half
        // Forwarders: peer_a.read → peer_b.write   (sender→receiver)
        //             peer_b.read → peer_a.write   (receiver→sender)
        let (sender_half, peer_a) = tokio::io::duplex(16384);
        let (peer_b, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);
        let (mut peer_a_read, mut peer_a_write) = tokio::io::split(peer_a);
        let (mut peer_b_read, mut peer_b_write) = tokio::io::split(peer_b);

        let original: Vec<u8> = (0..100).map(|i| (i * 3) as u8).collect();
        let orig = original.clone();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &orig,
                false, false, false, false, None,
            ).await.unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false)
                .await.unwrap()
        });

        // Forwarder sender→receiver: flip one byte in the payload of
        // the first 131-byte packet (SOH + num + !num + 128 data + 2
        // CRC).  For all subsequent bytes, forward verbatim.
        let s_to_r = tokio::spawn(async move {
            let mut buf = [0u8; 1];
            for i in 0..(3 + XMODEM_BLOCK_SIZE + 2) {
                if peer_a_read.read_exact(&mut buf).await.is_err() { return; }
                if i == 10 {
                    buf[0] ^= 0xFF; // flip all bits of one data byte
                }
                if peer_b_write.write_all(&buf).await.is_err() { return; }
            }
            tokio::io::copy(&mut peer_a_read, &mut peer_b_write).await.ok();
        });
        // Forwarder receiver→sender: verbatim.
        let r_to_s = tokio::spawn(async move {
            tokio::io::copy(&mut peer_b_read, &mut peer_a_write).await.ok();
        });

        send_task.await.unwrap();
        let (received, _) = recv_task.await.unwrap();
        let _ = s_to_r.await;
        let _ = r_to_s.await;
        assert_eq!(received, original, "receiver must recover correct data after NAK+retry");
    }

    /// Receiver must recover from a CRC-bad YMODEM block 0 by NAKing
    /// and successfully reading the sender's retransmit, rather than
    /// falling out of negotiation and NAK-looping the retransmit as a
    /// block-number mismatch.  Regression test for the pre-fix bug
    /// where a single corrupted byte in block 0 hung the session.
    #[tokio::test]
    async fn test_ymodem_receive_block_zero_crc_error_recovery() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let payload_data: Vec<u8> = b"ymodem-block0-retry".to_vec();
        let payload_clone = payload_data.clone();

        let send_task = tokio::spawn(async move {
            // Wait for the receiver's initial 'C'.
            let req = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(req, CRC_REQUEST);

            // Build block 0 carrying filename + size metadata.
            let mut block0 = [0u8; XMODEM_BLOCK_SIZE];
            let fname = b"retry.bin";
            block0[..fname.len()].copy_from_slice(fname);
            // block0[fname.len()] = 0  (already zero)
            let meta = format!("{} 0 0 0", payload_clone.len());
            let meta_start = fname.len() + 1;
            block0[meta_start..meta_start + meta.len()]
                .copy_from_slice(meta.as_bytes());

            // First transmit: deliberately corrupt one byte of the
            // payload BEFORE computing CRC, so the on-the-wire CRC
            // matches the corrupted data — but we then send the
            // *original* (uncorrupted) bytes with that CRC.  Result:
            // CRC mismatch on receive.
            let bad_crc = crc16_xmodem(&{
                let mut c = block0;
                c[10] ^= 0xFF;
                c
            });
            let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
            packet.push(SOH);
            packet.push(0);
            packet.push(0xFF);
            packet.extend_from_slice(&block0);
            packet.push((bad_crc >> 8) as u8);
            packet.push((bad_crc & 0xFF) as u8);
            send_write.write_all(&packet).await.unwrap();

            // Receiver should NAK the bad block 0.  After NAK the
            // negotiation loop bumps its attempt counter and re-sends
            // a CRC request at the top of the next iteration; both
            // bytes are acceptable in any order from a sender's POV
            // (real senders just keep retransmitting block 0 until
            // ACKed).  Drain whichever bytes the receiver emits up to
            // the next read of our retransmit.
            let mut saw_nak = false;
            loop {
                let b = raw_read_byte(&mut send_read, false).await.unwrap();
                if b == NAK {
                    saw_nak = true;
                } else if b == CRC_REQUEST {
                    // Redundant 'C' after NAK is benign — keep waiting
                    // for the receiver to settle.
                    break;
                } else {
                    panic!("unexpected receiver byte after bad block 0: 0x{:02X}", b);
                }
            }
            assert!(saw_nak, "receiver must NAK CRC-bad block 0");

            // Retransmit block 0 with the correct CRC.
            let good_crc = crc16_xmodem(&block0);
            let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
            packet.push(SOH);
            packet.push(0);
            packet.push(0xFF);
            packet.extend_from_slice(&block0);
            packet.push((good_crc >> 8) as u8);
            packet.push((good_crc & 0xFF) as u8);
            send_write.write_all(&packet).await.unwrap();

            // Receiver: ACK + 'C' to start data phase.
            let ack = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(ack, ACK, "receiver must ACK retransmitted block 0");
            let c = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(c, CRC_REQUEST, "receiver must request data phase");

            // Send block 1 carrying the payload (NUL-padded to 128).
            let mut data_block = [0u8; XMODEM_BLOCK_SIZE];
            data_block[..payload_clone.len()].copy_from_slice(&payload_clone);
            let crc = crc16_xmodem(&data_block);
            let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
            packet.push(SOH);
            packet.push(1);
            packet.push(!1u8);
            packet.extend_from_slice(&data_block);
            packet.push((crc >> 8) as u8);
            packet.push((crc & 0xFF) as u8);
            send_write.write_all(&packet).await.unwrap();
            let ack = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(ack, ACK);

            // EOT, end-of-batch handshake (final 'C' + null block 0).
            raw_write_byte(&mut send_write, EOT, false).await.unwrap();
            let _ack = raw_read_byte(&mut send_read, false).await.unwrap();
            let _c = raw_read_byte(&mut send_read, false).await.unwrap();
            let null0 = [0u8; XMODEM_BLOCK_SIZE];
            let crc = crc16_xmodem(&null0);
            let mut packet = Vec::with_capacity(3 + XMODEM_BLOCK_SIZE + 2);
            packet.push(SOH);
            packet.push(0);
            packet.push(0xFF);
            packet.extend_from_slice(&null0);
            packet.push((crc >> 8) as u8);
            packet.push((crc & 0xFF) as u8);
            send_write.write_all(&packet).await.unwrap();
            let _ack = raw_read_byte(&mut send_read, false).await.unwrap();
        });

        let (received, meta) = xmodem_receive(
            &mut recv_read,
            &mut recv_write,
            false,
            false,
            false,
        )
        .await
        .expect("receive must complete after CRC-error recovery");

        send_task.await.unwrap();
        let meta = meta.expect("YMODEM metadata must be parsed after retry");
        assert_eq!(meta.size, Some(payload_data.len() as u64));
        // YMODEM truncates received payload to the size declared in
        // block 0, stripping the NUL padding we sent to fill the
        // 128-byte block.
        assert_eq!(&received[..], &payload_data[..]);
    }

    /// Auto-detect, mode = Checksum but sender sent CRC-format trailer:
    /// the function should validate as CRC, lock the mode to CRC, and
    /// accept the block.  Models the negotiation timing race where our
    /// mode-flip to checksum landed mid-flight against a CRC-capable
    /// sender that already started transmitting in CRC mode.
    #[tokio::test]
    async fn test_receive_block_body_auto_detect_crc_under_checksum_mode() {
        // Build wire bytes for a CRC-format block 1 with a known
        // payload.  The block-num + complement bytes are NOT included
        // because the test calls receive_block_body which expects
        // those to have already been read by the caller.
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE).map(|i| (i * 7) as u8).collect();
        let crc = crc16_xmodem(&payload);
        let mut wire = payload.clone();
        wire.push((crc >> 8) as u8);
        wire.push((crc & 0xFF) as u8);

        let mut reader = std::io::Cursor::new(wire);
        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Checksum;

        let result = receive_block_body(
            &mut reader,
            1,    // block_num
            !1u8, // block_complement
            &mut expected,
            &mut mode,
            false, // is_tcp
            false, // verbose
            XMODEM_BLOCK_SIZE,
            &mut state,
            true, // auto_detect
        )
        .await
        .expect("auto-detect should accept CRC-format trailer");

        assert_eq!(result, payload);
        assert!(matches!(mode, TransferMode::Crc16),
            "mode must lock to CRC after auto-detect");
        assert_eq!(expected, 2);
    }

    /// Inverse: mode = CRC but sender sent a checksum-format trailer
    /// (vintage Christensen 1977 / CP/M MODEM7 / C64 BBS uploader that
    /// ignored our 'C' and started in checksum mode).  Auto-detect
    /// should fall back to checksum and pushback the would-be CRC-low
    /// byte for the next read.
    #[tokio::test]
    async fn test_receive_block_body_auto_detect_checksum_under_crc_mode() {
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE)
            .map(|i| ((i * 11) ^ 0x5A) as u8)
            .collect();
        let checksum = payload.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        // After payload + 1-byte checksum, the wire would carry the
        // next block's SOH (0x01) — simulate it so we can verify the
        // pushback restores it for the next read.
        let next_block_soh = SOH;
        let mut wire = payload.clone();
        wire.push(checksum);
        wire.push(next_block_soh);

        let mut reader = std::io::Cursor::new(wire);
        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Crc16;

        let result = receive_block_body(
            &mut reader,
            1,
            !1u8,
            &mut expected,
            &mut mode,
            false,
            false,
            XMODEM_BLOCK_SIZE,
            &mut state,
            true,
        )
        .await
        .expect("auto-detect should accept checksum-format trailer");

        assert_eq!(result, payload);
        assert!(matches!(mode, TransferMode::Checksum),
            "mode must lock to checksum after auto-detect");
        assert_eq!(state.pushback, Some(next_block_soh),
            "the byte read past the 1-byte checksum trailer must be pushed back");
        assert_eq!(expected, 2);
    }

    /// X1: mode = CRC (auto-detect) but the sender is a *strict lock-step*
    /// checksum-only peer — it sends the 128-byte block plus ONE checksum
    /// trailer byte and then waits for our ACK/NAK.  Unlike the streaming
    /// case above there is no next-block byte behind the checksum, so the
    /// CRC low-byte read must not block: after a short grace window the
    /// receiver falls back to 1-byte-checksum validation, accepts the block,
    /// and locks to checksum with nothing pushed back — instead of stalling
    /// for the full 60 s block-body timeout.
    ///
    /// The sender's silence is modeled with a duplex whose write half stays
    /// open so the missing low byte *pends* (exactly as against a waiting
    /// lock-step peer); a `Cursor` would hit EOF and never exercise the
    /// timeout path.  The paused clock makes the grace window elapse at once.
    #[tokio::test(start_paused = true)]
    async fn test_receive_block_body_auto_detect_lockstep_checksum_only() {
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE).map(|i| (i * 13) as u8).collect();
        let checksum = payload.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));

        let (mut reader, mut writer) = tokio::io::duplex(1024);
        // Data block + exactly ONE trailer byte (the checksum), then silence.
        writer.write_all(&payload).await.unwrap();
        writer.write_all(&[checksum]).await.unwrap();
        // `writer` is intentionally kept alive below so the absent CRC low
        // byte pends (a lock-step sender awaiting our ACK) rather than EOFing.

        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Crc16;

        let result = receive_block_body(
            &mut reader,
            1,
            !1u8,
            &mut expected,
            &mut mode,
            false, // is_tcp
            false, // verbose
            XMODEM_BLOCK_SIZE,
            &mut state,
            true, // auto_detect
        )
        .await
        .expect("lock-step checksum block must validate via the timeout fallback");

        assert_eq!(result, payload, "payload recovered from the checksum-only block");
        assert!(matches!(mode, TransferMode::Checksum),
            "session must lock to checksum after the lone-trailer fallback");
        assert!(state.pushback.is_none(),
            "no second trailer byte, so nothing may be pushed back");
        assert_eq!(expected, 2, "expected block advanced after acceptance");

        drop(writer); // release the deliberately kept-alive write half
    }

    /// X1 symmetry: the CHECKSUM-mode auto-detect branch must also gate its
    /// CRC-probe read behind the grace window.  Receiver in Checksum mode,
    /// first block's checksum MISMATCHES, and the sender is lock-step (sent
    /// one trailer byte, now waiting for ACK/NAK).  The extra CRC-probe read
    /// must time out and the block be rejected (→ NAK) instead of stalling
    /// for the full 60 s block-body timeout.  Without the fix the unbounded
    /// read has no timer and this test would hang under the paused clock.
    #[tokio::test(start_paused = true)]
    async fn test_receive_block_body_checksum_mode_lockstep_bad_block_no_stall() {
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE).map(|i| (i * 7) as u8).collect();
        let good_sum = payload.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        let bad_sum = good_sum ^ 0xFF; // guaranteed mismatch

        let (mut reader, mut writer) = tokio::io::duplex(1024);
        writer.write_all(&payload).await.unwrap();
        writer.write_all(&[bad_sum]).await.unwrap();
        // writer kept alive: the (absent) CRC-probe second byte pends.

        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Checksum;

        let err = receive_block_body(
            &mut reader,
            1,
            !1u8,
            &mut expected,
            &mut mode,
            false, // is_tcp
            false, // verbose
            XMODEM_BLOCK_SIZE,
            &mut state,
            true, // auto_detect
        )
        .await
        .expect_err("a mismatching lock-step checksum block must be rejected, not stall");
        assert!(err.contains("Checksum/CRC"), "expected checksum/CRC rejection, got: {err}");
        assert!(matches!(mode, TransferMode::Checksum), "mode must not flip on rejection");
        assert!(state.pushback.is_none(), "no byte may be pushed back on rejection");
        assert_eq!(expected, 1, "expected block must not advance on rejection");

        drop(writer);
    }

    /// Auto-detect must NOT commit its mode flip / pushback if the
    /// block ultimately fails the complement / duplicate / wrong-block
    /// checks.  Models the case where a corrupt block happens to have
    /// a trailer byte that coincidentally validates under the alternate
    /// mode — the receiver must NAK and stay in its original mode
    /// rather than wedging the session with a stale pushback.
    #[tokio::test]
    async fn test_receive_block_body_auto_detect_rolls_back_on_complement_mismatch() {
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE).map(|i| (i * 7) as u8).collect();
        let crc = crc16_xmodem(&payload);
        // CRC-format trailer is correct, but we'll feed a BAD complement.
        let mut wire = payload.clone();
        wire.push((crc >> 8) as u8);
        wire.push((crc & 0xFF) as u8);

        let mut reader = std::io::Cursor::new(wire);
        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Checksum;

        let err = receive_block_body(
            &mut reader,
            1,
            0xAB, // INTENTIONALLY wrong complement (correct = !1 = 0xFE)
            &mut expected,
            &mut mode,
            false,
            false,
            XMODEM_BLOCK_SIZE,
            &mut state,
            true, // auto_detect
        )
        .await
        .expect_err("complement-mismatch must reject the block");

        assert!(err.contains("complement"));
        assert!(matches!(mode, TransferMode::Checksum),
            "mode must NOT flip when the block ultimately fails validation");
        assert_eq!(state.pushback, None,
            "pushback must NOT be committed when the block ultimately fails validation");
    }

    /// Auto-detect off (subsequent blocks): a CRC-mode validation
    /// failure must NOT silently fall back to checksum.  Locks down
    /// the post-first-block mode-stability invariant.
    #[tokio::test]
    async fn test_receive_block_body_no_auto_detect_after_first_block() {
        let payload: Vec<u8> = (0..XMODEM_BLOCK_SIZE).map(|i| (i * 13) as u8).collect();
        let checksum = payload.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        // CRC mode expecting 2 bytes, but the sender sent only 1
        // (checksum-format).  With auto_detect=false, the receiver
        // reads 2 bytes (the checksum + the next block's SOH) and
        // validates them as CRC — which fails.  No fallback.
        let mut wire = payload.clone();
        wire.push(checksum);
        wire.push(SOH);

        let mut reader = std::io::Cursor::new(wire);
        let mut state = ReadState::default();
        let mut expected = 1u8;
        let mut mode = TransferMode::Crc16;

        let err = receive_block_body(
            &mut reader,
            1,
            !1u8,
            &mut expected,
            &mut mode,
            false,
            false,
            XMODEM_BLOCK_SIZE,
            &mut state,
            false, // auto_detect off
        )
        .await
        .expect_err("non-first-block must reject mode mismatch");

        assert!(err.contains("Checksum/CRC error"));
        assert!(matches!(mode, TransferMode::Crc16),
            "mode must NOT flip after first block");
    }

    /// Test 3: duplicate block from an unusual sender (phantom NAK
    /// caused retransmission) must be detected and silently ACKed by
    /// the real receiver, with no duplication in the output.
    #[tokio::test]
    async fn test_xmodem_receive_duplicate_block() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        // Build a 128-byte payload for block 1.
        let block1_data: Vec<u8> = (0..128u8).map(|i| i.wrapping_mul(5)).collect();
        let block1_data_clone = block1_data.clone();

        // Fake sender that transmits block 1 twice (same payload).
        let send_task = tokio::spawn(async move {
            // Wait for 'C' from receiver.
            let req = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(req, CRC_REQUEST);

            // Helper that builds an SOH+CRC packet for an arbitrary
            // block_num and payload.
            let build = |n: u8, data: &[u8]| -> Vec<u8> {
                let mut p = Vec::with_capacity(3 + 128 + 2);
                p.push(SOH);
                p.push(n);
                p.push(!n);
                p.extend_from_slice(data);
                let crc = crc16_xmodem(data);
                p.push((crc >> 8) as u8);
                p.push((crc & 0xFF) as u8);
                p
            };

            // Send block 1. Wait for ACK.
            send_write.write_all(&build(1, &block1_data_clone)).await.unwrap();
            let a1 = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(a1, ACK);

            // Send block 1 AGAIN (simulating retransmission after a
            // lost ACK).  Real receiver should recognize as duplicate.
            send_write.write_all(&build(1, &block1_data_clone)).await.unwrap();
            let a_dup = raw_read_byte(&mut send_read, false).await.unwrap();
            assert_eq!(a_dup, ACK, "receiver must ACK duplicate without error");

            // Proceed to EOT (NAK-first verification handshake).
            finish_plain_eot(&mut send_read, &mut send_write).await;
        });

        let (received, _) = xmodem_receive(
            &mut recv_read, &mut recv_write, false, false, false,
        ).await.unwrap();

        send_task.await.unwrap();
        // Data should appear exactly once, not doubled.
        assert_eq!(received, block1_data);
    }

    /// Test 4a: receiver returns "cancelled by sender" when the sender
    /// emits CAN×2 (consecutive) mid-transfer.  Forsberg's protocol
    /// notes recommend two consecutive CANs for abort so a stray 0x18
    /// from line noise doesn't false-abort a transfer.
    #[tokio::test]
    async fn test_xmodem_receive_aborts_on_sender_can() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let send_task = tokio::spawn(async move {
            // Wait for 'C'.
            let _ = raw_read_byte(&mut send_read, false).await.unwrap();
            // Send CAN×2 (consecutive) — the spec-conformant abort.
            raw_write_byte(&mut send_write, CAN, false).await.unwrap();
            raw_write_byte(&mut send_write, CAN, false).await.unwrap();
            // Drain whatever the receiver writes after the first CAN
            // (e.g. another 'C' from the negotiation loop) until the
            // task is aborted by the test driver.
            loop {
                let _ = raw_read_byte(&mut send_read, false).await;
            }
        });

        let result = xmodem_receive(
            &mut recv_read, &mut recv_write, false, false, false,
        ).await;

        // Receiver returned — abort the drain loop.  Splitting a
        // DuplexStream into Read/Write halves means dropping just
        // `recv_write` doesn't close the stream (recv_read still
        // holds it), so the cleanest way to terminate the spawn is
        // an explicit abort.
        send_task.abort();
        let _ = send_task.await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("cancelled by sender"),
            "expected cancel-by-sender error, got: {}", err,
        );
    }

    /// Test 4b: sender returns "cancelled by receiver" when the
    /// receiver sends CAN×2 in response to a data block.
    #[tokio::test]
    async fn test_xmodem_send_aborts_on_receiver_can_mid_transfer() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = b"payload".to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &data,
                false, false, false, false, None,
            ).await
        });

        let recv_task = tokio::spawn(async move {
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();
            // Read block 1 and respond with CAN×2.
            let _ = read_soh_crc_block(&mut recv_read).await;
            raw_write_byte(&mut recv_write, CAN, false).await.unwrap();
            raw_write_byte(&mut recv_write, CAN, false).await.unwrap();
        });

        let result = send_task.await.unwrap();
        recv_task.await.unwrap();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("cancelled by receiver"),
            "expected cancel-by-receiver error",
        );
    }

    /// Test 4c: sender returns cancel error when the receiver sends
    /// CAN×2 during negotiation (before any block has been transmitted).
    #[tokio::test]
    async fn test_xmodem_send_aborts_on_receiver_can_during_negotiation() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (_recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = b"never-sent".to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &data,
                false, false, false, false, None,
            ).await
        });

        // Send CAN×2 in place of 'C' or NAK.
        raw_write_byte(&mut recv_write, CAN, false).await.unwrap();
        raw_write_byte(&mut recv_write, CAN, false).await.unwrap();

        let result = send_task.await.unwrap();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("cancelled by receiver"),
            "expected cancel-by-receiver during negotiation",
        );
    }

    /// Test 5: `xmodem_send` times out and returns an error when the
    /// receiver never transmits 'C' or NAK.  Uses tokio's paused-time
    /// mode so the test doesn't actually wait the full negotiation
    /// window.
    #[tokio::test(start_paused = true)]
    async fn test_xmodem_send_negotiation_timeout() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        // Keep the receiver half alive so reads from sender block
        // (rather than EOF-ing) — we want the timeout path to fire.
        let _keep_alive = receiver_half;

        let data = b"data".to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &data,
                false, false, false, false, None,
            ).await
        });

        // Advance virtual time past any reasonable negotiation window.
        tokio::time::advance(std::time::Duration::from_secs(600)).await;

        let result = send_task.await.unwrap();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_lowercase().contains("timeout")
                || err.to_lowercase().contains("negotiation"),
            "expected negotiation-timeout error, got: {}", err,
        );
    }

    /// Test 6: receiver NAKs when the sender transmits a block with
    /// the wrong block number (out of sequence).
    #[tokio::test]
    async fn test_xmodem_receive_nak_on_out_of_sequence_block() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        // Real receiver.
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false)
                .await
        });

        // Wait for 'C' from receiver.
        let req = raw_read_byte(&mut send_read, false).await.unwrap();
        assert_eq!(req, CRC_REQUEST);

        // Fake sender: transmit block 5 instead of block 1.
        let bogus_data = vec![0xAAu8; XMODEM_BLOCK_SIZE];
        let crc = crc16_xmodem(&bogus_data);
        let mut pkt = Vec::new();
        pkt.push(SOH);
        pkt.push(5);
        pkt.push(!5);
        pkt.extend_from_slice(&bogus_data);
        pkt.push((crc >> 8) as u8);
        pkt.push((crc & 0xFF) as u8);
        send_write.write_all(&pkt).await.unwrap();

        // Receiver should respond with NAK (expected 1, got 5).
        let response = raw_read_byte(&mut send_read, false).await.unwrap();
        assert_eq!(
            response, NAK,
            "receiver must NAK an out-of-sequence block",
        );

        // Send CAN to terminate cleanly.
        raw_write_byte(&mut send_write, CAN, false).await.unwrap();
        let result = recv_task.await.unwrap();
        assert!(result.is_err());
    }

    /// Build a 128-byte SOH+CRC data block with the given block number and a
    /// constant fill byte.  Shared by the spec-recovery tests below.
    fn make_crc_data_block(block_num: u8, fill: u8) -> Vec<u8> {
        let data = vec![fill; XMODEM_BLOCK_SIZE];
        let crc = crc16_xmodem(&data);
        let mut pkt = vec![SOH, block_num, !block_num];
        pkt.extend_from_slice(&data);
        pkt.push((crc >> 8) as u8);
        pkt.push((crc & 0xFF) as u8);
        pkt
    }

    /// Spec D1: in the data phase, a missing/late block must be recovered by
    /// NAKing to re-prompt the sender (Forsberg/Christensen receiver retry
    /// loop), NOT by an immediate abort.  Uses tokio's paused clock so the
    /// inter-block timeout elapses instantly.
    #[tokio::test(start_paused = true)]
    async fn test_xmodem_receive_naks_and_recovers_on_interblock_timeout() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false).await
        });

        // Negotiation + block 1 (accepted).
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), CRC_REQUEST);
        send_write.write_all(&make_crc_data_block(1, 0x41)).await.unwrap();
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);

        // Block 2 is "lost" — send nothing.  Let the receiver settle onto its
        // inter-block timeout, advance virtual time past it, and confirm it
        // NAKs (re-prompts) instead of aborting.  Reading the NAK also parks
        // this task, so the paused clock auto-advances as a backstop.
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(25)).await;
        assert_eq!(
            raw_read_byte(&mut send_read, false).await.unwrap(),
            NAK,
            "D1: receiver must NAK on an inter-block timeout, not abort",
        );

        // Deliver block 2 — the transfer recovers and completes.
        send_write.write_all(&make_crc_data_block(2, 0x42)).await.unwrap();
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);
        finish_plain_eot(&mut send_read, &mut send_write).await;

        let (data, _) = recv_task.await.unwrap().unwrap();
        let mut expected = vec![0x41u8; XMODEM_BLOCK_SIZE];
        expected.extend_from_slice(&[0x42u8; XMODEM_BLOCK_SIZE]);
        assert_eq!(data, expected, "data recovers after the D1 timeout NAK");
    }

    /// Forsberg EOT verification (the serial-noise guard): a spurious EOT in
    /// the inter-block gap — e.g. a stray 0x04 from UART line noise — must be
    /// NAKed, NOT accepted as end-of-file.  The transfer then recovers and the
    /// file is NOT truncated.  This is the behavior that lets plain XMODEM
    /// claim full Forsberg receiver compliance; YMODEM keeps immediate-ACK
    /// (its size field + end-of-batch handshake already detect a short file).
    #[tokio::test]
    async fn test_xmodem_receive_naks_spurious_eot_then_recovers() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false).await
        });

        // Negotiation + block 1 (accepted).
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), CRC_REQUEST);
        send_write.write_all(&make_crc_data_block(1, 0x41)).await.unwrap();
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);

        // Spurious EOT (line noise) — the receiver must NAK to verify, not
        // accept it as end-of-file (which would truncate to just block 1).
        raw_write_byte(&mut send_write, EOT, false).await.unwrap();
        assert_eq!(
            raw_read_byte(&mut send_read, false).await.unwrap(),
            NAK,
            "a spurious EOT must be NAKed, not accepted as end-of-file",
        );

        // The sender resends the in-flight block (block 2) after the NAK; as a
        // fresh expected block it's accepted and the EOT guard re-arms.
        send_write.write_all(&make_crc_data_block(2, 0x42)).await.unwrap();
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);

        // Genuine end-of-transfer: the real EOT is NAK-verified afresh.
        finish_plain_eot(&mut send_read, &mut send_write).await;

        let (data, _) = recv_task.await.unwrap().unwrap();
        let mut expected = vec![0x41u8; XMODEM_BLOCK_SIZE];
        expected.extend_from_slice(&[0x42u8; XMODEM_BLOCK_SIZE]);
        assert_eq!(
            data, expected,
            "both blocks must survive — a spurious EOT must not truncate the file",
        );
    }

    /// Spec D3: in the data phase, a valid block (good CRC + complement)
    /// bearing a non-duplicate, unexpected sequence number is an
    /// unrecoverable sync loss — the receiver must cancel (CAN×3), not NAK.
    #[tokio::test]
    async fn test_xmodem_receive_cancels_on_main_loop_sequence_error() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false).await
        });

        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), CRC_REQUEST);
        // Block 1 accepted in the first-block path → ACK; expected advances to 2.
        send_write.write_all(&make_crc_data_block(1, 0x41)).await.unwrap();
        assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);
        // Now a valid block numbered 3 (expecting 2): not a duplicate, not the
        // expected block → fatal sequence error → cancel.
        send_write.write_all(&make_crc_data_block(3, 0x42)).await.unwrap();
        assert_eq!(
            raw_read_byte(&mut send_read, false).await.unwrap(),
            CAN,
            "D3: receiver must cancel on a non-duplicate sequence error",
        );

        let result = recv_task.await.unwrap();
        assert!(result.is_err(), "transfer must abort on a sequence error");
        assert!(
            result.unwrap_err().to_lowercase().contains("sequence"),
            "error should name the sequence failure",
        );
    }

    /// Test 9: YMODEM sender must retry block 0 (the filename header)
    /// when the receiver NAKs it, and complete successfully when the
    /// receiver eventually ACKs.
    #[tokio::test]
    async fn test_ymodem_send_block_zero_nak_retry() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        // 1024 bytes so the sender uses STX (1K block) — otherwise it
        // falls back to SOH and our post-block-0 read assertion fails.
        let original: Vec<u8> = (0..1024u16)
            .map(|i| (i as u8).wrapping_mul(3))
            .collect();
        let orig_clone = original.clone();
        let hdr = YmodemHeader {
            filename: "retry.bin".to_string(),
            size: original.len() as u64,
            modtime: None,
            mode: None,
        };

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &orig_clone,
                false, false, false, true /* use_1k */, Some(hdr),
            ).await.unwrap();
        });

        let recv_task = tokio::spawn(async move {
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();

            // Block 0 NAK'd twice.
            for _ in 0..2 {
                // Read full 128-byte block 0 + 2 CRC bytes.
                for _ in 0..(3 + XMODEM_BLOCK_SIZE + 2) {
                    raw_read_byte(&mut recv_read, false).await.unwrap();
                }
                raw_write_byte(&mut recv_write, NAK, false).await.unwrap();
            }

            // Third attempt: read + ACK.
            for _ in 0..(3 + XMODEM_BLOCK_SIZE + 2) {
                raw_read_byte(&mut recv_read, false).await.unwrap();
            }
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();

            // Second 'C' → start data phase.
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();

            // Receive the 1K STX data block.
            let hdr_byte = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(hdr_byte, STX);
            for _ in 0..(2 + XMODEM_1K_BLOCK_SIZE + 2) {
                raw_read_byte(&mut recv_read, false).await.unwrap();
            }
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();

            // EOT.
            let eot = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(eot, EOT);
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    /// Test 10: XMODEM-1K → XMODEM fallback not only completes but
    /// delivers the exact original bytes to the receiver.  Stronger
    /// assertion than the existing opportunistic-fallback test which
    /// only verified the transfer didn't error out.
    #[tokio::test]
    async fn test_xmodem_1k_fallback_preserves_exact_bytes() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        // Known-distinct payload to detect any corruption.
        let original: Vec<u8> = (0..XMODEM_1K_BLOCK_SIZE)
            .map(|i| ((i * 31 + 7) & 0xFF) as u8)
            .collect();
        let orig_clone = original.clone();

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &orig_clone,
                false, false, false, true /* use_1k */, None,
            ).await.unwrap();
        });

        let recv_task = tokio::spawn(async move {
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();

            // First attempt: STX (1K).  NAK it to force fallback.
            let hdr = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(hdr, STX);
            for _ in 0..(2 + XMODEM_1K_BLOCK_SIZE + 2) {
                raw_read_byte(&mut recv_read, false).await.unwrap();
            }
            raw_write_byte(&mut recv_write, NAK, false).await.unwrap();

            // Fallback: 8 SOH blocks covering the same 1024 bytes.
            let mut received = Vec::with_capacity(XMODEM_1K_BLOCK_SIZE);
            for expected_num in 1u8..=8 {
                let hdr = raw_read_byte(&mut recv_read, false).await.unwrap();
                assert_eq!(hdr, SOH);
                let blk = raw_read_byte(&mut recv_read, false).await.unwrap();
                assert_eq!(blk, expected_num);
                raw_read_byte(&mut recv_read, false).await.unwrap(); // !blk
                let mut payload = vec![0u8; XMODEM_BLOCK_SIZE];
                for b in payload.iter_mut() {
                    *b = raw_read_byte(&mut recv_read, false).await.unwrap();
                }
                // CRC.
                raw_read_byte(&mut recv_read, false).await.unwrap();
                raw_read_byte(&mut recv_read, false).await.unwrap();
                received.extend_from_slice(&payload);
                raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
            }

            // EOT.
            let eot = raw_read_byte(&mut recv_read, false).await.unwrap();
            assert_eq!(eot, EOT);
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();

            received
        });

        send_task.await.unwrap();
        let received = recv_task.await.unwrap();
        assert_eq!(
            received, original,
            "fallback path must preserve exact payload bytes",
        );
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_empty() {
        let received = xmodem_round_trip(&[]).await;
        assert!(received.is_empty());
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_one_over_block() {
        let original: Vec<u8> = (0..129).map(|i| (i & 0xFF) as u8).collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_two_exact_blocks() {
        let original: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[tokio::test]
    async fn test_xmodem_round_trip_data_with_protocol_bytes() {
        let original = vec![SOH, EOT, ACK, NAK, CAN, SUB, 0x00, 0xFF];
        let received = xmodem_round_trip(&original).await;
        assert_eq!(received, original);
    }

    #[test]
    fn test_crc16_full_zero_block() {
        let block = [0u8; XMODEM_BLOCK_SIZE];
        assert_eq!(crc16_xmodem(&block), 0x0000);
    }

    #[test]
    fn test_crc16_full_ff_block() {
        let block = [0xFFu8; XMODEM_BLOCK_SIZE];
        let crc = crc16_xmodem(&block);
        assert_ne!(crc, 0x0000);
        assert_eq!(crc, crc16_xmodem(&[0xFF; XMODEM_BLOCK_SIZE]));
    }

    #[test]
    fn test_crc16_sequential_block() {
        let block: Vec<u8> = (0..128).collect();
        let crc = crc16_xmodem(&block);
        assert_eq!(crc, crc16_xmodem(&(0u8..128).collect::<Vec<u8>>()));
        assert_ne!(crc, 0);
    }

    #[tokio::test]
    async fn test_xmodem_receive_rejects_oversized() {
        let oversized = vec![0xAA; MAX_FILE_SIZE + XMODEM_BLOCK_SIZE];
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let send_task = tokio::spawn(async move {
            let _ = xmodem_send(
                &mut send_read,
                &mut send_write,
                &oversized,
                false,
                false,
                false,
                false,
                None,
            )
            .await;
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false).await
        });

        send_task.await.unwrap();
        let result = recv_task.await.unwrap();
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("8 MB"),
            "Expected '8 MB' in error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_transfer_timeout_is_reasonable() {
        let cfg = config::get_config();
        assert!(
            cfg.xmodem_negotiation_timeout >= 30,
            "too short — user needs time to start sender"
        );
        assert!(cfg.xmodem_negotiation_timeout <= 300, "excessive negotiation timeout");
    }

    #[test]
    fn test_block_timeout_less_than_negotiation_timeout() {
        let cfg = config::get_config();
        assert!(cfg.xmodem_block_timeout < cfg.xmodem_negotiation_timeout);
    }

    #[test]
    fn test_max_retries_is_reasonable() {
        let cfg = config::get_config();
        assert!(cfg.xmodem_max_retries >= 3, "too few retries");
        assert!(cfg.xmodem_max_retries <= 50, "excessive retries");
    }

    #[tokio::test]
    async fn test_consume_telnet_sb_normal() {
        let data: Vec<u8> = vec![0x18, 0x00, 0x41, IAC, SE];
        let mut reader = std::io::Cursor::new(data);
        let result = consume_telnet_command(&mut reader, SB).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_consume_telnet_sb_long() {
        let mut data: Vec<u8> = Vec::new();
        data.extend(std::iter::repeat_n(0x42, 1000));
        data.push(IAC);
        data.push(SE);
        let mut reader = std::io::Cursor::new(data);
        let result = consume_telnet_command(&mut reader, SB).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_consume_telnet_sb_escaped_iac() {
        let data: Vec<u8> = vec![0x18, IAC, IAC, 0x01, IAC, SE];
        let mut reader = std::io::Cursor::new(data);
        let result = consume_telnet_command(&mut reader, SB).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_consume_telnet_will() {
        let data: Vec<u8> = vec![0x01];
        let mut reader = std::io::Cursor::new(data);
        let result = consume_telnet_command(&mut reader, WILL).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_consume_telnet_unknown_command() {
        let data: Vec<u8> = vec![];
        let mut reader = std::io::Cursor::new(data);
        let result = consume_telnet_command(&mut reader, 0xF1).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_xmodem_esc_key_petscii_false() {
        assert!(is_esc_key(0x1B, false));
        assert!(!is_esc_key(0x5F, false));
    }

    #[test]
    fn test_xmodem_esc_key_petscii_true() {
        assert!(is_esc_key(0x1B, true));
        assert!(is_esc_key(0x5F, true));
    }

    // ─── XMODEM/XMODEM-1K/YMODEM spec conformance tests ──────
    //
    // Drive `xmodem_send` against a minimal scripted receiver, capture
    // the wire bytes, and assert that each header/block/trailer matches
    // the byte-exact format mandated by:
    //   - XMODEM (Christensen 1977 / Forsberg's "YMODEM.DOC")
    //   - XMODEM-CRC (Forsberg)
    //   - XMODEM-1K (Forsberg, STX/1024-byte blocks)
    //   - YMODEM (Forsberg 1985, batch with block 0)

    /// Capture the bytes `xmodem_send` writes when driven by a scripted
    /// receiver.  `receiver_script` is the sequence of control bytes
    /// the receiver should emit (e.g. `[CRC_REQUEST, ACK, ACK, ACK]`)
    /// — one per block plus a final ACK for the EOT.  Returns the
    /// concatenated wire bytes the sender produced.
    async fn capture_xmodem_wire(
        data: &[u8],
        use_1k: bool,
        ymodem: Option<YmodemHeader>,
        receiver_script: &[u8],
    ) -> Vec<u8> {
        let (sender_half, receiver_half) = tokio::io::duplex(65536);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = data.to_vec();
        let script = receiver_script.to_vec();

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                false,
                false,
                false,
                use_1k,
                ymodem,
            )
            .await
            .ok();
        });

        let capture_task = tokio::spawn(async move {
            // Drive the script: emit one byte, then read until enough
            // bytes have arrived to plausibly complete a block (loose
            // bound; we just need to keep the sender unblocked).
            let mut captured: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            let mut script_pos = 0usize;
            loop {
                if script_pos < script.len() {
                    recv_write.write_all(&[script[script_pos]]).await.ok();
                    recv_write.flush().await.ok();
                    script_pos += 1;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    recv_read.read(&mut buf),
                )
                .await
                {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => captured.extend_from_slice(&buf[..n]),
                    Ok(Err(_)) => break,
                    Err(_) => {
                        if script_pos >= script.len() {
                            break;
                        }
                    }
                }
            }
            captured
        });

        let _ = send_task.await;
        capture_task.await.unwrap()
    }

    /// Heuristic: scan `wire` for the first occurrence of an XMODEM
    /// block (SOH or STX) and return the (header_byte, block_num,
    /// complement, payload_offset) tuple.
    fn find_first_block(wire: &[u8]) -> Option<(u8, u8, u8, usize)> {
        for (i, &b) in wire.iter().enumerate() {
            if (b == SOH || b == STX) && i + 2 < wire.len() {
                return Some((b, wire[i + 1], wire[i + 2], i + 3));
            }
        }
        None
    }

    #[tokio::test]
    async fn test_xmodem_christensen_checksum_block_layout() {
        // XMODEM (Christensen 1977): block = SOH (0x01) | block_num |
        // ~block_num | 128 bytes | checksum (1 byte sum mod 256).
        // Receiver requests checksum mode by sending NAK first.
        let data = b"Hello, XMODEM!";
        let wire = capture_xmodem_wire(data, false, None, &[NAK, ACK, ACK]).await;
        let (hdr, num, comp, off) = find_first_block(&wire).expect("no block in wire");
        assert_eq!(hdr, SOH, "checksum-mode XMODEM block must start with SOH");
        assert_eq!(num, 1, "first block number must be 1 (Christensen)");
        assert_eq!(comp, !num, "complement must be bitwise NOT of block num");
        assert!(off + 128 < wire.len(), "wire too short for SOH+128+cksum");
        // Verify the checksum (sum mod 256 of the 128 data bytes).
        let payload = &wire[off..off + 128];
        let cksum: u8 = payload.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(wire[off + 128], cksum, "checksum mismatch");
    }

    #[tokio::test]
    async fn test_xmodem_crc16_block_layout() {
        // XMODEM-CRC: same as Christensen but with CRC-16/XMODEM
        // (poly 0x1021) appended MSB-first instead of a 1-byte
        // checksum.  Triggered by receiver sending 'C' first.
        let data = b"CRC mode payload";
        let wire = capture_xmodem_wire(data, false, None, &[CRC_REQUEST, ACK, ACK]).await;
        let (hdr, num, _, off) = find_first_block(&wire).expect("no block");
        assert_eq!(hdr, SOH);
        assert_eq!(num, 1);
        assert!(off + 128 + 2 <= wire.len(), "wire too short for SOH+128+CRC");
        let payload = &wire[off..off + 128];
        let crc = crc16_xmodem(payload);
        // CRC is appended MSB-first per the spec.
        assert_eq!(
            wire[off + 128],
            (crc >> 8) as u8,
            "CRC high byte must come first"
        );
        assert_eq!(
            wire[off + 129],
            crc as u8,
            "CRC low byte must come second"
        );
    }

    #[tokio::test]
    async fn test_xmodem_1k_block_uses_stx_header() {
        // XMODEM-1K: 1024-byte blocks introduced with STX (0x02)
        // instead of SOH.  Forsberg specified this so receivers can
        // distinguish block sizes from the leading byte alone.
        let data: Vec<u8> = (0..1024u32).map(|i| (i & 0xFF) as u8).collect();
        let wire = capture_xmodem_wire(&data, true, None, &[CRC_REQUEST, ACK, ACK]).await;
        let (hdr, _, _, _) = find_first_block(&wire).expect("no block");
        assert_eq!(hdr, STX, "XMODEM-1K block must start with STX (0x02)");
    }

    #[tokio::test]
    async fn test_xmodem_1k_block_layout() {
        // XMODEM-1K: STX | num | ~num | 1024 bytes | CRC16 (2 bytes).
        let data: Vec<u8> = (0..1024u32).map(|i| (i.wrapping_mul(13) & 0xFF) as u8).collect();
        let wire = capture_xmodem_wire(&data, true, None, &[CRC_REQUEST, ACK, ACK]).await;
        let (hdr, num, comp, off) = find_first_block(&wire).expect("no block");
        assert_eq!(hdr, STX);
        assert_eq!(num, 1);
        assert_eq!(comp, !num);
        assert!(off + 1024 + 2 <= wire.len(), "wire too short for STX+1024+CRC");
        let crc = crc16_xmodem(&wire[off..off + 1024]);
        assert_eq!(wire[off + 1024], (crc >> 8) as u8);
        assert_eq!(wire[off + 1025], crc as u8);
    }

    #[tokio::test]
    async fn test_xmodem_block_number_increments_then_wraps() {
        // Block numbers are 8-bit, increment from 1, and wrap 255 → 0.
        // 257 blocks (256 × 128 + 1) → numbers 1, 2, ..., 255, 0, 1.
        // We probe the first two block numbers (cheaper than 257-block
        // wrap, which is expensive) — the wrap logic is exercised by
        // the existing internal round-trip tests.
        let data: Vec<u8> = (0..256u32).map(|i| (i & 0xFF) as u8).collect();
        let wire =
            capture_xmodem_wire(&data, false, None, &[CRC_REQUEST, ACK, ACK, ACK]).await;
        // First block is num=1.  Find the second SOH.
        let mut soh_positions: Vec<usize> = Vec::new();
        for (i, &b) in wire.iter().enumerate() {
            if b == SOH && i + 2 < wire.len() && wire[i + 2] == !wire[i + 1] {
                soh_positions.push(i);
            }
        }
        assert!(
            soh_positions.len() >= 2,
            "expected at least 2 blocks for 256-byte payload"
        );
        assert_eq!(wire[soh_positions[0] + 1], 1, "first block must be 1");
        assert_eq!(wire[soh_positions[1] + 1], 2, "second block must be 2");
    }

    #[tokio::test]
    async fn test_xmodem_eot_after_last_block() {
        // After the final data block + final ACK, the sender emits
        // EOT (0x04) to signal end-of-file.
        let data = b"short";
        let wire = capture_xmodem_wire(data, false, None, &[CRC_REQUEST, ACK, ACK]).await;
        assert!(
            wire.contains(&EOT),
            "wire must contain EOT (0x04) after last block, got: {:?}",
            wire.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_xmodem_pads_short_block_with_sub() {
        // XMODEM blocks are fixed-width.  The last block of a file
        // shorter than 128 bytes is padded with 0x1A (SUB / CP/M EOF).
        let data = b"abc"; // 3 bytes, must be padded to 128
        let wire = capture_xmodem_wire(data, false, None, &[CRC_REQUEST, ACK, ACK]).await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        let payload = &wire[off..off + 128];
        assert_eq!(&payload[..3], data);
        assert!(
            payload[3..].iter().all(|&b| b == SUB),
            "tail of short block must be padded with SUB (0x1A), got: {:?}",
            &payload[3..]
        );
    }

    #[tokio::test]
    async fn test_ymodem_block_zero_format() {
        // YMODEM (Forsberg §5): block 0 carries metadata as
        //   "filename\0size mtime mode\0...\0" padded to block size.
        // Block number is 0 (not 1) for the metadata block.
        let data = b"file body";
        let header = YmodemHeader {
            filename: "test.bin".to_string(),
            size: data.len() as u64,
            modtime: None,
            mode: None,
        };
        let wire =
            capture_xmodem_wire(data, false, Some(header), &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK])
                .await;
        let (hdr, num, comp, off) = find_first_block(&wire).expect("no block");
        assert_eq!(hdr, SOH, "YMODEM block 0 uses SOH (128 bytes)");
        assert_eq!(num, 0, "block 0 must have block number 0");
        assert_eq!(comp, !num);
        let payload = &wire[off..off + 128];
        // Filename comes first, NUL-terminated.
        let nul = payload.iter().position(|&b| b == 0).expect("no NUL after filename");
        assert_eq!(&payload[..nul], b"test.bin");
        // After the NUL, ASCII-decimal size, space-separated from
        // mtime / mode.  We just need to verify the size field is
        // present in decimal ASCII before the next NUL.
        let after_name = &payload[nul + 1..];
        let next_nul = after_name
            .iter()
            .position(|&b| b == 0)
            .expect("no NUL after metadata");
        let meta = std::str::from_utf8(&after_name[..next_nul]).unwrap();
        let first_field = meta.split_whitespace().next().unwrap();
        assert_eq!(
            first_field,
            "9",
            "size field must be decimal ASCII matching data length"
        );
    }

    #[tokio::test]
    async fn test_ymodem_block_zero_uses_crc16() {
        // YMODEM mandates CRC-16 (not checksum) for all blocks, so
        // even the receiver's negotiation byte before block 0 must
        // be 'C' — a NAK negotiation would put us in legacy XMODEM
        // mode where YMODEM features (size truncation, batch) don't
        // apply.  This test locks in that block 0 is followed by a
        // 2-byte CRC-16 trailer.
        let data = b"x";
        let header = YmodemHeader {
            filename: "a".to_string(),
            size: 1,
            modtime: None,
            mode: None,
        };
        let wire = capture_xmodem_wire(
            data,
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        // Block 0 = SOH + 2-byte hdr + 128 data + 2-byte CRC.
        let payload = &wire[off..off + 128];
        let crc = crc16_xmodem(payload);
        assert_eq!(wire[off + 128], (crc >> 8) as u8, "block 0 CRC high byte");
        assert_eq!(wire[off + 129], crc as u8, "block 0 CRC low byte");
    }

    /// Forsberg YMODEM §6.1: the metadata field after the filename
    /// NUL is `length<SP>modtime<SP>mode<SP>sno\0` where `length` is
    /// decimal and `modtime`/`mode`/`sno` are octal.  When the sender
    /// is given full metadata, it must emit all four fields.
    #[tokio::test]
    async fn test_ymodem_block_zero_emits_full_metadata() {
        let data = b"abc";
        let header = YmodemHeader {
            filename: "doc.txt".to_string(),
            size: 3,
            modtime: Some(0o12345670),
            mode: Some(0o100644),
        };
        let wire = capture_xmodem_wire(
            data,
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (hdr, num, _, off) = find_first_block(&wire).expect("no block");
        assert_eq!(hdr, SOH, "block 0 must use SOH");
        assert_eq!(num, 0, "block 0 number must be 0");
        let payload = &wire[off..off + 128];
        let nul = payload.iter().position(|&b| b == 0).expect("filename NUL");
        assert_eq!(&payload[..nul], b"doc.txt");
        let after_name = &payload[nul + 1..];
        let next_nul = after_name
            .iter()
            .position(|&b| b == 0)
            .expect("metadata-block NUL terminator");
        let meta = std::str::from_utf8(&after_name[..next_nul]).unwrap();
        let fields: Vec<&str> = meta.split_ascii_whitespace().collect();
        assert!(
            fields.len() >= 4,
            "must emit at least length/modtime/mode/sno, got {:?}",
            fields,
        );
        assert_eq!(fields[0], "3", "length must be decimal");
        assert_eq!(
            u64::from_str_radix(fields[1], 8).expect("modtime must be octal"),
            0o12345670,
        );
        assert_eq!(
            u32::from_str_radix(fields[2], 8).expect("mode must be octal") & 0o7777,
            0o100644 & 0o7777,
        );
        assert_eq!(fields[3], "0", "sno must be octal 0");
    }

    /// Length is decimal, modtime/mode are octal — emitting modtime
    /// in decimal would silently misrepresent the timestamp on parsers
    /// that follow the spec.  This test pins the radix on each field
    /// independently of the matching round-trip test.
    #[tokio::test]
    async fn test_ymodem_block_zero_octal_radix() {
        let data = b"y";
        let header = YmodemHeader {
            filename: "f".to_string(),
            size: 1,
            // 0o20 = 16 — distinguishable from its decimal form (20)
            // and its hex form (0x14) so a wrong radix would fail.
            modtime: Some(0o20),
            mode: Some(0o20),
        };
        let wire = capture_xmodem_wire(
            data,
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        let payload = &wire[off..off + 128];
        let nul = payload.iter().position(|&b| b == 0).unwrap();
        let after = &payload[nul + 1..];
        let end = after.iter().position(|&b| b == 0).unwrap();
        let meta = std::str::from_utf8(&after[..end]).unwrap();
        let fields: Vec<&str> = meta.split_ascii_whitespace().collect();
        assert_eq!(fields[1], "20", "modtime 0o20 must serialize as octal '20'");
        assert_eq!(fields[2], "20", "mode 0o20 must serialize as octal '20'");
    }

    /// End-to-end round-trip with full metadata — sender encodes,
    /// receiver decodes, both halves agree on the values.
    #[tokio::test]
    async fn test_ymodem_round_trip_modtime_mode_metadata() {
        let original = b"round trip body";
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = original.to_vec();
        let hdr = YmodemHeader {
            filename: "rt.bin".to_string(),
            size: data.len() as u64,
            modtime: Some(1_700_000_000),
            mode: Some(0o100755),
        };

        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read,
                &mut send_write,
                &data,
                false,
                false,
                false,
                true,
                Some(hdr),
            )
            .await
            .unwrap();
        });
        let recv_task = tokio::spawn(async move {
            xmodem_receive(&mut recv_read, &mut recv_write, false, false, false)
                .await
                .unwrap()
        });
        send_task.await.unwrap();
        let (received, meta) = recv_task.await.unwrap();
        assert_eq!(received, original, "data must round-trip exactly");
        let meta = meta.expect("YMODEM block 0 must surface meta");
        assert_eq!(meta.size, Some(original.len() as u64));
        assert_eq!(meta.modtime, Some(1_700_000_000));
        // Mode is parser-masked to 0o7777 (perms + setuid/setgid/sticky).
        assert_eq!(meta.mode, Some(0o100755 & 0o7777));
    }

    /// Minimal-sender compatibility: a block 0 with `filename\0length\0`
    /// (no modtime/mode/sno) must still parse — Forsberg explicitly
    /// permits trailing fields to be omitted.
    #[test]
    fn test_parse_block_zero_minimal_size_only() {
        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
        // Layout: "f.bin\0123\0..."  — filename, NUL, decimal length,
        // NUL terminator for the metadata block.
        let bytes: &[u8] = b"f.bin\x00123";
        payload[..bytes.len()].copy_from_slice(bytes);
        // payload[bytes.len()] stays 0 — terminates metadata.
        let meta = parse_ymodem_block_zero_payload(&payload).expect("must parse");
        assert_eq!(meta.size, Some(123));
        assert_eq!(meta.modtime, None);
        assert_eq!(meta.mode, None);
    }

    /// Even more minimal: filename only, no metadata block at all.
    /// Some pre-Forsberg-1988 senders did this; we should tolerate it
    /// by returning a meta with all `None` fields.
    #[test]
    fn test_parse_block_zero_filename_only() {
        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
        payload[..4].copy_from_slice(b"name");
        // payload[4] is the filename NUL; everything after is NUL fill.
        let meta = parse_ymodem_block_zero_payload(&payload).expect("must parse");
        assert_eq!(meta.size, None);
        assert_eq!(meta.modtime, None);
        assert_eq!(meta.mode, None);
    }

    /// End-of-batch null block 0 (filename starts with NUL) must not
    /// produce a meta — the parser distinguishes "no metadata" from
    /// "end of batch terminator block."
    #[test]
    fn test_parse_block_zero_end_of_batch_returns_none() {
        let payload = [0u8; XMODEM_BLOCK_SIZE];
        assert!(parse_ymodem_block_zero_payload(&payload).is_none());
    }

    /// Modtime of 0 (octal) is the spec-defined "unknown" sentinel —
    /// the parser must report `None` rather than `Some(0)`, so callers
    /// don't set the file's mtime to the UNIX epoch.
    #[test]
    fn test_parse_block_zero_zero_modtime_means_unknown() {
        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
        let meta_str: &[u8] = b"f\x0010 0 644 0";
        payload[..meta_str.len()].copy_from_slice(meta_str);
        let m = parse_ymodem_block_zero_payload(&payload).expect("must parse");
        assert_eq!(m.size, Some(10));
        assert_eq!(m.modtime, None, "octal 0 modtime must mean 'unknown'");
        assert_eq!(m.mode, Some(0o644));
    }

    /// Mode parser masks to 0o7777 — anything outside the permission
    /// and setuid/setgid/sticky bits (file-type bits such as 0o100000
    /// for "regular file") must be stripped before reaching the caller.
    #[test]
    fn test_parse_block_zero_mode_masking() {
        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
        // 0o100755 = regular file, rwxr-xr-x.  Mask should drop
        // the 0o100000 file-type bit.
        let meta_str: &[u8] = b"f\x001 0 100755 0";
        payload[..meta_str.len()].copy_from_slice(meta_str);
        let m = parse_ymodem_block_zero_payload(&payload).expect("must parse");
        assert_eq!(m.mode, Some(0o755));
    }

    /// Junk in the metadata field must not panic; well-formed earlier
    /// fields must still be returned.  A common failure mode for
    /// minimally-conformant senders is putting a non-numeric token
    /// where modtime should be.
    #[test]
    fn test_parse_block_zero_tolerates_junk_after_size() {
        let mut payload = [0u8; XMODEM_BLOCK_SIZE];
        let meta_str: &[u8] = b"f\x0042 not_a_number also_junk 0";
        payload[..meta_str.len()].copy_from_slice(meta_str);
        let m = parse_ymodem_block_zero_payload(&payload).expect("must parse");
        assert_eq!(m.size, Some(42));
        assert_eq!(m.modtime, None);
        assert_eq!(m.mode, None);
    }

    /// The metadata block in our emitted block 0 must be NUL-terminated
    /// (Forsberg §6.1 fixes this — the receiver looks for the NUL to
    /// know where the field block ends).  Pin it explicitly.
    #[tokio::test]
    async fn test_ymodem_block_zero_metadata_nul_terminated() {
        let header = YmodemHeader {
            filename: "z".to_string(),
            size: 1,
            modtime: Some(0o12345),
            mode: Some(0o644),
        };
        let wire = capture_xmodem_wire(
            b"z",
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        let payload = &wire[off..off + 128];
        // Filename NUL at offset 1 ("z\0..."); next NUL must terminate
        // the metadata block, after which the rest is NUL padding.
        assert_eq!(payload[0], b'z');
        assert_eq!(payload[1], 0, "filename must be NUL-terminated");
        // Find the metadata terminator.  After the filename NUL the
        // next NUL byte ends the metadata field block.
        let term = payload[2..]
            .iter()
            .position(|&b| b == 0)
            .expect("metadata terminator NUL")
            + 2;
        // Everything after the terminator must be NUL padding.
        for (i, &b) in payload[term + 1..].iter().enumerate() {
            assert_eq!(
                b, 0,
                "byte {} after metadata terminator must be NUL fill, got 0x{:02X}",
                i, b,
            );
        }
    }

    /// Callers who don't supply modtime/mode (e.g. pure in-memory
    /// senders that don't have a real file) must get spec-conformant
    /// `0` substitution rather than absence.  This keeps the
    /// space-separated field count at exactly 4, which simpler
    /// receivers may rely on for positional parsing.
    #[tokio::test]
    async fn test_ymodem_block_zero_none_metadata_emits_zeroes() {
        let header = YmodemHeader {
            filename: "n".to_string(),
            size: 5,
            modtime: None,
            mode: None,
        };
        let wire = capture_xmodem_wire(
            b"hello",
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        let payload = &wire[off..off + 128];
        let nul = payload.iter().position(|&b| b == 0).unwrap();
        let after = &payload[nul + 1..];
        let end = after.iter().position(|&b| b == 0).unwrap();
        let meta = std::str::from_utf8(&after[..end]).unwrap();
        let fields: Vec<&str> = meta.split_ascii_whitespace().collect();
        assert_eq!(fields, vec!["5", "0", "0", "0"]);
    }

    /// Mode is masked to 0o7777 BEFORE emission so a misbehaving caller
    /// can't smuggle file-type bits onto the wire.  This guards against
    /// a future caller passing the raw `st_mode` value (which includes
    /// 0o170000 file-type bits) without masking.
    #[tokio::test]
    async fn test_ymodem_block_zero_mode_masked_before_emission() {
        let header = YmodemHeader {
            filename: "m".to_string(),
            size: 1,
            modtime: Some(1),
            // 0o140755 = socket + rwxr-xr-x — caller passed the full
            // st_mode including the 0o140000 file-type bits.  We must
            // strip them.
            mode: Some(0o140755),
        };
        let wire = capture_xmodem_wire(
            b"m",
            false,
            Some(header),
            &[CRC_REQUEST, ACK, CRC_REQUEST, ACK, ACK],
        )
        .await;
        let (_, _, _, off) = find_first_block(&wire).expect("no block");
        let payload = &wire[off..off + 128];
        let nul = payload.iter().position(|&b| b == 0).unwrap();
        let after = &payload[nul + 1..];
        let end = after.iter().position(|&b| b == 0).unwrap();
        let meta = std::str::from_utf8(&after[..end]).unwrap();
        let fields: Vec<&str> = meta.split_ascii_whitespace().collect();
        let emitted_mode = u32::from_str_radix(fields[2], 8).unwrap();
        assert_eq!(emitted_mode & 0o170000, 0, "file-type bits must be stripped");
        assert_eq!(emitted_mode, 0o0755, "permission bits must survive");
    }

    /// The YMODEM end-of-batch (null-block-0) handshake is a courtesy:
    /// the file's bytes are already committed on the receiver after
    /// the EOT ACK, so a receiver that ignores the post-EOT 'C' must
    /// not stall the user for hundreds of seconds.  This test pins the
    /// budget contract — total worst-case stall stays under 10 s even
    /// if a future refactor changes the surrounding logic.
    ///
    /// Background: AnzioWin sends the post-EOT 'C' but then drops back
    /// to terminal mode without ACKing the null block 0, so the old
    /// (block_timeout × max_retries = 200 s) budget burned a couple of
    /// minutes on every successful download and sprayed `ÿ` characters
    /// onto the user's terminal on each retransmit.
    #[test]
    fn test_ymodem_end_of_batch_budget_bounded() {
        // 3 s × 2 retries = 6 s worst-case.  Anything larger than 10 s
        // would re-introduce the AnzioWin stall.  const-evaluated so a
        // budget regression fails compilation, not just CI.
        const _: () = assert!(
            EOB_TIMEOUT_SECS * EOB_MAX_RETRIES as u64 <= 10,
            "YMODEM end-of-batch worst-case stall must stay under 10 s",
        );
        const _: () = assert!(
            EOB_MAX_RETRIES <= 3,
            "more than 3 retries spams the receiver's terminal with binary noise \
             (each retry sends a 132-byte SOH packet)",
        );
    }

    #[test]
    fn test_xmodem_crc16_canonical_vector() {
        // Forsberg "XMODEM/YMODEM Protocol Reference" cites the
        // CRC-16/XMODEM canonical vector (poly 0x1021, init 0,
        // no reflection): "123456789" → 0x31C3.  Locks in the CRC
        // implementation as a separate spec-citation test from the
        // pre-existing internal vector test.
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }

    /// Forsberg's CAN×2 abort rule, unit-tested at the helper level.
    /// First CAN arms `pending_can` and returns false; second
    /// consecutive CAN returns true; any non-CAN byte clears the flag.
    #[test]
    fn test_is_can_abort_state_transitions() {
        let mut state = ReadState::default();
        // Single CAN: arms but doesn't abort.
        assert!(!is_can_abort(CAN, &mut state));
        assert!(state.pending_can);
        // Second consecutive CAN: aborts.
        assert!(is_can_abort(CAN, &mut state));
        // After aborting, flag is cleared.
        assert!(!state.pending_can);
        // Single CAN, then non-CAN byte: flag cleared, no abort.
        assert!(!is_can_abort(CAN, &mut state));
        assert!(state.pending_can);
        assert!(!is_can_abort(ACK, &mut state));
        assert!(!state.pending_can);
        // After a non-CAN byte clears the flag, a single CAN doesn't
        // abort even though there was a CAN before.  Only **consecutive**
        // CANs trigger abort per Forsberg's rule.
        assert!(!is_can_abort(CAN, &mut state));
        assert!(!is_can_abort(NAK, &mut state));
        assert!(!is_can_abort(CAN, &mut state));
        assert!(!is_can_abort(EOT, &mut state));
    }

    /// A single stray CAN during the receive main loop must NOT abort
    /// the transfer.  Sender sends block 1 normally, then a stray CAN
    /// (simulating line noise), then block 2.  Receiver should treat
    /// the lone CAN as noise and complete the transfer with both
    /// blocks intact.
    #[tokio::test]
    async fn test_xmodem_receive_single_can_is_noise() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let block1: Vec<u8> = (0..128u8).map(|i| i.wrapping_mul(7)).collect();
        let block2: Vec<u8> = (0..128u8).map(|i| i.wrapping_mul(11)).collect();
        let block1_clone = block1.clone();
        let block2_clone = block2.clone();

        let send_task = tokio::spawn(async move {
            // Wait for 'C'.
            let _ = raw_read_byte(&mut send_read, false).await.unwrap();
            // Send block 1.
            let mut pkt = vec![SOH, 1, !1u8];
            pkt.extend_from_slice(&block1_clone);
            let crc = crc16_xmodem(&block1_clone);
            pkt.push((crc >> 8) as u8);
            pkt.push((crc & 0xFF) as u8);
            send_write.write_all(&pkt).await.unwrap();
            assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);
            // Stray single CAN — must be ignored as noise.
            raw_write_byte(&mut send_write, CAN, false).await.unwrap();
            // Send block 2 immediately after.  The receiver must have
            // cleared `pending_can` on receipt of the SOH (non-CAN byte).
            let mut pkt = vec![SOH, 2, !2u8];
            pkt.extend_from_slice(&block2_clone);
            let crc = crc16_xmodem(&block2_clone);
            pkt.push((crc >> 8) as u8);
            pkt.push((crc & 0xFF) as u8);
            send_write.write_all(&pkt).await.unwrap();
            assert_eq!(raw_read_byte(&mut send_read, false).await.unwrap(), ACK);
            // EOT to end (NAK-first verification handshake).
            finish_plain_eot(&mut send_read, &mut send_write).await;
        });

        let (received, _) = xmodem_receive(
            &mut recv_read, &mut recv_write, false, false, false,
        )
        .await
        .expect("transfer must complete despite stray CAN");

        send_task.await.unwrap();
        let mut expected = block1;
        expected.extend_from_slice(&block2);
        assert_eq!(received, expected, "all data must round-trip");
    }

    /// CAN, non-CAN byte, CAN must NOT abort: the non-CAN byte
    /// breaks the "consecutive" run.  This pins the contract that
    /// the abort rule is *strictly* consecutive — a CAN followed by
    /// any other byte resets the state machine.
    #[tokio::test]
    async fn test_xmodem_receive_can_then_other_then_can_does_not_abort() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let block1: Vec<u8> = (0..128u8).map(|i| i.wrapping_mul(13)).collect();
        let block1_clone = block1.clone();

        // Drain receiver-side bytes until we see `target`, ignoring
        // the negotiation loop's 'C' retries that arrive after the
        // first CAN before the receiver sees the next data byte.
        async fn read_until(
            reader: &mut (impl AsyncRead + Unpin),
            target: u8,
        ) -> u8 {
            loop {
                let b = raw_read_byte(reader, false).await.unwrap();
                if b == target {
                    return b;
                }
            }
        }

        let send_task = tokio::spawn(async move {
            // Wait for 'C'.
            let _ = raw_read_byte(&mut send_read, false).await.unwrap();
            // CAN — arms pending_can on the receiver side.
            raw_write_byte(&mut send_write, CAN, false).await.unwrap();
            // Block 1 (SOH+...) — non-CAN bytes clear pending_can.
            let mut pkt = vec![SOH, 1, !1u8];
            pkt.extend_from_slice(&block1_clone);
            let crc = crc16_xmodem(&block1_clone);
            pkt.push((crc >> 8) as u8);
            pkt.push((crc & 0xFF) as u8);
            send_write.write_all(&pkt).await.unwrap();
            // Receiver may have sent additional 'C' requests after
            // the first CAN before reading our SOH; drain them.
            let _ = read_until(&mut send_read, ACK).await;
            // Another single CAN — should NOT abort because the SOH
            // and block body cleared the run.
            raw_write_byte(&mut send_write, CAN, false).await.unwrap();
            // EOT to gracefully end the transfer.  NAK-first-EOT: the
            // receiver NAKs the first EOT (drain to it past any stray
            // bytes), then ACKs the resent one.
            raw_write_byte(&mut send_write, EOT, false).await.unwrap();
            let _ = read_until(&mut send_read, NAK).await;
            raw_write_byte(&mut send_write, EOT, false).await.unwrap();
            let _ = read_until(&mut send_read, ACK).await;
        });

        let (received, _) = xmodem_receive(
            &mut recv_read, &mut recv_write, false, false, false,
        )
        .await
        .expect("two non-consecutive single CANs must not abort");

        send_task.await.unwrap();
        assert_eq!(received, block1);
    }

    /// Sender side of the same property: a single CAN from the
    /// receiver mid-transfer (e.g. line noise) must NOT abort the
    /// send — the sender keeps reading until either a definitive
    /// ACK/NAK arrives or a second consecutive CAN follows.
    #[tokio::test]
    async fn test_xmodem_send_single_can_then_ack_continues() {
        let (sender_half, receiver_half) = tokio::io::duplex(16384);
        let (mut send_read, mut send_write) = tokio::io::split(sender_half);
        let (mut recv_read, mut recv_write) = tokio::io::split(receiver_half);

        let data = b"hello, single-CAN-noise!".to_vec();
        let send_task = tokio::spawn(async move {
            xmodem_send(
                &mut send_read, &mut send_write, &data,
                false, false, false, false, None,
            ).await
        });

        let recv_task = tokio::spawn(async move {
            // Request CRC mode.
            raw_write_byte(&mut recv_write, CRC_REQUEST, false).await.unwrap();
            // Read block 1.
            let _ = read_soh_crc_block(&mut recv_read).await;
            // Stray single CAN, then ACK.  Sender must drain the CAN
            // and treat ACK as the definitive response.
            raw_write_byte(&mut recv_write, CAN, false).await.unwrap();
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
            // Read EOT, ACK it.
            assert_eq!(raw_read_byte(&mut recv_read, false).await.unwrap(), EOT);
            raw_write_byte(&mut recv_write, ACK, false).await.unwrap();
        });

        let result = send_task.await.unwrap();
        recv_task.await.unwrap();
        result.expect("sender must complete despite stray CAN from receiver");
    }

    #[test]
    fn test_xmodem_control_byte_constants_match_christensen() {
        // Christensen 1977 / Forsberg's YMODEM.DOC defines:
        //   SOH = 0x01, STX = 0x02, EOT = 0x04, ACK = 0x06,
        //   NAK = 0x15, CAN = 0x18, SUB = 0x1A.
        // Plus Forsberg's CRC-mode trigger 'C' = 0x43.
        const _: () = assert!(SOH == 0x01);
        const _: () = assert!(STX == 0x02);
        const _: () = assert!(EOT == 0x04);
        const _: () = assert!(ACK == 0x06);
        const _: () = assert!(NAK == 0x15);
        const _: () = assert!(CAN == 0x18);
        const _: () = assert!(SUB == 0x1A);
        const _: () = assert!(CRC_REQUEST == 0x43);
    }

    // ─── lrzsz interop tests (manual, #[ignore]) ────────────
    //
    // Mirror the ZMODEM lrzsz interop tests in src/zmodem.rs.  Run with:
    //   cargo test --release -- --ignored test_lrzsz_xmodem
    //   cargo test --release -- --ignored test_lrzsz_ymodem
    // Each test spawns a real sx/rx/sb/rb subprocess, drives our
    // sender/receiver against it through stdin/stdout, reaps the child
    // before unwrapping, and verifies the file bytes round-trip
    // unchanged.  Unix-only because lrzsz is.

    // ─── CCGMS XMODEM interop (env-gated, real CCGMS reference) ─────
    //
    // Drives our XMODEM sender/receiver against the genuine CCGMS XMODEM
    // reference (ccgmsterm/test/xmodem.c — the Georges Menie codec CCGMS
    // ships), via the combined harness in ~/claude/punter-ccgms-interop.
    // Build it with:
    //   cc -O2 -o ccgms-xfer harness.c \
    //      ~/ccgmsterm/test/{punter,xmodem,crc16}.c
    // then point CCGMS_XFER_BIN at it.  Skipped (not failed) when unset, so
    // CI without the binary stays green — same convention as the Punter
    // CCGMS tests in punter.rs.  Both sides build the same i*7+1, XFER_SIZE
    // (default 1000) byte pattern; 1000 is not a block multiple, so the final
    // short/CTRLZ-padded block is exercised.  CRC-16 is the negotiated mode in
    // both directions (CCGMS recv opens with 'C'; our recv requests 'C').

    // Gated `#[cfg(unix)]` like the interop tests that consume them — otherwise
    // they are dead code on Windows and trip `-D warnings` (dead_code) in CI.
    #[cfg(unix)]
    const CCGMS_XFER_SIZE: usize = 1000;

    #[cfg(unix)]
    fn ccgms_pattern() -> Vec<u8> {
        (0..CCGMS_XFER_SIZE).map(|i| (i * 7 + 1) as u8).collect()
    }

    /// Trim the trailing XMODEM final-block padding CCGMS's sender leaves.
    /// XMODEM carries no length field, so a non-block-aligned file is padded to
    /// the block boundary.  CCGMS's `xmodemTransmit` (the Georges Menie codec)
    /// pads with a *single* `CTRLZ` (0x1A) then `NUL` (0x00) fill — so a
    /// standard receiver (ours) that strips only trailing 0x1A can't remove the
    /// 0x00 run (and must not: real binaries legitimately end in 0x00).  The
    /// payload bytes are intact; the test trims `{0x1A,0x00}` from the end and
    /// compares.  Safe because our pattern's last byte (0x52) is neither.
    #[cfg(unix)]
    fn trim_xmodem_padding(mut v: Vec<u8>) -> Vec<u8> {
        while matches!(v.last(), Some(0x1A) | Some(0x00)) {
            v.pop();
        }
        v
    }

    /// Spawn the CCGMS harness in `mode`, returning the child (or None to skip).
    #[cfg(unix)]
    fn spawn_ccgms_xfer(mode: &str) -> Option<tokio::process::Child> {
        let bin = std::env::var("CCGMS_XFER_BIN").ok()?;
        use tokio::process::Command;
        Some(
            Command::new(&bin)
                .arg(mode)
                .env("XFER_SIZE", CCGMS_XFER_SIZE.to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .expect("spawn ccgms-xfer harness"),
        )
    }

    /// Our XMODEM sender → real CCGMS `xmodemReceive` (CRC, 128-byte blocks).
    #[cfg(unix)]
    #[tokio::test]
    async fn ccgms_xmodem_us_send_128() {
        let mut child = match spawn_ccgms_xfer("xrecv-crc") {
            Some(c) => c,
            None => { eprintln!("CCGMS_XFER_BIN not set; skipping"); return; }
        };
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();
        let data = ccgms_pattern();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            xmodem_send(&mut from_child, &mut to_child, &data, false, false, false, false, None),
        )
        .await;
        let status = child.wait().await;
        eprintln!("us_send_128: send={:?} child={:?}", res, status);
        res.expect("timed out").expect("our XMODEM send to CCGMS failed");
        assert!(status.unwrap().success(), "CCGMS receiver reported a mismatch");
    }

    /// Our XMODEM-1K sender → real CCGMS `xmodemReceive` (CRC, STX 1024 blocks).
    #[cfg(unix)]
    #[tokio::test]
    async fn ccgms_xmodem_us_send_1k() {
        let mut child = match spawn_ccgms_xfer("xrecv-crc") {
            Some(c) => c,
            None => { eprintln!("CCGMS_XFER_BIN not set; skipping"); return; }
        };
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();
        let data = ccgms_pattern();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            xmodem_send(&mut from_child, &mut to_child, &data, false, false, false, true, None),
        )
        .await;
        let status = child.wait().await;
        eprintln!("us_send_1k: send={:?} child={:?}", res, status);
        res.expect("timed out").expect("our XMODEM-1K send to CCGMS failed");
        assert!(status.unwrap().success(), "CCGMS receiver reported a mismatch");
    }

    /// Real CCGMS `xmodemTransmit` (128-byte blocks) → our XMODEM receiver.
    #[cfg(unix)]
    #[tokio::test]
    async fn ccgms_xmodem_us_recv_from_128() {
        let mut child = match spawn_ccgms_xfer("xsend-128") {
            Some(c) => c,
            None => { eprintln!("CCGMS_XFER_BIN not set; skipping"); return; }
        };
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            xmodem_receive(&mut from_child, &mut to_child, false, false, false),
        )
        .await;
        let _ = child.wait().await;
        let (data, _meta) = res.expect("timed out").expect("our XMODEM recv from CCGMS failed");
        assert_eq!(
            trim_xmodem_padding(data),
            ccgms_pattern(),
            "received data must match CCGMS sender after trimming final-block padding"
        );
    }

    /// Real CCGMS `xmodemTransmit` (STX 1024 blocks) → our XMODEM receiver.
    #[cfg(unix)]
    #[tokio::test]
    async fn ccgms_xmodem_us_recv_from_1k() {
        let mut child = match spawn_ccgms_xfer("xsend-1k") {
            Some(c) => c,
            None => { eprintln!("CCGMS_XFER_BIN not set; skipping"); return; }
        };
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            xmodem_receive(&mut from_child, &mut to_child, false, false, false),
        )
        .await;
        let _ = child.wait().await;
        let (data, _meta) = res.expect("timed out").expect("our XMODEM recv from CCGMS failed");
        assert_eq!(
            trim_xmodem_padding(data),
            ccgms_pattern(),
            "received data must match CCGMS 1K sender after trimming final-block padding"
        );
    }

    // ─── XMODEM: our sender → real `rx` ──────────────────────

    /// Our sender → real `rx -c` (CRC-16).  Validates our CRC-16
    /// negotiation and 128-byte SOH block stream against a real
    /// receiver.  Payload deliberately avoids trailing 0x1A so the
    /// SUB-strip on the receiving side doesn't confuse the assertion.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_xmodem_rx_crc() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("rx")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("rx (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("xmodem_rx_crc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let out_path = tmp.join("received.dat");

        // 256 bytes, no trailing 0x1A — picks every byte 1..=255 then 0,
        // so the last byte is 0x00 (not SUB).
        let payload: Vec<u8> = (1u16..=256u16)
            .map(|i| (i & 0xFF) as u8)
            .collect();

        let mut rx = Command::new("rx")
            .arg("-c") // CRC-16 mode
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rx");

        let mut rx_stdin = rx.stdin.take().unwrap();
        let mut rx_stdout = rx.stdout.take().unwrap();

        let send_result = xmodem_send(
            &mut rx_stdout,
            &mut rx_stdin,
            &payload,
            false,
            false,
            true,
            false,
            None,
        )
        .await;
        let _ = rx.wait().await;
        send_result.expect("xmodem_send against rx -c failed");

        let received = std::fs::read(&out_path).unwrap();
        // rx pads with 0x1A to the next 128-byte boundary.  Strip
        // trailing 0x1A bytes for the comparison — our sender pads
        // identically, and the original payload doesn't end in 0x1A.
        let stripped: Vec<u8> = {
            let mut v = received.clone();
            while v.last() == Some(&0x1A) {
                v.pop();
            }
            v
        };
        assert_eq!(stripped, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Our sender → real `rx` (no `-c`, defaults to checksum mode).
    /// `rx` opens the negotiation with NAK (0x15) so our sender falls
    /// back to the legacy 1-byte checksum path.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_xmodem_rx_checksum() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("rx")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("rx (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("xmodem_rx_cksum_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let out_path = tmp.join("received.dat");

        let payload = b"checksum-mode round trip across legacy XMODEM\n".to_vec();

        let mut rx = Command::new("rx")
            .arg(&out_path) // no -c: defaults to checksum
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rx");

        let mut rx_stdin = rx.stdin.take().unwrap();
        let mut rx_stdout = rx.stdout.take().unwrap();

        let send_result = xmodem_send(
            &mut rx_stdout,
            &mut rx_stdin,
            &payload,
            false,
            false,
            true,
            false,
            None,
        )
        .await;
        let _ = rx.wait().await;
        send_result.expect("xmodem_send against rx (checksum) failed");

        let received = std::fs::read(&out_path).unwrap();
        let stripped: Vec<u8> = {
            let mut v = received.clone();
            while v.last() == Some(&0x1A) {
                v.pop();
            }
            v
        };
        assert_eq!(stripped, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Our sender with `use_1k=true` → real `rx -c`.  Validates our
    /// XMODEM-1K STX/1024 path.  Payload is an exact multiple of 1024
    /// so we never fall back to a final SOH block, exercising the pure
    /// 1K path end-to-end.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_xmodem_rx_1k() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("rx")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("rx (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("xmodem_rx_1k_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let out_path = tmp.join("received.dat");

        // 4096 = exact 4 × 1024 STX blocks, no SOH fallback.
        let payload: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(13) & 0xFF) as u8)
            .collect();

        let mut rx = Command::new("rx")
            .arg("-c") // CRC-16; rx auto-detects STX vs SOH
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rx");

        let mut rx_stdin = rx.stdin.take().unwrap();
        let mut rx_stdout = rx.stdout.take().unwrap();

        let send_result = xmodem_send(
            &mut rx_stdout,
            &mut rx_stdin,
            &payload,
            false,
            false,
            true,
            true, // use_1k
            None,
        )
        .await;
        let _ = rx.wait().await;
        send_result.expect("xmodem_send (1K) against rx -c failed");

        let received = std::fs::read(&out_path).unwrap();
        let stripped: Vec<u8> = {
            let mut v = received.clone();
            while v.last() == Some(&0x1A) {
                v.pop();
            }
            v
        };
        assert_eq!(stripped, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── XMODEM: real `sx` → our receiver ─────────────────────

    /// Real `sx` → our receiver (128-byte SOH path).  `sx` defaults to
    /// XMODEM with CRC-16 negotiation.  Counterpart to the sender-
    /// direction tests above — catches receive-side regressions a real
    /// sender exposes that our internal duplex round-trip can't.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_xmodem_sx_to_us() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("sx")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("sx (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("xmodem_sx_basic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let payload: Vec<u8> = (0..512u32)
            .map(|i| (i.wrapping_mul(7) & 0xFF) as u8)
            .collect();
        let payload_path = tmp.join("payload.bin");
        std::fs::write(&payload_path, &payload).unwrap();

        let mut sx = Command::new("sx")
            .arg(&payload_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn sx");

        let mut sx_stdin = sx.stdin.take().unwrap();
        let mut sx_stdout = sx.stdout.take().unwrap();

        let recv_result = xmodem_receive(
            &mut sx_stdout,
            &mut sx_stdin,
            false,
            false,
            true,
        )
        .await;
        let _ = sx.wait().await;
        let (mut received, _) = recv_result.expect("xmodem_receive against sx failed");

        // sx pads with 0x1A; our receiver strips trailing SUB bytes for
        // plain XMODEM (no size info).  Strip any residual 0x1A in case
        // the boundary aligned exactly with the payload length.
        while received.last() == Some(&0x1A) {
            received.pop();
        }
        assert_eq!(received, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Real `sx -k` → our receiver.  Forces sx to emit STX/1024-byte
    /// blocks; validates our receiver auto-detects STX and decodes the
    /// 1K body correctly against a real sender.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_xmodem_sx_1k_to_us() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("sx")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("sx (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("xmodem_sx_1k_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let payload: Vec<u8> = (0..3072u32)
            .map(|i| (i.wrapping_mul(11) & 0xFF) as u8)
            .collect();
        let payload_path = tmp.join("payload.bin");
        std::fs::write(&payload_path, &payload).unwrap();

        let mut sx = Command::new("sx")
            .arg("-k") // force 1K blocks
            .arg(&payload_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn sx -k");

        let mut sx_stdin = sx.stdin.take().unwrap();
        let mut sx_stdout = sx.stdout.take().unwrap();

        let recv_result = xmodem_receive(
            &mut sx_stdout,
            &mut sx_stdin,
            false,
            false,
            true,
        )
        .await;
        let _ = sx.wait().await;
        let (mut received, _) = recv_result.expect("xmodem_receive against sx -k failed");

        while received.last() == Some(&0x1A) {
            received.pop();
        }
        assert_eq!(received, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── YMODEM: our sender → real `rb` ──────────────────────

    /// Our sender (YMODEM mode) → real `rb`.  Emits block 0 with
    /// filename + size, then data blocks, then end-of-batch.  `rb` is
    /// the YMODEM-batch lrzsz binary; verifies our YMODEM block-0
    /// format is acceptable to a real receiver and that the
    /// end-of-batch handshake completes cleanly.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_ymodem_rb_single() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("rb")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("rb (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("ymodem_rb_single_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let payload: Vec<u8> = (0..600u32)
            .map(|i| (i.wrapping_mul(17) & 0xFF) as u8)
            .collect();
        let header = YmodemHeader {
            filename: "ymodem_test.bin".to_string(),
            size: payload.len() as u64,
            modtime: None,
            mode: None,
        };

        let mut rb = Command::new("rb")
            .current_dir(&tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rb");

        let mut rb_stdin = rb.stdin.take().unwrap();
        let mut rb_stdout = rb.stdout.take().unwrap();

        let send_result = xmodem_send(
            &mut rb_stdout,
            &mut rb_stdin,
            &payload,
            false,
            false,
            true,
            false,
            Some(header),
        )
        .await;
        let _ = rb.wait().await;
        send_result.expect("xmodem_send (YMODEM) against rb failed");

        let received = std::fs::read(tmp.join("ymodem_test.bin")).unwrap();
        // YMODEM declares size in block 0, so rb truncates exactly —
        // no SUB-strip needed and the comparison is byte-exact.
        assert_eq!(received, payload);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Our sender (YMODEM mode, full metadata) → real `rb`.  Emits
    /// block 0 with the maximum-conformance metadata quartet
    /// (length/modtime/mode/sno) and validates that `rb` not only
    /// accepts the transfer but applies the modtime to the saved
    /// file.  This pins down end-to-end interop with the most
    /// common real-world YMODEM receiver when full metadata is in
    /// play, complementing the size-only path above.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_ymodem_rb_full_metadata() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("rb")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("rb (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("ymodem_rb_meta_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let payload: Vec<u8> = (0..512u32)
            .map(|i| (i.wrapping_mul(13) & 0xFF) as u8)
            .collect();
        // Use a clearly-in-the-past timestamp so we can distinguish
        // it from rb's "use now" fallback (which it would apply if
        // we omitted modtime).  2017-07-14 19:40:00 UTC.
        let target_modtime: u64 = 1_500_000_000;
        let header = YmodemHeader {
            filename: "ymodem_meta.bin".to_string(),
            size: payload.len() as u64,
            modtime: Some(target_modtime),
            mode: Some(0o100644),
        };

        let mut rb = Command::new("rb")
            .current_dir(&tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn rb");

        let mut rb_stdin = rb.stdin.take().unwrap();
        let mut rb_stdout = rb.stdout.take().unwrap();

        let send_result = xmodem_send(
            &mut rb_stdout,
            &mut rb_stdin,
            &payload,
            false,
            false,
            true,
            false,
            Some(header),
        )
        .await;
        let _ = rb.wait().await;
        send_result.expect("xmodem_send (YMODEM full meta) against rb failed");

        let saved_path = tmp.join("ymodem_meta.bin");
        let received = std::fs::read(&saved_path).unwrap();
        assert_eq!(received, payload, "data must round-trip");

        // rb honors block-0 modtime by setting the saved file's
        // mtime — verify it lands within ±1 second of what we sent
        // (filesystem second-granularity tolerance).
        let saved_mtime = std::fs::metadata(&saved_path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let delta = (saved_mtime - target_modtime as i64).abs();
        assert!(
            delta <= 1,
            "rb saved-file mtime ({}) must match block-0 modtime ({}); delta={}",
            saved_mtime,
            target_modtime,
            delta,
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── YMODEM: real `sb` → our receiver ─────────────────────

    /// Real `sb` → our receiver.  Validates our auto-detection of
    /// YMODEM via block 0, filename + size extraction, and the size-
    /// based truncation that preserves files ending in 0x1A.  Payload
    /// deliberately ends in 0x1A so a SUB-strip would corrupt it; if
    /// the assertion passes, size-truncation is working.
    ///
    /// The multi-file batch case (`sb file1 file2`) is covered by
    /// `test_lrzsz_ymodem_sb_to_us_batch` below (and deterministically, without
    /// lrzsz, by `test_ymodem_batch_two_files`).
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_ymodem_sb_to_us_single() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("sb")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("sb (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir()
            .join(format!("ymodem_sb_single_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Payload that ends in 0x1A — tests size-based truncation
        // (YMODEM block 0 declares the exact size).  If the receiver
        // SUB-strips instead, the trailing 0x1A would be lost.
        let mut payload: Vec<u8> = (0..500u32)
            .map(|i| (i.wrapping_mul(19) & 0xFF) as u8)
            .collect();
        payload.push(0x1A);
        payload.push(0x1A);
        let payload_path = tmp.join("ymodem_payload.bin");
        std::fs::write(&payload_path, &payload).unwrap();

        let mut sb = Command::new("sb")
            .arg(&payload_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn sb");

        let mut sb_stdin = sb.stdin.take().unwrap();
        let mut sb_stdout = sb.stdout.take().unwrap();

        let recv_result = xmodem_receive(
            &mut sb_stdout,
            &mut sb_stdin,
            false,
            false,
            true,
        )
        .await;
        let _ = sb.wait().await;
        let (received, meta) = recv_result.expect("xmodem_receive against sb failed");

        assert_eq!(
            received, payload,
            "YMODEM size-truncation should preserve trailing 0x1A bytes"
        );

        // sb populates the full block-0 metadata quartet — surface it
        // through our parser so this test pins the interop contract
        // for the modtime/mode fields, not just the data round-trip.
        let m = meta.expect("sb must emit block-0 metadata");
        assert_eq!(
            m.size,
            Some(payload.len() as u64),
            "block-0 length must match real file size",
        );
        // sb fills modtime from the source file's stat, which we just
        // wrote above — must be a recent value, not 0.
        let modtime = m.modtime.expect("sb must emit modtime");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            modtime > 0 && now.saturating_sub(modtime) < 60,
            "sb modtime ({}) must be a recent UNIX timestamp",
            modtime,
        );
        // sb emits the source file's mode; std::fs::write produces
        // 0o644 by default on Linux, possibly modified by umask.
        // Just check that the perm bits are non-zero and within
        // 0o7777.
        let mode = m.mode.expect("sb must emit mode");
        assert!(
            mode != 0 && mode & !0o7777 == 0,
            "sb mode ({:o}) must be a valid permission word",
            mode,
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The originally-deferred case: `sb file1 file2` → our batch receiver must
    /// recover BOTH files with the sender's block-0 names, exact bytes, and
    /// sizes.  This is the lrzsz ground-truth for the multi-file path;
    /// `test_ymodem_batch_two_files` covers the same logic in CI without lrzsz.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn test_lrzsz_ymodem_sb_to_us_batch() {
        use std::process::Stdio;
        use tokio::process::Command;

        if Command::new("sb")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            panic!("sb (lrzsz) not found on PATH");
        }

        let tmp = std::env::temp_dir().join(format!("ymodem_sb_batch_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // File A: small.  File B: multi-block, ending in 0x1A (exercises
        // size-truncation on a non-first file).
        let data_a = b"first file in the batch".to_vec();
        let mut data_b: Vec<u8> = (0..400u32).map(|i| (i.wrapping_mul(7) & 0xFF) as u8).collect();
        data_b.push(0x1A);
        data_b.push(0x1A);
        let path_a = tmp.join("batch_a.bin");
        let path_b = tmp.join("batch_b.dat");
        std::fs::write(&path_a, &data_a).unwrap();
        std::fs::write(&path_b, &data_b).unwrap();

        let mut sb = Command::new("sb")
            .arg(&path_a)
            .arg(&path_b)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn sb");
        let mut sb_stdin = sb.stdin.take().unwrap();
        let mut sb_stdout = sb.stdout.take().unwrap();

        let recv = xmodem_receive_batch(&mut sb_stdout, &mut sb_stdin, false, false, true).await;
        let _ = sb.wait().await;
        let files = recv.expect("xmodem_receive_batch against sb failed");

        assert_eq!(files.len(), 2, "sb sent two files; both must be received");
        assert_eq!(files[0].filename.as_deref(), Some("batch_a.bin"));
        assert_eq!(files[0].data, data_a, "file A must round-trip exactly");
        assert_eq!(files[1].filename.as_deref(), Some("batch_b.dat"));
        assert_eq!(
            files[1].data, data_b,
            "file B (multi-block, trailing 0x1A) must round-trip exactly via size truncation"
        );
        assert_eq!(files[1].meta.as_ref().and_then(|m| m.size), Some(data_b.len() as u64));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── lrzsz interop: recorded-wire replay ─────────────────
    //
    // Mirrors the ZMODEM recorded-wire replay in src/zmodem.rs.  The
    // fixtures in `tests/fixtures/xmodem_*.bin` / `ymodem_*.bin` were
    // produced once by `record_lrzsz_xmodem_fixtures` (an `#[ignore]`
    // test that drives real `sx`/`sb`) and are checked in, so these
    // replay tests run in CI on every platform with NO lrzsz needed.
    //
    // Why this catches what the in-process round-trips can't: a pure
    // round-trip uses our sender AND our receiver, so a shared framing
    // bug stays green on both sides.  Replaying bytes a *real* sender
    // emitted exposes any divergence between our decoder and the wire
    // format lrzsz actually produces.  The live `sx`/`sb` `#[ignore]`
    // tests above remain the ground-truth source (and the only cover
    // for our SEND path, which needs an interactive peer); these
    // fixtures lock the RECEIVE path deterministically.
    //
    // Both XMODEM modes are covered: CRC (receiver requests with 'C')
    // and checksum (sender produced 128-byte blocks with a 1-byte
    // sum).  The replay receiver always opens with 'C'; for the
    // checksum fixture its first-block auto-detect (receive_block_body
    // `auto_detect=true`) recognises the sum, locks the session to
    // checksum, and pushes back the stray trailer byte — so a single
    // prefill path decodes both.

    /// Drive `xmodem_receive` against a pre-recorded sender-side byte
    /// stream.  The capture is prefilled into the receiver's reader and
    /// the write half dropped so the receiver sees EOF after the last
    /// byte; our outbound 'C'/NAK/ACK responses are drained and
    /// discarded, since the capture already contains every block the
    /// original sender produced.
    async fn replay_xmodem_capture(
        capture: &[u8],
    ) -> Result<(Vec<u8>, Option<YmodemReceiveMeta>), String> {
        let (mut inbound_writer, mut inbound_reader) =
            tokio::io::duplex(capture.len() + 8192);
        inbound_writer
            .write_all(capture)
            .await
            .expect("prefill inbound");
        drop(inbound_writer);

        let (mut discard_reader, mut outbound_writer) =
            tokio::io::duplex(16 * 1024);
        let drain_task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match discard_reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        });

        let result =
            xmodem_receive(&mut inbound_reader, &mut outbound_writer, false, false, false)
                .await;
        drop(outbound_writer);
        let _ = drain_task.await;
        result
    }

    #[tokio::test]
    async fn test_lrzsz_replay_xmodem_crc() {
        let capture = include_bytes!("../tests/fixtures/xmodem_crc.bin");
        let expected = include_bytes!("../tests/fixtures/xmodem_crc.payload");
        let (mut got, meta) =
            replay_xmodem_capture(capture).await.expect("CRC replay failed");
        while got.last() == Some(&SUB) {
            got.pop();
        }
        assert_eq!(got, expected, "CRC capture must decode to the original payload");
        assert!(meta.is_none(), "plain XMODEM yields no YMODEM block-0 metadata");
    }

    #[tokio::test]
    async fn test_lrzsz_replay_xmodem_checksum() {
        // The receiver opens with 'C'; first-block auto-detect locks it
        // to checksum mode against this real `sx` 1-byte-sum stream.
        let capture = include_bytes!("../tests/fixtures/xmodem_checksum.bin");
        let expected = include_bytes!("../tests/fixtures/xmodem_checksum.payload");
        let (mut got, _) =
            replay_xmodem_capture(capture).await.expect("checksum replay failed");
        while got.last() == Some(&SUB) {
            got.pop();
        }
        assert_eq!(
            got, expected,
            "checksum capture must decode to the original payload"
        );
    }

    #[tokio::test]
    async fn test_lrzsz_replay_xmodem_1k() {
        let capture = include_bytes!("../tests/fixtures/xmodem_1k.bin");
        let expected = include_bytes!("../tests/fixtures/xmodem_1k.payload");
        let (mut got, _) =
            replay_xmodem_capture(capture).await.expect("1K replay failed");
        while got.last() == Some(&SUB) {
            got.pop();
        }
        assert_eq!(got, expected, "STX/1K capture must decode to the original payload");
    }

    #[tokio::test]
    async fn test_lrzsz_replay_ymodem() {
        let capture = include_bytes!("../tests/fixtures/ymodem_single.bin");
        let expected = include_bytes!("../tests/fixtures/ymodem_single.payload");
        let (got, meta) =
            replay_xmodem_capture(capture).await.expect("YMODEM replay failed");
        // YMODEM truncates by the block-0 size, so the trailing 0x1A
        // bytes survive — no SUB stripping, exact compare.
        assert_eq!(got, expected, "YMODEM capture must decode to the original payload");
        assert_eq!(
            meta.expect("YMODEM must surface block-0 metadata").size,
            Some(expected.len() as u64),
            "YMODEM block-0 size must match the payload length",
        );
    }

    // ─── YMODEM batch (multi-file) receive ────────────────────────────────

    /// Frame one 128-byte block: `SOH seq ~seq payload[128] crchi crclo`.
    fn ymodem_frame(seq: u8, payload: &[u8; 128]) -> Vec<u8> {
        let crc = crc16_xmodem(payload);
        let mut v = vec![SOH, seq, !seq];
        v.extend_from_slice(payload);
        v.push((crc >> 8) as u8);
        v.push((crc & 0xFF) as u8);
        v
    }

    /// Build a YMODEM block-0 payload: `filename\0<size>` then NUL fill.
    fn ymodem_block0_payload(name: &str, size: usize) -> [u8; 128] {
        ymodem_block0_payload_bytes(name.as_bytes(), size)
    }

    /// Like `ymodem_block0_payload` but takes the filename as raw bytes, so a
    /// test can build a block 0 with a non-UTF-8 name (`name\0<size>\0…`).
    fn ymodem_block0_payload_bytes(name: &[u8], size: usize) -> [u8; 128] {
        let mut p = [0u8; 128];
        let mut v = Vec::new();
        v.extend_from_slice(name);
        v.push(0);
        v.extend_from_slice(size.to_string().as_bytes());
        p[..v.len()].copy_from_slice(&v);
        p
    }

    /// Assemble the full wire stream a YMODEM batch sender (`sb file1 file2 …`)
    /// emits: per file a block 0 (name + size), its 128-byte data blocks
    /// (padded with SUB), and an EOT — then a single null block 0 to close the
    /// batch.  The receiver reads these in order, so a prefilled stream drives
    /// the whole exchange without a live sender.
    fn build_ymodem_batch_wire(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut wire = Vec::new();
        for (name, data) in files {
            wire.extend(ymodem_frame(0, &ymodem_block0_payload(name, data.len())));
            for (i, chunk) in data.chunks(XMODEM_BLOCK_SIZE).enumerate() {
                let mut payload = [SUB; XMODEM_BLOCK_SIZE];
                payload[..chunk.len()].copy_from_slice(chunk);
                wire.extend(ymodem_frame((i + 1) as u8, &payload));
            }
            wire.push(EOT);
        }
        // End-of-batch: a block 0 whose filename starts with NUL.
        wire.extend(ymodem_frame(0, &[0u8; XMODEM_BLOCK_SIZE]));
        wire
    }

    async fn replay_ymodem_batch(capture: &[u8]) -> Result<Vec<XmodemReceivedFile>, String> {
        let (mut inbound_writer, mut inbound_reader) = tokio::io::duplex(capture.len() + 8192);
        inbound_writer.write_all(capture).await.expect("prefill inbound");
        drop(inbound_writer);
        let (mut discard_reader, mut outbound_writer) = tokio::io::duplex(64 * 1024);
        let drain = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = discard_reader.read(&mut buf).await {
                if n == 0 { break; }
            }
        });
        let result =
            xmodem_receive_batch(&mut inbound_reader, &mut outbound_writer, false, false, false)
                .await;
        drop(outbound_writer);
        let _ = drain.await;
        result
    }

    /// The regression that was deferred: a multi-file YMODEM batch must yield
    /// every file (name + exact bytes + size), not just the first.  File A is
    /// sub-block-sized; file B spans multiple blocks and holds all byte values
    /// (protocol bytes included) so framing/CRC and per-file size truncation
    /// are both exercised.
    #[tokio::test]
    async fn test_ymodem_batch_two_files() {
        let data_a = b"Alpha file: hello, batch world!".to_vec();
        let data_b: Vec<u8> = (0u8..=255).cycle().take(300).collect();
        let wire = build_ymodem_batch_wire(&[("alpha.txt", &data_a), ("beta.bin", &data_b)]);

        let files = replay_ymodem_batch(&wire).await.expect("batch receive failed");

        assert_eq!(files.len(), 2, "both files in the batch must be received");
        assert_eq!(files[0].filename.as_deref(), Some("alpha.txt"));
        assert_eq!(files[0].data, data_a, "file A bytes must round-trip exactly");
        assert_eq!(files[0].meta.as_ref().and_then(|m| m.size), Some(data_a.len() as u64));
        assert_eq!(files[1].filename.as_deref(), Some("beta.bin"));
        assert_eq!(files[1].data, data_b, "file B (multi-block) bytes must round-trip exactly");
        assert_eq!(files[1].meta.as_ref().and_then(|m| m.size), Some(data_b.len() as u64));
    }

    /// A single-file YMODEM stream through the batch fn must still yield exactly
    /// one file (guards the wrapper's contract and the null-block-0 terminator).
    #[tokio::test]
    async fn test_ymodem_batch_single_file() {
        let data = b"just one file".to_vec();
        let wire = build_ymodem_batch_wire(&[("solo.dat", &data)]);
        let files = replay_ymodem_batch(&wire).await.expect("single-file batch failed");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename.as_deref(), Some("solo.dat"));
        assert_eq!(files[0].data, data);
    }

    /// Regression for the review finding: a mid-batch file whose block-0 name
    /// is NOT valid UTF-8 must still be received (with `filename == None`, so
    /// the caller names it) — it must NOT be mistaken for the null-block-0
    /// terminator, which would silently drop it and every later file.
    #[tokio::test]
    async fn test_ymodem_batch_non_utf8_middle_name() {
        let data_a = b"first".to_vec();
        let data_b = b"second file's bytes".to_vec();
        let data_c = b"third".to_vec();
        // Hand-build: A (ascii), B with a Latin-1 name (0xE9 = é, invalid UTF-8),
        // C (ascii), then the null terminator.
        let mut wire = Vec::new();
        wire.extend(ymodem_frame(0, &ymodem_block0_payload("a.txt", data_a.len())));
        wire.extend(ymodem_frame(1, &{
            let mut p = [SUB; XMODEM_BLOCK_SIZE];
            p[..data_a.len()].copy_from_slice(&data_a);
            p
        }));
        wire.push(EOT);
        wire.extend(ymodem_frame(0, &ymodem_block0_payload_bytes(b"caf\xe9.txt", data_b.len())));
        wire.extend(ymodem_frame(1, &{
            let mut p = [SUB; XMODEM_BLOCK_SIZE];
            p[..data_b.len()].copy_from_slice(&data_b);
            p
        }));
        wire.push(EOT);
        wire.extend(ymodem_frame(0, &ymodem_block0_payload("c.txt", data_c.len())));
        wire.extend(ymodem_frame(1, &{
            let mut p = [SUB; XMODEM_BLOCK_SIZE];
            p[..data_c.len()].copy_from_slice(&data_c);
            p
        }));
        wire.push(EOT);
        wire.extend(ymodem_frame(0, &[0u8; XMODEM_BLOCK_SIZE]));

        let files = replay_ymodem_batch(&wire).await.expect("batch failed");
        assert_eq!(files.len(), 3, "the non-UTF-8-named file must not truncate the batch");
        assert_eq!(files[0].filename.as_deref(), Some("a.txt"));
        assert_eq!(files[1].filename, None, "non-UTF-8 name decodes to None, file still received");
        assert_eq!(files[1].data, data_b, "the non-UTF-8-named file's data must survive");
        assert_eq!(files[2].filename.as_deref(), Some("c.txt"));
        assert_eq!(files[2].data, data_c);
    }

    /// A 3-file batch where a NON-first file ends in 0x1A — proves per-file
    /// size truncation runs for files 2..N (not just file 1), in CI.
    #[tokio::test]
    async fn test_ymodem_batch_three_files_trailing_sub() {
        let data_a = b"alpha".to_vec();
        let data_b = vec![0x1A_u8; 40]; // all-SUB payload; size truncation must keep them
        let data_c: Vec<u8> = (0u8..=255).cycle().take(200).collect();
        let wire = build_ymodem_batch_wire(&[
            ("a.bin", &data_a),
            ("b.bin", &data_b),
            ("c.bin", &data_c),
        ]);
        let files = replay_ymodem_batch(&wire).await.expect("3-file batch failed");
        assert_eq!(files.len(), 3);
        assert_eq!(files[1].data, data_b, "file 2's trailing 0x1A must survive via size truncation");
        assert_eq!(files[2].data, data_c);
    }

    /// The batch file-count cap (MAX_BATCH_FILES) must bound an unterminated /
    /// hostile stream rather than accumulate files without limit.
    #[tokio::test]
    async fn test_ymodem_batch_file_count_cap() {
        // MAX_BATCH_FILES + 1 tiny (empty) files, no early terminator.
        let names: Vec<String> = (0..=MAX_BATCH_FILES).map(|i| format!("f{i}.dat")).collect();
        let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &[][..])).collect();
        let wire = build_ymodem_batch_wire(&files);
        let err = replay_ymodem_batch(&wire)
            .await
            .expect_err("a batch over the file-count cap must be rejected");
        assert!(
            err.contains("file-count"),
            "must reject for the file-count cap specifically, got: {err}"
        );
    }

    /// A corrupt inter-file block 0 (bad CRC) with no following retransmit must
    /// end the batch gracefully — keeping the already-received files — rather
    /// than hang or drop file 1.  Exercises the give-up side of the bounded
    /// inter-file NAK-retry (the recovery side mirrors the first-file block-0
    /// retry, which `test_ymodem_receive_block_zero_crc_error_recovery` covers).
    #[tokio::test]
    async fn test_ymodem_batch_corrupt_interfile_block0_ends_gracefully() {
        let data_a = b"the first file survives".to_vec();
        let mut wire = Vec::new();
        wire.extend(ymodem_frame(0, &ymodem_block0_payload("a.txt", data_a.len())));
        wire.extend(ymodem_frame(1, &{
            let mut p = [SUB; XMODEM_BLOCK_SIZE];
            p[..data_a.len()].copy_from_slice(&data_a);
            p
        }));
        wire.push(EOT);
        // A would-be file 2 block 0 with valid framing but a flipped CRC byte,
        // and nothing after it (no retransmit) → receiver NAKs, re-reads, hits
        // EOF, and ends the batch keeping file 1.
        let mut bad = ymodem_frame(0, &ymodem_block0_payload("b.txt", 5));
        *bad.last_mut().unwrap() ^= 0xFF;
        wire.extend(bad);

        let files = replay_ymodem_batch(&wire)
            .await
            .expect("batch should end gracefully, not error");
        assert_eq!(files.len(), 1, "file 1 must survive a corrupt inter-file block 0");
        assert_eq!(files[0].filename.as_deref(), Some("a.txt"));
        assert_eq!(files[0].data, data_a);
    }

    /// Capture the wire bytes a real `sx` emits for a plain-XMODEM
    /// transfer.  Hand-rolls the lock-step receiver side (send the mode
    /// trigger, then ACK each block) so we can force checksum mode
    /// (trigger = NAK) as well as CRC (trigger = 'C') — our production
    /// `xmodem_receive` always opens with 'C', so it can't elicit a
    /// checksum stream from `sx`.  `extra_args` lets `-k` force 1K/STX
    /// blocks.  Returns only the sender → receiver bytes (what replay
    /// feeds back in).
    #[cfg(unix)]
    async fn capture_plain_xmodem(
        extra_args: &[&str],
        trigger: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        use std::process::Stdio;
        use tokio::process::Command;

        let tmp = std::env::temp_dir().join(format!(
            "xmodem_rec_{:02x}_{}",
            trigger,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let payload_path = tmp.join("payload.bin");
        std::fs::write(&payload_path, payload).unwrap();

        let mut cmd = Command::new("sx");
        for a in extra_args {
            cmd.arg(a);
        }
        let mut sx = cmd
            .arg(&payload_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn sx");

        let mut sx_stdin = sx.stdin.take().unwrap();
        let mut sx_stdout = sx.stdout.take().unwrap();

        let trailer_len = if trigger == CRC_REQUEST { 2 } else { 1 };
        let mut captured: Vec<u8> = Vec::new();

        // Request the mode; sx (stop-and-wait) sends block 1 in response.
        sx_stdin.write_all(&[trigger]).await.expect("send trigger");

        let mut hdr = [0u8; 1];
        loop {
            sx_stdout.read_exact(&mut hdr).await.expect("read header");
            captured.push(hdr[0]);
            match hdr[0] {
                EOT => {
                    // Mirror xmodem_receive's NAK-first-EOT: NAK the first
                    // EOT so `sx` resends it, and capture that confirming
                    // EOT into the fixture.  Without it the one-EOT capture
                    // would no longer satisfy the (now NAK-first) replay
                    // receiver, which reads a second EOT before finishing.
                    sx_stdin.write_all(&[NAK]).await.expect("nak first eot");
                    sx_stdout.read_exact(&mut hdr).await.expect("read confirming eot");
                    assert_eq!(hdr[0], EOT, "sx should resend EOT after a NAK");
                    captured.push(hdr[0]);
                    sx_stdin.write_all(&[ACK]).await.expect("ack eot");
                    break;
                }
                SOH | STX => {
                    let block_size = if hdr[0] == STX {
                        XMODEM_1K_BLOCK_SIZE
                    } else {
                        XMODEM_BLOCK_SIZE
                    };
                    // num + ~num + data + trailer
                    let mut rest = vec![0u8; 2 + block_size + trailer_len];
                    sx_stdout
                        .read_exact(&mut rest)
                        .await
                        .expect("read block body");
                    captured.extend_from_slice(&rest);
                    sx_stdin.write_all(&[ACK]).await.expect("ack block");
                }
                other => panic!("unexpected header byte 0x{:02X} from sx", other),
            }
        }

        let _ = sx.wait().await;
        let _ = std::fs::remove_dir_all(&tmp);
        captured
    }

    /// Refresh the checked-in lrzsz XMODEM/YMODEM fixtures.  Two-step
    /// opt-in mirrors the ZMODEM recorder: `#[ignore]` keeps it off the
    /// default pass, and the env-var keeps it off bulk `--ignored` runs
    /// where it would silently rewrite committed fixtures.  Run with:
    ///
    ///   XMODEM_RECORD_FIXTURES=1 cargo test --release \
    ///       record_lrzsz_xmodem_fixtures -- --ignored --exact --nocapture
    ///
    /// Each capture is round-tripped through `replay_xmodem_capture`
    /// before it's written, so a buggy recorder fails here rather than
    /// committing a bad fixture.
    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn record_lrzsz_xmodem_fixtures() {
        use std::process::Stdio;
        use tokio::process::Command;

        if std::env::var("XMODEM_RECORD_FIXTURES").is_err() {
            eprintln!(
                "record_lrzsz_xmodem_fixtures: skipped (set XMODEM_RECORD_FIXTURES=1 to refresh)"
            );
            return;
        }

        for bin in ["sx", "sb"] {
            if Command::new(bin)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| !s.success())
                .unwrap_or(true)
            {
                panic!("{} (lrzsz) not found on PATH — install lrzsz before refreshing fixtures", bin);
            }
        }

        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        let fixtures_dir =
            std::path::Path::new(&manifest_dir).join("tests/fixtures");
        std::fs::create_dir_all(&fixtures_dir).unwrap();

        // Sanity-check a plain-XMODEM capture by replaying it through
        // our real receiver before committing it.
        async fn check_plain(capture: &[u8], payload: &[u8], base: &str) {
            let (mut got, _) = replay_xmodem_capture(capture)
                .await
                .unwrap_or_else(|e| panic!("{}: replay failed: {}", base, e));
            while got.last() == Some(&SUB) {
                got.pop();
            }
            assert_eq!(got, payload, "{}: capture must round-trip to payload", base);
        }

        // ── XMODEM CRC: every byte value, two exact 128-byte blocks ──
        let crc_payload: Vec<u8> = (0u8..=255).collect();
        let crc_cap = capture_plain_xmodem(&[], CRC_REQUEST, &crc_payload).await;
        check_plain(&crc_cap, &crc_payload, "xmodem_crc").await;

        // ── XMODEM checksum: varied bytes, partial final block (SUB
        //    padding) — and a 1-byte sum trailer the receiver must
        //    auto-detect from a 'C' open. ──
        let csum_payload: Vec<u8> = (0..200u32)
            .map(|i| (i.wrapping_mul(13).wrapping_add(5) & 0xFF) as u8)
            .collect();
        let csum_cap = capture_plain_xmodem(&[], NAK, &csum_payload).await;
        check_plain(&csum_cap, &csum_payload, "xmodem_checksum").await;

        // ── XMODEM-1K: STX/1024-byte blocks (sx -k), three exact 1K ──
        let onek_payload: Vec<u8> = (0..3072u32)
            .map(|i| (i.wrapping_mul(11) & 0xFF) as u8)
            .collect();
        let onek_cap = capture_plain_xmodem(&["-k"], CRC_REQUEST, &onek_payload).await;
        check_plain(&onek_cap, &onek_payload, "xmodem_1k").await;

        // ── YMODEM single file: block-0 (name + size) then data.  The
        //    block-0 / end-of-batch handshake is interactive, so drive
        //    it with our real receiver and tee the inbound stream. ──
        let ym_payload: Vec<u8> = {
            let mut v: Vec<u8> = (0..500u32)
                .map(|i| (i.wrapping_mul(19) & 0xFF) as u8)
                .collect();
            // End in 0x1A to prove size-based truncation (not SUB strip).
            v.push(0x1A);
            v.push(0x1A);
            v
        };
        let ym_cap = {
            let tmp = std::env::temp_dir()
                .join(format!("ymodem_rec_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();
            // sb sends this basename as the YMODEM filename.
            let payload_path = tmp.join("ymodem_fixture.bin");
            std::fs::write(&payload_path, &ym_payload).unwrap();

            let mut sb = Command::new("sb")
                .arg(&payload_path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("failed to spawn sb");
            let mut sb_stdin = sb.stdin.take().unwrap();
            let mut sb_stdout = sb.stdout.take().unwrap();

            let (mut tee_write, mut tee_read) = tokio::io::duplex(1 << 20);
            let tee_task = tokio::spawn(async move {
                let mut captured: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    match sb_stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            captured.extend_from_slice(&buf[..n]);
                            if tee_write.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                captured
            });

            let recv_result =
                xmodem_receive(&mut tee_read, &mut sb_stdin, false, false, false).await;
            let _ = sb.wait().await;
            let captured = tee_task.await.unwrap();
            let (received, meta) =
                recv_result.expect("xmodem_receive against sb failed");
            assert_eq!(received, ym_payload, "ymodem: capture must round-trip");
            assert_eq!(
                meta.and_then(|m| m.size),
                Some(ym_payload.len() as u64),
                "ymodem: block-0 size must match"
            );
            let _ = std::fs::remove_dir_all(&tmp);
            captured
        };

        for (base, cap, payload) in [
            ("xmodem_crc", &crc_cap, &crc_payload),
            ("xmodem_checksum", &csum_cap, &csum_payload),
            ("xmodem_1k", &onek_cap, &onek_payload),
            ("ymodem_single", &ym_cap, &ym_payload),
        ] {
            std::fs::write(fixtures_dir.join(format!("{}.bin", base)), cap).unwrap();
            std::fs::write(fixtures_dir.join(format!("{}.payload", base)), payload)
                .unwrap();
            println!(
                "  recorded {}.bin ({} wire bytes for {} payload bytes)",
                base,
                cap.len(),
                payload.len()
            );
        }
    }

    // ─── proptest fuzz: parse_ymodem_block_zero_payload ─────────
    //
    // The block-0 parser sees adversarial bytes from any sender that
    // can drive an XMODEM-mode handshake.  Spec senders are
    // well-formed; broken or malicious senders may send anything in
    // the 128-byte payload.  Property: the parser never panics —
    // outcomes are `Some(meta)` or `None`.  An out-of-bounds index,
    // subtraction overflow, or UTF-8 unwrap would surface here.

    mod ymodem_parser_proptest {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                ..ProptestConfig::default()
            })]

            /// 128-byte payloads sized exactly as the receiver sees
            /// them — must never panic regardless of content.
            #[test]
            fn prop_parse_block_zero_full_size_no_panic(
                bytes in prop::collection::vec(any::<u8>(), XMODEM_BLOCK_SIZE..=XMODEM_BLOCK_SIZE),
            ) {
                let _ = parse_ymodem_block_zero_payload(&bytes);
            }

            /// Parser is also called on shorter slices in tests; must
            /// tolerate any length without panic.
            #[test]
            fn prop_parse_block_zero_arbitrary_length_no_panic(
                bytes in prop::collection::vec(any::<u8>(), 0..256),
            ) {
                let _ = parse_ymodem_block_zero_payload(&bytes);
            }
        }
    }
}
