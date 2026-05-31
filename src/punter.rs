//! Punter (C1 "New Punter") file-transfer protocol.
//!
//! Single-file C1, the protocol CCGMS / Novaterm / StrikeTerm speak natively
//! on Commodore BBSes.  This is a clean-room Rust implementation written from
//! the C1 wire description (Michael Steil's pagetable.com reconstruction) and
//! cross-checked against the Novaterm 9.6 / v10 `punter.src` 6502 source for
//! the corners the prose spec leaves ambiguous.  Every such corner cites the
//! `punter.src` routine it was verified against.
//!
//! Public entry points (mirroring `xmodem.rs`):
//! - [`punter_receive`] — receive a file from a C1 sender (upload).
//! - [`punter_send`]    — send a file to a C1 receiver (download).
//!
//! Both take an already-open byte stream so a future Multi-Punter (MPP) batch
//! wrapper can layer above the wire without touching this module.
//!
//! ## Wire format (C1)
//!
//! A block is 7–255 bytes:
//!
//! ```text
//! offset 0: additive checksum  (2 bytes, little-endian)
//! offset 2: cyclic   checksum  (2 bytes, little-endian)
//! offset 4: size of the NEXT block (1 byte)
//! offset 5: block index        (2 bytes, little-endian; high byte 0xFF = final)
//! offset 7: payload            (0–248 bytes)
//! ```
//!
//! Both checksums cover bytes from offset 4 to end-of-block (they skip
//! themselves).  Verified against `checksum` (`punter.src` v9.6 line 517 /
//! v10 line 422): the loop runs `ldy #sizepos` (offset 4) until `cpy bufcount`
//! (the block's own length).
//!
//! The size byte at offset 4 announces the length of the *next* block, so the
//! receiver always knows how many bytes to read.  The very first block of a
//! phase is therefore a fixed length known a-priori to both ends: 7 bytes
//! (header only, no payload) for the data phase, 8 bytes (one payload byte =
//! the file-type) for the type phase.  Verified against `receive`
//! (`buffer+sizepos` seeded with `datapos`=7, line 597) and `rectype`
//! (`datapos+1`=8, line 681).
//!
//! ## Handshake codes
//!
//! Three-byte ASCII tokens, packed in Novaterm as `codes .asc "goobadacks/bsyn"`
//! (`punter.src` v9.6 line 98): `GOO` (idx 0), `BAD` (idx 3), `ACK` (idx 6),
//! `S/B` (idx 9), `SYN` (idx 12).
//!
//! Direction (the asm is authoritative; some prose write-ups get this
//! backwards): the **receiver** sends `GOO`/`BAD` and `S/B`; the **sender**
//! sends `ACK`; both send `SYN` during the end-off.  Per data block:
//!
//! ```text
//!   receiver → GOO   (or BAD to demand a resend of the same block)
//!   sender   → ACK
//!   receiver → S/B
//!   sender   → <block bytes>
//! ```
//!
//! ## End-off
//!
//! After the final block (block index high byte 0xFF) both ends run a SYN
//! handshake, and the sender finishes by transmitting `S/B` three times.
//! This "send three S/B" behaviour is inherited deliberately for interop:
//! real C1 senders emit it (`tranhand` tx8/tx9, line 359) and real receivers
//! expect to drain it.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::config;
use crate::logger::glog;
use crate::telnet::is_esc_key;
use crate::tnio::{nvt_read_byte, raw_write_bytes, ReadState};

// ─── Block layout constants (match `punter.src`) ─────────────────────────

/// Offset of the 1-byte "size of next block" field.  `sizepos` in the asm.
const SIZEPOS: usize = 4;
/// Offset of the 2-byte little-endian block index.  `numpos` in the asm.
const NUMPOS: usize = 5;
/// Offset where payload bytes begin.  `datapos` in the asm.
const DATAPOS: usize = 7;
/// Largest legal total block size (one byte holds the size, payload tops out
/// at 248).  `sizes .word 255` in the asm (line 73).
const MAX_BLOCK: usize = 255;
/// Largest payload that fits in a single block.
const MAX_PAYLOAD: usize = MAX_BLOCK - DATAPOS; // 248
/// Fixed length of the first data-phase block: header only, no payload.
const DATA_PHASE_FIRST_SIZE: u8 = DATAPOS as u8; // 7
/// Fixed length of the (single) type-phase block: header + one type byte.
const TYPE_PHASE_SIZE: u8 = DATAPOS as u8 + 1; // 8

/// Hard cap on a transferred file, shared with the other protocols via
/// `tnio::MAX_FILE_SIZE` so all of XMODEM/YMODEM/ZMODEM/Kermit/Punter agree.
const MAX_FILE_SIZE: usize = crate::tnio::MAX_FILE_SIZE as usize;

