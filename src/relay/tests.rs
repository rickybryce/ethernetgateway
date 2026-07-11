//! Loopback test for the master/slave relay (Phase 1).
//!
//! Wires the master-side intake (`run_master_relay_session`) to an
//! in-process [`tokio::io::duplex`] socket and drives the far end as if it
//! were the remote serial device a slave is bridging.  This proves the
//! master accepts a relay stream and runs the **full session machinery**
//! over it — terminal detection, the main menu, and a clean quit — with
//! **raw serial semantics** (no telnet IAC interpretation) end to end.
//!
//! The real slave-side pump (`serial::online_mode_duplex`, now generic
//! over the async transport) keeps its existing in-process coverage via
//! the modem dial tests; Phase 2's SSH transport adds the over-the-wire
//! integration test.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    claim_remote_peer, parse_relay_command, parse_remote_peer_addr, register_remote_port,
    run_master_relay_dial, run_master_relay_session, ParsedRelay, RelayConnectError, RelayTarget,
    RELAY_ACTIVATE_BYTE,
};

/// Enable the `allow_peer_dial` gate that `run_master_relay_dial` (onward
/// dial, M-7) and `run_master_relay_peer` now require.  These tests drive
/// those paths directly, so without the opt-in the relay would refuse and
/// immediately shut the stream down (a transfer test would then hang waiting
/// for bytes that never arrive).  No test asserts the flag is *off*, so
/// setting it here is safe under parallel execution; we read-modify-write to
/// preserve any other fields a concurrent test may have set.
fn enable_peer_dial() {
    let mut cfg = crate::config::get_config();
    cfg.allow_peer_dial = true;
    crate::config::set_config_for_test(cfg);
}

/// The connect-error classes carry their detail through `Display`/`message`
/// (the slave reconnect loop, §9 #14, formats them into its log + chooses a
/// backoff by variant).
#[test]
fn test_relay_connect_error_message_and_display() {
    let n = RelayConnectError::Network("unreachable".into());
    let a = RelayConnectError::Auth("bad creds".into());
    let r = RelayConnectError::Refused("standalone".into());
    assert_eq!(n.message(), "unreachable");
    assert_eq!(a.message(), "bad creds");
    assert_eq!(r.message(), "standalone");
    // Display mirrors message() so existing `{}` log sites keep working.
    assert_eq!(format!("{}", a), "bad creds");
}

// ─── §9 relay hello / protocol-version handshake ─────────────

/// The wire hello is the "EGR" magic plus the current protocol version.
/// Value-locked so an accidental byte/version change is caught (both ends
/// share this constant, so a change would silently break every relay).
#[test]
fn test_relay_hello_bytes() {
    use super::{RELAY_HELLO, RELAY_PROTOCOL_VERSION};
    assert_eq!(&RELAY_HELLO[..3], b"EGR");
    assert_eq!(RELAY_HELLO[3], RELAY_PROTOCOL_VERSION);
    assert_eq!(RELAY_PROTOCOL_VERSION, 1, "bump deliberately on a wire change");
}

/// A valid hello (what the master writes on accept) is accepted.
#[tokio::test]
async fn test_read_relay_hello_accepts_valid() {
    let (mut master, mut slave) = tokio::io::duplex(64);
    master.write_all(&super::RELAY_HELLO).await.unwrap();
    assert!(super::read_relay_hello(&mut slave).await.is_ok());
}

/// A refusing master accepts the channel-open but never writes the hello,
/// so the slave sees EOF (channel closed) and classifies it `Refused` —
/// the fix for the smoke-test finding (russh `exec()` returns Ok even on
/// the master's `channel_failure`, so absence of the hello is the signal).
#[tokio::test]
async fn test_read_relay_hello_eof_is_refused() {
    let (master, mut slave) = tokio::io::duplex(64);
    drop(master); // master refused: channel open, no hello, then closed
    match super::read_relay_hello(&mut slave).await {
        Err(RelayConnectError::Refused(_)) => {}
        other => panic!("expected Refused on missing hello, got {:?}", other),
    }
}

