//! Virtual-modem "brain" for the CP/M emulator (Flavor B).
//!
//! The emulator's [`crate::cpm::CpmMachine`] exposes a byte channel (a TX ring
//! the guest writes and an RX ring it reads, via UART ports or the BDOS `AUX:`
//! device).  This layer is the async Hayes-modem state machine that sits on the
//! *other* side of that channel: it interprets the AT command stream the guest
//! sends, dials out (to a local serial Port A/B via the existing peer-dial
//! plumbing, or to a TCP host), and once connected pumps bytes both ways.  It
//! runs in the emulator's async driver loop ([`super::cpm_emu`]), so unlike the
//! blocking physical-serial modem it can simply `.await` a dial.
//!
//! Both directions work: **outbound** (`ATD A`/`B` for the gateway's own
//! ports, `ATD A@<remote-ip>` for a port on another gateway via the crossbar
//! master, `ATDT host:port` for TCP) and **inbound** — the emulator is dialable
//! as `CPM@<ip>`, ringing the guest (`RING`) which answers with `ATA` or
//! `ATS0=`*n* auto-answer.

use crate::config::SerialPortId;
use crate::serial::{request_peer_call, CpmIncomingCall, PeerCallOutcome};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// How long to wait for a dialed peer to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// Per-service poll window for inbound bytes when online.
const READ_POLL: Duration = Duration::from_millis(3);
/// RING cadence for an inbound call while the guest hasn't answered.
const RING_EVERY: Duration = Duration::from_secs(3);

/// Any async byte stream can back a live call (a peer `DuplexStream`, a
/// `TcpStream`, …).
trait ModemStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ModemStream for T {}

#[derive(PartialEq)]
enum Mode {
    Command,
    Online,
}

/// The CP/M virtual modem.  Owned by the CP/M session and pumped each CPU
/// batch with the bytes the guest has written; returns bytes for the guest.
pub(in crate::telnet) struct CpmModem {
    /// Whether a modem access mode is configured at all (`off` ⇒ inert).
    enabled: bool,
    mode: Mode,
    /// Command-line accumulator (command mode).
    line: Vec<u8>,
    /// The live connection while online.
    conn: Option<Box<dyn ModemStream>>,
    echo: bool,
    /// Consecutive `+` count for the (simplified) `+++` escape.
    plus_run: u8,
    /// An inbound call being rung (dialed as `CPM@<ip>`), pending answer.
    incoming: Option<CpmIncomingCall>,
    /// When to emit the next RING for `incoming`.
    ring_at: Option<Instant>,
    /// RINGs emitted for the current inbound call.
    rings: u8,
    /// Auto-answer after this many rings (S0; 0 = manual `ATA` only).
    autoanswer: u8,
    /// `ATV1` verbose result codes (default) vs `ATV0` numeric.
    verbose: bool,
    /// `ATQ1` suppresses result codes.
    quiet: bool,
    /// `ATX` result-code level (0..4); low levels collapse BUSY / NO ANSWER
    /// to NO CARRIER, as a real modem without call-progress detection does.
    x_level: u8,
    /// `AT&C` DCD handling (0 = DCD forced on; 1 = DCD tracks carrier).
    dcd_mode: u8,
    /// S-registers S0..S27 (S0 auto-answer is mirrored to `autoanswer`).
    s_regs: [u8; 28],
    /// Keeps a relay (SSH) session alive while a slave-originated remote call
    /// is online, alongside `conn` (its channel stream).  Type-erased so this
    /// module needn't name the russh types.  Cleared on hangup.
    relay_keepalive: Option<Box<dyn std::any::Any + Send>>,
}

impl CpmModem {
    pub(in crate::telnet) fn new(enabled: bool) -> Self {
        CpmModem {
            enabled,
            mode: Mode::Command,
            line: Vec::new(),
            conn: None,
            echo: true,
            plus_run: 0,
            incoming: None,
            ring_at: None,
            rings: 0,
            autoanswer: 0,
            verbose: true,
            quiet: false,
            x_level: 4,
            dcd_mode: 1,
            s_regs: default_s_regs(),
            relay_keepalive: None,
        }
    }