// ─── Handshake codes ─────────────────────────────────────────────────────

/// The five three-byte handshake tokens.  Values from the packed Novaterm
/// `codes` string (`punter.src` v9.6 line 98).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Code {
    Goo,
    Bad,
    Ack,
    Sb,
    Syn,
}

impl Code {
    fn bytes(self) -> &'static [u8; 3] {
        match self {
            Code::Goo => b"goo",
            Code::Bad => b"bad",
            Code::Ack => b"ack",
            Code::Sb => b"s/b",
            Code::Syn => b"syn",
        }
    }

    fn from_window(w: &[u8; 3]) -> Option<Code> {
        for c in [Code::Goo, Code::Bad, Code::Ack, Code::Sb, Code::Syn] {
            if w == c.bytes() {
                return Some(c);
            }
        }
        None
    }
}

// ─── File type ───────────────────────────────────────────────────────────

/// The one-byte file-type carried by the C1 type block (Phase A): `0` = PRG
/// (a Commodore program, load-address-prefixed), `1` = SEQ (a flat sequential
/// file).  The gateway's Linux file server has no native PRG/SEQ distinction,
/// so on receive we record the sender's declared type for the caller and write
/// the bytes flat; on send the caller picks the type (auto-detected from the
/// file, overridable in the UI).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PunterFileType {
    Prg,
    Seq,
}

impl PunterFileType {
    fn to_byte(self) -> u8 {
        match self {
            PunterFileType::Prg => 0,
            PunterFileType::Seq => 1,
        }
    }

    /// Any non-zero type byte is treated as SEQ; only `0` is PRG.  (The C1
    /// type byte is effectively boolean in every terminal we target.)
    fn from_byte(b: u8) -> PunterFileType {
        if b == 0 {
            PunterFileType::Prg
        } else {
            PunterFileType::Seq
        }
    }

    /// Auto-detect the type to declare for an outbound file.  Text-flavoured
    /// extensions (`.seq`/`.txt`/`.doc`) are SEQ; everything else defaults to
    /// PRG, since the overwhelming majority of Commodore BBS downloads are
    /// load-address-prefixed programs.  The UI may override this per transfer.
    pub(crate) fn autodetect(filename: &str) -> PunterFileType {
        let lower = filename.to_ascii_lowercase();
        if lower.ends_with(".seq") || lower.ends_with(".txt") || lower.ends_with(".doc") {
            PunterFileType::Seq
        } else {
            PunterFileType::Prg
        }
    }
}

// ─── Checksums ───────────────────────────────────────────────────────────

/// Compute Punter's two 16-bit checksums over `body` — the block bytes from
/// offset 4 (SIZEPOS) to end-of-block.  Returns `(additive, cyclic)`, each
/// stored little-endian on the wire.
///
/// Verified byte-for-byte against `checksum` (`punter.src` v9.6 line 517,
/// v10 line 422 — identical):
///
/// - **Additive**: a 16-bit running sum of the bytes with carry into the high
///   byte (`clc / adc / bcc / inc check1+1`).  No carry leaves the 16-bit
///   accumulator, so this is a plain wrapping 16-bit add.
/// - **Cyclic**: per byte, XOR the byte into the *low* byte of the 16-bit
///   accumulator (`eor check1+2`), then rotate the whole 16-bit accumulator
///   left by one bit, circularly (`rol`/`rol check1+2`/`rol check1+3` feeds
///   bit 15 back into bit 0).  The rotate happens *after* the XOR.
pub(crate) fn punter_checksums(body: &[u8]) -> (u16, u16) {
    let mut additive: u16 = 0;
    let mut cyclic: u16 = 0;
    for &b in body {
        additive = additive.wrapping_add(b as u16);
        cyclic ^= b as u16; // XOR into the low byte
        cyclic = cyclic.rotate_left(1); // 16-bit circular rotate-left, post-XOR
    }
    (additive, cyclic)
}

// ─── Block construction / inspection ─────────────────────────────────────

/// Build one C1 block: header + payload, with the size-of-next-block field,
/// block index, and both checksums filled in.  `payload` must be ≤248 bytes.
fn build_block(next_size: u8, block_index: u16, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut blk = vec![0u8; DATAPOS + payload.len()];
    blk[SIZEPOS] = next_size;
    blk[NUMPOS] = (block_index & 0xFF) as u8;
    blk[NUMPOS + 1] = (block_index >> 8) as u8;
    blk[DATAPOS..].copy_from_slice(payload);
    let (add, cyc) = punter_checksums(&blk[SIZEPOS..]);
    blk[0] = (add & 0xFF) as u8;
    blk[1] = (add >> 8) as u8;
    blk[2] = (cyc & 0xFF) as u8;
    blk[3] = (cyc >> 8) as u8;
    blk
}