/// A version-skewed master fails cleanly (Refused, with an upgrade hint)
/// rather than desyncing the session.
#[tokio::test]
async fn test_read_relay_hello_version_mismatch() {
    let (mut master, mut slave) = tokio::io::duplex(64);
    master.write_all(b"EGR\x63").await.unwrap(); // magic OK, version 99
    match super::read_relay_hello(&mut slave).await {
        Err(RelayConnectError::Refused(m)) => {
            assert!(m.contains("version mismatch"), "got: {}", m)
        }
        other => panic!("expected Refused version mismatch, got {:?}", other),
    }
}

/// Bytes that aren't our magic (a non-relay endpoint, or a pre-handshake
/// build that sent session data first) are rejected, not misread as data.
#[tokio::test]
async fn test_read_relay_hello_bad_magic() {
    let (mut master, mut slave) = tokio::io::duplex(64);
    master.write_all(b"\r\nPr").await.unwrap(); // e.g. a telnet prompt
    match super::read_relay_hello(&mut slave).await {
        Err(RelayConnectError::Refused(_)) => {}
        other => panic!("expected Refused on bad magic, got {:?}", other),
    }
}

/// Read from `dev` into `acc` until `needle` appears in the accumulated
/// (lossy-UTF-8) output, or the overall deadline elapses.  Returns true if
/// the needle was seen.  Tolerates the byte-at-a-time, sleep-laced output
/// of terminal detection by reading in small chunks under a per-read
/// timeout and re-checking after each chunk.
async fn read_until<R>(dev: &mut R, acc: &mut Vec<u8>, needle: &str) -> bool
where
    R: AsyncReadExt + Unpin,
{
    let deadline = Duration::from_secs(10);
    let mut buf = [0u8; 256];
    let result = tokio::time::timeout(deadline, async {
        loop {
            // Fast path: already buffered.
            if String::from_utf8_lossy(acc).contains(needle) {
                return true;
            }
            match dev.read(&mut buf).await {
                Ok(0) => return false, // EOF before needle
                Ok(n) => acc.extend_from_slice(&buf[..n]),
                Err(_) => return false,
            }
        }
    })
    .await;
    matches!(result, Ok(true))
}

/// Full loopback: a relay stream handed to `run_master_relay_session`
/// drives a complete session (detect → menu → quit) over raw bytes.
#[tokio::test]
async fn test_master_relay_runs_full_session_over_loopback() {
    // The relay transport: one end is the master's intake, the other is
    // the test playing the remote device.
    let (master_stream, device_stream) = tokio::io::duplex(64 * 1024);
    let (master_read, master_write) = tokio::io::split(master_stream);
    let (mut dev_read, mut dev_write) = tokio::io::split(device_stream);

    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let lockouts = Arc::new(Mutex::new(HashMap::new()));
    let session_writers = Arc::new(tokio::sync::Mutex::new(Vec::new()));

    let master = tokio::spawn(run_master_relay_session(
        Box::new(master_read),
        Box::new(master_write),
        Some("192.168.1.50".parse().unwrap()),
        shutdown,
        restart,
        session_writers,
        lockouts,
    ));

    let mut acc: Vec<u8> = Vec::new();

    // 1. Terminal detection: the master prompts for the BACKSPACE probe.
    assert!(
        read_until(&mut dev_read, &mut acc, "Press BACKSPACE").await,
        "master should prompt for terminal detection over the relay; got: {}",
        String::from_utf8_lossy(&acc)
    );
    // A printable byte (not 0x14/0x08/0x7F) selects ASCII — plain text,
    // no color sequences to complicate banner matching.
    dev_write.write_all(b"?").await.unwrap();

    assert!(
        read_until(&mut dev_read, &mut acc, "Terminal detected: ASCII").await,
        "master should detect ASCII; got: {}",
        String::from_utf8_lossy(&acc)
    );

    // 2. Color prompt — decline, with a raw 0xFF (telnet IAC) prefix as a
    // transparency probe.  The color loop ignores any non-Y/N byte, so on
    // a relay session (raw serial semantics, IAC NOT filtered) the 0xFF is
    // skipped and the following 'N' is honored.  If IAC were wrongly
    // filtered the 0xFF would swallow the 'N' as a telnet command byte,
    // the color prompt would never be answered, and this step would hang
    // to the read deadline.
    assert!(
        read_until(&mut dev_read, &mut acc, "color? (Y/N)").await,
        "master should ask the color question; got: {}",
        String::from_utf8_lossy(&acc)
    );
    dev_write.write_all(&[0xFF, b'N']).await.unwrap();

    // 3. The main menu renders over the relay — the heart of P1: the
    // master handed the relay stream to the real session machinery.
    assert!(
        read_until(&mut dev_read, &mut acc, "ETHERNET GATEWAY").await,
        "master should render the main menu over the relay; got: {}",
        String::from_utf8_lossy(&acc)
    );

    // 4. Quit from the main menu.
    dev_write.write_all(b"x").await.unwrap();

    assert!(
        read_until(&mut dev_read, &mut acc, "John 3:16").await,
        "quitting should print the farewell over the relay; got: {}",
        String::from_utf8_lossy(&acc)
    );

    // 5. The session ends, its writer is shut down, and the device sees a
    // clean EOF — i.e. the relay closes.
    let mut tail = [0u8; 64];
    let eof = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match dev_read.read(&mut tail).await {
                Ok(0) => return true,
                Ok(_) => continue, // drain remaining farewell bytes
                Err(_) => return false,
            }
        }
    })
    .await;
    assert!(matches!(eof, Ok(true)), "relay should close cleanly at EOF");

    // The master task should have returned.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), master)
            .await
            .is_ok(),
        "master relay session task should complete after quit"
    );
}