    /// Reset AT state to power-on defaults (ATZ / AT&F).
    fn reset_defaults(&mut self) {
        self.echo = true;
        self.verbose = true;
        self.quiet = false;
        self.x_level = 4;
        self.dcd_mode = 1;
        self.autoanswer = 0;
        self.s_regs = default_s_regs();
    }

    pub(in crate::telnet) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether carrier (DCD) is asserted for the guest.  A live connection
    /// asserts it; `AT&C0` forces it always on (DCD ignored), matching the
    /// physical modem's `&C` handling.
    pub(in crate::telnet) fn carrier_asserted(&self) -> bool {
        self.dcd_mode == 0 || self.conn.is_some()
    }

    /// True when the modem is idle enough to accept a new inbound call
    /// (command mode, not already ringing or connected).
    pub(in crate::telnet) fn can_answer(&self) -> bool {
        self.enabled && self.mode == Mode::Command && self.incoming.is_none() && self.conn.is_none()
    }

    /// Take an inbound `CPM@<ip>` call and start ringing the guest.
    pub(in crate::telnet) fn accept_incoming(&mut self, call: CpmIncomingCall) {
        if !self.can_answer() {
            return; // busy — let the call drop (caller sees BUSY/error)
        }
        self.incoming = Some(call);
        self.rings = 0;
        self.ring_at = Some(Instant::now()); // ring on the next service tick
    }