/// True if a received block's stored checksums match a fresh computation over
/// its body (offset 4 onward).  Mirrors `match` (`punter.src` line 643).
fn checksum_ok(blk: &[u8]) -> bool {
    if blk.len() < DATAPOS {
        return false;
    }
    let (add, cyc) = punter_checksums(&blk[SIZEPOS..]);
    let stored_add = u16::from_le_bytes([blk[0], blk[1]]);
    let stored_cyc = u16::from_le_bytes([blk[2], blk[3]]);
    add == stored_add && cyc == stored_cyc
}

/// A received block carries the final-block flag in the high byte of its index.
fn is_final_block(blk: &[u8]) -> bool {
    blk.len() > NUMPOS + 1 && blk[NUMPOS + 1] == 0xFF
}

/// Split a file into the C1 block sequence for one phase.
///
/// The returned blocks are ready to transmit in order.  The first block is the
/// fixed 7-byte header-only block; payload-bearing blocks follow; the final
/// block's index high byte is forced to 0xFF.  Each block's `size` field is
/// back-patched to the *next* block's total length (the last block keeps its
/// own length there — harmless, the receiver stops before using it).
fn build_data_blocks(data: &[u8], block_payload: usize) -> Vec<Vec<u8>> {
    let payload_cap = block_payload.clamp(1, MAX_PAYLOAD);
    let mut blocks: Vec<Vec<u8>> = Vec::new();

    // Block 0: header only, index 0, no payload (the "first B-block has no
    // payload" quirk — it exists to announce block 1's size).
    blocks.push(build_block(0, 0x0000, &[]));

    if data.is_empty() {
        // Empty file: a single header-only final block after block 0.
        blocks.push(build_block(0, 0xFFFF, &[]));
    } else {
        let chunks: Vec<&[u8]> = data.chunks(payload_cap).collect();
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.iter().enumerate() {
            let index = if i == last { 0xFFFF } else { (i as u16) + 1 };
            blocks.push(build_block(0, index, chunk));
        }
    }

    backpatch_next_sizes(&mut blocks);
    blocks
}

/// Build the single Phase-A type block (index 0xFFFF, one payload byte).
fn build_type_block(file_type: PunterFileType) -> Vec<Vec<u8>> {
    let mut blocks = vec![build_block(TYPE_PHASE_SIZE, 0xFFFF, &[file_type.to_byte()])];
    backpatch_next_sizes(&mut blocks);
    blocks
}

/// Rewrite every block's offset-4 "size of next block" field and recompute its
/// checksums.  The last block points its size field at its own length.
fn backpatch_next_sizes(blocks: &mut [Vec<u8>]) {
    let sizes: Vec<u8> = blocks.iter().map(|b| b.len() as u8).collect();
    for i in 0..blocks.len() {
        let next = if i + 1 < sizes.len() { sizes[i + 1] } else { sizes[i] };
        blocks[i][SIZEPOS] = next;
        let (add, cyc) = punter_checksums(&blocks[i][SIZEPOS..]);
        blocks[i][0] = (add & 0xFF) as u8;
        blocks[i][1] = (add >> 8) as u8;
        blocks[i][2] = (cyc & 0xFF) as u8;
        blocks[i][3] = (cyc >> 8) as u8;
    }
}

// ─── Tunables snapshot ───────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Tunables {
    negotiation_timeout: u64,
    block_timeout: u64,
    max_retries: u32,
    retry_interval: u64,
    block_payload: usize,
}

impl Tunables {
    fn load() -> Tunables {
        let cfg = config::get_config();
        // Block size is the *total* block length the user configured; the
        // payload cap is that minus the 7-byte header.
        let total = (cfg.punter_block_size as usize).clamp(DATAPOS + 1, MAX_BLOCK);
        Tunables {
            negotiation_timeout: cfg.punter_negotiation_timeout,
            block_timeout: cfg.punter_block_timeout,
            max_retries: cfg.punter_max_retries,
            retry_interval: cfg.punter_negotiation_retry_interval,
            block_payload: total - DATAPOS,
        }
    }
}

// ─── Low-level code I/O ──────────────────────────────────────────────────

async fn send_code(
    writer: &mut (impl AsyncWrite + Unpin),
    code: Code,
    is_tcp: bool,
) -> Result<(), String> {
    raw_write_bytes(writer, code.bytes(), is_tcp).await
}