/// The slave's `RelayTarget::exec_command` and the master's
/// `parse_relay_command` are two halves of one wire contract; round-trip
/// them so the two gateways can't drift apart.
#[test]
fn test_relay_command_round_trip() {
    // Menu target.
    let cmd = RelayTarget::Menu.exec_command("A");
    assert_eq!(cmd, "serial-relay A menu");
    assert_eq!(
        parse_relay_command(&cmd),
        Some(ParsedRelay {
            port_label: "A".into(),
            dial: None,
            peer: None,
        })
    );

    // Onward-dial target.
    let target = RelayTarget::Dial {
        host: "bbs.example.com".into(),
        port: 6400,
    };
    let cmd = target.exec_command("B");
    assert_eq!(cmd, "serial-relay B dial bbs.example.com:6400");
    assert_eq!(
        parse_relay_command(&cmd),
        Some(ParsedRelay {
            port_label: "B".into(),
            dial: Some(("bbs.example.com".into(), 6400)),
            peer: None,
        })
    );

    // Peer-dial target (Phase 2): `<Port>@<host>` round-trips verbatim.
    let target = RelayTarget::Peer { addr: "B@192.168.1.50".into() };
    let cmd = target.exec_command("A");
    assert_eq!(cmd, "serial-relay A peer B@192.168.1.50");
    assert_eq!(
        parse_relay_command(&cmd),
        Some(ParsedRelay {
            port_label: "A".into(),
            dial: None,
            peer: Some("B@192.168.1.50".into()),
        })
    );

    // An IPv6-ish host:port still splits on the LAST colon.
    let cmd = RelayTarget::Dial {
        host: "10.0.0.5".into(),
        port: 23,
    }
    .exec_command("A");
    assert_eq!(
        parse_relay_command(&cmd).unwrap().dial,
        Some(("10.0.0.5".into(), 23))
    );
}

/// The master refuses anything that isn't a well-formed relay command —
/// it is not a general command-exec shell.
#[test]
fn test_parse_relay_command_rejects_garbage() {
    assert_eq!(parse_relay_command(""), None);
    assert_eq!(parse_relay_command("rm -rf /"), None);
    assert_eq!(parse_relay_command("serial-relay A bogus"), None);
    assert_eq!(parse_relay_command("serial-relay A dial nohostport"), None);
    assert_eq!(parse_relay_command("serial-relay A dial host:0"), None);
    assert_eq!(parse_relay_command("serial-relay A dial host:notaport"), None);
    // `peer` with no address is malformed.
    assert_eq!(parse_relay_command("serial-relay A peer"), None);
    // Missing port label defaults to "?" but is still a valid menu relay.
    assert_eq!(
        parse_relay_command("serial-relay"),
        Some(ParsedRelay {
            port_label: "?".into(),
            dial: None,
            peer: None,
        })
    );
}

