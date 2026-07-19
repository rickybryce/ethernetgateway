//! Virtual-modem "brain" for the CP/M emulator (Flavor B) — Slice A: outbound.
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
//! Slice A covers **outbound** calls.  Being *dialable* as `CPM@<ip>` (inbound
//! RING) is Slice B.

use crate::config::SerialPortId;
use crate::serial::{request_peer_call, PeerCallOutcome};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How long to wait for a dialed peer to answer.
const ANSWER_WAIT: Duration = Duration::from_secs(30);
/// Per-service poll window for inbound bytes when online.
const READ_POLL: Duration = Duration::from_millis(3);

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
        }
    }

    pub(in crate::telnet) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Service one pump cycle: consume the bytes the guest wrote (`guest_tx`),
    /// act on them (AT commands in command mode, forward to the peer in online
    /// mode), poll the connection for incoming bytes, and return everything the
    /// guest should now read (result codes + received data).
    pub(in crate::telnet) async fn service(&mut self, guest_tx: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.enabled {
            return out;
        }
        match self.mode {
            Mode::Command => {
                for b in guest_tx {
                    self.feed_command_byte(b, &mut out).await;
                }
            }
            Mode::Online => {
                self.feed_online(&guest_tx, &mut out).await;
            }
        }
        // Drain anything the peer has sent us while online.
        if self.mode == Mode::Online {
            self.poll_connection(&mut out).await;
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

    /// Handle one complete AT command line.
    async fn dispatch(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let s: String = String::from_utf8_lossy(line).trim().to_ascii_uppercase();
        if s.is_empty() {
            return;
        }
        let Some(body) = s.strip_prefix("AT") else {
            self.result(out, "ERROR");
            return;
        };
        // Dial is the one command that changes connection state.
        if let Some(rest) = body.strip_prefix('D') {
            self.dial(rest, out).await;
            return;
        }
        match body {
            "" => self.result(out, "OK"),          // bare AT
            "E0" => { self.echo = false; self.result(out, "OK"); }
            "E1" | "E" => { self.echo = true; self.result(out, "OK"); }
            "H" | "H0" => { self.hangup(out, false); self.result(out, "OK"); }
            "O" | "O0" => {
                if self.conn.is_some() {
                    self.mode = Mode::Online;
                    self.result(out, "CONNECT");
                } else {
                    self.result(out, "NO CARRIER");
                }
            }
            // Reset / config strings we don't model: accept leniently so a
            // program's init string (ATZ, AT&F, ATE0Q1V1, ATS0=0, …) succeeds.
            _ => self.result(out, "OK"),
        }
    }

    /// Dial an outbound call: a local serial Port A/B (peer-dial) or a TCP
    /// `host:port`.
    async fn dial(&mut self, target: &str, out: &mut Vec<u8>) {
        // Strip a leading tone/pulse modifier.
        let t = target.trim();
        let t = t.strip_prefix(['T', 'P']).unwrap_or(t).trim();

        // Local serial port: "A"/"B", optionally "A@<host>" (Slice A dials the
        // local port; remote-relay routing over @<host> is Slice B).
        let label = t.split('@').next().unwrap_or("").trim();
        if let Some(id) = local_port(label) {
            match request_peer_call(id, ANSWER_WAIT).await {
                Ok(duplex) => {
                    self.conn = Some(Box::new(duplex));
                    self.mode = Mode::Online;
                    self.result(out, "CONNECT");
                }
                Err(PeerCallOutcome::Busy) => self.result(out, "BUSY"),
                Err(PeerCallOutcome::NoAnswer) => self.result(out, "NO ANSWER"),
                Err(_) => self.result(out, "NO CARRIER"),
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

    /// Online mode: watch for the `+++` escape, else forward to the peer.
    async fn feed_online(&mut self, guest_tx: &[u8], out: &mut Vec<u8>) {
        for &b in guest_tx {
            if b == b'+' {
                self.plus_run += 1;
                if self.plus_run >= 3 {
                    self.plus_run = 0;
                    self.mode = Mode::Command;
                    self.result(out, "OK"); // back to command mode, call held
                    return;
                }
            } else {
                self.plus_run = 0;
            }
        }
        if let Some(conn) = self.conn.as_mut() {
            if conn.write_all(guest_tx).await.is_err() {
                self.hangup(out, true);
            }
        }
    }

    /// Non-blocking-ish poll of the connection for received bytes.
    async fn poll_connection(&mut self, out: &mut Vec<u8>) {
        let Some(conn) = self.conn.as_mut() else { return };
        let mut buf = [0u8; 1024];
        match tokio::time::timeout(READ_POLL, conn.read(&mut buf)).await {
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
        self.mode = Mode::Command;
        self.plus_run = 0;
        if carrier_lost {
            self.result(out, "NO CARRIER");
        }
    }

    /// Emit a Hayes result code in verbose form (`\r\n<CODE>\r\n`).
    fn result(&self, out: &mut Vec<u8>, code: &str) {
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(code.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
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
        assert!(m.service(b"ATZ\r".to_vec()).await.is_empty());
    }

    #[tokio::test]
    async fn test_bare_at_ok_and_echo() {
        let mut m = CpmModem::new(true);
        let out = m.service(b"AT\r".to_vec()).await;
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("AT")); // echoed
        assert!(s.contains("OK"));
    }

    #[tokio::test]
    async fn test_non_at_line_errors() {
        let mut m = CpmModem::new(true);
        let out = m.service(b"HELLO\r".to_vec()).await;
        assert!(String::from_utf8_lossy(&out).contains("ERROR"));
    }

    #[tokio::test]
    async fn test_echo_toggle_and_init_string_ok() {
        let mut m = CpmModem::new(true);
        let _ = m.service(b"ATE0\r".to_vec()).await;
        // Echo now off: an init string returns OK without echoing the command.
        let out = m.service(b"ATQ0V1S0=0\r".to_vec()).await;
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("ATQ0")); // not echoed
        assert!(s.contains("OK"));
    }

    #[tokio::test]
    async fn test_tcp_dial_to_dead_port_reports_no_carrier() {
        let mut m = CpmModem::new(true);
        // Port 1 on loopback is (almost certainly) closed → NO CARRIER.
        let out = m.service(b"ATDT 127.0.0.1:1\r".to_vec()).await;
        assert!(String::from_utf8_lossy(&out).contains("NO CARRIER"));
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