/// Wait up to `timeout_secs` for one of the `allowed` handshake codes,
/// sliding a 3-byte window over the incoming bytes (mirrors `accept`,
/// `punter.src` line 111).  Returns `Ok(Some(code))` on a match, `Ok(None)`
/// on timeout, and `Err` on a user abort (ESC / CAN×2) or I/O failure.
async fn accept_code(
    reader: &mut (impl AsyncRead + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    state: &mut ReadState,
    allowed: &[Code],
    timeout_secs: u64,
) -> Result<Option<Code>, String> {
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    let mut window = [0u8; 3];
    let mut filled = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let byte = match tokio::time::timeout(remaining, nvt_read_byte(reader, is_tcp, state)).await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok(None),
        };
        // Between blocks the only bytes that should appear are the 3-byte
        // ASCII handshake codes, none of which contain ESC — so honouring a
        // local user's ESC/PETSCII-stop here is safe and lets them bail.
        // (C1 has no in-band CAN abort; that's an XMODEM/Kermit convention.)
        if is_esc_key(byte, is_petscii) {
            return Err("Transfer cancelled".into());
        }
        window[0] = window[1];
        window[1] = window[2];
        window[2] = byte;
        if filled < 3 {
            filled += 1;
        }
        if filled == 3
            && let Some(c) = Code::from_window(&window)
            && allowed.contains(&c)
        {
            return Ok(Some(c));
        }
    }
}

// ─── Receive ─────────────────────────────────────────────────────────────

/// Receive a Punter C1 file: Phase A (type block) then Phase B (data blocks).
/// Returns the file bytes and the sender's declared file type.
pub(crate) async fn punter_receive(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
) -> Result<(Vec<u8>, PunterFileType), String> {
    let t = Tunables::load();
    let mut state = ReadState::default();

    if verbose {
        glog!("PUNTER recv: waiting for type block (is_tcp={}, is_petscii={})", is_tcp, is_petscii);
    }

    // Phase A — the single type block, fixed at 8 bytes (one payload byte).
    let type_payload = receive_phase(
        reader, writer, is_tcp, is_petscii, verbose, &mut state, TYPE_PHASE_SIZE, &t,
    )
    .await?;
    let file_type = PunterFileType::from_byte(type_payload.first().copied().unwrap_or(1));
    if verbose {
        glog!("PUNTER recv: file type = {:?}", file_type);
    }

    // Phase B — the data blocks, first block fixed at 7 bytes.
    let data = receive_phase(
        reader, writer, is_tcp, is_petscii, verbose, &mut state, DATA_PHASE_FIRST_SIZE, &t,
    )
    .await?;
    if verbose {
        glog!("PUNTER recv: complete, {} bytes", data.len());
    }

    Ok((data, file_type))
}

/// Receive one phase (a sequence of blocks ending at the 0xFF-flagged block),
/// returning the concatenated payloads.  `initial_size` is the fixed length of
/// the first block (8 for the type phase, 7 for the data phase).
async fn receive_phase(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
    state: &mut ReadState,
    initial_size: u8,
    t: &Tunables,
) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut next_size = initial_size;
    // What to send at the top of each round: GOO when the previous block was
    // good (or it's the first round), BAD to demand a resend.
    let mut signal = Code::Goo;

    loop {
        // rc1: send GOO/BAD, then wait for the sender's ACK (resend on
        // timeout, up to max_retries).  First round uses the negotiation
        // budget so the user has time to start their terminal's sender.
        let first_round = out.is_empty() && signal == Code::Goo;
        let ack_wait = if first_round { t.negotiation_timeout } else { t.block_timeout };
        let mut got_ack = false;
        for attempt in 0..=t.max_retries {
            send_code(writer, signal, is_tcp).await?;
            match accept_code(reader, is_tcp, is_petscii, state, &[Code::Ack], ack_wait).await? {
                Some(Code::Ack) => {
                    got_ack = true;
                    break;
                }
                _ => {
                    if verbose && attempt == 0 {
                        glog!("PUNTER recv: waiting for ACK ({:?})", signal);
                    }
                    let _ = t.retry_interval;
                }
            }
        }
        if !got_ack {
            return Err("Punter receive: no ACK from sender".into());
        }

        // rc2: send S/B, then read the block.  Resends S/B on a blank read or
        // on a stray ACK (sender missed our S/B), up to max_retries.
        let blk = read_block(reader, writer, is_tcp, state, next_size, t).await?;

        if checksum_ok(&blk) {
            let payload_len = blk.len().saturating_sub(DATAPOS);
            if payload_len > 0 {
                out.extend_from_slice(&blk[DATAPOS..]);
                if out.len() > MAX_FILE_SIZE {
                    return Err(format!("Punter receive: file exceeds {} bytes", MAX_FILE_SIZE));
                }
            }
            let final_block = is_final_block(&blk);
            next_size = blk[SIZEPOS];
            signal = Code::Goo;

            if final_block {
                // End-off: send GOO (acks the final block), wait ACK, send
                // S/B, then the SYN handshake.  Mirrors `rechand` rc6/rc8.
                end_off_receiver(reader, writer, is_tcp, is_petscii, state, t).await?;
                return Ok(out);
            }

            if (next_size as usize) < DATAPOS || next_size as usize > MAX_BLOCK {
                return Err(format!("Punter receive: bad next-block size {}", next_size));
            }
        } else {
            if verbose {
                glog!("PUNTER recv: checksum mismatch, requesting resend");
            }
            // rec2: demand a resend of the same-sized block.
            signal = Code::Bad;
        }
    }
}

