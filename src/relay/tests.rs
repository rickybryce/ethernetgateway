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
    parse_relay_command, run_master_relay_dial, run_master_relay_session, ParsedRelay,
    RelayTarget,
};

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
            dial: None
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
    // Missing port label defaults to "?" but is still a valid menu relay.
    assert_eq!(
        parse_relay_command("serial-relay"),
        Some(ParsedRelay {
            port_label: "?".into(),
            dial: None
        })
    );
}

/// Onward dial (Model B): `run_master_relay_dial` connects to the target
/// and pipes the relay channel through transparently in both directions.
#[tokio::test]
async fn test_master_relay_dial_pipes_both_ways() {
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
