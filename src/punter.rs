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
//! handshake, and the sender finishes by transmitting `S/B` three times.  This
//! is a *sender* obligation, enforced by `end_off_sender`: a real CCGMS
//! receiver stalls if fewer than three `S/B` arrive after the final block.
//! Our own receiver does not mirror that with fixed drain-reads — `end_off_
//! receiver` runs a tolerant best-effort re-handshake (ACK the final block,
//! exchange SYN/`S/B`) and any extra `S/B` the peer sends are harmlessly slid
//! past by `accept_code`'s resync window.  (Do not "fix" the receiver to add
//! blind drain-reads: they would swallow the next phase's opening signal.)
//!
//! Critically, after the type block CCGMS sends `GOO` **twice** — the block-ack
//! `GOO` (inside its `recv_block`) and a second `GOO` before the `S/B` — and
//! each needs its own `ACK`.  The sender's end-off therefore re-sends `ACK` on
//! a short cadence until the `S/B` arrives (see `end_off_sender`); a single
//! slow `ACK` strands CCGMS looping `GOO` forever (the "Downloading…" hang).

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

/// Upper bound on accepted non-final blocks that carry no payload.  A
/// conformant C1 sender emits exactly one such block per phase — block 0,
/// the header-only opener that only announces block 1's size (see
/// [`build_data_blocks`]); every later non-final block carries payload.  A
/// peer that instead streams valid-checksum, non-final, empty blocks would
/// otherwise spin `receive_phase` forever: `out` never grows (so
/// `MAX_FILE_SIZE` never trips) and the checksum passes (so `max_bad_rounds`
/// never trips).  The cushion above 1 tolerates trivial retransmit oddities
/// while still bounding that path.
const MAX_EMPTY_NONFINAL_BLOCKS: u32 = 4;

/// Hard cap on a transferred file, shared with the other protocols via
/// `tnio::MAX_FILE_SIZE` so all of XMODEM/YMODEM/ZMODEM/Kermit/Punter agree.
const MAX_FILE_SIZE: usize = crate::tnio::MAX_FILE_SIZE as usize;

// ─── Handshake codes ─────────────────────────────────────────────────────

/// The five three-byte handshake tokens.  Values from the packed Novaterm
/// `codes` string (`punter.src` v9.6 line 98).
///
/// **On the wire these are UPPERCASE** (`GOO`/`BAD`/`ACK`/`S/B`/`SYN`).  The
/// Novaterm source renders them as `.asc "goobadacks/bsyn"`, but a real C64
/// assembles that in the default uppercase/graphics PETSCII charset, so the
/// bytes that actually leave the machine are 0x47 0x4F 0x4F … — confirmed by
/// capturing CCGMS, which transmits `0x47 'G' 0x4F 'O' 0x4F 'O'`.  We send
/// uppercase so real C64 peers recognise us, and [`Code::from_window`] matches
/// case-insensitively so a peer that happens to send lowercase still works.
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
            Code::Goo => b"GOO",
            Code::Bad => b"BAD",
            Code::Ack => b"ACK",
            Code::Sb => b"S/B",
            Code::Syn => b"SYN",
        }
    }

    /// Recognise a code in the sliding window, case-insensitively.  Real C64
    /// peers send uppercase; we fold to uppercase before comparing so a
    /// lowercase variant is still accepted.  The `/` in `S/B` (0x2F) is
    /// unaffected by case folding.
    fn from_window(w: &[u8; 3]) -> Option<Code> {
        let up = [
            w[0].to_ascii_uppercase(),
            w[1].to_ascii_uppercase(),
            w[2].to_ascii_uppercase(),
        ];
        [Code::Goo, Code::Bad, Code::Ack, Code::Sb, Code::Syn]
            .into_iter()
            .find(|c| &up == c.bytes())
    }
}

// ─── File type ───────────────────────────────────────────────────────────

/// The one-byte file-type carried by the C1 type block (Phase A).  Matches
/// Novaterm's directory-entry table (`api/head.src` lines 423-426):
///
/// ```text
/// 0 = PRG     load-address-prefixed Commodore program
/// 1 = SEQ     flat sequential file
/// 2 = USR     flat user-defined file
/// 3 = ---     unknown / none
/// ```
///
/// CBM filesystems carry this in the directory entry; on Linux we don't have
/// that, so to preserve the round-trip we append the matching extension on
/// receive when the saved filename lacks one (`.prg` / `.seq` / `.usr`).
/// `Unknown` is left without a suffix — the operator named the file
/// explicitly and we don't second-guess that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PunterFileType {
    Prg,
    Seq,
    Usr,
    Unknown,
}

impl PunterFileType {
    fn to_byte(self) -> u8 {
        match self {
            PunterFileType::Prg => 0,
            PunterFileType::Seq => 1,
            PunterFileType::Usr => 2,
            PunterFileType::Unknown => 3,
        }
    }

    /// Map a Phase-A type byte back to the CBM-aligned enum.  Bytes outside
    /// the documented 0..=3 range are treated as `Unknown` rather than
    /// silently coerced to SEQ.
    fn from_byte(b: u8) -> PunterFileType {
        match b {
            0 => PunterFileType::Prg,
            1 => PunterFileType::Seq,
            2 => PunterFileType::Usr,
            _ => PunterFileType::Unknown,
        }
    }