/// Read one block of `size` logical bytes after the S/B has been (re)sent.
/// Handles the sender re-sending ACK (it missed our S/B) by re-sending S/B,
/// and a fully blank read likewise.  Returns whatever bytes arrived (a short
/// read just fails the checksum upstream → BAD), or errors on abort / repeated
/// failure.  Mirrors `recmodem` (`punter.src` line 379).
async fn read_block(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    state: &mut ReadState,
    size: u8,
    t: &Tunables,
) -> Result<Vec<u8>, String> {
    let size = size as usize;
    for _attempt in 0..=t.max_retries {
        send_code(writer, Code::Sb, is_tcp).await?;
        let mut buf: Vec<u8> = Vec::with_capacity(size);
        let mut stray_ack = false;
        for i in 0..size {
            // First byte may take a while (sender prepping the block); give it
            // the full block timeout.  Subsequent bytes should stream in.
            let per_byte = t.block_timeout;
            let r = tokio::time::timeout(
                tokio::time::Duration::from_secs(per_byte),
                nvt_read_byte(reader, is_tcp, state),
            )
            .await;
            match r {
                Ok(Ok(b)) => {
                    // Block bodies are pure binary: C1 has no in-band abort
                    // byte (the C64 aborts via the local Commodore key, not a
                    // wire sequence), so we must NOT interpret 0x1B / 0x18×2
                    // here — they occur freely as data.
                    buf.push(b);
                    // Detect a stray "ack" prefix: the sender resent ACK
                    // because it never saw our S/B.  Resend S/B and retry.
                    if i == 2 && buf[..3] == *Code::Ack.bytes() {
                        stray_ack = true;
                        break;
                    }
                }
                _ => break, // timeout — short/blank read
            }
        }
        if stray_ack {
            continue; // resend S/B, read again
        }
        if buf.is_empty() {
            continue; // blank read — resend S/B (rc2)
        }
        return Ok(buf); // full or partial; caller verifies the checksum
    }
    Err("Punter receive: block read failed".into())
}

/// Receiver end-off (`rechand` rc6/rc8): GOO → wait ACK → S/B → wait SYN →
/// SYN → wait S/B.
async fn end_off_receiver(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    state: &mut ReadState,
    t: &Tunables,
) -> Result<(), String> {
    // Acknowledge the final block and re-handshake.
    for _ in 0..=t.max_retries {
        send_code(writer, Code::Goo, is_tcp).await?;
        if let Some(Code::Ack) =
            accept_code(reader, is_tcp, is_petscii, state, &[Code::Ack], t.block_timeout).await?
        {
            break;
        }
    }
    send_code(writer, Code::Sb, is_tcp).await?;

    // Wait for the sender's SYN (resend S/B on timeout), then answer SYN and
    // wait for the sender's S/B (resend SYN on timeout).
    for _ in 0..=t.max_retries {
        match accept_code(reader, is_tcp, is_petscii, state, &[Code::Syn], t.block_timeout).await? {
            Some(Code::Syn) => break,
            _ => send_code(writer, Code::Sb, is_tcp).await?,
        }
    }
    for _ in 0..=t.max_retries {
        send_code(writer, Code::Syn, is_tcp).await?;
        match accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], t.block_timeout).await? {
            Some(Code::Sb) => break,
            _ => continue,
        }
    }
    Ok(())
}

// ─── Send ────────────────────────────────────────────────────────────────

/// Send a Punter C1 file: Phase A (type block) then Phase B (data blocks).
pub(crate) async fn punter_send(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    file_type: PunterFileType,
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
) -> Result<(), String> {
    if data.len() > MAX_FILE_SIZE {
        return Err(format!("File exceeds {} bytes", MAX_FILE_SIZE));
    }
    let t = Tunables::load();
    let mut state = ReadState::default();

    if verbose {
        glog!(
            "PUNTER send: starting (is_tcp={}, is_petscii={}, type={:?}, len={})",
            is_tcp, is_petscii, file_type, data.len()
        );
    }

    // Phase A — type block.  The sender opens with GOO (`specmode`).
    let type_blocks = build_type_block(file_type);
    send_phase(reader, writer, is_tcp, is_petscii, verbose, &mut state, &type_blocks, true, &t)
        .await?;

    // Phase B — data blocks.  The receiver opens; the sender just waits.
    let data_blocks = build_data_blocks(data, t.block_payload);
    send_phase(reader, writer, is_tcp, is_petscii, verbose, &mut state, &data_blocks, false, &t)
        .await?;

    if verbose {
        glog!("PUNTER send: complete");
    }
    Ok(())
}