    /// Service one pump cycle: consume the bytes the guest wrote (`guest_tx`),
    /// act on them (AT commands in command mode, forward to the peer in online
    /// mode), poll the connection for incoming bytes, and return everything the
    /// guest should now read (result codes + received data).
    /// `rx_budget` is how many bytes the guest's RX ring can still accept;
    /// the peer poll reads no more than that, so a slow guest applies
    /// backpressure to the peer instead of overflowing the ring.
    pub(in crate::telnet) async fn service(&mut self, guest_tx: Vec<u8>, rx_budget: usize) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        // Ring a pending inbound call (RING to the guest + keep the caller
        // waiting); auto-answer if S0 is set.
        if self.incoming.is_some() && self.mode == Mode::Command {
            self.service_ring(&mut out).await;
        }
        // Dispatch per byte so a mode change mid-batch (an `ATD…` that dials
        // and flips to online) routes the remaining bytes correctly.
        for b in guest_tx {
            match self.mode {
                Mode::Command => self.feed_command_byte(b, &mut out).await,
                Mode::Online => self.feed_online_byte(b, &mut out).await,
            }
        }
        // Drain anything the peer has sent us while online, up to what the
        // guest's RX ring can still hold (backpressure).
        if self.mode == Mode::Online {
            self.poll_connection(&mut out, rx_budget).await;
        }
        out
    }

    /// Accumulate a command-mode byte; on CR, dispatch the line.
    async fn feed_command_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        if self.echo {
            out.push(b);
        }
        match b {
            b'\r' => {
                let line = std::mem::take(&mut self.line);
                self.dispatch(&line, out).await;
            }
            b'\n' => {} // ignore LF; CR terminates
            0x08 | 0x7F => {
                self.line.pop();
            }
            _ => {
                if self.line.len() < 128 {
                    self.line.push(b);
                }
            }
        }
    }

    /// Handle one complete AT command line, walking a (possibly chained) body
    /// such as `ATE0Q0V1X4S0=1` left to right and applying each command.  `D`
    /// (dial), `A` (answer) and `O` (online) are terminal — they consume the
    /// rest and produce their own result; everything else sets state and the
    /// line ends with `OK`.
    async fn dispatch(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let s: String = String::from_utf8_lossy(line).trim().to_ascii_uppercase();
        if s.is_empty() {
            return;
        }
        let Some(body) = s.strip_prefix("AT") else {
            self.result(out, "ERROR");
            return;
        };
        let b = body.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            i += 1;
            match c {
                b' ' => {}
                b'D' => {
                    // Dial consumes the rest of the line.
                    self.dial(&body[i..], out).await;
                    return;
                }
                b'A' => {
                    let _ = read_digit(b, &mut i);
                    self.answer_incoming(out).await;
                    return;
                }
                b'O' => {
                    let _ = read_digit(b, &mut i);
                    if self.conn.is_some() {
                        self.mode = Mode::Online;
                        self.result(out, "CONNECT");
                    } else {
                        self.result(out, "NO CARRIER");
                    }
                    return;
                }
                b'E' => self.echo = read_digit(b, &mut i) != 0,
                b'Q' => self.quiet = read_digit(b, &mut i) != 0,
                b'V' => self.verbose = read_digit(b, &mut i) != 0,
                b'X' => self.x_level = read_digit(b, &mut i),
                b'H' => {
                    let _ = read_digit(b, &mut i);
                    self.hangup(out, false);
                }
                b'Z' => {
                    let _ = read_digit(b, &mut i);
                    self.reset_defaults();
                }
                b'I' => {
                    let _ = read_digit(b, &mut i);
                    out.extend_from_slice(b"\r\nCP/M virtual modem (Ethernet Gateway)\r\n");
                }
                b'S' => self.parse_s_register(b, &mut i, out),
                b'&' if i < b.len() => {
                    let sub = b[i];
                    i += 1;
                    let n = read_digit(b, &mut i);
                    match sub {
                        b'C' => self.dcd_mode = n, // &C DCD handling
                        b'D' => {}                 // &D DTR: accepted, not modeled
                        b'F' => self.reset_defaults(), // &F factory reset
                        _ => {}
                    }
                }
                _ => {} // unknown: ignore leniently so a chain still succeeds
            }
        }
        self.result(out, "OK");
    }

    /// Dial an outbound call: a local serial Port A/B (peer-dial) or a TCP
    /// `host:port`.
    /// Carrier-wait timeout for a dial, from S7 (seconds); falls back to the
    /// default when S7 is 0.
    fn carrier_wait(&self) -> Duration {
        match self.s_regs[7] {
            0 => ANSWER_WAIT,
            s => Duration::from_secs(s as u64),
        }
    }

    async fn dial(&mut self, target: &str, out: &mut Vec<u8>) {
        // Strip a leading tone/pulse modifier.
        let t = target.trim();
        let t = t.strip_prefix(['T', 'P']).unwrap_or(t).trim();

        // Serial port: "A"/"B", optionally "A@<host>".  A bare label or a
        // local host rings the gateway's own port; a remote host targets a
        // port a slave registered with this gateway's crossbar (master).
        let label = t.split('@').next().unwrap_or("").trim();
        if let Some(id) = local_port(label) {
            // Peer-dialing a serial port is gated like the physical modem's
            // ATD <Port>@<host>: when the operator hasn't enabled it, behave
            // like a failed dial (no hint).  (ATDT to a TCP host below is not
            // peer-dial and stays ungated, matching the physical modem.)
            let cfg = crate::config::get_config();
            if !cfg.allow_peer_dial {
                self.result(out, "NO CARRIER");
                return;
            }
            let host = t.split_once('@').map(|x| x.1.trim()).filter(|h| !h.is_empty());
            match host {
                // Remote serial port.  On a slave, relay the address to the
                // master (which resolves/crossbars it); on a master/standalone,
                // claim it directly from the crossbar registry.
                Some(h) if !crate::serial::host_is_local_addr(h) => {
                    if cfg.gateway_role == "slave" {
                        let target = crate::relay::RelayTarget::Peer {
                            addr: format!("{label}@{h}"),
                        };
                        match crate::relay::connect_master_relay(
                            &cfg.slave_master_host,
                            cfg.slave_master_port,
                            &cfg.slave_master_username,
                            &cfg.slave_master_password,
                            &target,
                            "CPM",
                        )
                        .await
                        {
                            Ok(crate::relay::MasterRelay { _session, stream }) => {
                                self.relay_keepalive = Some(Box::new(_session));
                                self.conn = Some(Box::new(stream));
                                self.mode = Mode::Online;
                                self.result(out, "CONNECT");
                            }
                            Err(_) => self.result(out, "NO CARRIER"),
                        }
                    } else {
                        match h.parse::<std::net::IpAddr>() {
                            Ok(ip) => match crate::relay::claim_remote_peer(ip, label).await {
                                Some(dup) => {
                                    self.conn = Some(Box::new(dup));
                                    self.mode = Mode::Online;
                                    self.result(out, "CONNECT");
                                }
                                None => self.result(out, "NO CARRIER"),
                            },
                            Err(_) => self.result(out, "NO CARRIER"),
                        }
                    }
                }
                _ => match request_peer_call(id, self.carrier_wait()).await {
                    Ok(duplex) => {
                        self.conn = Some(Box::new(duplex));
                        self.mode = Mode::Online;
                        self.result(out, "CONNECT");
                    }
                    Err(PeerCallOutcome::Busy) => self.result(out, "BUSY"),
                    Err(PeerCallOutcome::NoAnswer) => self.result(out, "NO ANSWER"),
                    Err(_) => self.result(out, "NO CARRIER"),
                },
            }
            return;
        }

        // TCP host:port.
        if let Some((host, port)) = parse_host_port(t) {
            match tokio::net::TcpStream::connect((host.as_str(), port)).await {
                Ok(stream) => {
                    self.conn = Some(Box::new(stream));
                    self.mode = Mode::Online;
                    self.result(out, "CONNECT");
                }
                Err(_) => self.result(out, "NO CARRIER"),
            }
            return;
        }

        self.result(out, "NO CARRIER");
    }

    /// Online mode: forward one byte to the peer, tracking the (simplified)
    /// `+++` escape — three consecutive `+` returns to command mode with the
    /// call held.  Bytes are forwarded as they arrive (including `+`), so data
    /// containing a stray `+` isn't dropped.
    async fn feed_online_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        if b == b'+' {
            self.plus_run += 1;
        } else {
            self.plus_run = 0;
        }
        if let Some(conn) = self.conn.as_mut() {
            if conn.write_all(&[b]).await.is_err() {
                self.hangup(out, true);
                return;
            }
        }
        if self.plus_run >= 3 {
            self.plus_run = 0;
            self.mode = Mode::Command;
            self.result(out, "OK"); // escaped to command mode, call held
        }
    }

    /// Non-blocking-ish poll of the connection for received bytes, reading at
    /// most `rx_budget` bytes so a full guest RX ring leaves bytes in the
    /// socket/duplex (TCP / duplex backpressure) rather than losing them.
    async fn poll_connection(&mut self, out: &mut Vec<u8>, rx_budget: usize) {
        if rx_budget == 0 {
            return; // guest ring full — don't read, let the peer stall
        }
        let Some(conn) = self.conn.as_mut() else { return };
        let mut buf = [0u8; 1024];
        let cap = rx_budget.min(buf.len());
        match tokio::time::timeout(READ_POLL, conn.read(&mut buf[..cap])).await {
            Ok(Ok(0)) => self.hangup(out, true),      // peer closed
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
            Ok(Err(_)) => self.hangup(out, true),
            Err(_) => {} // timeout: nothing waiting
        }
    }

    /// Drop the connection and return to command mode.  `carrier_lost` emits
    /// NO CARRIER (peer hangup); a clean local `ATH` emits nothing extra.
    fn hangup(&mut self, out: &mut Vec<u8>, carrier_lost: bool) {
        self.conn = None;
        self.relay_keepalive = None; // drop the relay SSH session, if any
        self.mode = Mode::Command;
        self.plus_run = 0;
        if carrier_lost {
            self.result(out, "NO CARRIER");
        }
    }

    /// Emit RINGs for a pending inbound call on the RING cadence, keeping the
    /// caller waiting (progress `0`); auto-answer once S0 rings are reached.
    /// Drops the call if the caller has gone away.
    async fn service_ring(&mut self, out: &mut Vec<u8>) {
        let now = Instant::now();
        if self.ring_at.map(|t| now < t).unwrap_or(true) && self.rings > 0 {
            return; // not time for the next ring yet
        }
        // Clone the progress sender so we can `.await` it without holding a
        // borrow of `self.incoming` while we then mutate self.
        let prog = self.incoming.as_ref().map(|c| c.progress.clone());
        let Some(prog) = prog else { return };
        if prog.send(0).await.is_err() {
            // Caller gave up / cancelled — stop ringing.
            self.incoming = None;
            self.ring_at = None;
            self.rings = 0;
            return;
        }
        self.result(out, "RING");
        self.rings = self.rings.saturating_add(1);
        self.ring_at = Some(now + RING_EVERY);
        if self.autoanswer > 0 && self.rings >= self.autoanswer {
            self.answer_incoming(out).await;
        }
    }

    /// Answer a pending inbound call: acknowledge the caller (progress `1`),
    /// go online against its duplex, and emit CONNECT.  A no-op (NO CARRIER)
    /// if nothing is ringing.
    async fn answer_incoming(&mut self, out: &mut Vec<u8>) {
        let Some(call) = self.incoming.take() else {
            self.result(out, "NO CARRIER");
            return;
        };
        let _ = call.progress.send(1).await;
        self.conn = Some(Box::new(call.bridge));
        self.mode = Mode::Online;
        self.ring_at = None;
        self.rings = 0;
        self.result(out, "CONNECT");
    }

    /// Parse an `S<n>` clause at `b[*i-1]=='S'`: `S<n>=<v>` sets a register,
    /// `S<n>?` queries it (emitting the value), bare `S<n>` selects it (no-op).
    fn parse_s_register(&mut self, b: &[u8], i: &mut usize, out: &mut Vec<u8>) {
        let reg = read_number(b, i) as usize;
        if *i < b.len() && b[*i] == b'=' {
            *i += 1;
            let val = read_number(b, i).min(255) as u8;
            if reg < self.s_regs.len() {
                self.s_regs[reg] = val;
                if reg == 0 {
                    self.autoanswer = val; // S0 mirrors auto-answer
                }
            }
        } else if *i < b.len() && b[*i] == b'?' {
            *i += 1;
            let v = self.s_regs.get(reg).copied().unwrap_or(0);
            out.extend_from_slice(format!("\r\n{v:03}\r\n").as_bytes());
        }
    }

    /// Emit a Hayes result code, honouring `ATQ` (quiet), `ATV` (verbose vs
    /// numeric) and `ATX` (low levels collapse BUSY / NO ANSWER to NO CARRIER,
    /// as a modem without call-progress detection does).
    fn result(&self, out: &mut Vec<u8>, code: &str) {
        if self.quiet {
            return;
        }
        let code = if self.x_level == 0 && matches!(code, "BUSY" | "NO ANSWER") {
            "NO CARRIER"
        } else {
            code
        };
        if self.verbose {
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(code.as_bytes());
            out.extend_from_slice(b"\r\n");
        } else {
            let n: u8 = match code {
                "OK" => b'0',
                "CONNECT" => b'1',
                "RING" => b'2',
                "NO CARRIER" => b'3',
                "BUSY" => b'7',
                "NO ANSWER" => b'8',
                _ => b'4', // ERROR and anything unmapped
            };
            out.push(n);
            out.push(b'\r');
        }
    }
}