/// Phase 2b: parse a peer-dial address into a remote-registry key.
#[test]
fn test_parse_remote_peer_addr() {
    use std::net::IpAddr;
    assert_eq!(
        parse_remote_peer_addr("B@192.168.1.50"),
        Some(("192.168.1.50".parse::<IpAddr>().unwrap(), "B".to_string()))
    );
    // Label case-folds to upper (registry keys are uppercase).
    assert_eq!(
        parse_remote_peer_addr("a@10.0.0.9"),
        Some(("10.0.0.9".parse::<IpAddr>().unwrap(), "A".to_string()))
    );
    // Bracketed IPv6 literal.
    assert_eq!(
        parse_remote_peer_addr("B@[::1]"),
        Some(("::1".parse::<IpAddr>().unwrap(), "B".to_string()))
    );
    // A hostname (not an IP) can't be a registry key; a bad/absent label.
    assert_eq!(parse_remote_peer_addr("B@example.com"), None);
    assert_eq!(parse_remote_peer_addr("C@10.0.0.1"), None);
    assert_eq!(parse_remote_peer_addr("192.168.1.1"), None);
}

/// Phase 2b: claiming a registered remote port removes it, writes the
/// activate byte the slave waits for, and hands back the master's stream.
#[tokio::test]
async fn test_claim_remote_peer_activates() {
    use std::net::IpAddr;
    // TEST-NET-2, distinct from other tests, so the global registry key
    // can't collide under parallel execution.
    let ip: IpAddr = "198.51.100.7".parse().unwrap();
    let (mut device_end, master_end) = tokio::io::duplex(64);
    let _gen = register_remote_port(ip, "A".to_string(), master_end);

    let claimed = claim_remote_peer(ip, "A").await;
    assert!(claimed.is_some(), "a registered port is claimable");
    let mut buf = [0u8; 1];
    device_end.read_exact(&mut buf).await.unwrap();
    assert_eq!(buf[0], RELAY_ACTIVATE_BYTE, "slave receives the activate byte");
    // The claim removed it — a second claim finds nothing.
    assert!(claim_remote_peer(ip, "A").await.is_none());
}

/// Slave link-state (§9 #10) round-trips through the per-port atomic and
/// its labels are stable (the status screen prints them).  Uses port index
/// 1 (B) so it can't race another test on index 0.
#[test]
fn test_slave_link_state_roundtrip() {
    use super::{set_slave_link, slave_link_state, SlaveLinkState};
    for st in [
        SlaveLinkState::Down,
        SlaveLinkState::Connecting,
        SlaveLinkState::Registered,
        SlaveLinkState::Bridging,
    ] {
        set_slave_link(1, st);
        assert_eq!(slave_link_state(1), st);
    }
    assert_eq!(SlaveLinkState::Down.label(), "down");
    assert_eq!(SlaveLinkState::Registered.label(), "registered");
    assert_eq!(SlaveLinkState::Bridging.label(), "bridging");
    // Out-of-range index is a no-op read → Down (only A/B exist).
    assert_eq!(slave_link_state(9), SlaveLinkState::Down);
    // Leave index 1 back at Down so other tests see a clean slate.
    set_slave_link(1, SlaveLinkState::Down);
}