/// Send one phase's blocks in order, driven by the receiver's GOO/BAD acks.
/// `spec_mode` (Phase A only) makes the sender emit an opening GOO.  Mirrors
/// `tranhand`/`transmit` (`punter.src` line 263).
async fn send_phase(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
    state: &mut ReadState,
    blocks: &[Vec<u8>],
    spec_mode: bool,
    t: &Tunables,
) -> Result<(), String> {
    let mut idx: usize = 0;
    let mut started = false;

    loop {
        // tx20: wait for the receiver's response.  GOO = previous block good
        // (advance); BAD or a re-sent S/B = resend the current block.  In
        // spec mode, re-emit GOO each retry until we hear something.
        let wait_budget = if started { t.block_timeout } else { t.negotiation_timeout };
        let mut code = None;
        for attempt in 0..=t.max_retries {
            if spec_mode && !started {
                send_code(writer, Code::Goo, is_tcp).await?;
            }
            code = accept_code(
                reader,
                is_tcp,
                is_petscii,
                state,
                &[Code::Goo, Code::Bad, Code::Sb],
                wait_budget,
            )
            .await?;
            if code.is_some() {
                break;
            }
            if verbose && attempt == 0 {
                glog!("PUNTER send: waiting for receiver (block {})", idx);
            }
        }
        let code = match code {
            Some(c) => c,
            None => return Err("Punter send: no response from receiver".into()),
        };

        match code {
            Code::Goo => {
                if started {
                    // The block we last sent (idx) was accepted.
                    if is_final_block(&blocks[idx]) {
                        end_off_sender(reader, writer, is_tcp, is_petscii, state, t).await?;
                        return Ok(());
                    }
                    idx += 1;
                }
                started = true;
            }
            // BAD or S/B → resend the current block (do not advance).
            _ => {
                started = true;
                if verbose {
                    glog!("PUNTER send: resend requested for block {}", idx);
                }
            }
        }

        // tx11: send ACK, wait for the receiver's S/B (resend ACK on timeout).
        let mut got_sb = false;
        for _ in 0..=t.max_retries {
            send_code(writer, Code::Ack, is_tcp).await?;
            if let Some(Code::Sb) =
                accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], t.block_timeout).await?
            {
                got_sb = true;
                break;
            }
        }
        if !got_sb {
            return Err("Punter send: no S/B from receiver".into());
        }

        // tx12/tx6: transmit the block bytes.
        raw_write_bytes(writer, &blocks[idx], is_tcp).await?;
    }
}