    /// File-extension stand-in for the CBM directory-entry type, used by the
    /// receiver to preserve the declared type when saving to a Linux
    /// filesystem.  Returns `None` for `Unknown` so the operator's chosen
    /// filename is left as-is.
    pub(crate) fn extension(self) -> Option<&'static str> {
        match self {
            PunterFileType::Prg => Some("prg"),
            PunterFileType::Seq => Some("seq"),
            PunterFileType::Usr => Some("usr"),
            PunterFileType::Unknown => None,
        }
    }

    /// Auto-detect the type to declare for an outbound file.  Text-flavoured
    /// extensions (`.seq`/`.txt`/`.doc`) are SEQ; `.usr` is USR; everything
    /// else defaults to PRG, since the overwhelming majority of Commodore
    /// BBS downloads are load-address-prefixed programs.  The UI may
    /// override this per transfer.
    pub(crate) fn autodetect(filename: &str) -> PunterFileType {
        let lower = filename.to_ascii_lowercase();
        if lower.ends_with(".seq") || lower.ends_with(".txt") || lower.ends_with(".doc") {
            PunterFileType::Seq
        } else if lower.ends_with(".usr") {
            PunterFileType::Usr
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

/// Largest non-final block index we can safely emit.  Indices 0xFF00..=0xFFFF
/// all set the high-byte flag that `is_final_block` reads, so the final block
/// uses 0xFFFF and intermediate data blocks must stay strictly below 0xFF00.
const MAX_DATA_BLOCK_INDEX: u16 = 0xFEFF;

/// Split a file into the C1 block sequence for one phase.
///
/// The returned blocks are ready to transmit in order.  The first block is the
/// fixed 7-byte header-only block; payload-bearing blocks follow; the final
/// block's index high byte is forced to 0xFF.  Each block's `size` field is
/// back-patched to the *next* block's total length (the last block keeps its
/// own length there — harmless, the receiver stops before using it).
///
/// Returns an error if the file would require more non-final data blocks than
/// the 16-bit block-index field can address without colliding with the
/// final-block flag (high byte 0xFF) — e.g. a small `block_payload` and a
/// many-megabyte file.  This guards the receiver from a silent truncation
/// where an intermediate block's index lands in 0xFF00..=0xFFFE and is
/// mistaken for the end of the transfer.
fn build_data_blocks(data: &[u8], block_payload: usize) -> Result<Vec<Vec<u8>>, String> {
    let payload_cap = block_payload.clamp(1, MAX_PAYLOAD);
    let mut blocks: Vec<Vec<u8>> = Vec::new();

    // Block 0: header only, index 0, no payload (the "first B-block has no
    // payload" quirk — it exists to announce block 1's size).
    blocks.push(build_block(0, 0x0000, &[]));

    if data.is_empty() {
        // Empty file: a single header-only final block after block 0.
        blocks.push(build_block(0, 0xFFFF, &[]));
    } else {
        let chunk_count = data.len().div_ceil(payload_cap);
        if chunk_count.saturating_sub(1) > MAX_DATA_BLOCK_INDEX as usize {
            return Err(format!(
                "Punter send: file too large for block payload {} ({} chunks exceeds {} addressable blocks)",
                payload_cap,
                chunk_count,
                MAX_DATA_BLOCK_INDEX as usize + 1,
            ));
        }
        let chunks: Vec<&[u8]> = data.chunks(payload_cap).collect();
        let last = chunks.len() - 1;
        for (i, chunk) in chunks.iter().enumerate() {
            let index = if i == last { 0xFFFF } else { (i as u16) + 1 };
            blocks.push(build_block(0, index, chunk));
        }
    }

    backpatch_next_sizes(&mut blocks);
    Ok(blocks)
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
    /// Per-stage handshake-code / block re-send count (read_block S/B resends,
    /// end-off ACK/SYN/S-B resends).  The recovery action for a *missed* code.
    max_retries: u32,
    /// Consecutive corrupt-block resend rounds tolerated before giving up.
    /// Deliberately decoupled from (and larger than) `max_retries`: a real C64
    /// peer never caps these — it re-requests a block until it arrives clean or
    /// the user aborts at the keyboard — and C1 has no in-band abort, so a low
    /// cap makes the gateway quit first and strand the peer.  See
    /// `receive_phase`/`send_phase`.
    max_bad_rounds: u32,
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
            max_bad_rounds: cfg.punter_max_bad_rounds,
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
    verbose: bool,
) -> Result<(), String> {
    if verbose {
        glog!("PUNTER tx code: {:?}", code);
    }
    raw_write_bytes(writer, code.bytes(), is_tcp).await
}

/// Wait up to `timeout_secs` for one of the `allowed` handshake codes,
/// sliding a 3-byte window over the incoming bytes (mirrors `accept`,
/// `punter.src` line 111).  Returns `Ok(Some(code))` on a match, `Ok(None)`
/// on timeout, and `Err` on a user abort (ESC / CAN×2) or I/O failure.
#[allow(clippy::too_many_arguments)]
async fn accept_code(
    reader: &mut (impl AsyncRead + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    state: &mut ReadState,
    allowed: &[Code],
    timeout_secs: u64,
    verbose: bool,
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
        // Surface every byte that arrives during a handshake wait.  These
        // windows only ever carry 3-byte ASCII codes, so the volume is tiny
        // — and when a real peer is sending something we don't recognise
        // (e.g. a case/charset mismatch in the codes), this is the only way
        // to see it.  Printable bytes show their char; others show hex.
        if verbose {
            let b = byte;
            if b.is_ascii_graphic() {
                glog!("PUNTER rx byte: 0x{:02X} '{}'", b, b as char);
            } else {
                glog!("PUNTER rx byte: 0x{:02X}", b);
            }
        }
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
            if verbose {
                glog!("PUNTER rx code: {:?}", c);
            }
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
    // Phase A is fixed at TYPE_PHASE_SIZE = 8 bytes (header + one type byte),
    // so a missing payload byte means the negotiation was malformed.  Map it
    // to `Unknown` rather than silently defaulting to a real type.
    let file_type =
        PunterFileType::from_byte(type_payload.first().copied().unwrap_or(3));
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
#[allow(clippy::too_many_arguments)]
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
    // Non-final blocks accepted with an empty payload.  Bounded by
    // MAX_EMPTY_NONFINAL_BLOCKS so a peer streaming valid empty blocks can't
    // spin this loop forever (they neither grow `out` nor fail the checksum).
    let mut empty_nonfinal_blocks: u32 = 0;
    // What to send at the top of each round: GOO when the previous block was
    // good (or it's the first round), BAD to demand a resend.
    let mut signal = Code::Goo;
    // Count consecutive BAD rounds against the SAME logical block so a peer
    // that keeps shipping corrupt bodies can't loop forever.  Reset each
    // time we accept a good block.
    let mut bad_rounds: u32 = 0;

    loop {
        // rc1: send GOO/BAD, then wait for the sender's ACK.  Re-send the
        // signal on a SHORT cadence rather than blocking the whole budget on a
        // single read: at a phase boundary the C1 sender is briefly draining
        // its end-off handshake (`tranhand` tx9 sends three S/B and reads-and-
        // discards between them) and will swallow our first signal, so we must
        // re-probe promptly to resync.  This mirrors Novaterm `rc1`, which
        // resends GOO every `accept` timeout (a short per-attempt wait, looped
        // via `codecyc`) instead of waiting once for a long window.
        //
        // The very first contact of the transfer gets the full negotiation
        // budget (the user may need a moment to start their terminal's sender);
        // mid-transfer it is one block timeout.  Either way we re-probe every
        // `retry_interval` seconds until the budget is spent.
        let first_round = out.is_empty() && signal == Code::Goo;
        let total_budget = if first_round { t.negotiation_timeout } else { t.block_timeout };
        let probe = total_budget.min(t.retry_interval.max(1));
        let attempts = total_budget.div_ceil(probe.max(1)).max(1);
        let mut got_ack = false;
        for attempt in 0..attempts {
            send_code(writer, signal, is_tcp, verbose).await?;
            match accept_code(reader, is_tcp, is_petscii, state, &[Code::Ack], probe, verbose).await? {
                Some(Code::Ack) => {
                    got_ack = true;
                    break;
                }
                _ => {
                    if verbose && attempt == 0 {
                        glog!("PUNTER recv: waiting for ACK ({:?})", signal);
                    }
                }
            }
        }
        if !got_ack {
            return Err("Punter receive: no ACK from sender".into());
        }

        // rc2: send S/B, then read the block.  Resends S/B on a blank read or
        // on a stray ACK (sender missed our S/B), up to max_retries.
        let blk = read_block(reader, writer, is_tcp, state, next_size, t, verbose).await?;

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
            bad_rounds = 0;

            // Bound the one otherwise-unbounded accept path: a non-final block
            // that carries no payload.  Exactly one is legitimate (block 0);
            // a peer streaming more is malfunctioning or hostile.
            if payload_len == 0 && !final_block {
                empty_nonfinal_blocks += 1;
                if empty_nonfinal_blocks > MAX_EMPTY_NONFINAL_BLOCKS {
                    return Err(format!(
                        "Punter receive: {} empty non-final blocks, giving up",
                        empty_nonfinal_blocks
                    ));
                }
            }

            if final_block {
                // End-off: send GOO (acks the final block), wait ACK, send
                // S/B, then the SYN handshake.  Mirrors `rechand` rc6/rc8.
                end_off_receiver(reader, writer, is_tcp, is_petscii, verbose, state, t).await?;
                return Ok(out);
            }

            if (next_size as usize) < DATAPOS || next_size as usize > MAX_BLOCK {
                return Err(format!("Punter receive: bad next-block size {}", next_size));
            }
        } else {
            if verbose {
                glog!("PUNTER recv: checksum mismatch, requesting resend");
            }
            // rec2: demand a resend of the same-sized block.  A real C1 sender
            // (`transmit`/`tranhand`) re-sends a block as many times as we BAD
            // it, with no outer cap, so we tolerate up to `max_bad_rounds`
            // (well above `max_retries`) before giving up — otherwise the
            // gateway quits first and, since C1 has no in-band abort, leaves
            // the C64 sender spinning.
            signal = Code::Bad;
            bad_rounds = bad_rounds.saturating_add(1);
            if bad_rounds > t.max_bad_rounds {
                return Err(format!(
                    "Punter receive: {} consecutive bad blocks, giving up",
                    bad_rounds
                ));
            }
        }
    }
}

/// Read one block of `size` logical bytes after the S/B has been (re)sent.
/// Handles the sender re-sending ACK (it missed our S/B) by re-sending S/B,
/// and a fully blank read likewise.  Returns whatever bytes arrived (a short
/// read just fails the checksum upstream → BAD), or errors on abort / repeated
/// failure.  Mirrors `recmodem` (`punter.src` line 379).
///
/// The per-byte budget is a short window — `min(block_timeout, retry_interval)`
/// — applied uniformly to every byte, mirroring `recmodem`, which `rcm5` clears
/// and re-arms with the *same* short timeout before every character (it does
/// not grant a fresh full block_timeout per byte).  Once a block is flowing,
/// bytes arrive back-to-back; a gap means a dropped byte, and we want to detect
/// it in a few seconds (→ BAD → resend), not sit silent for the full
/// `block_timeout` (which, used per-byte, made a single mid-block drop look
/// like a ~20 s hang).  The window is still generous — seconds-per-byte is
/// enormous slack even at 300 baud — so slow-but-steady links are unaffected.
/// It doubles as the first-byte wait so a peer that missed our S/B is
/// re-prompted promptly.
///
/// Unlike the handshake waits (which bound total time via a probe budget), the
/// S/B re-send here is **count-based** (`max_retries`): each re-send is the
/// recovery action for a stray ACK or a blank read, and stray-ACK recovery
/// alone needs at least two passes (one to consume the "ack", one to read the
/// resent block) — so the loop count must not collapse with the time budget
/// (it would, e.g., whenever `retry_interval >= block_timeout`).  The dead-peer
/// bound is therefore ~`max_retries × byte_wait`; the per-byte `byte_wait`
/// preserves slow-link tolerance for a block already arriving.  The first
/// missing byte ends the read; a short buffer simply fails the checksum
/// upstream → BAD → resend.
///
/// Cancel-safety: the `byte_wait` timeout drops the in-flight `nvt_read_byte`
/// future on expiry.  That is safe for `ReadState` — `pushback` is read
/// synchronously at entry, so a dropped future never tears it, and a 1-byte
/// `read_exact` can't lose a partial prefix.  The one residual is TCP-only:
/// `raw_read_byte` makes two sequential reads for an `IAC IAC` escape, so a
/// timeout landing *between* those two reads consumes but loses the first
/// byte (the `IAC`).  This requires a multi-second gap *inside* a single
/// escape pair — both bytes are emitted by one `raw_write_bytes`/`write_all`,
/// so TCP effectively never splits them that long.  Even if it did, the
/// lost/stale byte mis-frames the block → the
/// dual checksum fails → BAD → full resend; a persistent desync trips
/// `max_bad_rounds` and aborts loudly.  A misframed block coincidentally
/// passing both the additive and cyclic checksums is astronomically unlikely,
/// so this degrades to a failed transfer, never silent file corruption.
async fn read_block(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    state: &mut ReadState,
    size: u8,
    t: &Tunables,
    verbose: bool,
) -> Result<Vec<u8>, String> {
    let size = size as usize;
    // Uniform short per-byte window: long enough to re-prompt a peer that
    // missed our S/B (first byte) and to ride out real link latency between
    // bytes, but far short of the full block_timeout so a dropped byte
    // mid-block becomes a fast BAD/resend instead of a ~block_timeout stall.
    let byte_wait = t.block_timeout.min(t.retry_interval.max(1));
    for _attempt in 0..=t.max_retries {
        send_code(writer, Code::Sb, is_tcp, verbose).await?;
        let mut buf: Vec<u8> = Vec::with_capacity(size);
        for _i in 0..size {
            let r = tokio::time::timeout(
                tokio::time::Duration::from_secs(byte_wait),
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
                }
                _ => break, // timeout — short/blank read
            }
        }
        // A real block is at least DATAPOS=7 bytes (the header).  If exactly
        // "ack" arrived and nothing else, the sender re-transmitted ACK
        // because it never saw our S/B (`recmodem` rc2); resend S/B and
        // retry.  Checking the buffer length rather than just the prefix
        // avoids a false positive when a data block's checksum-pair bytes
        // coincidentally spell "ack" — those blocks carry a full payload
        // behind them, so they're longer than 3 bytes.
        if buf == *Code::Ack.bytes() {
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
///
/// Like Novaterm's `rechand`, an unanswered SYN handshake here is NOT
/// treated as a transfer failure — by the time we reach end-off the final
/// data block has already been ack'd, so the file on disk is complete.
/// Real C1 peers commonly tear down immediately after the final S/B; the
/// receiver's later SYN/S-B exchanges land in a closed pipe with no harm.
/// `verbose` enables a per-stage warning so operators can still see when
/// the handshake didn't fully complete.
async fn end_off_receiver(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
    state: &mut ReadState,
    t: &Tunables,
) -> Result<(), String> {
    // Everything here is best-effort: the final data block was already ack'd,
    // so the file is complete.  A peer that tears down (EOF / closed pipe,
    // which `send_code`/`accept_code` surface as Err) or a local ESC must NOT
    // turn a finished transfer into a failure.  Run the handshake with `?` in
    // an inner future and discard any error — a teardown just stops the
    // re-handshake early.  (Timeouts return `Ok(None)`, so the per-stage
    // verbose warnings still fire.)
    let _ = async {
        // Acknowledge the final block and re-handshake.
        let mut got_ack = false;
        for _ in 0..=t.max_retries {
            send_code(writer, Code::Goo, is_tcp, verbose).await?;
            if let Some(Code::Ack) =
                accept_code(reader, is_tcp, is_petscii, state, &[Code::Ack], t.block_timeout, verbose).await?
            {
                got_ack = true;
                break;
            }
        }
        if verbose && !got_ack {
            glog!("PUNTER recv: end-off ACK not received (peer may have torn down)");
        }
        send_code(writer, Code::Sb, is_tcp, verbose).await?;

        // Wait for the sender's SYN (resend S/B on timeout), then answer SYN
        // and wait for the sender's S/B (resend SYN on timeout).
        let mut got_syn = false;
        for _ in 0..=t.max_retries {
            match accept_code(reader, is_tcp, is_petscii, state, &[Code::Syn], t.block_timeout, verbose).await? {
                Some(Code::Syn) => {
                    got_syn = true;
                    break;
                }
                _ => send_code(writer, Code::Sb, is_tcp, verbose).await?,
            }
        }
        if verbose && !got_syn {
            glog!("PUNTER recv: end-off SYN not received (peer may have torn down)");
        }
        let mut got_final_sb = false;
        for _ in 0..=t.max_retries {
            send_code(writer, Code::Syn, is_tcp, verbose).await?;
            match accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], t.block_timeout, verbose).await? {
                Some(Code::Sb) => {
                    got_final_sb = true;
                    break;
                }
                _ => continue,
            }
        }
        if verbose && !got_final_sb {
            glog!("PUNTER recv: end-off final S/B not received (peer may have torn down)");
        }
        Ok::<(), String>(())
    }
    .await;
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
    let data_blocks = build_data_blocks(data, t.block_payload)?;
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
#[allow(clippy::too_many_arguments)]
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
    // Cap consecutive resend requests against the same block so a peer that
    // keeps returning BAD or stray S/B can't loop forever.  Reset each time
    // we advance to the next block on GOO.
    let mut resend_rounds: u32 = 0;

    loop {
        // tx20: wait for the receiver's response.  GOO = previous block good
        // (advance); BAD or a re-sent S/B = resend the current block.  In
        // spec mode, re-emit GOO each retry until we hear something.
        //
        // Re-probe on a short `retry_interval` cadence covering the budget
        // rather than blocking the whole budget on a single read (mirrors the
        // receiver and Novaterm's `codecyc`-bounded retry).  This also bounds
        // the total wait to one budget — an unresponsive peer fails in
        // ~negotiation_timeout, not max_retries × that.
        let total_budget = if started { t.block_timeout } else { t.negotiation_timeout };
        let probe = total_budget.min(t.retry_interval.max(1));
        let attempts = total_budget.div_ceil(probe.max(1)).max(1);
        let mut code = None;
        for attempt in 0..attempts {
            if spec_mode && !started {
                send_code(writer, Code::Goo, is_tcp, verbose).await?;
            }
            code = accept_code(
                reader,
                is_tcp,
                is_petscii,
                state,
                &[Code::Goo, Code::Bad, Code::Sb],
                probe,
                verbose,
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
                        end_off_sender(reader, writer, is_tcp, is_petscii, verbose, state, t)
                            .await?;
                        return Ok(());
                    }
                    idx += 1;
                }
                started = true;
                resend_rounds = 0;
            }
            // BAD or S/B → resend the current block (do not advance).
            _ => {
                started = true;
                if verbose {
                    glog!("PUNTER send: resend requested for block {}", idx);
                }
                resend_rounds = resend_rounds.saturating_add(1);
                // As on the receive side: a real C1 receiver re-requests a
                // block without limit, so honour up to `max_bad_rounds` resends
                // (> `max_retries`) before quitting — a low cap makes the
                // gateway bail first and strand the C64, which can't be told to
                // stop (C1 has no in-band abort).
                if resend_rounds > t.max_bad_rounds {
                    return Err(format!(
                        "Punter send: {} consecutive resend requests for block {}, giving up",
                        resend_rounds, idx
                    ));
                }
            }
        }

        // tx11: send ACK, wait for the receiver's S/B (resend ACK on timeout).
        // Same short re-probe cadence: retransmit ACK every `retry_interval`
        // up to one block timeout, so a dropped ACK recovers quickly and an
        // unresponsive peer fails in ~block_timeout rather than 11× it.
        let sb_probe = t.block_timeout.min(t.retry_interval.max(1));
        let sb_attempts = t.block_timeout.div_ceil(sb_probe.max(1)).max(1);
        let mut got_sb = false;
        for _ in 0..sb_attempts {
            send_code(writer, Code::Ack, is_tcp, verbose).await?;
            if let Some(Code::Sb) =
                accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], sb_probe, verbose).await?
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

/// Sender end-off: ACK (re-sent until S/B) → SYN (re-sent until SYN) → S/B ×3.
/// The ACK stage re-sends on a short cadence because CCGMS sends GOO twice
/// after the type block (block-ack + a second GOO), each needing an ACK; the
/// three closing S/B feed the receiver's one-consume-plus-two-drain.
///
/// Like Novaterm's `tranhand`, missed handshake responses here are NOT
/// treated as failures — the last data block was acknowledged before we
/// got here, so the file has already arrived intact.  `verbose` enables
/// a per-stage warning when the handshake doesn't complete.
async fn end_off_sender(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    is_tcp: bool,
    is_petscii: bool,
    verbose: bool,
    state: &mut ReadState,
    t: &Tunables,
) -> Result<(), String> {
    // Best-effort, exactly like the receiver end-off: the final block was
    // already acknowledged, so a peer that tears down (EOF / closed pipe, which
    // `send_code`/`accept_code` surface as Err) or a local ESC must not fail a
    // finished transfer.  Run the SYN handshake with `?` in an inner future and
    // discard any error — a teardown just stops it early.  (Timeouts return
    // `Ok(None)`, so the per-stage verbose warnings still fire.)
    // Re-send each code on a SHORT cadence rather than blocking the whole
    // block_timeout on one read.  This mirrors CCGMS's reference sender, whose
    // `handshake("ACK","S/B")` re-sends ACK every ~1s until S/B arrives — which
    // is essential because CCGMS's receiver sends GOO *twice* after the type
    // block (the block-ack GOO inside recv_block, then a second GOO before the
    // S/B), and EACH needs its own ACK.  A single slow ACK strands CCGMS
    // looping GOO forever ("Downloading..." hang).  Total patience is still
    // ~block_timeout, just spread across prompt re-sends.
    let probe = t.block_timeout.min(t.retry_interval.max(1));
    let attempts = t.block_timeout.div_ceil(probe.max(1)).max(1);
    let _ = async {
        // tx41: ACK (re-sent every probe) until the receiver's S/B.
        let mut got_sb = false;
        for _ in 0..attempts {
            send_code(writer, Code::Ack, is_tcp, verbose).await?;
            if let Some(Code::Sb) =
                accept_code(reader, is_tcp, is_petscii, state, &[Code::Sb], probe, verbose).await?
            {
                got_sb = true;
                break;
            }
        }
        if verbose && !got_sb {
            glog!("PUNTER send: end-off S/B not received (peer may have torn down)");
        }
        // tx5: SYN (re-sent every probe) until the receiver's SYN comes back.
        let mut got_syn = false;
        for _ in 0..attempts {
            send_code(writer, Code::Syn, is_tcp, verbose).await?;
            if let Some(Code::Syn) =
                accept_code(reader, is_tcp, is_petscii, state, &[Code::Syn], probe, verbose).await?
            {
                got_syn = true;
                break;
            }
        }
        if verbose && !got_syn {
            glog!("PUNTER send: end-off SYN not received (peer may have torn down)");
        }
        Ok::<(), String>(())
    }
    .await;
    // tx9: three closing S/B.  CCGMS's receiver consumes one to complete its
    // SYN/S-B end-off, then *drains two more* (two fixed 3-byte reads) before
    // sleeping and opening the next phase — verified against the real CCGMS
    // receiver, which starves (and stalls ~20s) if fewer than three arrive.
    // We deliberately do NOT read here: consuming bytes would swallow the
    // receiver's opening signal for the next phase.  Best-effort — the final
    // block was already acknowledged, so a failed write here is not an error.
    for _ in 0..3 {
        let _ = send_code(writer, Code::Sb, is_tcp, verbose).await;
    }
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // Live interop against the real CCGMS receiver (compiled from
    // mist64/ccgmsterm test/punter.c). Set CCGMS_RECV_BIN to the compiled
    // binary path to run; otherwise skipped. The child speaks raw bytes over
    // its stdio, so is_tcp=false.
    #[tokio::test]
    async fn ccgms_real_receiver_interop() {
        let bin = match std::env::var("CCGMS_RECV_BIN") {
            Ok(b) => b,
            Err(_) => {
                eprintln!("CCGMS_RECV_BIN not set; skipping");
                return;
            }
        };
        use tokio::process::Command;
        let mut child = Command::new(&bin)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn ccgms receiver");
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();

        let data: Vec<u8> = (0..300u32).map(|i| (i * 7 + 1) as u8).collect();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            punter_send(
                &mut from_child,
                &mut to_child,
                &data,
                PunterFileType::Seq,
                false,
                false,
                true,
            ),
        )
        .await;
        eprintln!("punter_send result: {:?}", res);
        let _ = child.kill().await;
    }

    // Live interop the other direction: the gateway RECEIVES from CCGMS's real
    // sender (punter_xmit, sends 300 bytes = i*7+1, type SEQ). Set
    // CCGMS_SEND_BIN to the compiled sender binary path to run; else skipped.
    #[tokio::test]
    async fn ccgms_real_sender_interop() {
        let bin = match std::env::var("CCGMS_SEND_BIN") {
            Ok(b) => b,
            Err(_) => {
                eprintln!("CCGMS_SEND_BIN not set; skipping");
                return;
            }
        };
        use tokio::process::Command;
        let mut child = Command::new(&bin)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn ccgms sender");
        let mut to_child = child.stdin.take().unwrap();
        let mut from_child = child.stdout.take().unwrap();

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            punter_receive(&mut from_child, &mut to_child, false, false, true),
        )
        .await;
        let _ = child.kill().await;
        let expected: Vec<u8> = (0..300u32).map(|i| (i * 7 + 1) as u8).collect();
        match res {
            Ok(Ok((data, ft))) => {
                eprintln!("punter_receive: {} bytes, type {:?}", data.len(), ft);
                assert_eq!(data, expected, "received data must match CCGMS sender");
                assert_eq!(ft, PunterFileType::Seq);
            }
            other => panic!("punter_receive failed: {:?}", other),
        }
    }

    // — Handshake code wire format —

    #[test]
    fn handshake_codes_are_uppercase_on_the_wire() {
        // Real C64 peers (confirmed against captured CCGMS) transmit the codes
        // uppercase: GOO = 0x47 0x4F 0x4F, etc.  We must emit the same or a
        // real receiver never recognises us.
        assert_eq!(Code::Goo.bytes(), b"GOO");
        assert_eq!(Code::Bad.bytes(), b"BAD");
        assert_eq!(Code::Ack.bytes(), b"ACK");
        assert_eq!(Code::Sb.bytes(), b"S/B");
        assert_eq!(Code::Syn.bytes(), b"SYN");
        assert_eq!(Code::Goo.bytes(), &[0x47, 0x4F, 0x4F]);
    }

    #[test]
    fn from_window_matches_either_case() {
        // We send uppercase, but tolerate a lowercase peer on receive.
        assert_eq!(Code::from_window(b"GOO"), Some(Code::Goo));
        assert_eq!(Code::from_window(b"goo"), Some(Code::Goo));
        assert_eq!(Code::from_window(b"S/B"), Some(Code::Sb));
        assert_eq!(Code::from_window(b"s/b"), Some(Code::Sb));
        assert_eq!(Code::from_window(b"xyz"), None);
    }

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
        let blocks = build_data_blocks(&[1, 2, 3], 255).unwrap();
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
        let blocks = build_data_blocks(&data, 47).unwrap();
        // 1 header + ceil(100/40)=3 payload blocks = 4.
        assert_eq!(blocks.len(), 4);
        assert!(is_final_block(blocks.last().unwrap()));
    }

    // — File-type mapping (verified against head.src:423-426 prg/seq/usr/---) —

    #[test]
    fn file_type_byte_roundtrips_all_known_types() {
        for ft in [
            PunterFileType::Prg,
            PunterFileType::Seq,
            PunterFileType::Usr,
            PunterFileType::Unknown,
        ] {
            assert_eq!(PunterFileType::from_byte(ft.to_byte()), ft);
        }
        // The four documented wire values map to the four enum variants.
        assert_eq!(PunterFileType::from_byte(0), PunterFileType::Prg);
        assert_eq!(PunterFileType::from_byte(1), PunterFileType::Seq);
        assert_eq!(PunterFileType::from_byte(2), PunterFileType::Usr);
        assert_eq!(PunterFileType::from_byte(3), PunterFileType::Unknown);
    }

    #[test]
    fn file_type_out_of_range_byte_is_unknown_not_seq() {
        // Anything past the documented 0..=3 range maps to Unknown (matches
        // filetype3 "---"), never silently coerced to a real type.
        for b in 4u8..=255 {
            assert_eq!(PunterFileType::from_byte(b), PunterFileType::Unknown);
        }
    }

    #[test]
    fn file_type_extension_matches_head_src_suffixes() {
        assert_eq!(PunterFileType::Prg.extension(), Some("prg"));
        assert_eq!(PunterFileType::Seq.extension(), Some("seq"));
        assert_eq!(PunterFileType::Usr.extension(), Some("usr"));
        // Unknown ("---") has no real suffix — the operator's name wins.
        assert_eq!(PunterFileType::Unknown.extension(), None);
    }

    #[test]
    fn autodetect_picks_type_from_extension() {
        assert_eq!(PunterFileType::autodetect("game"), PunterFileType::Prg);
        assert_eq!(PunterFileType::autodetect("game.prg"), PunterFileType::Prg);
        assert_eq!(PunterFileType::autodetect("readme.txt"), PunterFileType::Seq);
        assert_eq!(PunterFileType::autodetect("notes.doc"), PunterFileType::Seq);
        assert_eq!(PunterFileType::autodetect("data.seq"), PunterFileType::Seq);
        assert_eq!(PunterFileType::autodetect("scratch.usr"), PunterFileType::Usr);
        // Case-insensitive.
        assert_eq!(PunterFileType::autodetect("README.TXT"), PunterFileType::Seq);
        assert_eq!(PunterFileType::autodetect("SCRATCH.USR"), PunterFileType::Usr);
    }

    // — is_final_block edge: only high byte 0xFF flags the final block —

    #[test]
    fn final_flag_is_exactly_the_index_high_byte() {
        // 0xFEFF is the largest non-final index; 0xFF00 is the smallest final.
        assert!(!is_final_block(&build_block(0, 0xFEFF, &[1])));
        assert!(is_final_block(&build_block(0, 0xFF00, &[1])));
        assert!(is_final_block(&build_block(0, 0xFFFF, &[1])));
        assert!(!is_final_block(&build_block(0, 0x0000, &[1])));
        assert!(!is_final_block(&build_block(0, 0x00FF, &[1]))); // low byte 0xFF ≠ final
    }

    #[test]
    fn header_only_block_is_seven_bytes_and_checksum_verifies() {
        // The data-phase block 0 carries no payload; it must still checksum.
        let blocks = build_data_blocks(&[1, 2, 3], 248).unwrap();
        assert_eq!(blocks[0].len(), DATAPOS);
        assert!(checksum_ok(&blocks[0]));
        assert!(!is_final_block(&blocks[0]));
    }

    // — Block-index overflow guard (precise boundary) —

    #[test]
    fn build_data_blocks_accepts_max_addressable_block_count() {
        // With payload 1, chunk_count == data.len(). The largest non-final
        // index is chunk_count-1, which must stay ≤ MAX_DATA_BLOCK_INDEX
        // (0xFEFF). 0xFF00 chunks → max non-final index 0xFEFF: still legal.
        let data = vec![0u8; MAX_DATA_BLOCK_INDEX as usize + 1]; // 0xFF00
        let blocks = build_data_blocks(&data, 1).expect("0xFF00 chunks must fit");
        // No non-final block may carry an index whose high byte is 0xFF.
        for b in &blocks[..blocks.len() - 1] {
            assert_ne!(b[NUMPOS + 1], 0xFF, "non-final block masquerades as final");
        }
        assert!(is_final_block(blocks.last().unwrap()));
    }

    #[test]
    fn build_data_blocks_rejects_too_many_blocks() {
        // One chunk past the addressable range must error rather than silently
        // emit an intermediate block with a 0xFF high byte (false "final").
        let data = vec![0u8; MAX_DATA_BLOCK_INDEX as usize + 2]; // 0xFF01
        let err = build_data_blocks(&data, 1).unwrap_err();
        assert!(err.contains("too large"), "unexpected error text: {err}");
    }

    // — Round-trip over an in-memory duplex pipe —

    async fn round_trip(data: &[u8], ftype: PunterFileType) -> (Vec<u8>, PunterFileType) {
        round_trip_opts(data, ftype, false).await
    }

    /// As `round_trip`, but with `is_tcp` controllable so the telnet IAC
    /// escaping path (`tnio::raw_write_bytes`/`nvt_read_byte`) is exercised
    /// end to end — every transfer's final block carries index 0xFFFF (two
    /// 0xFF/IAC bytes), so the TCP path must double-and-collapse those, while
    /// CR (0x0D) bytes pass through literally (no NVT CR-NUL stuffing).
    async fn round_trip_opts(
        data: &[u8],
        ftype: PunterFileType,
        is_tcp: bool,
    ) -> (Vec<u8>, PunterFileType) {
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
            punter_send(&mut rd, &mut wr, &data_owned, ftype, is_tcp, false, false).await
        });
        let receiver = tokio::spawn(async move {
            let mut rd = s_to_r_b;
            let mut wr = r_to_s_a;
            punter_receive(&mut rd, &mut wr, is_tcp, false, false).await
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

    #[tokio::test]
    async fn round_trip_over_tcp_escapes_iac_passes_cr() {
        // is_tcp=true routes blocks through raw_write_bytes/nvt_read_byte, so
        // 0xFF (IAC) is doubled on the wire and collapsed on read, while 0x0D
        // (CR) — and any 0x00 after it — passes through literally (no NVT
        // CR-NUL stuffing). Pack the payload with both, plus runs that would
        // desync if IAC escaping were one-sided or if CR were still stuffed.
        let data: Vec<u8> = vec![
            0xFF, 0xFF, 0x0D, 0x00, 0x0D, 0x0A, 0xFF, 0x0D, 0xFF, 0x00, 0x18, 0x1B, 0x5F,
        ];
        let (out, ft) = round_trip_opts(&data, PunterFileType::Prg, true).await;
        assert_eq!(out, data);
        assert_eq!(ft, PunterFileType::Prg);
    }

    #[tokio::test]
    async fn round_trip_over_tcp_multi_block_all_byte_values() {
        // Several blocks of every byte value over the TCP path — the final
        // block's 0xFFFF index alone guarantees IAC bytes in the header, and
        // the payload covers 0xFF/0x0D throughout.
        let data: Vec<u8> = (0..2000u32).map(|i| (i % 256) as u8).collect();
        let (out, _) = round_trip_opts(&data, PunterFileType::Seq, true).await;
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn round_trip_over_tcp_empty_preserves_iac_final_header() {
        // Even an empty file ships a final block with index 0xFFFF (IAC IAC in
        // the header) over TCP; it must round-trip cleanly.
        let (out, ft) = round_trip_opts(&[], PunterFileType::Unknown, true).await;
        assert_eq!(out, Vec::<u8>::new());
        assert_eq!(ft, PunterFileType::Unknown);
    }

    #[tokio::test]
    async fn read_block_gives_up_bounded_on_a_silent_peer() {
        // A connected-but-silent peer must make read_block fail in about
        // max_retries × retry_interval (the short uniform per-byte window),
        // not max_retries × block_timeout.
        let t = Tunables {
            negotiation_timeout: 5,
            block_timeout: 5, // would dominate if used as the per-byte wait
            max_retries: 3,
            max_bad_rounds: 30,
            retry_interval: 1,
            block_payload: MAX_PAYLOAD,
        };
        let (_hold, mut rd) = duplex(64); // held open, never written → reads block
        let (mut wr, _drain) = duplex(256);
        let mut state = ReadState::default();

        let start = std::time::Instant::now();
        let res = read_block(&mut rd, &mut wr, false, &mut state, 7, &t, false).await;
        let elapsed = start.elapsed();

        assert!(res.is_err(), "silent peer must fail the block read");
        // Bounded by max_retries × retry_interval (the short per-byte wait):
        // (3+1) S/B re-sends × ~1s ≈ 4s, NOT max_retries × block_timeout (20s).
        // Generous slack for CI jitter.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "read_block took {elapsed:?}; per-byte wait should bound it"
        );
    }

    #[tokio::test]
    async fn end_off_receiver_tolerates_peer_teardown() {
        // Real C1 senders commonly close the link right after the final S/B.
        // By the time we reach end-off the file is already complete, so a
        // closed pipe (EOF) must NOT turn the transfer into a failure.
        let t = Tunables {
            negotiation_timeout: 1,
            block_timeout: 1,
            max_retries: 2,
            max_bad_rounds: 30,
            retry_interval: 1,
            block_payload: MAX_PAYLOAD,
        };
        let (peer_wr, mut rd) = duplex(64);
        drop(peer_wr); // immediate EOF on every read
        let (mut wr, _drain) = duplex(256);
        let mut state = ReadState::default();
        let res =
            end_off_receiver(&mut rd, &mut wr, false, false, false, &mut state, &t).await;
        assert!(res.is_ok(), "peer teardown during end-off must not fail a complete transfer");
    }

    #[tokio::test]
    async fn end_off_sender_tolerates_peer_teardown() {
        let t = Tunables {
            negotiation_timeout: 1,
            block_timeout: 1,
            max_retries: 2,
            max_bad_rounds: 30,
            retry_interval: 1,
            block_payload: MAX_PAYLOAD,
        };
        let (peer_wr, mut rd) = duplex(64);
        drop(peer_wr);
        let (mut wr, _drain) = duplex(256);
        let mut state = ReadState::default();
        let res = end_off_sender(&mut rd, &mut wr, false, false, false, &mut state, &t).await;
        assert!(res.is_ok(), "peer teardown during end-off must not fail a complete transfer");
    }

    #[tokio::test]
    async fn send_phase_gives_up_quickly_on_a_silent_peer() {
        // A connected-but-silent receiver must make the sender fail in about
        // one negotiation budget, NOT max_retries × it. The peer's write half
        // stays open (so reads block rather than EOF) but nothing is ever sent.
        let t = Tunables {
            negotiation_timeout: 2,
            block_timeout: 1,
            max_retries: 5,
            max_bad_rounds: 30,
            retry_interval: 1,
            block_payload: MAX_PAYLOAD,
        };
        let blocks = build_data_blocks(&[1, 2, 3], MAX_PAYLOAD).unwrap();

        // reader: held-open but silent; writer: drained.
        let (_hold, mut rd) = duplex(64);
        let (mut wr, _drain) = duplex(1024);
        let mut state = ReadState::default();

        let start = std::time::Instant::now();
        let res =
            send_phase(&mut rd, &mut wr, false, false, false, &mut state, &blocks, false, &t).await;
        let elapsed = start.elapsed();

        assert!(res.is_err(), "silent peer must fail, not succeed");
        // Bounded to ~negotiation_timeout. The pre-fix code waited
        // (max_retries+1) × negotiation_timeout (here 6×2=12s; in production
        // 11×45≈495s). Allow generous slack but well under that.
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "send_phase took {elapsed:?}; expected ~negotiation_timeout"
        );
    }

    #[tokio::test]
    async fn receive_phase_bounds_a_flood_of_empty_nonfinal_blocks() {
        // A peer that streams valid-checksum, non-final, zero-payload blocks
        // neither grows `out` (so MAX_FILE_SIZE never trips) nor fails the
        // checksum (so max_bad_rounds never trips).  Only block 0 is
        // legitimately empty; the receiver must give up once the count passes
        // MAX_EMPTY_NONFINAL_BLOCKS instead of spinning forever.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let t = Tunables {
            negotiation_timeout: 2,
            block_timeout: 1,
            max_retries: 5,
            max_bad_rounds: 30,
            retry_interval: 1,
            block_payload: MAX_PAYLOAD,
        };

        // receiver writes → peer reads; peer writes → receiver reads.
        let (recv_to_peer_a, recv_to_peer_b) = duplex(1 << 16);
        let (peer_to_recv_a, peer_to_recv_b) = duplex(1 << 16);

        // Malicious peer: ACK every GOO/BAD, and answer every S/B with an
        // empty, non-final block that announces a 7-byte (header-only) next
        // block — so it can keep this up indefinitely.
        let peer = tokio::spawn(async move {
            let mut rd = recv_to_peer_b;
            let mut wr = peer_to_recv_a;
            let mut buf = [0u8; 3];
            loop {
                if rd.read_exact(&mut buf).await.is_err() {
                    break;
                }
                match &buf {
                    b"GOO" | b"BAD" => {
                        if wr.write_all(b"ACK").await.is_err() {
                            break;
                        }
                    }
                    b"S/B" => {
                        let blk = build_block(DATA_PHASE_FIRST_SIZE, 0x0000, &[]);
                        if wr.write_all(&blk).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        let mut rd = peer_to_recv_b;
        let mut wr = recv_to_peer_a;
        let mut state = ReadState::default();
        let res = receive_phase(
            &mut rd, &mut wr, false, false, false, &mut state, DATA_PHASE_FIRST_SIZE, &t,
        )
        .await;
        drop(wr); // let the peer observe EOF and exit
        let _ = peer.await;

        match res {
            Err(e) => assert!(
                e.contains("empty non-final"),
                "expected the empty-block bound to fire, got: {e}"
            ),
            Ok(_) => panic!("a flood of empty non-final blocks must be rejected"),
        }
    }

    #[tokio::test]
    async fn round_trip_preserves_usr_and_unknown_types() {
        // The declared file type survives Phase A end to end for every variant
        // — not just the original PRG/SEQ pair.
        let (_, ft) = round_trip(&[1, 2, 3], PunterFileType::Usr).await;
        assert_eq!(ft, PunterFileType::Usr);
        let (_, ft) = round_trip(&[1, 2, 3], PunterFileType::Unknown).await;
        assert_eq!(ft, PunterFileType::Unknown);
    }

    #[tokio::test]
    async fn round_trip_recovers_from_a_single_corrupted_block() {
        // An interposer flips one byte deep in the sender→receiver stream
        // (a data-block body byte), then passes everything else verbatim. The
        // receiver's checksum catches it, demands a resend (BAD), and the
        // sender's retransmit — past the corruption point — restores the file.
        // Exercises the rec2/badinc ↔ tx10/badinc resend loop the clean
        // round-trips never touch.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let data: Vec<u8> = (0..4000u32).map(|i| (i * 13 + 7) as u8).collect();
        let data_owned = data.clone();

        let (s_out_a, mut s_out_b) = duplex(1 << 20); // sender → interposer
        let (mut r_in_a, r_in_b) = duplex(1 << 20); // interposer → receiver
        let (r_out_a, r_out_b) = duplex(1 << 20); // receiver → sender (direct)

        let interposer = tokio::spawn(async move {
            let corrupt_at = 400usize;
            let mut count = 0usize;
            let mut byte = [0u8; 1];
            loop {
                match s_out_b.read(&mut byte).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if count == corrupt_at {
                            byte[0] ^= 0xFF; // single one-shot corruption
                        }
                        count += 1;
                        if r_in_a.write_all(&byte).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let sender = tokio::spawn(async move {
            let mut rd = r_out_b;
            let mut wr = s_out_a;
            punter_send(&mut rd, &mut wr, &data_owned, PunterFileType::Prg, false, false, false)
                .await
        });
        let receiver = tokio::spawn(async move {
            let mut rd = r_in_b;
            let mut wr = r_out_a;
            punter_receive(&mut rd, &mut wr, false, false, false).await
        });

        let send_res = sender.await.unwrap();
        let recv_res = receiver.await.unwrap();
        interposer.await.unwrap();
        send_res.expect("send should complete despite one corrupted block");
        let (out, _) = recv_res.expect("receive should recover from the corruption");
        assert_eq!(out, data);
    }

    // — Stray-ACK recovery: sender re-sent ACK because it missed our S/B —

    #[tokio::test]
    async fn read_block_swallows_lone_ack_then_returns_real_block() {
        use tokio::io::AsyncWriteExt;

        // Short per-byte budget so the test doesn't wait the 20s default.
        let t = Tunables {
            negotiation_timeout: 1,
            block_timeout: 1,
            max_retries: 3,
            max_bad_rounds: 30,
            retry_interval: 0,
            block_payload: MAX_PAYLOAD,
        };
        // next-size field is arbitrary here (read_block returns raw bytes;
        // the caller validates the checksum, which build_block fills in).
        let real = build_block(10, 0x0001, &[10, 20, 30]);
        let real_for_assert = real.clone();
        let size = real.len() as u8;

        // reader: bytes flow peer → read_block; writer: read_block's S/B codes.
        let (mut peer_wr, mut rb_rd) = duplex(512);
        let (mut rb_wr, _drain) = duplex(512);

        let feeder = tokio::spawn(async move {
            // The stray "ack" arrives first, alone.
            peer_wr.write_all(Code::Ack.bytes()).await.unwrap();
            // A gap longer than block_timeout makes read_block time out on the
            // would-be 4th byte and recognise the lone "ack" — exactly the
            // "sender missed our S/B" case (recmodem rcm4/rc2). Only then does
            // the real block follow, in the next S/B round.
            tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;
            peer_wr.write_all(&real).await.unwrap();
            // Keep the write half alive until read_block has consumed the block.
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        });

        let mut state = ReadState::default();
        let got = read_block(&mut rb_rd, &mut rb_wr, false, &mut state, size, &t, false)
            .await
            .expect("read_block should recover from the stray ack");
        assert_eq!(got, real_for_assert);
        assert!(checksum_ok(&got));
        feeder.await.unwrap();
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
                let blocks = build_data_blocks(&data, block_size - DATAPOS).unwrap();
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

// ─── Independent reference-codec interop ──────────────────────────────────
//
// The in-pipe round-trips above run our sender against our receiver, so they
// cannot catch a *mutual* wrong assumption — a bug both halves share. This
// module is a second, independent C1 ("New Punter") implementation written
// only from the wire spec / Novaterm `punter.src`: its own checksum, framing,
// block builder, and full sender/receiver handshakes, deliberately NOT reusing
// any of `punter.rs` (only the public `punter_send`/`punter_receive` entry
// points). The two implementations talk over a real loopback TCP socket. If
// they interoperate cleanly, that is strong evidence both match the protocol;
// if they don't, one of them has a real bug. This is the closest automated
// stand-in for a real CCGMS / Novaterm peer (no Linux Punter client exists).
#[cfg(test)]
mod reference_interop {
    use super::{punter_receive, punter_send, PunterFileType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    // Uppercase: the bytes a real C64 puts on the wire (confirmed against
    // captured CCGMS — `0x47 'G' 0x4F 'O' 0x4F 'O'`).  See `Code::bytes`.
    const GOO: [u8; 3] = *b"GOO";
    const BAD: [u8; 3] = *b"BAD";
    const ACK: [u8; 3] = *b"ACK";
    const SB: [u8; 3] = *b"S/B";
    const SYN: [u8; 3] = *b"SYN";

    // Independently coded from the asm `checksum` routine: additive is a 16-bit
    // wrapping sum; cyclic XORs the byte into the low byte then rotates the
    // whole 16-bit accumulator left one bit. Computed over the block body
    // (offset 4 onward).
    fn checksums(body: &[u8]) -> (u16, u16) {
        let mut add: u16 = 0;
        let mut cyc: u16 = 0;
        for &b in body {
            add = add.wrapping_add(b as u16);
            cyc ^= b as u16;
            cyc = cyc.rotate_left(1);
        }
        (add, cyc)
    }

    fn build_block(next_size: u8, idx: u16, payload: &[u8]) -> Vec<u8> {
        let mut blk = vec![0u8; 7 + payload.len()];
        blk[4] = next_size;
        blk[5] = (idx & 0xFF) as u8;
        blk[6] = (idx >> 8) as u8;
        blk[7..].copy_from_slice(payload);
        let (a, c) = checksums(&blk[4..]);
        blk[0] = a as u8;
        blk[1] = (a >> 8) as u8;
        blk[2] = c as u8;
        blk[3] = (c >> 8) as u8;
        blk
    }

    fn checksum_ok(blk: &[u8]) -> bool {
        if blk.len() < 7 {
            return false;
        }
        let (a, c) = checksums(&blk[4..]);
        a == u16::from_le_bytes([blk[0], blk[1]]) && c == u16::from_le_bytes([blk[2], blk[3]])
    }

    /// Patch each block's "size of next block" field and recompute checksums;
    /// the last block points at its own length.
    fn backpatch(blocks: &mut [Vec<u8>]) {
        let sizes: Vec<u8> = blocks.iter().map(|b| b.len() as u8).collect();
        for (i, blk) in blocks.iter_mut().enumerate() {
            let next = if i + 1 < sizes.len() { sizes[i + 1] } else { sizes[i] };
            blk[4] = next;
            let (a, c) = checksums(&blk[4..]);
            blk[0] = a as u8;
            blk[1] = (a >> 8) as u8;
            blk[2] = c as u8;
            blk[3] = (c >> 8) as u8;
        }
    }

    fn build_type_blocks(ftype: u8) -> Vec<Vec<u8>> {
        let mut b = vec![build_block(8, 0xFFFF, &[ftype])];
        backpatch(&mut b);
        b
    }

    fn build_data_blocks(data: &[u8]) -> Vec<Vec<u8>> {
        let cap = 248usize;
        let mut blocks = vec![build_block(0, 0x0000, &[])]; // header-only block 0
        if data.is_empty() {
            blocks.push(build_block(0, 0xFFFF, &[]));
        } else {
            let chunks: Vec<&[u8]> = data.chunks(cap).collect();
            let last = chunks.len() - 1;
            for (i, chunk) in chunks.iter().enumerate() {
                let idx = if i == last { 0xFFFF } else { (i as u16) + 1 };
                blocks.push(build_block(0, idx, chunk));
            }
        }
        backpatch(&mut blocks);
        blocks
    }

    // — raw socket helpers —

    async fn put(s: &mut TcpStream, code: [u8; 3]) {
        s.write_all(&code).await.unwrap();
        s.flush().await.unwrap();
    }

    async fn put_bytes(s: &mut TcpStream, bytes: &[u8]) {
        s.write_all(bytes).await.unwrap();
        s.flush().await.unwrap();
    }

    /// Slide a 3-byte window over the stream until one of `allowed` matches.
    async fn get_code(s: &mut TcpStream, allowed: &[[u8; 3]]) -> [u8; 3] {
        let mut w = [0u8; 3];
        let mut filled = 0;
        let mut b = [0u8; 1];
        loop {
            s.read_exact(&mut b).await.unwrap();
            w = [w[1], w[2], b[0]];
            if filled < 3 {
                filled += 1;
            }
            if filled == 3 && allowed.contains(&w) {
                return w;
            }
        }
    }

    async fn get_block(s: &mut TcpStream, size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; size];
        s.read_exact(&mut buf).await.unwrap();
        buf
    }

    /// A one-block fault the reference applies during the DATA phase:
    /// `(position, times)` — disrupt the data block at array position `position`
    /// on its first `times` transmissions, then let it through.  When the
    /// reference is the *sender* it corrupts the block's body (so the gateway
    /// receiver's checksum demands a resend); when it's the *receiver* it sends
    /// `BAD` despite a good checksum (so the gateway sender resends).  Either
    /// way it drives the cross-codec BAD/resend recovery the clean round-trips
    /// never reach.  `times` above the old fixed cap (10) but below
    /// `punter_max_bad_rounds` (30) proves the gateway now perseveres instead
    /// of bailing and stranding the peer.
    type Fault = (usize, u32);

    // — independent RECEIVER (mirrors rechand/receive) —

    async fn ref_receive(s: &mut TcpStream, fault: Option<Fault>) -> (Vec<u8>, u8) {
        // Faults only apply to the data phase, not the single type block.
        let type_payload = ref_recv_phase(s, 8, None).await;
        let ftype = type_payload.first().copied().unwrap_or(3);
        let data = ref_recv_phase(s, 7, fault).await;
        (data, ftype)
    }

    async fn ref_recv_phase(s: &mut TcpStream, first_size: usize, fault: Option<Fault>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut next = first_size;
        let mut signal = GOO; // GOO = good/advance; BAD = demand a resend
        let mut idx = 0usize; // 0-based position of the block we're about to read
        let mut faults_done = 0u32;
        loop {
            // send GOO/BAD, wait for the sender's ACK (skip its spec-mode GOO).
            put(s, signal).await;
            get_code(s, &[ACK]).await;
            // send S/B, then read the announced-size block.
            put(s, SB).await;
            let blk = get_block(s, next).await;
            assert!(checksum_ok(&blk), "reference receiver: gateway sent a bad checksum");
            // Optionally reject this (checksum-good) block to force the gateway
            // sender to resend it — a noisy-receiver stand-in.  Keep `next` and
            // `idx` unchanged so the same-size block is re-read next round.
            let reject = match fault {
                Some((pos, times)) => idx == pos && faults_done < times,
                None => false,
            };
            if reject {
                faults_done += 1;
                signal = BAD;
                continue;
            }
            if blk.len() > 7 {
                out.extend_from_slice(&blk[7..]);
            }
            let final_block = blk[6] == 0xFF;
            next = blk[4] as usize;
            signal = GOO;
            idx += 1;
            if final_block {
                // end-off: ack final, S/B, then the SYN handshake.
                put(s, GOO).await;
                get_code(s, &[ACK]).await;
                put(s, SB).await;
                get_code(s, &[SYN]).await;
                put(s, SYN).await;
                get_code(s, &[SB]).await;
                return out;
            }
        }
    }

    // — independent SENDER (mirrors tranhand/transmit) —

    async fn ref_send(s: &mut TcpStream, data: &[u8], ftype: u8, fault: Option<Fault>) {
        // Faults only apply to the data phase, not the single type block.
        ref_send_phase(s, &build_type_blocks(ftype), true, None).await;
        ref_send_phase(s, &build_data_blocks(data), false, fault).await;
    }

    async fn ref_send_phase(
        s: &mut TcpStream,
        blocks: &[Vec<u8>],
        spec: bool,
        fault: Option<Fault>,
    ) {
        let mut idx = 0usize;
        let mut started = false;
        let mut faults_done = 0u32;
        loop {
            if spec && !started {
                put(s, GOO).await;
            }
            let code = get_code(s, &[GOO, BAD, SB]).await;
            if code == GOO {
                if started {
                    if blocks[idx][6] == 0xFF {
                        // end-off (mirrors end_off_sender): ACK→S/B, SYN→SYN,
                        // then three S/B for the receiver to drain.
                        put(s, ACK).await;
                        get_code(s, &[SB]).await;
                        put(s, SYN).await;
                        get_code(s, &[SYN]).await;
                        put(s, SB).await;
                        put(s, SB).await;
                        put(s, SB).await;
                        return;
                    }
                    idx += 1;
                }
                started = true;
            } else {
                started = true; // BAD/S-B → resend the current block
            }
            put(s, ACK).await;
            get_code(s, &[SB]).await;
            // Optionally corrupt this block's body on its first `times` sends so
            // the gateway receiver's checksum catches it and demands a resend.
            // Flip a byte (length unchanged) so framing stays aligned.
            let mut blk = blocks[idx].clone();
            if let Some((pos, times)) = fault {
                if idx == pos && faults_done < times {
                    let last = blk.len() - 1;
                    blk[last] ^= 0xFF;
                    faults_done += 1;
                }
            }
            put_bytes(s, &blk).await;
        }
    }

    // — harness: one side is the real gateway, the other the reference —

    async fn gateway_receives_from_reference(
        data: Vec<u8>,
        ftype: PunterFileType,
        fault: Option<Fault>,
    ) -> (Vec<u8>, PunterFileType) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ftype_byte = ftype.to_byte();

        let gateway = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut rd, mut wr) = sock.into_split();
            punter_receive(&mut rd, &mut wr, false, false, false).await
        });
        let reference = tokio::spawn(async move {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            ref_send(&mut sock, &data, ftype_byte, fault).await;
        });

        reference.await.unwrap();
        gateway.await.unwrap().expect("gateway receive failed")
    }

    async fn gateway_sends_to_reference(
        data: Vec<u8>,
        ftype: PunterFileType,
        fault: Option<Fault>,
    ) -> (Vec<u8>, u8) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let data_for_gateway = data.clone();

        let gateway = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (mut rd, mut wr) = sock.into_split();
            punter_send(&mut rd, &mut wr, &data_for_gateway, ftype, false, false, false).await
        });
        let reference = tokio::spawn(async move {
            let mut sock = TcpStream::connect(addr).await.unwrap();
            ref_receive(&mut sock, fault).await
        });

        let got = reference.await.unwrap();
        gateway.await.unwrap().expect("gateway send failed");
        got
    }

    /// Fail fast instead of hanging forever if the two implementations desync.
    async fn with_timeout<F: std::future::Future>(f: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(30), f)
            .await
            .expect("interop exchange timed out — implementations desynced")
    }

    #[tokio::test]
    async fn gateway_receives_what_reference_sends() {
        for (data, ft) in [
            (vec![], PunterFileType::Seq),
            (vec![0x42], PunterFileType::Prg),
            ((0..=255u8).collect::<Vec<u8>>(), PunterFileType::Usr),
            ((0..3000u32).map(|i| (i * 31 + 5) as u8).collect(), PunterFileType::Unknown),
        ] {
            let expect = data.clone();
            let (got, got_ft) = with_timeout(gateway_receives_from_reference(data, ft, None)).await;
            assert_eq!(got, expect, "payload mismatch (gateway receiving)");
            assert_eq!(got_ft, ft, "file type mismatch (gateway receiving)");
        }
    }

    #[tokio::test]
    async fn reference_receives_what_gateway_sends() {
        for (data, ft) in [
            (vec![], PunterFileType::Unknown),
            (vec![0x99], PunterFileType::Seq),
            ((0..=255u8).collect::<Vec<u8>>(), PunterFileType::Prg),
            ((0..3000u32).map(|i| (i * 17 + 9) as u8).collect(), PunterFileType::Usr),
        ] {
            let expect = data.clone();
            let (got, got_ft) = with_timeout(gateway_sends_to_reference(data, ft, None)).await;
            assert_eq!(got, expect, "payload mismatch (gateway sending)");
            assert_eq!(got_ft, ft.to_byte(), "file type mismatch (gateway sending)");
        }
    }

    // — Cross-codec BAD/resend recovery (the clean interop tests never hit it) —

    #[tokio::test]
    async fn gateway_receiver_recovers_from_one_corrupt_block() {
        // The independent reference sender corrupts a data block on its first
        // transmission; our receiver's checksum must catch it, send BAD, and
        // accept the clean resend.  Proves the receive-side BAD path against an
        // INDEPENDENT codec, not just our own sender.
        let data: Vec<u8> = (0..2000u32).map(|i| (i * 11 + 3) as u8).collect();
        let expect = data.clone();
        let (got, ft) = with_timeout(gateway_receives_from_reference(
            data,
            PunterFileType::Seq,
            Some((1, 1)),
        ))
        .await;
        assert_eq!(got, expect, "gateway receiver must recover from one corrupt block");
        assert_eq!(ft, PunterFileType::Seq);
    }

    #[tokio::test]
    async fn gateway_sender_recovers_from_one_receiver_reject() {
        // The independent reference receiver rejects a data block once (sends
        // BAD despite a good checksum); our sender must resend and complete.
        // Proves the send-side resend path against an INDEPENDENT codec.
        let data: Vec<u8> = (0..2000u32).map(|i| (i * 5 + 1) as u8).collect();
        let expect = data.clone();
        let (got, ft) = with_timeout(gateway_sends_to_reference(
            data,
            PunterFileType::Prg,
            Some((1, 1)),
        ))
        .await;
        assert_eq!(got, expect, "gateway sender must recover from one rejected block");
        assert_eq!(ft, PunterFileType::Prg.to_byte());
    }

    #[tokio::test]
    async fn gateway_receiver_recovers_past_old_retry_cap() {
        // 15 consecutive corruptions of the SAME block — more than the old
        // fixed cap (DEFAULT_PUNTER_MAX_RETRIES = 10), fewer than the new
        // dedicated bad-round cap (DEFAULT_PUNTER_MAX_BAD_ROUNDS = 30).  Under
        // the old conflated cap the gateway gave up at round 11 and stranded
        // the C64; now it perseveres like a real peer and recovers.
        let data: Vec<u8> = (0..3000u32).map(|i| (i * 7 + 1) as u8).collect();
        let expect = data.clone();
        let (got, _) = with_timeout(gateway_receives_from_reference(
            data,
            PunterFileType::Seq,
            Some((2, 15)),
        ))
        .await;
        assert_eq!(got, expect, "gateway must recover after 15 bad rounds (cap is 30)");
    }

    #[tokio::test]
    async fn gateway_sender_recovers_past_old_retry_cap() {
        // Mirror on the send path: 15 consecutive receiver rejects of the same
        // block must recover under the 30-round cap, where the old cap of 10
        // would have made the gateway quit and leave the C64 hanging.
        let data: Vec<u8> = (0..3000u32).map(|i| (i * 13 + 9) as u8).collect();
        let expect = data.clone();
        let (got, _) = with_timeout(gateway_sends_to_reference(
            data,
            PunterFileType::Prg,
            Some((2, 15)),
        ))
        .await;
        assert_eq!(got, expect, "gateway must recover after 15 rejects (cap is 30)");
    }
}