/// Power-on S-register defaults (S0..S27); mirrors a typical Hayes modem.
fn default_s_regs() -> [u8; 28] {
    let mut s = [0u8; 28];
    s[2] = 43; // escape char '+'
    s[3] = 13; // CR
    s[4] = 10; // LF
    s[5] = 8; // backspace
    s[7] = 50; // wait for carrier (seconds)
    s[12] = 50; // escape guard time (1/50 s)
    s
}

/// Read a single decimal digit at `b[*i]` (advancing `i`); 0 if none.
fn read_digit(b: &[u8], i: &mut usize) -> u8 {
    if *i < b.len() && b[*i].is_ascii_digit() {
        let v = b[*i] - b'0';
        *i += 1;
        v
    } else {
        0
    }
}

/// Read a (multi-digit) decimal number at `b[*i]` (advancing `i`); 0 if none.
fn read_number(b: &[u8], i: &mut usize) -> u16 {
    let mut n: u16 = 0;
    while *i < b.len() && b[*i].is_ascii_digit() {
        n = n.saturating_mul(10).saturating_add((b[*i] - b'0') as u16);
        *i += 1;
    }
    n
}

/// Map a dial-string label to a local serial port, if it names one.
fn local_port(label: &str) -> Option<SerialPortId> {
    match label {
        "A" | "PORTA" => Some(SerialPortId::A),
        "B" | "PORTB" => Some(SerialPortId::B),
        _ => None,
    }
}