/// The console-mode remote-port registry (§9 #12): register, list,
/// claim-removes, and a second claim finds nothing.  Uses a TEST-NET-3 IP
/// (203.0.113.x) so it can't collide with another test's registry keys.
#[tokio::test]
async fn test_remote_port_registry() {
    use std::net::IpAddr;
    let ip: IpAddr = "203.0.113.7".parse().unwrap();
    let (master_a, _dev_a) = tokio::io::duplex(64);
    let (master_b, _dev_b) = tokio::io::duplex(64);

    let _ = super::register_remote_port(ip, "A".into(), master_a);
    let _ = super::register_remote_port(ip, "B".into(), master_b);

    let listed = super::list_remote_ports();
    assert!(listed.contains(&(ip, "A".to_string())));
    assert!(listed.contains(&(ip, "B".to_string())));

    // Claiming removes the entry; a second claim finds nothing.
    assert!(super::remove_remote_port(ip, "A").is_some());
    assert!(super::remove_remote_port(ip, "A").is_none());
    assert!(!super::list_remote_ports().contains(&(ip, "A".to_string())));

    // Clean up so the global registry doesn't leak into other tests.
    let _ = super::remove_remote_port(ip, "B");
    assert!(!super::list_remote_ports().contains(&(ip, "B".to_string())));
}

/// Re-registration race guard (§9 #12): if a slave re-registers the SAME
/// `(IP, label)` on a fresh channel before the master observes the old
/// channel close, the old channel's generation-stamped teardown must NOT
/// evict the new, live registration.  Only a matching generation removes;
/// a picker claim stays generation-agnostic.  TEST-NET-3 IP so it can't
/// collide with another test's registry keys.
#[tokio::test]
async fn test_remote_port_reregister_generation_guard() {
    use std::net::IpAddr;
    let ip: IpAddr = "203.0.113.9".parse().unwrap();

    // First registration (old channel) -> gen0.
    let (master_old, _dev_old) = tokio::io::duplex(64);
    let gen0 = super::register_remote_port(ip, "A".into(), master_old);

    // Slave re-registers "A" on a new channel before the old one tore
    // down -> gen1 overwrites the map entry.
    let (master_new, _dev_new) = tokio::io::duplex(64);
    let gen1 = super::register_remote_port(ip, "A".into(), master_new);
    assert_ne!(gen0, gen1, "each registration gets a fresh generation");

    // The OLD channel tears down: removing by its stale generation must be
    // a no-op (the live entry is gen1), and the live registration survives.
    assert!(
        super::remove_remote_port_gen(ip, "A", gen0).is_none(),
        "stale-generation teardown must not evict the newer registration"
    );
    assert!(
        super::list_remote_ports().contains(&(ip, "A".to_string())),
        "the live (gen1) registration must survive the old channel teardown"
    );

    // The new channel's own teardown (matching generation) removes it.
    assert!(super::remove_remote_port_gen(ip, "A", gen1).is_some());
    assert!(!super::list_remote_ports().contains(&(ip, "A".to_string())));

    // A picker claim ignores generation — it takes whatever is current.
    let (master_c, _dev_c) = tokio::io::duplex(64);
    let _gen2 = super::register_remote_port(ip, "A".into(), master_c);
    assert!(super::remove_remote_port(ip, "A").is_some());
    assert!(super::remove_remote_port(ip, "A").is_none());
}

/// Onward dial (Model B): `run_master_relay_dial` connects to the target
/// and pipes the relay channel through transparently in both directions.
#[tokio::test]
async fn test_master_relay_dial_pipes_both_ways() {
    enable_peer_dial();
    // A fake "BBS" that echoes everything it receives.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });

    // The relay channel, modeled as a duplex: master end ↔ device end.
    // run_master_relay_dial takes the WHOLE stream (copy_bidirectional).
    let (master_end, device_end) = tokio::io::duplex(8192);
    let (mut d_read, mut d_write) = tokio::io::split(device_end);

    let dialer = tokio::spawn(run_master_relay_dial(
        master_end,
        "127.0.0.1".to_string(),
        addr.port(),
    ));

    // Device → master → BBS → master → device.
    d_write.write_all(b"PING").await.unwrap();
    let mut got = Vec::new();
    let echoed = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 64];
        while !got.windows(4).any(|w| w == b"PING") {
            match d_read.read(&mut buf).await {
                Ok(0) => return false,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => return false,
            }
        }
        true
    })
    .await;
    assert!(
        matches!(echoed, Ok(true)),
        "onward-dial should echo PING back through the relay; got {:?}",
        String::from_utf8_lossy(&got)
    );

    // Closing the device side tears the dial down cleanly.
    drop(d_write);
    drop(d_read);
    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