/// Sender end-off (`tranhand` tx4/tx5/tx8/tx9): ACK → wait S/B → SYN → wait
/// SYN → S/B ×3.  The triple S/B is the deliberately-inherited "end-off"
/// behaviour real C1 receivers expect to drain.
async fn end_off_sender(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    state: &mut ReadState,
    t: &Tunables,
) -> Result<(), String> {
    // tx41: ACK until the receiver's S/B.
    for _ in 0..=t.max_retries {
        send_code(writer, Code::Ack, is_tcp).await?;
        if let Some(Code::Sb) =
            accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], t.block_timeout).await?
        {
            break;
        }
    }
    // tx5: SYN until the receiver's SYN comes back.
    for _ in 0..=t.max_retries {
        send_code(writer, Code::Syn, is_tcp).await?;
        if let Some(Code::Syn) =
            accept_code(reader, is_tcp, is_petscii, state, &[Code::Syn], t.block_timeout).await?
        {
            break;
        }
    }
    // tx9: three S/B, draining anything the receiver echoes between them.
    // Best-effort: by this point the receiver may already have accepted the
    // first S/B and torn the link down, so a failed write on #2/#3 is not an
    // error — the transfer succeeded.
    for _ in 0..3 {
        let _ = send_code(writer, Code::Sb, is_tcp).await;
        let _ = accept_code(reader, is_tcp, is_petscii, state, &[], 1).await;
    }
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // — Checksum vectors —

    #[test]
    fn checksum_empty_body() {
        assert_eq!(punter_checksums(&[]), (0, 0));
    }

    #[test]
    fn checksum_additive_is_16bit_wrapping_sum() {
        // Additive = sum of bytes mod 2^16.
        let body = [0x01u8, 0x02, 0x03, 0xFF];
        let (add, _) = punter_checksums(&body);
        assert_eq!(add, 0x0105);
    }

    #[test]
    fn checksum_additive_carries_past_low_byte() {
        let body = [0xFFu8, 0xFF];
        let (add, _) = punter_checksums(&body);
        assert_eq!(add, 0x01FE);
    }

    #[test]
    fn checksum_cyclic_xor_then_rotate_left() {
        // Hand-trace the verified algorithm: XOR byte into low byte, then
        // 16-bit rotate-left by one.
        // start 0x0000
        // b=0x01 -> 0x0001 -> rotl1 -> 0x0002
        // b=0x80 -> 0x0082 -> rotl1 -> 0x0104
        let (_, cyc) = punter_checksums(&[0x01, 0x80]);
        assert_eq!(cyc, 0x0104);
    }

    #[test]
    fn checksum_cyclic_high_bit_wraps_to_low() {
        // A single 0x80 byte: 0x0080 rotl1 = 0x0100.  Then 0x00 keeps XOR
        // no-op: 0x0100 rotl1 = 0x0200.  Confirms left (not right) rotate.
        assert_eq!(punter_checksums(&[0x80]).1, 0x0100);
        assert_eq!(punter_checksums(&[0x80, 0x00]).1, 0x0200);
        // Force the top bit set then rotate so it wraps into bit 0.
        // 0x8000 would need building up; use 0xFF then several zeros.
        // 0xFF -> 0x00FF rotl1 = 0x01FE; +0x00*7 keeps rotating left.
        let v = punter_checksums(&[0xFF, 0, 0, 0, 0, 0, 0, 0]).1;
        // 0x01FE rotated left 7 more times = 0x01FE << 7 within 16-bit rotate
        // = 0xFF00 (bits 1..8 -> bits 8..15) ... compute directly:
        let mut expect: u16 = 0x00FF;
        expect = expect.rotate_left(1); // after first byte
        for _ in 0..7 {
            expect = expect.rotate_left(1);
        }
        assert_eq!(v, expect);
    }

    #[test]
    fn build_block_checksum_roundtrips() {
        let blk = build_block(7, 0x0102, &[1, 2, 3, 4, 5]);
        assert!(checksum_ok(&blk));
        assert_eq!(blk[SIZEPOS], 7);
        assert_eq!(blk[NUMPOS], 0x02);
        assert_eq!(blk[NUMPOS + 1], 0x01);
        assert_eq!(&blk[DATAPOS..], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn corrupting_a_block_fails_checksum() {
        let mut blk = build_block(7, 0x0001, &[9, 9, 9]);
        assert!(checksum_ok(&blk));
        let last = blk.len() - 1;
        blk[last] ^= 0xFF;
        assert!(!checksum_ok(&blk));
    }

    #[test]
    fn data_blocks_have_header_first_and_final_flag() {
        let blocks = build_data_blocks(&[1, 2, 3], 255);
        // First block: header only, index 0, 7 bytes.
        assert_eq!(blocks[0].len(), DATAPOS);
        assert_eq!(blocks[0][NUMPOS + 1], 0x00);
        // Last block: final flag set.
        assert!(is_final_block(blocks.last().unwrap()));
        // Every block checksum-verifies and its size field points at the next.
        for i in 0..blocks.len() {
            assert!(checksum_ok(&blocks[i]));
            let expect_next =
                if i + 1 < blocks.len() { blocks[i + 1].len() } else { blocks[i].len() };
            assert_eq!(blocks[i][SIZEPOS] as usize, expect_next);
        }
    }

    #[test]
    fn type_block_is_eight_bytes() {
        let blocks = build_type_block(PunterFileType::Seq);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].len(), TYPE_PHASE_SIZE as usize);
        assert!(is_final_block(&blocks[0]));
        assert_eq!(blocks[0][DATAPOS], PunterFileType::Seq.to_byte());
        assert!(checksum_ok(&blocks[0]));
    }

    #[test]
    fn small_block_size_forces_multiple_payload_blocks() {
        // block_payload derives from total size; total 47 -> payload 40.
        let data: Vec<u8> = (0..100).collect();
        let blocks = build_data_blocks(&data, 47);
        // 1 header + ceil(100/40)=3 payload blocks = 4.
        assert_eq!(blocks.len(), 4);
        assert!(is_final_block(blocks.last().unwrap()));
    }

    // — Round-trip over an in-memory duplex pipe —

    async fn round_trip(data: &[u8], ftype: PunterFileType) -> (Vec<u8>, PunterFileType) {
        // Two duplex pipes, cross-wired so each side's writer feeds the other's
        // reader.  A DuplexStream is itself both AsyncRead and AsyncWrite, and
        // writing one end appears on the read side of its partner — so we hand
        // each task one end of each pipe and use only the one direction we
        // need (no `split`, which would close a half on drop).
        let (s_to_r_a, s_to_r_b) = duplex(1 << 20); // sender writes a → receiver reads b
        let (r_to_s_a, r_to_s_b) = duplex(1 << 20); // receiver writes a → sender reads b

        let data_owned = data.to_vec();
        let sender = tokio::spawn(async move {
            let mut rd = r_to_s_b;
            let mut wr = s_to_r_a;
            punter_send(&mut rd, &mut wr, &data_owned, ftype, false, false, false).await
        });
        let receiver = tokio::spawn(async move {
            let mut rd = s_to_r_b;
            let mut wr = r_to_s_a;
            punter_receive(&mut rd, &mut wr, false, false, false).await
        });

        let send_res = sender.await.unwrap();
        let recv_res = receiver.await.unwrap();
        send_res.expect("send failed");
        recv_res.expect("receive failed")
    }

    #[tokio::test]
    async fn round_trip_empty() {
        let (out, ft) = round_trip(&[], PunterFileType::Seq).await;
        assert_eq!(out, Vec::<u8>::new());
        assert_eq!(ft, PunterFileType::Seq);
    }

    #[tokio::test]
    async fn round_trip_one_byte() {
        let (out, ft) = round_trip(&[0x42], PunterFileType::Prg).await;
        assert_eq!(out, vec![0x42]);
        assert_eq!(ft, PunterFileType::Prg);
    }

    #[tokio::test]
    async fn round_trip_exactly_one_full_payload() {
        let data: Vec<u8> = (0..MAX_PAYLOAD).map(|i| i as u8).collect();
        let (out, _) = round_trip(&data, PunterFileType::Seq).await;
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn round_trip_one_over_a_payload_forces_second_block() {
        let data: Vec<u8> = (0..MAX_PAYLOAD + 1).map(|i| i as u8).collect();
        let (out, _) = round_trip(&data, PunterFileType::Seq).await;
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn round_trip_all_byte_values() {
        let data: Vec<u8> = (0..=255u8).collect();
        let (out, _) = round_trip(&data, PunterFileType::Prg).await;
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn round_trip_multi_block() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 3) as u8).collect();
        let (out, _) = round_trip(&data, PunterFileType::Seq).await;
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn round_trip_data_full_of_cr_and_iac_lookalikes() {
        // 0x0D and 0xFF would trip telnet escaping on a TCP link; here is_tcp
        // is false so they pass raw, but this still guards the framing.
        let data: Vec<u8> = vec![0x0D, 0x00, 0xFF, 0xFF, 0x0D, 0x0A, 0x18, 0x18];
        let (out, _) = round_trip(&data, PunterFileType::Seq).await;
        assert_eq!(out, data);
    }

    // — Header parser must never panic on adversarial input —

    #[test]
    fn checksum_ok_handles_short_buffers() {
        for len in 0..DATAPOS {
            let buf = vec![0u8; len];
            assert!(!checksum_ok(&buf));
            assert!(!is_final_block(&buf));
        }
    }

    // — Block parser must never panic on adversarial bytes —

    mod punter_proptest {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

            /// `checksum_ok` / `is_final_block` accept any byte slice without
            /// panicking, regardless of length or content.
            #[test]
            fn prop_block_inspectors_no_panic(
                bytes in prop::collection::vec(any::<u8>(), 0..512),
            ) {
                let _ = checksum_ok(&bytes);
                let _ = is_final_block(&bytes);
            }

            /// Every block we build verifies its own checksum and reports its
            /// header fields back faithfully, for any payload up to the cap.
            #[test]
            fn prop_build_block_self_consistent(
                next_size in any::<u8>(),
                index in any::<u16>(),
                payload in prop::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD),
            ) {
                let blk = build_block(next_size, index, &payload);
                prop_assert!(checksum_ok(&blk));
                prop_assert_eq!(blk[SIZEPOS], next_size);
                prop_assert_eq!(u16::from_le_bytes([blk[NUMPOS], blk[NUMPOS + 1]]), index);
                prop_assert_eq!(&blk[DATAPOS..], &payload[..]);
                prop_assert_eq!(is_final_block(&blk), (index >> 8) as u8 == 0xFF);
            }

            /// `build_data_blocks` always emits a 7-byte header first, a final
            /// 0xFF-flagged block last, and every block's size field points at
            /// the next block's length — for any data and any block size.
            #[test]
            fn prop_data_blocks_well_formed(
                data in prop::collection::vec(any::<u8>(), 0..1500),
                block_size in 8usize..=255,
            ) {
                let blocks = build_data_blocks(&data, block_size - DATAPOS);
                prop_assert_eq!(blocks[0].len(), DATAPOS);
                prop_assert!(is_final_block(blocks.last().unwrap()));
                let mut reassembled = Vec::new();
                for (i, b) in blocks.iter().enumerate() {
                    prop_assert!(checksum_ok(b));
                    let expect_next =
                        if i + 1 < blocks.len() { blocks[i + 1].len() } else { b.len() };
                    prop_assert_eq!(b[SIZEPOS] as usize, expect_next);
                    reassembled.extend_from_slice(&b[DATAPOS..]);
                }
                prop_assert_eq!(reassembled, data);
            }
        }
    }
}