/// Parse a `host:port` dial target.  Requires an explicit port.
fn parse_host_port(t: &str) -> Option<(String, u16)> {
    let (host, port) = t.rsplit_once(':')?;
    let host = host.trim();
    let port: u16 = port.trim().parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_disabled_is_inert() {
        let mut m = CpmModem::new(false);
        assert!(m.service(b"ATZ\r".to_vec(), 65536).await.is_empty());
    }

    #[tokio::test]
    async fn test_bare_at_ok_and_echo() {
        let mut m = CpmModem::new(true);
        let out = m.service(b"AT\r".to_vec(), 65536).await;
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("AT")); // echoed
        assert!(s.contains("OK"));
    }

    #[tokio::test]
    async fn test_non_at_line_errors() {
        let mut m = CpmModem::new(true);
        let out = m.service(b"HELLO\r".to_vec(), 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("ERROR"));
    }

    #[tokio::test]
    async fn test_echo_toggle_and_init_string_ok() {
        let mut m = CpmModem::new(true);
        let _ = m.service(b"ATE0\r".to_vec(), 65536).await;
        // Echo now off: an init string returns OK without echoing the command.
        let out = m.service(b"ATQ0V1S0=0\r".to_vec(), 65536).await;
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("ATQ0")); // not echoed
        assert!(s.contains("OK"));
    }

    #[tokio::test]
    async fn test_tcp_dial_to_dead_port_reports_no_carrier() {
        let mut m = CpmModem::new(true);
        // Port 1 on loopback is (almost certainly) closed → NO CARRIER.
        let out = m.service(b"ATDT 127.0.0.1:1\r".to_vec(), 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("NO CARRIER"));
    }

    #[tokio::test]
    async fn test_inbound_ring_answer_and_data() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Build an inbound call: `far` is the modem's bridge; `near` is the
        // caller's end; `rx` receives the progress signals.
        let (mut near, far) = tokio::io::duplex(1024);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(8);
        let mut m = CpmModem::new(true);

        assert!(m.can_answer());
        m.accept_incoming(CpmIncomingCall { bridge: far, progress: tx });
        assert!(!m.can_answer()); // busy ringing now

        // A service tick rings the guest and signals the caller (progress 0).
        let out = m.service(vec![], 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("RING"));
        assert_eq!(rx.recv().await, Some(0));

        // Guest answers with ATA → CONNECT, caller gets progress 1, online.
        let out = m.service(b"ATA\r".to_vec(), 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("CONNECT"));
        assert_eq!(rx.recv().await, Some(1));

        // Online: a guest write reaches the caller end.
        let _ = m.service(b"hi".to_vec(), 65536).await;
        let mut buf = [0u8; 2];
        near.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");

        // A byte from the caller is delivered to the guest on the next tick.
        near.write_all(b"yo").await.unwrap();
        let out = m.service(vec![], 65536).await;
        assert!(out.windows(2).any(|w| w == b"yo"));
    }

    #[tokio::test]
    async fn test_rx_budget_zero_applies_backpressure() {
        use tokio::io::AsyncWriteExt;
        let (mut near, far) = tokio::io::duplex(1024);
        let (tx, _rx) = tokio::sync::mpsc::channel::<u8>(8);
        let mut m = CpmModem::new(true);
        m.accept_incoming(CpmIncomingCall { bridge: far, progress: tx });
        let _ = m.service(vec![], 65536).await; // ring
        let _ = m.service(b"ATA\r".to_vec(), 65536).await; // answer → online
        near.write_all(b"data").await.unwrap();
        // With a full guest ring (budget 0) the peer byte is NOT read.
        let out = m.service(vec![], 0).await;
        assert!(!out.windows(4).any(|w| w == b"data"));
        // Once room frees up, the same byte is delivered (still in the duplex).
        let out = m.service(vec![], 65536).await;
        assert!(out.windows(4).any(|w| w == b"data"));
    }

    #[tokio::test]
    async fn test_numeric_result_codes_and_quiet() {
        let mut m = CpmModem::new(true);
        // Echo off so only result codes appear in the output.
        let _ = m.service(b"ATE0\r".to_vec(), 65536).await;
        // ATV0: numeric result codes ("0\r" for OK).
        let out = m.service(b"ATV0\r".to_vec(), 65536).await;
        assert_eq!(out, b"0\r");
        // ATQ1: result codes suppressed entirely (echo off ⇒ nothing at all).
        let out = m.service(b"ATQ1\r".to_vec(), 65536).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn test_chained_at_init_string_applies_each_command() {
        let mut m = CpmModem::new(true);
        // A single chained init string: echo off, quiet off, verbose, X4, S0=2.
        let out = m.service(b"ATE0Q0V1X4S0=2\r".to_vec(), 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("OK"));
        assert!(!m.echo); // E0 applied
        assert!(!m.quiet); // Q0 applied
        assert!(m.verbose); // V1 applied
        assert_eq!(m.x_level, 4); // X4 applied
        assert_eq!(m.autoanswer, 2); // S0=2 applied
        assert_eq!(m.s_regs[0], 2);
    }

    #[tokio::test]
    async fn test_s_register_set_and_query() {
        let mut m = CpmModem::new(true);
        let _ = m.service(b"ATS7=45\r".to_vec(), 65536).await;
        assert_eq!(m.s_regs[7], 45);
        let out = m.service(b"ATS7?\r".to_vec(), 65536).await;
        assert!(String::from_utf8_lossy(&out).contains("045"));
    }

    #[tokio::test]
    async fn test_atz_and_atf_reset_defaults() {
        let mut m = CpmModem::new(true);
        let _ = m.service(b"ATE0S0=5\r".to_vec(), 65536).await;
        assert!(!m.echo);
        let _ = m.service(b"ATZ\r".to_vec(), 65536).await;
        assert!(m.echo); // reset to power-on
        assert_eq!(m.autoanswer, 0);
        let _ = m.service(b"ATE0\r".to_vec(), 65536).await;
        let _ = m.service(b"AT&F\r".to_vec(), 65536).await;
        assert!(m.echo); // factory reset too
    }

    #[tokio::test]
    async fn test_and_c_sets_dcd_mode() {
        let mut m = CpmModem::new(true);
        let _ = m.service(b"AT&C0\r".to_vec(), 65536).await;
        assert_eq!(m.dcd_mode, 0);
        let _ = m.service(b"AT&C1\r".to_vec(), 65536).await;
        assert_eq!(m.dcd_mode, 1);
    }

    #[test]
    fn test_dial_target_parsing() {
        assert_eq!(local_port("A"), Some(SerialPortId::A));
        assert_eq!(local_port("B"), Some(SerialPortId::B));
        assert_eq!(local_port("Z"), None);
        assert_eq!(parse_host_port("bbs.example.com:23"), Some(("bbs.example.com".into(), 23)));
        assert_eq!(parse_host_port("noport"), None);
    }
}