// ─── In-process transfer-over-relay harness (§1 complement / #11) ─────
//
// `GatewayRemainingWork.md` §1 asks for a CI-able harness that drives a
// scripted **binary file transfer through the relay** so transfers over
// the relay stop being manual-only.  These tests cover the **onward-dial
// (Model B) path** — `device ↔ slave ↔ master ↔ BBS` — end to end: a real
// XMODEM / YMODEM / ZMODEM transfer runs between a simulated slave-attached
// device and a simulated external BBS, with every byte crossing
// `run_master_relay_dial`'s `copy_bidirectional`.  This is the exact code
// path an `ATDT host:port` from a relayed device takes to reach a file
// server on the master's network, and it needs no menu, no disk, and no
// global config, so it runs in CI.
//
// The transfers use raw serial semantics (`is_tcp = false`, no telnet IAC
// escaping) on both endpoints — the relay hop itself does no telnet
// negotiation, so a bare `0xFF` must survive as a single `0xFF` end to
// end.  The payload deliberately includes every transparency-sensitive
// byte (`0x00`, `0xFF`, CR/LF, `0x1A` SUB, `0x18` CAN/ZDLE, XON/XOFF) so a
// regression that re-introduced IAC doubling or CR-NUL stuffing on the
// relay path (the class of bug the 2026-06-28 CR-NUL fix addressed) would
// corrupt the transfer and fail these tests.
//
// NOT covered here (still manual — see `GatewayRemainingWork.md` §1):
//   * A menu-driven upload landing on the *master's* `transfer_dir`
//     (scenario 3, menu case) — the master session resolves `transfer_dir`
//     from the process-global config singleton, which a parallel CI test
//     can't set without racing every other test; and it writes real files
//     to CWD.  The full-session loopback test above proves the menu path's
//     raw-byte transparency (the `0xFF` color-prompt probe); the disk
//     landing stays a two-instance manual check.
//   * The slave-side `serial::online_mode_duplex` pump carrying a binary
//     transfer — it reads a blocking `SerialPort`, so it needs a mock-port
//     trait seam (tracked with the "drive DCD" work).  Its transparency-
//     critical byte handling (`process_online_bytes`, `+++` guard) is unit-
//     covered by `serial::tests::test_process_bytes`.

/// A multi-block payload that exercises every byte value in varied
/// positions plus an explicit run of the bytes the relay's transparency
/// claim rests on.  4 KiB guarantees multiple blocks for all three
/// protocols (128 B / 1 KiB / ZMODEM subpackets).
fn adversarial_payload() -> Vec<u8> {
    let mut v = Vec::with_capacity(4096 + 16);
    // 16 XOR-permuted sweeps: each pass still contains all 256 byte
    // values (XOR by a constant is a bijection), but at shifting offsets
    // so block boundaries land on different values each pass.
    for pass in 0u8..16 {
        for b in 0u8..=255 {
            v.push(b ^ pass);
        }
    }
    // Explicit torture run: IAC, NUL, CR, LF, SUB, CAN/ZDLE, XON, XOFF,
    // and a double-CAN (an abort look-alike that must pass as data).
    v.extend_from_slice(&[0xFF, 0x00, 0x0D, 0x0A, 0x1A, 0x18, 0x11, 0x13, 0x18, 0x18]);
    v
}

/// Stand up an onward-dial relay hop and return the two endpoints a
/// real transfer protocol runs over: the **device** end (what a slave-
/// attached machine drives) and the **BBS** end (the external host the
/// master dialed).  Every byte between them traverses
/// `run_master_relay_dial`'s `copy_bidirectional`.  The returned join
/// handle is the master dialer task (await it to confirm clean teardown).
async fn onward_dial_endpoints() -> (
    tokio::io::DuplexStream,
    tokio::net::TcpStream,
    tokio::task::JoinHandle<()>,
) {
    enable_peer_dial();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Ample buffer so neither direction blocks the copy loop on a slow
    // stop-and-wait protocol.
    let (master_end, device_end) = tokio::io::duplex(64 * 1024);
    let dialer = tokio::spawn(run_master_relay_dial(
        master_end,
        "127.0.0.1".to_string(),
        addr.port(),
    ));
    let (bbs, _) = listener.accept().await.unwrap();
    (device_end, bbs, dialer)
}

/// XMODEM upload over the relay: the relayed device SENDS, the external
/// BBS RECEIVES, and the bytes must arrive byte-identical after crossing
/// the master's onward-dial pipe.
#[tokio::test]
async fn test_relay_onward_dial_xmodem_upload() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::xmodem::xmodem_send(&mut r, &mut w, &data, false, false, false, false, None).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::xmodem::xmodem_receive(&mut r, &mut w, false, false, false).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay XMODEM upload timed out");

    send_res.unwrap().expect("XMODEM sender failed");
    let (received, _meta) = recv_res.unwrap().expect("XMODEM receiver failed");
    // XMODEM pads the final block to a 128-byte boundary; the receiver
    // strips trailing SUB (0x1A).  Our payload ends in 0x13, so the
    // non-padded content compares exactly.
    assert_eq!(
        received, payload,
        "XMODEM upload over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// XMODEM download over the relay: the external BBS SENDS, the relayed
/// device RECEIVES — the other direction of the transparent pipe.
#[tokio::test]
async fn test_relay_onward_dial_xmodem_download() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::xmodem::xmodem_send(&mut r, &mut w, &data, false, false, false, false, None).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::xmodem::xmodem_receive(&mut r, &mut w, false, false, false).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay XMODEM download timed out");

    send_res.unwrap().expect("XMODEM sender failed");
    let (received, _meta) = recv_res.unwrap().expect("XMODEM receiver failed");
    assert_eq!(
        received, payload,
        "XMODEM download over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// YMODEM upload over the relay: exercises the block-0 filename/size
/// metadata header across the onward-dial pipe (the receiver auto-detects
/// YMODEM from block 0 and reports the sender-declared size).
#[tokio::test]
async fn test_relay_onward_dial_ymodem_upload() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let size = payload.len() as u64;
    let sender = tokio::spawn(async move {
        let hdr = crate::xmodem::YmodemHeader {
            filename: "relay.bin".to_string(),
            size,
            modtime: None,
            mode: None,
        };
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::xmodem::xmodem_send(&mut r, &mut w, &data, false, false, false, true, Some(hdr)).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::xmodem::xmodem_receive(&mut r, &mut w, false, false, false).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay YMODEM upload timed out");

    send_res.unwrap().expect("YMODEM sender failed");
    let (received, meta) = recv_res.unwrap().expect("YMODEM receiver failed");
    // YMODEM's block-0 size field drives exact-length truncation, so the
    // received bytes match regardless of block padding.
    assert_eq!(
        received, payload,
        "YMODEM upload over relay corrupted the file"
    );
    assert_eq!(
        meta.and_then(|m| m.size),
        Some(size),
        "YMODEM block-0 size should survive the relay hop"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// ZMODEM upload over the relay: the workhorse batch protocol, device
/// SENDS → BBS RECEIVES, filename + bytes intact across the onward-dial
/// pipe.  ZMODEM's own ZDLE escaping rides transparently on the raw relay.
#[tokio::test]
async fn test_relay_onward_dial_zmodem_upload() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::zmodem::zmodem_send(&mut r, &mut w, &[("relay.bin", &data)], false, false).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::zmodem::zmodem_receive(&mut r, &mut w, false, false, |_, _, _| true).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay ZMODEM upload timed out");

    send_res.unwrap().expect("ZMODEM sender failed");
    let files = recv_res.unwrap().expect("ZMODEM receiver failed");
    assert_eq!(files.len(), 1, "expected exactly one file");
    assert_eq!(files[0].filename, "relay.bin");
    assert_eq!(
        files[0].data, payload,
        "ZMODEM upload over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// ZMODEM download over the relay: BBS SENDS → device RECEIVES.
#[tokio::test]
async fn test_relay_onward_dial_zmodem_download() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::zmodem::zmodem_send(&mut r, &mut w, &[("relay.bin", &data)], false, false).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::zmodem::zmodem_receive(&mut r, &mut w, false, false, |_, _, _| true).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay ZMODEM download timed out");

    send_res.unwrap().expect("ZMODEM sender failed");
    let files = recv_res.unwrap().expect("ZMODEM receiver failed");
    assert_eq!(files.len(), 1, "expected exactly one file");
    assert_eq!(
        files[0].data, payload,
        "ZMODEM download over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// Kermit upload over the relay: the device SENDS via Kermit, the BBS
/// RECEIVES — proving the Columbia-protocol handshake + packets survive
/// the onward-dial pipe (completes the protocol matrix alongside
/// XMODEM/YMODEM/ZMODEM above; Punter is next).
#[tokio::test]
async fn test_relay_onward_dial_kermit_upload() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let file = crate::kermit::KermitSendFile {
            name: "relay.bin",
            data: &data,
            modtime: None,
            mode: None,
        };
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::kermit::kermit_send(&mut r, &mut w, &[file], false, false, false).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::kermit::kermit_receive(&mut r, &mut w, false, false, false).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay KERMIT upload timed out");

    send_res.unwrap().expect("KERMIT sender failed");
    let files = recv_res.unwrap().expect("KERMIT receiver failed");
    assert_eq!(files.len(), 1, "expected exactly one file");
    assert_eq!(
        files[0].data, payload,
        "KERMIT upload over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// Punter (C1) upload over the relay: the device SENDS, the BBS RECEIVES —
/// the two-phase dual-checksum handshake survives the onward-dial pipe.
/// Punter is stop-and-wait, so this also exercises the relay under a
/// per-block ack/retry protocol.
#[tokio::test]
async fn test_relay_onward_dial_punter_upload() {
    let payload = adversarial_payload();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::punter::punter_send(
            &mut r,
            &mut w,
            &data,
            crate::punter::PunterFileType::Prg,
            false,
            false,
            false,
        )
        .await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::punter::punter_receive(&mut r, &mut w, false, false, false).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay PUNTER upload timed out");

    send_res.unwrap().expect("PUNTER sender failed");
    let (received, ftype) = recv_res.unwrap().expect("PUNTER receiver failed");
    assert_eq!(
        received, payload,
        "PUNTER upload over relay corrupted the file"
    );
    assert_eq!(ftype, crate::punter::PunterFileType::Prg);

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}

/// Large ZMODEM upload over the relay (64 KiB): stresses the onward-dial
/// pipe's flow control across many subpackets, beyond the ~4 KiB
/// adversarial payload the other cases use.
#[tokio::test]
async fn test_relay_onward_dial_zmodem_large() {
    // 64 KiB, every byte value cycling, so a dropped/duplicated chunk shows.
    let payload: Vec<u8> = (0..65536u32).map(|i| (i & 0xFF) as u8).collect();
    let (device_end, bbs, dialer) = onward_dial_endpoints().await;

    let data = payload.clone();
    let sender = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(device_end);
        crate::zmodem::zmodem_send(&mut r, &mut w, &[("large.bin", &data)], false, false).await
    });
    let receiver = tokio::spawn(async move {
        let (mut r, mut w) = tokio::io::split(bbs);
        crate::zmodem::zmodem_receive(&mut r, &mut w, false, false, |_, _, _| true).await
    });

    let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(sender, receiver)
    })
    .await
    .expect("relay large ZMODEM upload timed out");

    send_res.unwrap().expect("ZMODEM sender failed");
    let files = recv_res.unwrap().expect("ZMODEM receiver failed");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].data.len(), payload.len());
    assert_eq!(
        files[0].data, payload,
        "large ZMODEM upload over relay corrupted the file"
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), dialer).await;
}
