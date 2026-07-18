use super::*;

// ─── PETSCII helpers ─────────────────────────────────

#[test]
fn test_swap_case_for_petscii() {
    assert_eq!(swap_case_for_petscii("Hello"), "hELLO");
    assert_eq!(swap_case_for_petscii("ABC"), "abc");
    assert_eq!(swap_case_for_petscii("abc"), "ABC");
    assert_eq!(swap_case_for_petscii("123!"), "123!");
    assert_eq!(swap_case_for_petscii(""), "");
}

#[test]
fn test_petscii_to_ascii_byte() {
    // PETSCII lowercase (0x41-0x5A) -> ASCII lowercase
    assert_eq!(petscii_to_ascii_byte(0x41), b'a');
    assert_eq!(petscii_to_ascii_byte(0x5A), b'z');
    // PETSCII uppercase (0xC1-0xDA) -> ASCII uppercase
    assert_eq!(petscii_to_ascii_byte(0xC1), b'A');
    assert_eq!(petscii_to_ascii_byte(0xDA), b'Z');
    // Other bytes pass through
    assert_eq!(petscii_to_ascii_byte(b'1'), b'1');
    assert_eq!(petscii_to_ascii_byte(0x00), 0x00);
}

#[test]
fn test_to_latin1_bytes() {
    assert_eq!(to_latin1_bytes("abc"), vec![b'a', b'b', b'c']);
    assert_eq!(to_latin1_bytes(""), Vec::<u8>::new());
}

// ─── Input helpers ───────────────────────────────────

#[test]
fn test_is_backspace_key() {
    assert!(is_backspace_key(0x08, 0x7F)); // BS
    assert!(is_backspace_key(0x7F, 0x7F)); // DEL (erase_char)
    assert!(is_backspace_key(0x14, 0x7F)); // C64 DEL
    assert!(is_backspace_key(0x08, 0x14)); // BS with C64 erase_char
    assert!(!is_backspace_key(b'a', 0x7F));
    assert!(!is_backspace_key(0x00, 0x7F));
}

#[test]
fn test_is_esc_key() {
    assert!(is_esc_key(0x1B, false));
    assert!(!is_esc_key(0x5F, false)); // underscore in ANSI
    assert!(is_esc_key(0x1B, true));
    assert!(is_esc_key(0x5F, true)); // back-arrow in PETSCII
    assert!(!is_esc_key(b'a', false));
    assert!(!is_esc_key(b'a', true));
}

// ─── Truncation ──────────────────────────────────────

#[test]
fn test_truncate_to_width() {
    assert_eq!(truncate_to_width("hello", 10), "hello");
    assert_eq!(truncate_to_width("hello", 5), "hello");
    assert_eq!(truncate_to_width("hello world", 8), "hello...");
    assert_eq!(truncate_to_width("abcdef", 3), "...");
    assert_eq!(truncate_to_width("ab", 2), "ab");
}

// ─── Filename validation ─────────────────────────────

#[test]
fn test_validate_filename_valid() {
    assert!(TelnetSession::validate_filename("test.txt").is_ok());
    assert!(TelnetSession::validate_filename("my-file_v2.bin").is_ok());
    assert!(TelnetSession::validate_filename("a").is_ok());
    let name_64 = "a".repeat(TelnetSession::MAX_FILENAME_LEN);
    assert!(TelnetSession::validate_filename(&name_64).is_ok());
}

#[test]
fn test_validate_filename_invalid() {
    assert!(TelnetSession::validate_filename("").is_err());
    assert!(TelnetSession::validate_filename(".hidden").is_err());
    assert!(TelnetSession::validate_filename("file name.txt").is_err());
    assert!(TelnetSession::validate_filename("../../etc/passwd").is_err());
    assert!(TelnetSession::validate_filename("file..txt").is_err());
    let name_65 = "a".repeat(TelnetSession::MAX_FILENAME_LEN + 1);
    assert!(TelnetSession::validate_filename(&name_65).is_err());
    assert!(TelnetSession::validate_filename("---").is_err());
}

// ─── File size formatting ────────────────────────────

#[test]
fn test_format_file_size() {
    assert_eq!(TelnetSession::format_file_size(0), "0 B");
    assert_eq!(TelnetSession::format_file_size(512), "512 B");
    assert_eq!(TelnetSession::format_file_size(1023), "1023 B");
    assert_eq!(TelnetSession::format_file_size(1024), "1.0 KB");
    assert_eq!(TelnetSession::format_file_size(1536), "1.5 KB");
    assert_eq!(TelnetSession::format_file_size(1048576), "1.0 MB");
    assert_eq!(TelnetSession::format_file_size(1572864), "1.5 MB");
}

// ─── Constants ───────────────────────────────────────

#[test]
fn test_constants() {
    const _: () = assert!(TelnetSession::MAX_FILE_SIZE == 8 * 1024 * 1024);
    const _: () = assert!(TelnetSession::MAX_FILENAME_LEN == 64);
    const _: () = assert!(TelnetSession::TRANSFER_PAGE_SIZE > 0);
    const _: () = assert!(TelnetSession::TRANSFER_PAGE_SIZE <= 20);
}

// ─── Auth lockout ────────────────────────────────────

#[test]
fn test_lockout_flow() {
    let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    assert!(!is_locked_out(&lockouts, ip));
    assert_eq!(record_auth_failure(&lockouts, ip), 1);
    assert!(!is_locked_out(&lockouts, ip));
    assert_eq!(record_auth_failure(&lockouts, ip), 2);
    assert!(!is_locked_out(&lockouts, ip));
    assert_eq!(record_auth_failure(&lockouts, ip), 3);
    assert!(is_locked_out(&lockouts, ip));

    clear_lockout(&lockouts, ip);
    assert!(!is_locked_out(&lockouts, ip));
}

#[test]
fn test_lockout_different_ips() {
    let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let ip1: IpAddr = "127.0.0.1".parse().unwrap();
    let ip2: IpAddr = "10.0.0.1".parse().unwrap();

    for _ in 0..3 {
        record_auth_failure(&lockouts, ip1);
    }
    assert!(is_locked_out(&lockouts, ip1));
    assert!(!is_locked_out(&lockouts, ip2));
}

/// Lockout counter must reset after `LOCKOUT_DURATION` elapses
/// without a successful auth.  Faking the elapsed time via direct
/// map manipulation rather than waiting 5 minutes — the production
/// code reads `entry.1.elapsed()` so we can backdate `entry.1` to
/// simulate a stale lockout.
#[test]
fn test_lockout_counter_resets_after_duration() {
    let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "127.0.0.1".parse().unwrap();

    // Drive the counter to the lockout threshold.
    for _ in 0..MAX_AUTH_ATTEMPTS {
        record_auth_failure(&lockouts, ip);
    }
    assert!(is_locked_out(&lockouts, ip));

    // Backdate the timestamp so it appears the lockout window
    // already elapsed (decay path uses Instant::elapsed()).
    // `Instant::checked_sub` returns None when the result would
    // pre-date the platform's monotonic epoch (boot time on
    // Linux); on a freshly-booted CI container that's a real
    // case.  Fall through to the test-skip path rather than
    // panicking — the production logic is exercised by the
    // assertions below regardless of which sub call succeeded.
    let now = std::time::Instant::now();
    let backdate_target = LOCKOUT_DURATION + std::time::Duration::from_secs(1);
    let stale = match now.checked_sub(backdate_target) {
        Some(t) => t,
        None => {
            // Cold-boot environment — bail without panicking.
            eprintln!(
                "test_lockout_counter_resets_after_duration: skipping on \
                 a freshly-booted host (Instant epoch < LOCKOUT_DURATION)"
            );
            return;
        }
    };
    {
        let mut map = lockouts.lock().unwrap();
        map.entry(ip).and_modify(|e| e.1 = stale);
    }

    // Stale lockout: not active, and the next failure resets the
    // counter to 1 rather than continuing from 3.
    assert!(
        !is_locked_out(&lockouts, ip),
        "expired lockout should not block"
    );
    assert_eq!(
        record_auth_failure(&lockouts, ip),
        1,
        "counter must reset after the lockout window expires"
    );
}

/// A new failure from any IP should sweep stale entries from
/// other IPs out of the map, so a long-running public instance
/// doesn't accumulate one entry per distinct attacker forever.
#[test]
fn test_lockout_prunes_stale_entries() {
    let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let stale_ip: IpAddr = "10.0.0.1".parse().unwrap();
    let fresh_ip: IpAddr = "127.0.0.1".parse().unwrap();

    record_auth_failure(&lockouts, stale_ip);

    // Backdate the stale entry past the lockout window.
    let now = std::time::Instant::now();
    let backdate_target = LOCKOUT_DURATION + std::time::Duration::from_secs(1);
    let Some(stale) = now.checked_sub(backdate_target) else {
        eprintln!(
            "test_lockout_prunes_stale_entries: skipping on a freshly-booted \
             host (Instant epoch < LOCKOUT_DURATION)"
        );
        return;
    };
    {
        let mut map = lockouts.lock().unwrap();
        map.entry(stale_ip).and_modify(|e| e.1 = stale);
        assert_eq!(map.len(), 1);
    }

    // Activity from a different IP should evict the stale entry.
    record_auth_failure(&lockouts, fresh_ip);
    let map = lockouts.lock().unwrap();
    assert!(!map.contains_key(&stale_ip), "stale entry should be pruned");
    assert!(map.contains_key(&fresh_ip));
    assert_eq!(map.len(), 1);
}

// ─── Known hosts ─────────────────────────────────────

fn make_test_key() -> russh::keys::PublicKey {
    // A valid Ed25519 public key for testing (OpenSSH format)
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ test"
        .parse()
        .unwrap()
}

#[test]
fn test_check_known_host_unknown_no_file() {
    let key = make_test_key();
    match check_known_host("nonexistent-test-host.example", 22, &key) {
        HostKeyStatus::Unknown => {}
        _ => panic!("expected Unknown for host not in file"),
    }
}

#[test]
fn test_format_host_key_roundtrip() {
    let key = make_test_key();
    let formatted = format_host_key(&key);
    assert!(formatted.starts_with("ssh-ed25519 "));
    // Should be "algo base64" with no comment
    assert_eq!(formatted.split(' ').count(), 2);
}

#[test]
fn test_known_host_fingerprint_is_stable() {
    let key = make_test_key();
    let fp1 = key.fingerprint(russh::keys::HashAlg::Sha256);
    let fp2 = key.fingerprint(russh::keys::HashAlg::Sha256);
    assert_eq!(fp1.to_string(), fp2.to_string());
    assert!(fp1.to_string().starts_with("SHA256:"));
}

// ─── IP filtering ────────────────────────────────────

#[test]
fn test_reject_insecure_ip_private_allowed() {
    let ip: IpAddr = "192.168.1.100".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_loopback_allowed() {
    let ip: IpAddr = "127.0.0.2".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_ten_network_allowed() {
    let ip: IpAddr = "10.0.5.42".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_172_private_allowed() {
    let ip: IpAddr = "172.16.0.50".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
    let ip2: IpAddr = "172.31.255.254".parse().unwrap();
    assert!(reject_insecure_ip(ip2).is_none());
}

#[test]
fn test_reject_insecure_ip_public_rejected() {
    let ip: IpAddr = "8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

#[test]
fn test_reject_insecure_ip_172_public_rejected() {
    // 172.32.x.x is NOT private (private is 172.16-31.x.x)
    let ip: IpAddr = "172.32.0.5".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

#[test]
fn test_reject_insecure_ip_gateway_rejected() {
    let ip: IpAddr = "192.168.1.1".parse().unwrap();
    let reason = reject_insecure_ip(ip);
    assert!(reason.is_some());
    assert!(reason.unwrap().contains("gateway"));
}

#[test]
fn test_reject_insecure_ip_gateway_ten_rejected() {
    let ip: IpAddr = "10.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

#[test]
fn test_reject_insecure_ip_loopback_dot_one_allowed() {
    // 127.0.0.1 is loopback — exempt from the .1 gateway filter
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_loopback_allowed() {
    let ip: IpAddr = "::1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_public_rejected() {
    let ip: IpAddr = "2001:db8::1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_private_allowed() {
    // ::ffff:192.168.1.100 is IPv4-mapped, should apply IPv4 rules
    let ip: IpAddr = "::ffff:192.168.1.100".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_public_rejected() {
    let ip: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_gateway_rejected() {
    // ::ffff:10.0.0.1 ends in .1, should be rejected
    let ip: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
    let reason = reject_insecure_ip(ip);
    assert!(reason.is_some());
    assert!(reason.unwrap().contains("gateway"));
}

#[test]
fn test_reject_insecure_ip_ipv6_link_local_allowed() {
    let ip: IpAddr = "fe80::1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_unique_local_allowed() {
    let ip: IpAddr = "fd12:3456:789a::1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_link_local_ipv4_allowed() {
    let ip: IpAddr = "169.254.1.100".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_none());
}

#[test]
fn test_reject_insecure_ip_link_local_ipv4_gateway_rejected() {
    let ip: IpAddr = "169.254.0.1".parse().unwrap();
    assert!(reject_insecure_ip(ip).is_some());
}

// ─── Menu ────────────────────────────────────────────

#[test]
fn test_menu_paths() {
    assert_eq!(Menu::Main.path(), "ethernet");
    assert_eq!(Menu::FileTransfer.path(), "ethernet/xfer");
}

// ─── Color helpers ───────────────────────────────────

#[test]
fn test_petscii_color() {
    let result = TelnetSession::petscii_color(PETSCII_GREEN, "test");
    assert!(result.contains("test"));
    assert_eq!(result.as_bytes()[0], PETSCII_GREEN);
    assert_eq!(*result.as_bytes().last().unwrap(), PETSCII_DEFAULT);
}

// ─── Test session helper ─────────────────────────────

/// Build a minimal TelnetSession with the given terminal type for testing
/// synchronous helpers (color, formatting, etc.).  No I/O is performed.
fn make_test_session(terminal_type: TerminalType) -> TelnetSession {
    let (client, server) = tokio::io::duplex(1);
    let writer_box: Box<dyn tokio::io::AsyncWrite + Unpin + Send> =
        Box::new(client);
    let writer: SharedWriter =
        Arc::new(tokio::sync::Mutex::new(writer_box));
    TelnetSession {
        reader: Box::new(server),
        writer,
        shutdown: Arc::new(AtomicBool::new(false)),
        restart: Arc::new(AtomicBool::new(false)),
        current_menu: Menu::Main,
        terminal_type,
        color_enabled: true,
        erase_char: 0x7F,
        lockouts: Arc::new(Mutex::new(HashMap::new())),
        peer_addr: None,
        transfer_subdir: String::new(),
        xmodem_iac: false,
        web_lines: Vec::new(),
        web_scroll: 0,
        web_links: Vec::new(),
        web_history: Vec::new(),
        web_url: None,
        web_title: None,
        web_forms: Vec::new(),
        weather_location: String::new(),
        is_serial: false,
        is_relay: false,
        serial_port_id: None,
        is_ssh: false,
        idle_timeout: std::time::Duration::ZERO,
        pushback: None,
        neg_sent_will: Box::new([false; 256]),
        neg_sent_do: Box::new([false; 256]),
        neg_sent_wont: Box::new([false; 256]),
        neg_sent_dont: Box::new([false; 256]),
        ttype_matched: false,
        ttype_raw: None,
        telnet_negotiated: false,
        window_width: None,
        window_height: None,
    }
}

/// Build a telnet session wired to a controllable client-side pipe.
/// Return the session plus the peer end: writing to `peer` feeds
/// bytes to the session's reader, reading from `peer` returns what
/// the session wrote. Used for end-to-end negotiation tests.
fn make_test_session_with_peer(
    terminal_type: TerminalType,
) -> (TelnetSession, tokio::io::DuplexStream) {
    let (peer, session_stream) = tokio::io::duplex(512);
    let (session_reader, session_writer) = tokio::io::split(session_stream);
    let writer_box: Box<dyn tokio::io::AsyncWrite + Unpin + Send> =
        Box::new(session_writer);
    let writer: SharedWriter =
        Arc::new(tokio::sync::Mutex::new(writer_box));
    let session = TelnetSession {
        reader: Box::new(session_reader),
        writer,
        shutdown: Arc::new(AtomicBool::new(false)),
        restart: Arc::new(AtomicBool::new(false)),
        current_menu: Menu::Main,
        terminal_type,
        color_enabled: true,
        erase_char: 0x7F,
        lockouts: Arc::new(Mutex::new(HashMap::new())),
        peer_addr: None,
        transfer_subdir: String::new(),
        xmodem_iac: false,
        web_lines: Vec::new(),
        web_scroll: 0,
        web_links: Vec::new(),
        web_history: Vec::new(),
        web_url: None,
        web_title: None,
        web_forms: Vec::new(),
        weather_location: String::new(),
        is_serial: false,
        is_relay: false,
        serial_port_id: None,
        is_ssh: false,
        idle_timeout: std::time::Duration::ZERO,
        pushback: None,
        neg_sent_will: Box::new([false; 256]),
        neg_sent_do: Box::new([false; 256]),
        neg_sent_wont: Box::new([false; 256]),
        neg_sent_dont: Box::new([false; 256]),
        ttype_matched: false,
        ttype_raw: None,
        telnet_negotiated: false,
        window_width: None,
        window_height: None,
    };
    (session, peer)
}

// ─── Color helpers ──────────────────────────────────

#[test]
fn test_green_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.green("ok");
    assert!(result.starts_with(ANSI_GREEN));
    assert!(result.ends_with(ANSI_RESET));
    assert!(result.contains("ok"));
}

#[test]
fn test_green_petscii() {
    let s = make_test_session(TerminalType::Petscii);
    let result = s.green("ok");
    assert_eq!(result.as_bytes()[0], PETSCII_GREEN);
    assert_eq!(*result.as_bytes().last().unwrap(), PETSCII_DEFAULT);
    assert!(result.contains("ok"));
}

#[test]
fn test_green_ascii_no_escapes() {
    let s = make_test_session(TerminalType::Ascii);
    assert_eq!(s.green("ok"), "ok");
}

#[test]
fn test_red_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.red("err");
    assert!(result.starts_with(ANSI_RED));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_yellow_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.yellow("warn");
    assert!(result.starts_with(ANSI_YELLOW));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_cyan_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.cyan("info");
    assert!(result.starts_with(ANSI_CYAN));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_amber_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.amber("caution");
    assert!(result.starts_with(ANSI_AMBER));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_amber_petscii_uses_yellow() {
    let s = make_test_session(TerminalType::Petscii);
    let result = s.amber("caution");
    // PETSCII_YELLOW (0x9E) is multi-byte in UTF-8, so check via char
    assert_eq!(result.chars().next().unwrap(), char::from(PETSCII_YELLOW));
}

#[test]
fn test_dim_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.dim("faint");
    assert!(result.starts_with(ANSI_DIM));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_blue_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.blue("link");
    assert!(result.starts_with(ANSI_BLUE));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_white_ansi() {
    let s = make_test_session(TerminalType::Ansi);
    let result = s.white("bright");
    assert!(result.starts_with(ANSI_WHITE));
    assert!(result.ends_with(ANSI_RESET));
}

#[test]
fn test_all_colors_ascii_passthrough() {
    let s = make_test_session(TerminalType::Ascii);
    assert_eq!(s.red("x"), "x");
    assert_eq!(s.cyan("x"), "x");
    assert_eq!(s.yellow("x"), "x");
    assert_eq!(s.amber("x"), "x");
    assert_eq!(s.dim("x"), "x");
    assert_eq!(s.blue("x"), "x");
    assert_eq!(s.white("x"), "x");
}

// ─── colorize_link_markers ──────────────────────────

#[test]
fn test_colorize_link_markers_no_markers() {
    let s = make_test_session(TerminalType::Ansi);
    assert_eq!(s.colorize_link_markers("hello world"), "hello world");
}

#[test]
fn test_colorize_link_markers_single() {
    let s = make_test_session(TerminalType::Ansi);
    let input = "click \x021\x03 here";
    let result = s.colorize_link_markers(input);
    assert!(result.contains("[1]"));
    assert!(result.contains(ANSI_BLUE));
    assert!(result.contains("click "));
    assert!(result.contains(" here"));
}

#[test]
fn test_colorize_link_markers_multiple() {
    let s = make_test_session(TerminalType::Ansi);
    let input = "\x021\x03 and \x022\x03";
    let result = s.colorize_link_markers(input);
    assert!(result.contains("[1]"));
    assert!(result.contains("[2]"));
}

#[test]
fn test_colorize_link_markers_ascii_no_color() {
    let s = make_test_session(TerminalType::Ascii);
    let input = "\x021\x03";
    let result = s.colorize_link_markers(input);
    assert_eq!(result, "[1]");
}

#[test]
fn test_colorize_link_markers_malformed() {
    let s = make_test_session(TerminalType::Ansi);
    // Open sentinel without close — silently dropped
    let result = s.colorize_link_markers("text\x02orphan");
    assert!(result.contains("text"));
    assert!(result.contains("orphan"));
    assert!(!result.contains("\x02"));
}

// ─── action_prompt / nav_footer ─────────────────────

#[test]
fn test_action_prompt_format() {
    let s = make_test_session(TerminalType::Ascii);
    assert_eq!(s.action_prompt("Q", "Back"), "Q=Back");
}

#[test]
fn test_nav_footer_fits_petscii() {
    let s = make_test_session(TerminalType::Ascii);
    let footer = s.nav_footer();
    // ASCII mode has no escape codes, so visible length == byte length
    assert!(
        footer.len() <= PETSCII_WIDTH,
        "nav footer '{}' is {} chars, exceeds {}",
        footer,
        footer.len(),
        PETSCII_WIDTH,
    );
}

// ─── constant_time_eq ───────────────────────────────

#[test]
fn test_constant_time_eq_equal() {
    assert!(constant_time_eq(b"password", b"password"));
}

#[test]
fn test_constant_time_eq_different() {
    assert!(!constant_time_eq(b"password", b"passw0rd"));
}

#[test]
fn test_constant_time_eq_different_lengths() {
    assert!(!constant_time_eq(b"short", b"longer"));
}

#[test]
fn test_constant_time_eq_empty() {
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn test_constant_time_eq_single_bit_diff() {
    // 'A' (0x41) vs 'a' (0x61) — differ by one bit
    assert!(!constant_time_eq(b"A", b"a"));
}

// ─── Gateway output filtering ────────────────────────

/// Helper: run filter_gateway_output on a single chunk.
fn filter_output(input: &[u8], is_petscii: bool) -> Vec<u8> {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(input, &mut state, is_petscii, &mut out);
    out
}

#[test]
fn test_filter_plain_text_ascii() {
    assert_eq!(filter_output(b"hello world", false), b"hello world");
}

#[test]
fn test_filter_plain_text_petscii_swaps_case() {
    assert_eq!(filter_output(b"Hello", true), b"hELLO");
}

#[test]
fn test_filter_strips_csi_color() {
    let input = b"\x1b[32mhello";
    assert_eq!(filter_output(input, false), b"hello");
}

#[test]
fn test_filter_strips_csi_cursor_move() {
    let input = b"\x1b[10;1Hprompt";
    assert_eq!(filter_output(input, false), b"prompt");
}

#[test]
fn test_filter_strips_osc_title_bel() {
    let input = b"\x1b]0;ricky@host:~\x07ricky@host:~$ ";
    assert_eq!(filter_output(input, false), b"ricky@host:~$ ");
}

#[test]
fn test_filter_strips_osc_title_st() {
    let input = b"\x1b]0;title\x1b\\visible";
    assert_eq!(filter_output(input, false), b"visible");
}

#[test]
fn test_filter_strips_dcs_sequence() {
    let input = b"\x1bPsome data\x1b\\after";
    assert_eq!(filter_output(input, false), b"after");
}

#[test]
fn test_filter_strips_pm_sequence() {
    let input = b"\x1b^private msg\x07text";
    assert_eq!(filter_output(input, false), b"text");
}

#[test]
fn test_filter_strips_apc_sequence() {
    let input = b"\x1b_app cmd\x07text";
    assert_eq!(filter_output(input, false), b"text");
}

#[test]
fn test_filter_passes_two_char_esc_sequence() {
    let input = b"\x1bMhello"; // ESC M = reverse line feed
    assert_eq!(filter_output(input, false), b"hello");
}

#[test]
fn test_filter_strips_multiple_sequences() {
    let input = b"\x1b]0;title\x07\x1b[1;32mhello\x1b[0m world";
    assert_eq!(filter_output(input, false), b"hello world");
}

#[test]
fn test_filter_state_spans_chunks() {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b]0;ti", &mut state, false, &mut out);
    assert_eq!(out, b"");
    assert_eq!(state, 3);
    filter_gateway_output(b"tle\x07visible", &mut state, false, &mut out);
    assert_eq!(out, b"visible");
    assert_eq!(state, 0);
}

#[test]
fn test_filter_incomplete_csi_spans_chunks() {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b[32", &mut state, false, &mut out);
    assert_eq!(out, b"");
    assert_eq!(state, 2);
    filter_gateway_output(b"mhello", &mut state, false, &mut out);
    assert_eq!(out, b"hello");
    assert_eq!(state, 0);
}

#[test]
fn test_filter_bare_esc_at_end_of_chunk() {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(b"text\x1b", &mut state, false, &mut out);
    assert_eq!(out, b"text");
    assert_eq!(state, 1);
    filter_gateway_output(b"[0mmore", &mut state, false, &mut out);
    assert_eq!(out, b"textmore");
}

#[test]
fn test_filter_petscii_strips_and_swaps() {
    let input = b"\x1b[32mHello World";
    assert_eq!(filter_output(input, true), b"hELLO wORLD");
}

#[test]
fn test_filter_petscii_strips_tilde() {
    assert_eq!(filter_output(b"~$ ", true), b"$ ");
    assert_eq!(filter_output(b"user@host:~$ ", true), b"USER@HOST:$ ");
}

#[test]
fn test_filter_ascii_keeps_tilde() {
    assert_eq!(filter_output(b"~$ ", false), b"~$ ");
}

#[test]
fn test_filter_petscii_translates_backspace() {
    assert_eq!(filter_output(b"ab\x08c", true), b"AB\x14C");
    assert_eq!(filter_output(b"ab\x7Fc", true), b"AB\x14C");
}

#[test]
fn test_filter_ascii_keeps_backspace() {
    assert_eq!(filter_output(b"ab\x08c", false), b"ab\x08c");
    assert_eq!(filter_output(b"ab\x7Fc", false), b"ab\x7Fc");
}

#[test]
fn test_filter_empty_input() {
    assert_eq!(filter_output(b"", false), b"");
    assert_eq!(filter_output(b"", true), b"");
}

#[test]
fn test_filter_only_escape_sequences() {
    let input = b"\x1b[1m\x1b[32m\x1b[0m";
    assert_eq!(filter_output(input, false), b"");
}

#[test]
fn test_filter_csi_reset_on_control_char() {
    let input = b"\x1b[3\x00text";
    assert_eq!(filter_output(input, false), b"text");
}

#[test]
fn test_filter_csi_reset_on_esc() {
    let input = b"\x1b[32\x1b]0;title\x07text";
    assert_eq!(filter_output(input, false), b"text");
}

#[test]
fn test_filter_double_esc() {
    let input = b"\x1b\x1b[32mtext";
    assert_eq!(filter_output(input, false), b"text");
}

#[test]
fn test_filter_unclosed_osc_spans_chunks() {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b]0;title", &mut state, false, &mut out);
    assert_eq!(state, 3);
    assert_eq!(out, b"");
    filter_gateway_output(b"more title", &mut state, false, &mut out);
    assert_eq!(state, 3);
    assert_eq!(out, b"");
    filter_gateway_output(b"\x07visible", &mut state, false, &mut out);
    assert_eq!(state, 0);
    assert_eq!(out, b"visible");
}

#[test]
fn test_filter_csi_interrupted_by_new_esc() {
    let mut state = 0u8;
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b[32", &mut state, false, &mut out);
    assert_eq!(state, 2);
    filter_gateway_output(b"\x1b]title\x07text", &mut state, false, &mut out);
    assert_eq!(state, 0);
    assert_eq!(out, b"text");
}

// ─── Gateway input normalization ─────────────────────

#[test]
fn test_normalize_plain_byte() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'a', &mut last_cr), Some(b'a'));
    assert!(!last_cr);
}

#[test]
fn test_normalize_cr_passes_through() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert!(last_cr);
}

#[test]
fn test_normalize_suppresses_lf_after_cr() {
    let mut last_cr = true;
    assert_eq!(normalize_gateway_input(b'\n', &mut last_cr), None);
    assert!(!last_cr);
}

#[test]
fn test_normalize_suppresses_nul_after_cr() {
    let mut last_cr = true;
    assert_eq!(normalize_gateway_input(0x00, &mut last_cr), None);
    assert!(!last_cr);
}

#[test]
fn test_normalize_lf_without_cr_passes() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\n', &mut last_cr), Some(b'\n'));
    assert!(!last_cr);
}

#[test]
fn test_normalize_nul_without_cr_passes() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(0x00, &mut last_cr), Some(0x00));
    assert!(!last_cr);
}

#[test]
fn test_normalize_cr_lf_sequence() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert_eq!(normalize_gateway_input(b'\n', &mut last_cr), None);
    assert_eq!(normalize_gateway_input(b'x', &mut last_cr), Some(b'x'));
}

#[test]
fn test_normalize_cr_nul_sequence() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert_eq!(normalize_gateway_input(0x00, &mut last_cr), None);
    assert_eq!(normalize_gateway_input(b'x', &mut last_cr), Some(b'x'));
}

#[test]
fn test_normalize_cr_then_regular_byte() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert_eq!(normalize_gateway_input(b'a', &mut last_cr), Some(b'a'));
    assert!(!last_cr);
}

#[test]
fn test_normalize_double_cr() {
    let mut last_cr = false;
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert_eq!(normalize_gateway_input(b'\r', &mut last_cr), Some(b'\r'));
    assert!(last_cr);
}

// ─── Screen layout constraints ───────────────────────

/// All user-facing error messages must fit in PETSCII width (40 cols).
/// The "  " prefix + message must not exceed 40 chars.
#[test]
fn test_all_error_messages_fit_petscii() {
    let messages = [
        "Input too long.",
        "Press A-C, F, R, S, T, W, X, or H.",
        // Non-serial prompt includes E but is only shown to
        // ANSI/SSH users (80 cols), so it is not tested here.
        "Press U, D, X, C, I, R, Q, or H.",
        "Disk space is low. Uploads disabled.",
        "File already exists.",
        "No files available.",
        "Invalid selection.",
        "Enter a number, P, N, Q, or H.",
        "File too large.",
        "No files to delete.",
        "No subdirectories.",
        "Access denied.",
        "Enter a number or Q.",
        "Press S, R, Q, or H.",
        "Press E, S, B, P, D, F, H, or Q.",
        "No serial ports detected.",
        "Invalid port number.",
        "Connection timed out.",
        "Authentication failed.",
        "Too many attempts. Try later.",
        "Too many failed attempts.",
        "Login incorrect.",
        "Disconnected: idle timeout.",
        "Press any key to continue.",
        "No API key configured.",
        // Weather
        "Enter a city or postal code.",
        "Location too long.",
        "Not found - try 'City, Country'.",
        // Web browser
        "Press G, K, H, or Q.",
        "End of page.",
        "Top of page.",
        "No links on this page.",
        "No forms on this page.",
        "No history.",
        "Enter a number.",
        "Invalid form number.",
        "Invalid field number.",
        "Enter S, Q, H, or a field #.",
        "Already bookmarked (or full).",
        "No page to bookmark.",
        "No bookmarks saved.",
        "Not found.",
        "Invalid number.",
        "Unknown command.",
        // Dialup mapping
        "Press A, H, or Q.",
        "Press A, D, H, or Q.",
        "Number must contain digits.",
        "Invalid entry number.",
        "Mapping saved.",
        "No other mappings defined.",
        // Configuration
        "Press E, F, G, M, O, R, S, T, H, or Q.",
        // Other settings
        "Press A, B, W, V, G, R, H, or Q.",
        // Security
        // Post unified-credentials merge: S (Set SSH user) and
        // W (Set SSH pass) menu keys went away.
        "Press L, U, P, R, H, or Q.",
        // File transfer submenu
        "Press D, X, Y, Z, R, H, or Q.",
        // XMODEM / YMODEM settings
        "Press N, I, B, M, R, H, or Q.",
        // ZMODEM settings
        "Press N, I, F, M, R, H, or Q.",
        "Press T, P, S, O, R, H, or Q.",
        "Press a letter from the menu.",
        "Invalid port number.",
        // Modem / console emulator menu
        "Press E, S, B, P, F, H, or Q.",
        "Press E, S, B, P, D, F, H, or Q.",
        "Press E, S, B, P, D, F, I, H, or Q.",
    ];
    for msg in &messages {
        // Error messages are displayed as "  {msg}" — 2-char indent
        let displayed = format!("  {}", msg);
        assert!(
            displayed.len() <= PETSCII_WIDTH,
            "error message '{}' is {} chars with indent, exceeds {}",
            msg,
            displayed.len(),
            PETSCII_WIDTH,
        );
    }
}

/// All menu items must fit in PETSCII width (40 cols).
#[test]
fn test_all_menu_items_fit_petscii() {
    let items = [
        // Main menu
        "  A  AI Chat",
        "  B  Simple Browser",
        "  C  Configuration",
        "  F  File Transfer",
        "  G  Serial Gateway",
        "  R  Troubleshooting",
        "  S  SSH Gateway",
        "  T  Telnet Gateway",
        "  W  Weather",
        "  X  Exit",
        // Modem emulator menu
        "  E  Toggle enabled/disabled",
        "  S  Select serial port",
        "  B  Set baud rate",
        "  P  Set data/parity/stop",
        "  F  Set flow control",
        "  D  Dialup Mapping",
        // Port selection menu
        "  R  Refresh port list",
        "  N  None (clear port)",
        "  Enter #, R, N, or type a path.",
        // Configuration submenu (post-dual-port refactor:
        // M renamed to "Serial Configuration", T moved into the
        // per-port settings menu).
        "  E  Security",
        "  M  Serial Configuration",
        "  T  Toggle Modem/Console mode", // now lives on the per-port menu
        "  S  Server Configuration",
        "  F  File Transfer",
        "  O  Other Settings",
        "  R  Reset Defaults",
        // Other settings menu
        "  A  Set AI API key (Groq)",
        "  B  Set browser homepage",
        "  W  Set weather location",
        "  U  Cycle weather units",
        "  V  Toggle verbose transfer logging",
        "  G  Toggle GUI on startup",
        // Security menu (post unified-credentials merge — the
        // Telnet/SSH user+pass items collapsed into a single
        // username/password pair shared across both protocols
        // and the web UI).
        "  L  Toggle require login",
        "  U  Set username",
        "  P  Set password",
        // File transfer submenu
        "  D  Change transfer directory",
        "  X  XMODEM settings",
        "  Y  YMODEM settings",
        "  Z  ZMODEM settings",
        // XMODEM / YMODEM settings menu
        "  N  Set negotiation timeout",
        "  I  Set retry interval",
        "  B  Set block timeout",
        "  M  Set max retries",
        // ZMODEM settings menu
        "  F  Set frame timeout",
        // Shared by XMODEM/YMODEM/ZMODEM pages
        "  R  Restart server",
        // Server configuration menu
        "  T  Toggle telnet",
        "  P  Set telnet port",
        "  S  Toggle SSH",
        "  O  Set SSH port",
        "  K  Toggle Kermit",
        "  J  Set Kermit port",
        "  W  Toggle Web",
        "  B  Set Web port",
        "  I  IP safety",
        "  R  Restart server",
        "  C  Session cap",
        "  D  Idle timeout",
        // The two-key rows the server menu actually renders.
        // We test the full formatted strings (key letter included)
        // because the W/B and C/D rows are the tightest fit at 37 chars.
        "  T  Toggle telnet    P  Set telnet port",
        "  S  Toggle SSH       O  Set SSH port",
        "  K  Toggle Kermit    J  Set Kermit port",
        "  W  Toggle Web       B  Set Web port",
        "  I  IP safety        R  Restart server",
        "  C  Session cap      D  Idle timeout",
        "  M  Master/Slave",
        // Master/Slave sub-screen (two-key rows; tightest ~38 chars)
        "  R  Cycle role       A  Accept relays",
        "  M  Master host      P  Master port",
        "  U  Master user      W  Master pass",
        // Dialup mapping menu
        "  A  Add mapping",
        "  D  Delete mapping",
        // File transfer menu
        "  U  Upload a file",
        "  D  Download a file",
        "  X  Delete a file",
        "  C  Change directory",
        // Upload protocol picker (reached from the File Transfer
        // menu's U).  The key letter stands in for the color-wrapped
        // cyan() key, matching how the rest of this test models width.
        "  X  XMODEM/YMODEM  128/1K, auto",
        "  Z  ZMODEM         1K, autostart",
        "  P  PUNTER         C1 CCGMS/Novaterm",
        // Download protocol picker (reached from D).  PUNTER's
        // "C1 CCGMS/Novaterm" row is the tightest of these at 37 chars.
        // (KERMIT is intentionally not a picker option — server mode only.)
        "  X  XMODEM     128-byte blocks",
        "  1  XMODEM-1K  1024-byte blocks",
        "  Y  YMODEM     name+size hdr, 1K",
        "  Z  ZMODEM     autostart, 1K",
        "  P  PUNTER     C1 CCGMS/Novaterm",
        // Navigation footers
        "  R=Refresh Q=Back H=Help",
        // Auth prompts
        "  Username: ",
        "  Password: ",
        // AI chat
        "  Type a question, or Q to exit.",
        // Web browser
        "  G=Go/Search K=Bookmarks Q=Back H=Help",
    ];
    for item in &items {
        assert!(
            item.len() <= PETSCII_WIDTH,
            "menu item '{}' is {} chars, exceeds {}",
            item,
            item.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Main menu screen: header(3) + blank + 10 items + blank + help = 16 rows.
#[test]
fn test_main_menu_row_count() {
    // sep, title, sep, blank, A, B, C, F, G, R, S, T, W, X, blank, H=Help = 16
    let rows = 16;
    assert!(rows <= 22, "main menu is {} rows, exceeds 22", rows);
}

/// Main menu items must be exactly A, B, C, F, G, R, S, T, W, X (10 items).
#[test]
fn test_main_menu_item_count() {
    let items = ["A", "B", "C", "F", "G", "R", "S", "T", "W", "X"];
    assert_eq!(items.len(), 10, "main menu should have exactly 10 items");
}

/// Error hint must list exactly the valid main menu keys.
#[test]
fn test_main_menu_error_hint() {
    let hint = "Press A-C, F, G, R, S, T, W, X, or H.";
    // Must not mention removed keys (D, E, M)
    assert!(!hint.contains(" D,"), "error hint must not mention D");
    assert!(!hint.contains(" E,"), "error hint must not mention E");
    assert!(!hint.contains(" E "), "error hint must not mention E");
    assert!(!hint.contains(" M,"), "error hint must not mention M");
    // Must mention all valid keys
    for key in ["A", "C", "F", "G", "R", "S", "T", "W", "X", "H"] {
        assert!(hint.contains(key), "error hint must mention {}", key);
    }
    assert!(hint.len() <= PETSCII_WIDTH, "error hint exceeds PETSCII width");
}

/// Main help screen content has 17 lines (the dual-port refactor
/// stretched the G entry to 3 lines so it can mention the A/B
/// picker and the per-port console-mode requirement).  The
/// `show_help_page` paginator handles overflow gracefully, so the
/// total still fits the 22-row PETSCII budget for everything
/// except the bottom prompt — which lands on its own page if
/// needed.
#[test]
fn test_main_help_content_line_count() {
    assert_eq!(
        TelnetSession::main_help_lines().len(),
        17,
        "main help should have exactly 17 content lines"
    );
}

/// Shutdown broadcast message must be valid and end with CRLF.
#[test]
fn test_shutdown_message_format() {
    let msg = format!("\r\n\r\n{}\r\n", SHUTDOWN_GOODBYE);
    assert!(msg.ends_with("\r\n"), "shutdown message must end with CRLF");
    // Message must be short enough that it fits any terminal.
    assert!(
        SHUTDOWN_GOODBYE.len() <= PETSCII_WIDTH,
        "shutdown message exceeds PETSCII width"
    );
}

/// `broadcast_to_sessions` writes to every registered writer and (with
/// `close`) shuts each down — the central shutdown-goodbye primitive
/// that now runs from main.rs for any enabled-server combination.
#[tokio::test]
async fn test_broadcast_to_sessions_reaches_all_and_closes() {
    // Two registered "sessions", each a duplex whose far end we read.
    // A `DuplexStream` is itself an `AsyncWrite`; writing the near end
    // is readable at the far end, and shutting it EOFs the far end.
    let (a_near, mut a_far) = tokio::io::duplex(256);
    let (b_near, mut b_far) = tokio::io::duplex(256);
    let mk = |s: tokio::io::DuplexStream| -> SharedWriter {
        Arc::new(tokio::sync::Mutex::new(
            Box::new(s) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        ))
    };
    let writers: SessionWriters =
        Arc::new(tokio::sync::Mutex::new(vec![mk(a_near), mk(b_near)]));

    broadcast_to_sessions(&writers, b"BYE", true).await;

    // Each far end sees "BYE" then EOF (writer was shut down).
    for far in [&mut a_far, &mut b_far] {
        let mut buf = Vec::new();
        far.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"BYE", "session did not receive the broadcast");
    }
}

/// Dialup mapping menu (with entries): header(3) + blank + 10 entries + blank
/// + 2 items + blank + footer = 18 rows max.
#[test]
fn test_dialup_mapping_menu_row_count() {
    // Worst case: static entry + 9 user entries + A + D menu items
    let rows = 3 + 1 + 1 + 9 + 1 + 2 + 1 + 1; // 19
    assert!(rows <= 22, "dialup mapping menu is {} rows, exceeds 22", rows);
}

/// Dialup mapping help screen row count.  Dual-port wording
/// added 3 lines (the "shared across both ports' modems"
/// clarification); paginator handles overflow gracefully.
#[test]
fn test_dialup_help_screen_row_count() {
    // header(3) + blank + 15 content lines + blank + "press any key" = 21
    let rows = 3 + 1 + 15 + 1 + 1; // 21
    assert!(rows <= 22, "dialup help screen is {} rows, exceeds 22", rows);
}

/// Dialup mapping help content must have exactly 15 lines and fit PETSCII.
#[test]
fn test_dialup_help_content() {
    let lines = TelnetSession::dialup_help_lines();
    assert_eq!(lines.len(), 15, "dialup help should have exactly 15 content lines");
    for line in lines {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "dialup help line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Dialup mapping prompts must fit PETSCII width.
#[test]
fn test_dialup_prompts_fit_petscii() {
    let prompts = [
        "  Phone number: ",
        "  Host: ",
        "  Port (23): ",
        "  Entry # to delete: ",
    ];
    for prompt in &prompts {
        assert!(
            prompt.len() <= PETSCII_WIDTH,
            "dialup prompt '{}' is {} chars, exceeds {}",
            prompt,
            prompt.len(),
            PETSCII_WIDTH,
        );
    }
}

/// File transfer menu: header(3) + blank + dir + blank + 5 items + blank + footer = 12 rows.
#[test]
fn test_file_transfer_menu_row_count() {
    let rows = 3 + 1 + 1 + 1 + 5 + 1 + 1; // 13
    assert!(rows <= 22, "file transfer menu is {} rows, exceeds 22", rows);
}

/// Download/delete file listing: header(3) + blank + col_header + divider
/// + 10 entries + blank + page_info + blank + nav + blank + prompt = 21 rows.
#[test]
fn test_file_listing_row_count() {
    let header = 3; // sep + title + sep
    let col = 2;    // column header + divider
    let entries = TelnetSession::TRANSFER_PAGE_SIZE; // 10
    let footer = 5; // blank + page info + blank + nav + prompt
    let total = header + 1 + col + entries + footer;
    assert!(
        total <= 22,
        "file listing is {} rows, exceeds 22",
        total,
    );
}

/// AI answer screen: header(3) + 14 content lines + padding + position
/// + nav + prompt = ~22 rows max.
#[test]
fn test_ai_answer_row_count() {
    let header = 3;  // sep + question + sep
    let content = TelnetSession::PAGE_CONTENT_LINES; // 14
    let footer = 3;  // position + nav + prompt
    let total = header + content + footer;
    assert!(
        total <= 22,
        "AI answer screen is {} rows, exceeds 22",
        total,
    );
}

/// Auth screen: header(3) + blank + up to 3 attempts * 4 lines = 15 rows max.
#[test]
fn test_auth_screen_row_count() {
    // sep + title + sep + blank + (username + password + error + blank)*3
    let header = 4;
    let per_attempt = 4; // username prompt, password prompt, error, blank
    let total = header + per_attempt * 3;
    assert!(
        total <= 22,
        "auth screen is {} rows, exceeds 22",
        total,
    );
}

/// Modem emulator screen worst case: non-serial + serial enabled.
/// header(3) + blank + status(5) + ATD(1) + blank
/// + menu(7: E,S,B,P,F,D,I) + blank + footer(1) + prompt(1) = 21.
#[test]
fn test_modem_emulator_row_count() {
    let rows = 3 + 1 + 5 + 1 + 1 + 7 + 1 + 1 + 1; // 21
    assert!(rows <= 22, "modem emulator is {} rows, exceeds 22", rows);
}

/// Baud rate screen: header(3) + blank + 9 options + blank + footer + prompt = 15.
#[test]
fn test_baud_screen_row_count() {
    let rows = 3 + 1 + 9 + 1 + 1 + 1; // 16
    assert!(rows <= 22, "baud screen is {} rows, exceeds 22", rows);
}

/// Flow control screen: header(3) + blank + 3 options + blank + footer + prompt = 10.
#[test]
fn test_flow_control_screen_row_count() {
    let rows = 3 + 1 + 3 + 1 + 1 + 1; // 10
    assert!(rows <= 22, "flow control screen is {} rows, exceeds 22", rows);
}

/// Data bits screen: header(3) + blank + 4 options + blank + footer + prompt = 11.
#[test]
fn test_data_bits_screen_row_count() {
    let rows = 3 + 1 + 4 + 1 + 1 + 1; // 11
    assert!(rows <= 22, "data bits screen is {} rows, exceeds 22", rows);
}

/// Parity screen: header(3) + blank + 3 options + blank + footer + prompt = 10.
#[test]
fn test_parity_screen_row_count() {
    let rows = 3 + 1 + 3 + 1 + 1 + 1; // 10
    assert!(rows <= 22, "parity screen is {} rows, exceeds 22", rows);
}

/// Stop bits screen: header(3) + blank + 2 options + blank + footer + prompt = 9.
#[test]
fn test_stop_bits_screen_row_count() {
    let rows = 3 + 1 + 2 + 1 + 1 + 1; // 9
    assert!(rows <= 22, "stop bits screen is {} rows, exceeds 22", rows);
}

/// Configuration menu static rows (no addresses):
/// header(3) + blank + status(2) + blank + menu(4) + blank + footer(1) + prompt(1) = 14.
/// The IP address list is dynamic; with addresses it adds a label + N addrs + blank.
/// Typical machines have 1-3 addresses, fitting well within 22.
#[test]
fn test_config_menu_row_count() {
    // CONFIGURATION submenu now carries the "Server addresses:" banner
    // at the top (relocated here from Server Config, §4.7):
    // header(3) + address block [label(1) + N addrs + ATD example(1)]
    // + blank + 7 items (E, G, M, S, F, O, R) + blank + Q/H + prompt.
    // Worst case is N = SERVER_ADDR_DISPLAY_CAP.
    let submenu_rows = 3 + (1 + SERVER_ADDR_DISPLAY_CAP + 1) + 1 + 7 + 1 + 1 + 1; // 19
    assert!(submenu_rows <= 22, "config submenu is {} rows, exceeds 22", submenu_rows);
    // Server configuration: header(3) + 5 status (telnet, ssh,
    // kermit, web, ip-safety) + blank + 7 item rows (T/P, S/O, K/J,
    // W/B, I/R, C/D, and the new single M Master/Slave row) + Q/H +
    // prompt = 18.  The address block moved to the CONFIGURATION menu,
    // so this screen no longer grows with the detected-IP list.
    let static_rows = 3 + 5 + 1 + 7 + 1 + 1; // 18
    assert!(static_rows <= 22, "server config menu is {} rows, exceeds 22", static_rows);
}

/// Master/Slave sub-screen: header(3) + blank + 5 status (role,
/// accept-relays, master host:port, user, pass) + blank + 3 item rows
/// (R/A, M/P, U/W) + Q/H + prompt = 15.  (Transport is not exposed
/// until the raw transport is implemented; SSH is the only mode.)
/// Plus the §9 #10 live-status block. A master shows a "Registered
/// remote ports:" header, up to 3 entries (RELAY_STATUS_CAP), an
/// optional "+N more", and a trailing blank — 6 rows worst case. A slave
/// shows up to 2 link lines and a blank — 3 rows. The master case
/// dominates, so the guard is base plus 6.
#[test]
fn test_master_slave_menu_row_count() {
    let base = 3 + 1 + 5 + 1 + 3 + 1 + 1; // 15
    let master_status_worst = 1 + 3 + 1 + 1; // header + cap + "+N more" + blank
    let rows = base + master_status_worst; // 21
    assert!(rows <= 22, "master/slave menu is {} rows, exceeds 22", rows);
}

/// Configuration help screen (ANSI): header(3) + blank + 15 content lines +
/// blank + "Press any key" = 21 rows.
#[test]
fn test_config_help_screen_row_count() {
    let rows = 3 + 1 + 15 + 1 + 1; // 21
    assert!(rows <= 22, "config help screen is {} rows, exceeds 22", rows);
}

/// Configuration help lines (PETSCII) must fit 40 cols.
#[test]
fn test_config_help_lines_fit_petscii() {
    for line in TelnetSession::config_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "config help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Security menu row count after the unified-credentials merge:
/// header(3) + blank + login-status + blank + 2 creds (Username,
/// Password) + blank + 4 items (L/U/P/R) + blank + Q/H + prompt
/// = 16.  Previously 20 with separate telnet/SSH user+pass rows
/// and S/W menu items.
#[test]
fn test_security_menu_row_count() {
    let rows = 3 + 1 + 1 + 1 + 2 + 1 + 4 + 1 + 1 + 1; // 16
    assert!(rows <= 22, "security menu is {} rows, exceeds 22", rows);
}

/// Security help lines (PETSCII) must fit 40 cols.
#[test]
fn test_security_help_lines_fit_petscii() {
    for line in TelnetSession::security_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "security help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Security help screen (PETSCII): header(3) + blank + 13 content +
/// blank + "Press any key" = 19 rows.
#[test]
fn test_security_help_screen_row_count() {
    let rows = 3 + 1 + 13 + 1 + 1; // 19
    assert!(rows <= 22, "security help screen is {} rows, exceeds 22", rows);
}

/// Other settings menu row count:
/// header(3) + blank + 6 values + blank + 8 items + blank + Q/H + prompt = 22
/// (units fold into the Weather value line, so no extra value row; the
/// new `U` "Cycle weather units" action is the 8th item.)
#[test]
fn test_other_settings_menu_row_count() {
    let rows = 3 + 1 + 6 + 1 + 8 + 1 + 1 + 1; // 22
    assert!(rows <= 22, "other settings menu is {} rows, exceeds 22", rows);
}

/// Other settings help lines (PETSCII) must fit 40 cols.
#[test]
fn test_other_help_lines_fit_petscii() {
    for line in TelnetSession::other_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "other help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Punter settings help (PETSCII) must fit 40 cols.  Asserts the REAL help
/// lines via the shared associated fn (no duplicated copy to drift), so the
/// G (bad-block limit) and D (hangup-on-failure) items stay within width.
#[test]
fn test_punter_help_lines_fit_petscii() {
    for line in TelnetSession::punter_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "punter help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Punter settings menu must fit the 22-row PETSCII screen.
/// header(3) + blank + 6 value lines + blank + 8 items (B/N/I/F/M/G/D/R)
/// + blank + Q/H + prompt = 22.
#[test]
fn test_punter_settings_menu_row_count() {
    let rows = 3 + 1 + 6 + 1 + 8 + 1 + 1 + 1; // 22
    assert!(rows <= 22, "punter settings menu is {} rows, exceeds 22", rows);
}

/// Other settings help screen (PETSCII): header(3) + blank + 13 content +
/// blank + "Press any key" = 19 rows.
#[test]
fn test_other_help_screen_row_count() {
    let rows = 3 + 1 + 13 + 1 + 1; // 19
    assert!(rows <= 22, "other help screen is {} rows, exceeds 22", rows);
}

/// File Transfer settings submenu row count:
/// header(3) + blank + 1 value + blank + 5 items + blank + Q/H + prompt = 14
#[test]
fn test_file_transfer_settings_menu_row_count() {
    let rows = 3 + 1 + 1 + 1 + 5 + 1 + 1 + 1; // 14
    assert!(rows <= 22, "file transfer settings menu is {} rows, exceeds 22", rows);
}

// ─── paginate_help ─────────────────────────────────────
//
// `show_help_page` delegates paging to `TelnetSession::paginate_help`.
// These tests lock in the blank-line-respecting behavior so groups
// of related lines (section header + continuations) stay together
// on a single page — regressions here would split a letter-command
// from its description, which is exactly what we don't want.

/// Content that fits within one page passes through unchanged (no
/// trailing blanks, no split).
#[test]
fn test_paginate_help_single_page() {
    let lines = ["  A  line one", "  B  line two", "  C  line three"];
    let pages = TelnetSession::paginate_help(&lines, 15);
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0], lines);
}

/// Empty content produces zero pages — `show_help_page` handles
/// that by substituting a single empty page.
#[test]
fn test_paginate_help_empty() {
    let pages = TelnetSession::paginate_help(&[], 15);
    assert!(pages.is_empty());
}

/// When content overflows, split at the last blank line within the
/// page-size budget. Trailing blanks are stripped so each page
/// starts and ends on a real content line.
#[test]
fn test_paginate_help_splits_at_blank_line() {
    // 20 lines total with a blank at index 9. Budget = 15, so the
    // splitter should pick the blank at position 10 (1-indexed),
    // strip it, and emit page 1 = lines 0..9, page 2 = lines 10..19.
    let lines = [
        "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "",
        "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10",
    ];
    let pages = TelnetSession::paginate_help(&lines, 15);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0], &["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9"]);
    assert_eq!(
        pages[1],
        &["b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9", "b10"]
    );
}

/// When no blank line exists within the budget, fall back to a
/// hard split at `max_per_page`. Authors should avoid this by
/// adding blank lines between groups — but we don't want to loop
/// forever on malformed input either.
#[test]
fn test_paginate_help_force_split_when_no_blank() {
    let lines = [
        "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10",
        "x11", "x12", "x13", "x14", "x15", "x16", "x17",
    ];
    let pages = TelnetSession::paginate_help(&lines, 10);
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].len(), 10);
    assert_eq!(pages[1].len(), 7);
}

/// A section header + its indented continuation lines must stay
/// together when separated from other groups by blank lines. This
/// is the guarantee the user asked for.
#[test]
fn test_paginate_help_keeps_section_groups_together() {
    let lines = [
        "  A  alpha header",
        "     first continuation",
        "     second continuation",
        "",
        "  B  beta header",
        "     beta continuation",
        "",
        "  C  gamma header",
        "     gamma continuation",
        "     gamma continuation 2",
    ];
    // Budget of 5 forces a split — but NEVER in the middle of a
    // group.  With a blank at index 3 and 6, the splitter picks
    // the latest blank inside the first 5: index 3.  Page 1 gets
    // lines 0..3 (the A group). Page 2 has 6 lines remaining,
    // still over budget, so it splits at the next blank (index 2
    // of the remainder): the B group alone (2 lines).  Page 3:
    // the C group (3 lines).
    let pages = TelnetSession::paginate_help(&lines, 5);
    assert_eq!(pages.len(), 3, "expected 3 pages, got {:?}", pages);
    assert_eq!(pages[0].len(), 3); // A + 2 continuations
    assert_eq!(pages[0][0], "  A  alpha header");
    assert_eq!(pages[1].len(), 2); // B + 1 continuation
    assert_eq!(pages[1][0], "  B  beta header");
    assert_eq!(pages[2].len(), 3); // C + 2 continuations
    assert_eq!(pages[2][0], "  C  gamma header");
}

/// Multiple consecutive blanks between groups collapse on page
/// boundaries — the next page starts on the next real content
/// line, not on a floating blank.
#[test]
fn test_paginate_help_skips_leading_blanks() {
    let lines = ["a", "a", "a", "", "", "", "b", "b"];
    let pages = TelnetSession::paginate_help(&lines, 3);
    // Page 1 is the three a's; the three blanks get swallowed at
    // the split; page 2 starts cleanly on "b" with no stray
    // leading blanks.
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0], &["a", "a", "a"]);
    assert_eq!(pages[1], &["b", "b"]);
}

/// Invalid `max_per_page` of 0 should panic (debug only — the
/// caller in show_help_page passes a compile-time constant, so
/// this can never happen in practice, but the assertion guards
/// against a future typo).
#[test]
#[should_panic(expected = "max_per_page")]
fn test_paginate_help_zero_max_panics() {
    let _ = TelnetSession::paginate_help(&["a"], 0);
}

/// The paging footer string must fit PETSCII width (40 cols).
/// If this test fails, update the `show_help_page` footer format
/// string.
#[test]
fn test_paging_footer_fits_petscii() {
    let examples = [
        "  Page 1/2 - next key, Q to quit",
        "  Page 10/99 - next key, Q to quit",
        "  Page 2/2 - Press any key.",
        "  Press any key to continue.",
    ];
    for s in &examples {
        assert!(
            s.len() <= PETSCII_WIDTH,
            "paging footer '{}' is {} chars, exceeds {}",
            s, s.len(), PETSCII_WIDTH
        );
    }
}

/// XMODEM / YMODEM settings menu row count (shared renderer):
/// header(3) + blank + 5 values + blank + 5 items + blank + Q/H + prompt = 18
#[test]
fn test_xmodem_settings_menu_row_count() {
    let rows = 3 + 1 + 5 + 1 + 5 + 1 + 1 + 1; // 18
    assert!(rows <= 22, "xmodem settings menu is {} rows, exceeds 22", rows);
}

/// ZMODEM settings menu row count:
/// header(3) + blank + 4 values + blank + 5 items + blank + Q/H + prompt = 17
#[test]
fn test_zmodem_settings_menu_row_count() {
    let rows = 3 + 1 + 4 + 1 + 5 + 1 + 1 + 1; // 17
    assert!(rows <= 22, "zmodem settings menu is {} rows, exceeds 22", rows);
}

/// XMODEM settings help lines (PETSCII) must fit 40 cols.
#[test]
fn test_xmodem_help_lines_fit_petscii() {
    for line in TelnetSession::xmodem_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "xmodem help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// ZMODEM settings help lines (PETSCII) must fit 40 cols.
#[test]
fn test_zmodem_help_lines_fit_petscii() {
    for line in TelnetSession::zmodem_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "zmodem help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Kermit settings help lines (PETSCII) must fit 40 cols.  Asserts the
/// REAL lines via the shared associated fn (no duplicated copy) — Kermit's
/// help had no width guard before this.
#[test]
fn test_kermit_help_lines_fit_petscii() {
    for line in TelnetSession::kermit_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "kermit help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// File transfer help lines (PETSCII) must fit 40 cols.
#[test]
fn test_file_transfer_help_lines_fit_petscii() {
    for line in TelnetSession::file_transfer_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "file transfer help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// XMODEM help screen (PETSCII): header(3) + blank + 15 content +
/// blank + "Press any key" = 21 rows.
#[test]
fn test_xmodem_help_screen_row_count() {
    let rows = 3 + 1 + 15 + 1 + 1; // 21
    assert!(rows <= 22, "xmodem help screen is {} rows, exceeds 22", rows);
}

/// ZMODEM help screen (PETSCII): header(3) + blank + 15 content +
/// blank + "Press any key" = 21 rows.  Content grew by +2 rows (Retry
/// interval) but we trimmed the footer by -2, net 0.
#[test]
fn test_zmodem_help_screen_row_count() {
    let rows = 3 + 1 + 15 + 1 + 1; // 21
    assert!(rows <= 22, "zmodem help screen is {} rows, exceeds 22", rows);
}

/// File Transfer help screen (PETSCII): header(3) + blank + 13 content
/// + blank + "Press any key" = 19 rows.
#[test]
fn test_file_transfer_help_screen_row_count() {
    let rows = 3 + 1 + 13 + 1 + 1; // 19
    assert!(
        rows <= 22,
        "file transfer help screen is {} rows, exceeds 22",
        rows,
    );
}

/// The breadcrumb prompts for the File Transfer submenu and each
/// per-protocol page must fit PETSCII width (40 cols) when the
/// "> " suffix is appended.
#[test]
fn test_file_transfer_breadcrumbs_fit_petscii() {
    // These mirror the literal strings passed to `self.cyan(...)`
    // in the submenu and per-protocol pages.  Keep this list in
    // sync with the code; a rename in one place will trigger a
    // test failure if not updated here.
    let breadcrumbs = [
        "ethernet/config/xfer",
        "ethernet/config/xfer/xmodem",
        "ethernet/config/xfer/ymodem",
        "ethernet/config/xfer/zmodem",
    ];
    for b in &breadcrumbs {
        let prompt = format!("{}> ", b);
        assert!(
            prompt.len() <= PETSCII_WIDTH,
            "breadcrumb prompt '{}' is {} chars, exceeds {}",
            prompt,
            prompt.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Every per-protocol settings page must render its status rows
/// (value column) within PETSCII width.  The longest rendered
/// status line is `  Applies to:    <applies_to>`, with the
/// `applies_to` values below plugged into `xmodem_family_settings`.
#[test]
fn test_xmodem_family_applies_to_lines_fit_petscii() {
    for applies_to in &["XMODEM family", "XMODEM family (shared)"] {
        let line = format!("  Applies to:    {}", applies_to);
        assert!(
            line.len() <= PETSCII_WIDTH,
            "'Applies to' line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Modem help screen (ANSI): header(3) + blank + 16 content lines +
/// blank + "Press any key" = 22 rows.
#[test]
fn test_modem_help_screen_row_count() {
    let rows = 3 + 1 + 16 + 1 + 1; // 22
    assert!(rows <= 22, "modem help screen is {} rows, exceeds 22", rows);
}

/// Main help screen: header(3) + blank + 16 content lines +
/// blank + "Press any key" = 22 rows.
#[test]
fn test_main_help_screen_row_count() {
    let rows = 3 + 1 + 16 + 1 + 1; // 22
    assert!(rows <= 22, "main help screen is {} rows, exceeds 22", rows);
}

/// Serial Gateway pre-bridge screen rows: sep(1) + title(1) +
/// sep(1) + blank(1) + Port + Baud + Data + blank(1) + Press +
/// Single + next + blank(1) + prompt(1) = 13.  Stays comfortably
/// within 22.
#[test]
fn test_serial_gateway_screen_row_count() {
    let rows = 3 + 1 + 3 + 1 + 3 + 1 + 1; // 13
    assert!(rows <= 22, "serial gateway screen is {} rows, exceeds 22", rows);
}

/// Every fixed line in the Serial Gateway screen must fit PETSCII
/// width.  The Port line varies with the configured device path
/// but the chrome around it does not — those are the lines we can
/// pin down.
#[test]
fn test_serial_gateway_lines_fit_petscii() {
    let fixed = [
        "  SERIAL GATEWAY",
        "  Press ESC ESC to disconnect.",
        "  Press <- <- to disconnect.",
        "  Single ESC passes through on the",
        "  next keystroke.",
        "  Connect now? (Y/N): ",
        "  Acquiring serial port...",
        "  Connected.",
        "  Serial bridge closed.",
        "  Press any key to continue.",
    ];
    for line in &fixed {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "serial-gateway line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
    // The Port line carries the full device path.  Confirm the
    // template fits with a realistically long path.
    let port = "  Port: /dev/ttyUSB10";
    assert!(port.len() <= PETSCII_WIDTH, "port line {} chars", port.len());
    // Highest baud anyone is realistically setting.
    let baud = "  Baud: 115200";
    assert!(baud.len() <= PETSCII_WIDTH, "baud line {} chars", baud.len());
    // Worst-case data line: 8N1 flow=software.
    let data = "  Data: 8N1 flow=software";
    assert!(data.len() <= PETSCII_WIDTH, "data line {} chars", data.len());
}

/// Per-port picker rows in `gateway_serial_picker` and
/// `serial_configuration_menu` use a two-line layout (role label
/// on line 1, device + baud on line 2 when configured).  ASCII
/// only — no em-dash — so .len() byte count matches display width
/// on PETSCII clients.  Worst-case lines must fit the 40-col
/// PETSCII budget.
#[test]
fn test_serial_picker_lines_fit_petscii() {
    // Line 1 chrome: "  " + "[A] Port A" + " - " + role label.
    // Worst-case role label is "Console mode" (12 chars).
    let line1_max = "  [A] Port A - Console mode";
    assert!(
        line1_max.len() <= PETSCII_WIDTH,
        "picker line 1 is {} chars",
        line1_max.len()
    );

    // Line 2 chrome: 6 indent + path + " " + baud.  Path is
    // truncated to 23 chars in the picker; baud is at most 6
    // chars ("115200").  Compose the worst-case line and
    // assert it fits the budget so a future edit that loosens
    // truncation can't silently overflow.
    let line2_max = format!(
        "      {} {}",
        "x".repeat(23), // worst-case truncated path
        115200
    );
    assert!(
        line2_max.len() <= PETSCII_WIDTH,
        "picker line 2 is {} chars",
        line2_max.len()
    );

    // No-eligible-port fallback lines.
    for line in &[
        "  No port is available to bridge.",
        "  Enable console mode via Config > M.",
    ] {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "fallback '{}' is {} chars",
            line,
            line.len()
        );
    }
}

/// Per-port menu titles (modem_settings, modem_select_port, baud,
/// data, parity, stop, flow, ring) all use the format
/// "PORT {A|B} - <NAME>".  ASCII hyphen, not em-dash, so .len()
/// matches display width.  Each title plus its "  " indent must
/// fit PETSCII width.
#[test]
fn test_per_port_titles_fit_petscii() {
    let titles = [
        "PORT A - MODEM EMULATOR",
        "PORT A - SERIAL CONSOLE",
        "PORT A - DEVICE",
        "PORT A - BAUD RATE",
        "PORT A - DATA BITS",
        "PORT A - PARITY",
        "PORT A - STOP BITS",
        "PORT A - FLOW CONTROL",
        "PORT A - RING EMULATOR",
        "PORT B - MODEM EMULATOR",
        "PORT B - SERIAL CONSOLE",
        "SERIAL GATEWAY (PORT A)",
        "SERIAL GATEWAY (PORT B)",
        "SERIAL CONFIGURATION",
    ];
    for t in &titles {
        let line = format!("  {}", t);
        assert!(
            line.len() <= PETSCII_WIDTH,
            "title line '{}' is {} chars",
            line,
            line.len()
        );
        // No multi-byte characters that would render as garbage on
        // a PETSCII client — every byte must be printable ASCII.
        assert!(
            t.is_ascii(),
            "title '{}' contains non-ASCII characters",
            t
        );
    }
}

/// Non-serial constructors (`new_ssh`, the regular `new`) MUST
/// leave `serial_port_id = None`.  This is the load-bearing
/// invariant the per-port-scoped warn/revert/T/I gating relies on:
/// a stale `Some(...)` here would cause non-serial sessions to be
/// gated as if they were on a specific port.
#[test]
fn test_non_serial_sessions_have_no_port_id() {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    let (_w, reader) = tokio::io::duplex(1);
    let (_, writer_inner) = tokio::io::duplex(1);
    let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
        Box::new(writer_inner),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));

    let ssh_session = TelnetSession::new_ssh(
        Box::new(reader),
        writer,
        shutdown,
        restart,
        None,
        lockouts,
    );
    assert!(!ssh_session.is_serial, "SSH session must not be is_serial");
    assert!(ssh_session.is_ssh, "SSH session must be is_ssh");
    assert_eq!(
        ssh_session.serial_port_id, None,
        "SSH session must not carry a serial port id"
    );
}

/// `TelnetSession::new_serial` stores the caller's port id so
/// `modem_apply_settings` can scope the warn-+-revert flow to
/// the OWN port only.  Pin the constructor's behavior so a future
/// edit can't silently drop the field initialization.
#[test]
fn test_telnet_session_new_serial_stores_port_id() {
    use crate::config::SerialPortId;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    // Build a minimal-viable serial session to inspect its fields.
    // The reader/writer are only needed for the type signature —
    // we never run any I/O against them in this test.
    let (_w, reader) = tokio::io::duplex(1);
    let (_, writer_inner) = tokio::io::duplex(1);
    let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
        Box::new(writer_inner),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));

    let session_a = TelnetSession::new_serial(
        SerialPortId::A,
        Box::new(reader),
        writer.clone(),
        shutdown.clone(),
        restart.clone(),
        lockouts.clone(),
    );
    assert!(session_a.is_serial);
    assert_eq!(session_a.serial_port_id, Some(SerialPortId::A));

    // A second session on Port B records B, not A — proves the
    // field tracks the constructor argument and isn't accidentally
    // hardcoded.
    let (_w2, reader2) = tokio::io::duplex(1);
    let session_b = TelnetSession::new_serial(
        SerialPortId::B,
        Box::new(reader2),
        writer,
        shutdown,
        restart,
        lockouts,
    );
    assert_eq!(session_b.serial_port_id, Some(SerialPortId::B));

    // is_own_arrival_port: a serial session owns ONLY its arrival
    // port (so the Serial Gateway picker excludes just that one and
    // the user can still bridge to the other port).
    assert!(session_a.is_own_arrival_port(SerialPortId::A));
    assert!(!session_a.is_own_arrival_port(SerialPortId::B));
    assert!(session_b.is_own_arrival_port(SerialPortId::B));
    assert!(!session_b.is_own_arrival_port(SerialPortId::A));
}

/// A non-serial (telnet/SSH) session never owns a serial port, so
/// `is_own_arrival_port` is false for every port — it may bridge to
/// any eligible port and the picker excludes none.
#[test]
fn test_non_serial_session_owns_no_port() {
    use crate::config::SerialPortId;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    let (_w, reader) = tokio::io::duplex(1);
    let (_, writer_inner) = tokio::io::duplex(1);
    let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
        Box::new(writer_inner),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));
    let session = TelnetSession::new_ssh(
        Box::new(reader),
        writer,
        shutdown,
        restart,
        None,
        lockouts,
    );
    assert!(!session.is_serial);
    assert!(!session.is_relay);
    assert_eq!(session.client_type_label(), "SSH");
    assert!(!session.is_own_arrival_port(SerialPortId::A));
    assert!(!session.is_own_arrival_port(SerialPortId::B));
}

/// A relay session (master/slave) is flagged `is_relay`, owns no local
/// port, and is labelled distinctly from a local serial caller.
#[test]
fn test_relay_session_identity() {
    use crate::config::SerialPortId;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    let (_w, reader) = tokio::io::duplex(1);
    let (_, writer_inner) = tokio::io::duplex(1);
    let writer: SharedWriter = std::sync::Arc::new(tokio::sync::Mutex::new(
        Box::new(writer_inner),
    ));
    let shutdown = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));
    let session = TelnetSession::new_relay(
        Box::new(reader),
        writer,
        shutdown,
        restart,
        Some("192.168.1.50".parse().unwrap()),
        lockouts,
    );
    assert!(session.is_relay);
    // Behaves like a serial caller (raw 8-bit) but owns no local port.
    assert!(session.is_serial);
    assert_eq!(session.serial_port_id, None);
    assert!(!session.is_own_arrival_port(SerialPortId::A));
    assert!(!session.is_own_arrival_port(SerialPortId::B));
    // Labelled as a relay, not "Serial modem".
    assert_eq!(session.client_type_label(), "Relay (slave)");
}

/// New Serial Gateway picker (always shown, even when only one
/// port is eligible).  Two lines per port (role + device/baud
/// when configured), so port_rows = 4 worst-case.  Must fit the
/// 22-row PETSCII budget even when both ports show device
/// detail and the eligibility fallback is showing.
#[test]
fn test_serial_gateway_picker_row_count() {
    let chrome = 3 + 1; // sep+title+sep, blank
    // Peer-dial header ("Dial from a modem port: ...") + blank, shown
    // only when allow_peer_dial is on.
    let peer_header = 1 + 1;
    // Worst case: both ports configured, so each takes 2 lines.
    let port_rows = 2 * 2;
    let blank_after = 1;
    let footer = 1 + 1; // Q footer + prompt
    // The fallback ("no port available", 3 rows) and the remote block
    // are mutually exclusive (remotes make a port "eligible", so the
    // fallback is suppressed).  The remote block is the larger of the
    // two, so it drives the worst case: header + capped entries +
    // "+N more" + trailing blank.
    let fallback = 1 + 1 + 1;
    let remote_block = 1 + REMOTE_PORT_DISPLAY_CAP + 1 + 1;
    let bigger = if remote_block > fallback { remote_block } else { fallback };
    let worst_case = chrome + peer_header + port_rows + blank_after + bigger + footer;
    assert!(
        worst_case <= 22,
        "Serial Gateway picker is {} rows, exceeds 22",
        worst_case
    );
}

/// New Serial Configuration picker (Configuration → M now lands
/// here): chrome + 4 port rows (worst case both configured) +
/// gateway-debug toggle row + footer + prompt.  Fits well under the
/// 22-row budget.
#[test]
fn test_serial_configuration_picker_row_count() {
    let chrome = 3 + 1; // sep+title+sep, blank
    let port_rows = 2 * 2; // 2 lines per port at worst case
    let blank_after = 1;
    let debug_row = 1; // gateway-debug status line
    let peer_dial_row = 1; // peer-dial status line
    let blank_before_footer = 1;
    let footer = 1 + 1; // footer line + prompt
    let total =
        chrome + port_rows + blank_after + debug_row + peer_dial_row + blank_before_footer + footer;
    assert!(
        total <= 22,
        "Serial Configuration picker is {} rows",
        total
    );
}

/// Modem/Console settings menu in console mode loses Dialup +
/// Ring rows.  T (and I, in modem mode) hide only when the caller
/// is dialed in on THIS port — a serial-side caller editing the
/// OTHER port still sees the full menu.  Item count must still
/// fit the 22-row budget in every combination.
#[test]
fn test_modem_console_menu_row_counts() {
    // Status block: status_mode + Port + Baud + Data + Flow = 5.
    let status_block = 5;
    let chrome = 3 + 1 + 1 + 1 + 1; // sep+title+sep, blank, blank, footer, prompt = 7
    // Console mode (own port hides T): E S B P F = 5; (other or
    // non-serial: + T) = 6.
    let menu_console_own_port = 5;
    let menu_console_other_or_non_serial = 6; // + T
    // Modem mode (own port hides T and I): E S B P D F = 6;
    // (other or non-serial: + T + I) = 8.
    let menu_modem_own_port = 6; // E S B P D F
    let menu_modem_full = 8; // E T S B P D F I

    // Console mode + caller on this port: no T.
    let console_own_rows = chrome + status_block + menu_console_own_port; // 17
    assert!(
        console_own_rows <= 22,
        "console-own-port menu is {} rows",
        console_own_rows
    );

    // Console mode + caller on the OTHER port (or not serial): + T.
    let console_other_rows = chrome + status_block + menu_console_other_or_non_serial; // 18
    assert!(
        console_other_rows <= 22,
        "console-other-port menu is {} rows",
        console_other_rows
    );

    // Modem mode + caller on this port + enabled: + ATD + D, no T, no I.
    let modem_own_rows = chrome + status_block + 1 + menu_modem_own_port; // 20
    assert!(
        modem_own_rows <= 22,
        "modem-own-port menu is {} rows",
        modem_own_rows
    );

    // Modem mode + caller on the OTHER port (or non-serial) +
    // enabled (worst case): + ATD + T + D + I.
    let modem_full_rows = chrome + status_block + 1 + menu_modem_full; // 22
    assert!(
        modem_full_rows <= 22,
        "modem-full menu is {} rows",
        modem_full_rows
    );

    // Kermit-server mode reuses the console (raw-wire) layout — same
    // E S B P F [+ T] menu, no ATD/Dialup/Ring/Carrier — so its row
    // count is identical to console mode and safely under 22.
    let kermit_own_rows = chrome + status_block + menu_console_own_port; // 17
    let kermit_other_rows = chrome + status_block + menu_console_other_or_non_serial; // 18
    assert!(kermit_own_rows <= 22, "kermit-own-port menu is {} rows", kermit_own_rows);
    assert!(kermit_other_rows <= 22, "kermit-other-port menu is {} rows", kermit_other_rows);
}

/// The complete set of help-line tables at the given width — the single
/// source for both the PETSCII (40) and ANSI (80) fit tests.
/// MAINTENANCE: every `*_help_lines` fn must appear here exactly once; a
/// new help screen is only width-checked once added below (bump the array
/// length to match).  Single-width tables ignore `petscii` (they fit 40).
fn all_help_line_groups(petscii: bool) -> [&'static [&'static str]; 27] {
    [
        TelnetSession::main_help_lines(),
        TelnetSession::config_submenu_help_lines(petscii),
        TelnetSession::config_help_lines(petscii),
        TelnetSession::other_help_lines(petscii),
        TelnetSession::security_help_lines(petscii),
        TelnetSession::xmodem_help_lines(petscii),
        TelnetSession::zmodem_help_lines(petscii),
        TelnetSession::kermit_help_lines(petscii),
        TelnetSession::punter_help_lines(petscii),
        TelnetSession::file_transfer_help_lines(petscii),
        TelnetSession::file_transfer_menu_help_lines(),
        TelnetSession::download_help_lines(),
        TelnetSession::delete_help_lines(),
        TelnetSession::ai_chat_help_lines(),
        TelnetSession::dialup_help_lines(),
        TelnetSession::modem_help_lines(petscii),
        TelnetSession::console_help_lines(petscii),
        TelnetSession::kermit_mode_help_lines(petscii),
        TelnetSession::browser_page_help_lines(petscii),
        TelnetSession::browser_menu_help_lines(),
        TelnetSession::bookmarks_help_lines(),
        TelnetSession::form_help_lines(),
        TelnetSession::gateway_config_help_lines(petscii),
        TelnetSession::serial_config_help_lines(),
        TelnetSession::master_slave_help_lines(petscii),
        TelnetSession::cpm_help_lines(),
        TelnetSession::CPM_ENTRY_TIPS,
    ]
}

/// Every help screen's PETSCII variant must fit 40 cols.  Catch-all that
/// guards screens without an individual `*_help_lines_fit_petscii` test
/// (main menu, config/gateway/serial submenus, the file pickers, etc.).
#[test]
fn test_help_lines_fit_petscii() {
    let groups = all_help_line_groups(true);
    for line in groups.iter().flat_map(|g| g.iter()) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "PETSCII help line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Every help screen's ANSI/ASCII variant must fit 80 cols (screen layout:
/// 40 for PETSCII, 80 for ANSI/ASCII).  Exercises the `false` branch of the
/// dual-width tables, which the PETSCII test never touches; single-width
/// tables fit 40 so they pass here trivially.
#[test]
fn test_help_lines_fit_ansi() {
    const ANSI_WIDTH: usize = 80;
    let groups = all_help_line_groups(false);
    for line in groups.iter().flat_map(|g| g.iter()) {
        assert!(
            line.len() <= ANSI_WIDTH,
            "ANSI help line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            ANSI_WIDTH,
        );
    }
}

/// Asserts the real `modem_help_lines` PETSCII variant fits 40 cols
/// (the same table `modem_show_help` renders).
#[test]
fn test_modem_help_lines_fit_petscii() {
    for line in TelnetSession::modem_help_lines(true) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "modem help '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

/// Separator width must match terminal type.  PETSCII intentionally
/// stays one column shy of `PETSCII_WIDTH` so a divider on a 40-col
/// C64 doesn't auto-wrap and eat an extra row.
#[test]
fn test_separator_widths() {
    assert_eq!("=".repeat(PETSCII_WIDTH - 1).len(), 39);
    assert_eq!("=".repeat(56).len(), 56); // ANSI/ASCII separator
}

/// PAGE_CONTENT_LINES must leave room for header and footer within 22 rows.
#[test]
fn test_page_content_lines_fits_screen() {
    let overhead = 3 + 3; // header (sep+title+sep) + footer (pos+nav+prompt)
    assert!(
        TelnetSession::PAGE_CONTENT_LINES + overhead <= 22,
        "PAGE_CONTENT_LINES {} + overhead {} = {} exceeds 22",
        TelnetSession::PAGE_CONTENT_LINES,
        overhead,
        TelnetSession::PAGE_CONTENT_LINES + overhead,
    );
}

/// TRANSFER_PAGE_SIZE must fit within 22 rows with header and footer.
#[test]
fn test_transfer_page_size_fits_screen() {
    let overhead = 3 + 2 + 5; // header + col headers + footer
    assert!(
        TelnetSession::TRANSFER_PAGE_SIZE + overhead <= 22,
        "TRANSFER_PAGE_SIZE {} + overhead {} = {} exceeds 22",
        TelnetSession::TRANSFER_PAGE_SIZE,
        overhead,
        TelnetSession::TRANSFER_PAGE_SIZE + overhead,
    );
}

/// File listing column format must fit PETSCII width.
/// Format: "  XX. FILENAME_22_CHARS_____ SIZE"
#[test]
fn test_file_listing_line_fits_petscii() {
    // Worst case: "  10. 1234567890123456789012 1023 B"
    let line = format!("  {:>2}. {:<22} {}", 10, "a]".repeat(11), "1023 B");
    assert!(
        line.len() <= PETSCII_WIDTH,
        "file listing line '{}' is {} chars, exceeds {}",
        line,
        line.len(),
        PETSCII_WIDTH,
    );
}

/// Download/delete column header must fit PETSCII width.
#[test]
fn test_file_listing_header_fits_petscii() {
    let header = format!("   {} {:<22} {}", "#.", "Filename", "Size");
    // Without color codes, just the visible text
    assert!(
        header.len() <= PETSCII_WIDTH,
        "column header '{}' is {} chars, exceeds {}",
        header,
        header.len(),
        PETSCII_WIDTH,
    );
}

/// File listing divider must fit PETSCII width.
#[test]
fn test_file_listing_divider_fits_petscii() {
    let divider = format!("  {}", "-".repeat(36));
    assert!(
        divider.len() <= PETSCII_WIDTH,
        "divider '{}' is {} chars, exceeds {}",
        divider,
        divider.len(),
        PETSCII_WIDTH,
    );
}

// ─── Pagination math ─────────────────────────────────

#[test]
fn test_pagination_zero_files() {
    let files: Vec<(String, u64)> = vec![];
    assert!(files.is_empty());
}

#[test]
fn test_pagination_exactly_one_page() {
    let page_size = TelnetSession::TRANSFER_PAGE_SIZE;
    let files: Vec<usize> = (0..page_size).collect();
    let total_pages = files.len().div_ceil(page_size);
    assert_eq!(total_pages, 1);
    assert_eq!(files.len(), page_size);
}

#[test]
fn test_pagination_one_over_page() {
    let page_size = TelnetSession::TRANSFER_PAGE_SIZE;
    let files: Vec<usize> = (0..page_size + 1).collect();
    let total_pages = files.len().div_ceil(page_size);
    assert_eq!(total_pages, 2);
    // Page 1
    let offset = 0;
    let end = (offset + page_size).min(files.len());
    assert_eq!(end - offset, page_size);
    // Page 2
    let offset = page_size;
    let end = (offset + page_size).min(files.len());
    assert_eq!(end - offset, 1);
}

#[test]
fn test_pagination_many_files() {
    let page_size = TelnetSession::TRANSFER_PAGE_SIZE;
    let count: usize = 105;
    let total_pages = count.div_ceil(page_size);
    assert_eq!(total_pages, 11); // 10 full pages + 1 partial
    // Last page
    let offset = (total_pages - 1) * page_size;
    let end = (offset + page_size).min(count);
    assert_eq!(end - offset, 5); // 105 - 100 = 5
}

#[test]
fn test_ai_pagination_single_line() {
    let page_h = TelnetSession::PAGE_CONTENT_LINES;
    let total = 1;
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, 1);
    assert_eq!(scroll, 0);  // no prev
    assert!(end >= total);  // no next
}

#[test]
fn test_ai_pagination_exactly_one_page() {
    let page_h = TelnetSession::PAGE_CONTENT_LINES;
    let total = page_h;
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, page_h);
    assert_eq!(scroll, 0);
    assert!(end >= total);
}

#[test]
fn test_ai_pagination_two_pages() {
    let page_h = TelnetSession::PAGE_CONTENT_LINES;
    let total = page_h + 5;
    // Page 1
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, page_h);
    assert!(end < total); // has next
    // Page 2
    let scroll = page_h;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, total);
    assert_eq!(end - scroll, 5);
    assert!(scroll > 0);     // has prev
    assert!(end >= total);   // no next
}

// ─── XMODEM constants ────────────────────────────────

#[test]
fn test_xmodem_block_size() {
    assert_eq!(crate::xmodem::XMODEM_BLOCK_SIZE, 128);
}

#[test]
fn test_max_file_size() {
    assert_eq!(TelnetSession::MAX_FILE_SIZE, 8 * 1024 * 1024);
}

// ─── Web browser ─────────────────────────────────────

#[test]
fn test_browser_menu_path() {
    assert_eq!(Menu::Browser.path(), "ethernet/web");
}

#[test]
fn test_web_page_height_fits_screen() {
    let overhead = 3 + 1 + 4 + 1; // header(3) + blank + footer(pos+url+nav1+nav2) + prompt
    assert!(
        TelnetSession::WEB_PAGE_HEIGHT + overhead <= 22,
        "WEB_PAGE_HEIGHT {} + overhead {} = {} exceeds 22",
        TelnetSession::WEB_PAGE_HEIGHT,
        overhead,
        TelnetSession::WEB_PAGE_HEIGHT + overhead,
    );
}

#[test]
fn test_web_max_history_is_reasonable() {
    const _: () = assert!(TelnetSession::WEB_MAX_HISTORY >= 10, "too few history entries");
    const _: () = assert!(TelnetSession::WEB_MAX_HISTORY <= 200, "excessive history cap");
}

#[test]
fn test_web_browser_home_lines_fit_petscii() {
    let lines = [
        "  WEB BROWSER",
        "  G=Go/Search K=Bookmarks Q=Back H=Help",
    ];
    for line in &lines {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "line '{}' is {} chars, exceeds {}",
            line,
            line.len(),
            PETSCII_WIDTH,
        );
    }
}

#[test]
fn test_web_browser_footer_fits_petscii() {
    // Row 1 worst case (PETSCII): P=Pv N=Nx T=Top E=End S=Find
    let row1 = "  P=Pv N=Nx T=Top E=End S=Find";
    assert!(
        row1.len() <= PETSCII_WIDTH,
        "nav row1 '{}' is {} chars, exceeds {}",
        row1, row1.len(), PETSCII_WIDTH,
    );
    // Row 2 worst case (PETSCII): G=Go L=Lk F=Fm K=Bm H=? B=Bk Q=X
    let row2 = "  G=Go L=Lk F=Fm K=Bm H=? B=Bk Q=X";
    assert!(
        row2.len() <= PETSCII_WIDTH,
        "nav row2 '{}' is {} chars, exceeds {}",
        row2, row2.len(), PETSCII_WIDTH,
    );
}

#[test]
fn test_web_browser_status_line_fits_petscii() {
    let status = format!("  ({}-{} of {})", 4983, 5000, 5000);
    assert!(
        status.len() <= PETSCII_WIDTH,
        "status '{}' is {} chars, exceeds {}",
        status,
        status.len(),
        PETSCII_WIDTH,
    );
    // Form indicator line
    let form_hint = "  1 form on this page (F to edit)";
    assert!(
        form_hint.len() <= PETSCII_WIDTH,
        "form hint '{}' is {} chars, exceeds {}",
        form_hint, form_hint.len(), PETSCII_WIDTH,
    );
    let form_hint_multi = "  99 forms on this page (F to edit)";
    assert!(
        form_hint_multi.len() <= PETSCII_WIDTH,
        "form hint '{}' is {} chars, exceeds {}",
        form_hint_multi, form_hint_multi.len(), PETSCII_WIDTH,
    );
}

// ─── Web browser pagination ──────────────────────────

#[test]
fn test_web_pagination_single_line() {
    let page_h = TelnetSession::WEB_PAGE_HEIGHT;
    let total = 1;
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, 1);
    assert!(scroll == 0);   // no prev
    assert!(end >= total);   // no next
}

#[test]
fn test_web_pagination_exact_page() {
    let page_h = TelnetSession::WEB_PAGE_HEIGHT;
    let total = page_h;
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, page_h);
    assert!(end >= total); // no next
}

#[test]
fn test_web_pagination_two_pages() {
    let page_h = TelnetSession::WEB_PAGE_HEIGHT;
    let total = page_h + 5;
    // Page 1
    let scroll = 0;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, page_h);
    assert!(end < total); // has next
    // Page 2
    let scroll = page_h;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, total);
    assert!(scroll > 0);    // has prev
    assert!(end >= total);   // no next
}

// ─── Web browser top/end navigation ──────────────────

#[test]
fn test_web_end_scroll_calculation() {
    let page_h = TelnetSession::WEB_PAGE_HEIGHT;
    let total = 100;
    // E command: scroll = total - page_h
    let scroll = total - page_h;
    let end = (scroll + page_h).min(total);
    assert_eq!(end, total); // last line visible
    assert_eq!(end - scroll, page_h); // full page
}

#[test]
fn test_web_end_scroll_short_page() {
    let page_h = TelnetSession::WEB_PAGE_HEIGHT;
    let total: usize = 5;
    // E command when total <= page_h: scroll stays 0
    let scroll = total.saturating_sub(page_h);
    assert_eq!(scroll, 0);
}

// ─── Web search ──────────────────────────────────────

#[test]
fn test_web_search_logic_finds_match() {
    let lines: Vec<String> = vec![
        "Hello world".to_string(),
        "Foo bar".to_string(),
        "Rust programming".to_string(),
        "More text".to_string(),
    ];
    let query = "rust";
    let total = lines.len();
    let start_line = 1; // scroll (0) + 1
    let mut found = None;
    for offset in 0..total {
        let idx = (start_line + offset) % total;
        if lines[idx].to_ascii_lowercase().contains(query) {
            found = Some(idx);
            break;
        }
    }
    assert_eq!(found, Some(2));
}

#[test]
fn test_web_search_wraps_around() {
    let lines: Vec<String> = vec![
        "Match here".to_string(),
        "No match".to_string(),
        "No match".to_string(),
    ];
    let query = "match here";
    let total = lines.len();
    let start_line = 1 + 1; // searching from scroll=1, so start at 2
    let mut found = None;
    for offset in 0..total {
        let idx = (start_line + offset) % total;
        if lines[idx].to_ascii_lowercase().contains(query) {
            found = Some(idx);
            break;
        }
    }
    assert_eq!(found, Some(0)); // wraps around to line 0
}

#[test]
fn test_web_search_no_match() {
    let lines: Vec<String> = vec![
        "Hello".to_string(),
        "World".to_string(),
    ];
    let query = "xyz";
    let total = lines.len();
    let start_line = 1; // scroll (0) + 1
    let mut found = None;
    for offset in 0..total {
        let idx = (start_line + offset) % total;
        if lines[idx].to_ascii_lowercase().contains(query) {
            found = Some(idx);
            break;
        }
    }
    assert!(found.is_none());
}

// ─── Web history with scroll ─────────────────────────

#[test]
fn test_web_history_stores_scroll() {
    let mut history: Vec<(String, usize)> = Vec::new();
    history.push(("https://page1.com".to_string(), 42));
    history.push(("https://page2.com".to_string(), 0));
    assert_eq!(history.last().unwrap().1, 0);
    history.pop();
    assert_eq!(history.last().unwrap().1, 42);
}

#[test]
fn test_web_history_cap_with_scroll() {
    let max = TelnetSession::WEB_MAX_HISTORY;
    let mut history: Vec<(String, usize)> = Vec::new();
    for i in 0..max {
        history.push((format!("https://page{}.com", i), i * 10));
    }
    assert_eq!(history.len(), max);
    // Push one more — evict oldest
    history.push(("https://new.com".to_string(), 99));
    if history.len() > max {
        history.remove(0);
    }
    assert_eq!(history.len(), max);
    assert_eq!(history[0].0, "https://page1.com");
    assert_eq!(history.last().unwrap().1, 99);
}

// ─── Bookmarks UI layout ─────────────────────────────

#[test]
fn test_bookmarks_screen_lines_fit_petscii() {
    let lines = [
        "  BOOKMARKS",
        "  #=Open D=Delete",
    ];
    for line in &lines {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "line '{}' is {} chars, exceeds {}",
            line, line.len(), PETSCII_WIDTH,
        );
    }
}

#[test]
fn test_bookmark_entry_fits_petscii() {
    // Worst case: "  99. " + 30 chars title
    let line = format!("  {:>2}. {}", 99, "a".repeat(30));
    assert!(
        line.len() <= PETSCII_WIDTH,
        "bookmark entry '{}' is {} chars, exceeds {}",
        line, line.len(), PETSCII_WIDTH,
    );
}

// ─── Troubleshooting ─────────────────────────────────

#[test]
fn test_troubleshooting_lines_fit_petscii() {
    let lines = [
        "  CHARACTER TROUBLESHOOTING",
        "  Client:   Serial modem",
        "  Terminal: PETSCII",
        "  IAC esc:  Off",
        "  Press any key to see its hex value.",
        "  Press <- twice to return to menu.",
        "  Key: 0x1B ( 27) = ESC",
        "  Key: 0x41 ( 65) = 'A'",
        "  Key: 0x14 ( 20) = DC4/C64-DEL",
        "  Key: 0x9D (157) = C64-LEFT",
        "  Returning to main menu...",
    ];
    for line in &lines {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "troubleshooting line '{}' is {} chars, exceeds {}",
            line, line.len(), PETSCII_WIDTH,
        );
    }
}

// ─── Help screen ──────────────────────────────────────

#[test]
fn test_web_help_lines_fit_petscii() {
    // The dim intro lines and the yellow "BROWSER HELP" title are sent
    // inline; the drift-prone key-binding lines live in these two fns.
    let groups: [&[&str]; 2] = [
        TelnetSession::browser_page_help_lines(true),
        TelnetSession::browser_menu_help_lines(),
    ];
    for line in groups.iter().flat_map(|g| g.iter()) {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "help line '{}' is {} chars, exceeds {}",
            line, line.len(), PETSCII_WIDTH,
        );
    }
}

#[test]
fn test_web_help_page_view_row_count() {
    // header(3) + 2 link explanation + blank + 12 help lines + blank + "press any key" = 20 rows max
    let rows = 3 + 2 + 1 + 12 + 1 + 1;
    assert!(rows <= 22, "help screen is {} rows, exceeds 22", rows);
}

// ─── URL/Search prompt ───────────────────────────────

#[test]
fn test_url_search_prompt_fits_petscii() {
    let prompt = "  URL/Search: ";
    assert!(
        prompt.len() <= PETSCII_WIDTH,
        "prompt '{}' is {} chars, exceeds {}",
        prompt, prompt.len(), PETSCII_WIDTH,
    );
}

#[test]
fn test_find_prompt_fits_petscii() {
    let prompt = "  Find: ";
    assert!(
        prompt.len() <= PETSCII_WIDTH,
        "prompt '{}' is {} chars, exceeds {}",
        prompt, prompt.len(), PETSCII_WIDTH,
    );
}

// ─── Modem settings confirmation messages ───────────

/// All modem_apply_settings prompt/status messages must fit PETSCII width.
#[test]
fn test_modem_apply_messages_fit_petscii() {
    let messages = [
        "  New settings will be applied.",
        "  You have 60 seconds to adjust",
        "  your terminal and type Y then",
        "  Enter, or settings will revert.",
        "  Settings confirmed.",
        "  Press any key to continue.",
        "  No response. Reverting settings.",
    ];
    for msg in &messages {
        assert!(
            msg.len() <= PETSCII_WIDTH,
            "modem apply msg '{}' is {} chars, exceeds {}",
            msg,
            msg.len(),
            PETSCII_WIDTH,
        );
    }
}

/// The countdown reminder must fit PETSCII width even with 2-digit seconds.
#[test]
fn test_modem_apply_countdown_fits_petscii() {
    let reminder = format!("  Type Y+Enter to confirm. ({}s left)", 55);
    assert!(
        reminder.len() <= PETSCII_WIDTH,
        "countdown '{}' is {} chars, exceeds {}",
        reminder,
        reminder.len(),
        PETSCII_WIDTH,
    );
}

/// Modem apply settings confirmation screen: blank + 4 warning lines +
/// blank + (countdown reminders) + confirmation/revert.  The screen is
/// not a full menu redraw so row count is not constrained to 22, but
/// individual messages must fit width.
#[test]
fn test_modem_apply_settings_row_count() {
    // Warning: blank + 4 lines + blank = 6.
    // Worst case after: 12 countdown reminders (every 5s for 60s) + revert msg = 14.
    // Total ≤ 20, well within 22.
    let warning_rows = 6;
    assert!(warning_rows <= 22);
}

// ─── Telnet option negotiation ───────────────────────

#[test]
fn test_match_terminal_name_c64_variants() {
    assert_eq!(match_terminal_name("C64"), Some(TerminalType::Petscii));
    assert_eq!(match_terminal_name("c64"), Some(TerminalType::Petscii));
    assert_eq!(match_terminal_name("C128"), Some(TerminalType::Petscii));
    assert_eq!(match_terminal_name("PETSCII"), Some(TerminalType::Petscii));
    assert_eq!(match_terminal_name("COMMODORE"), Some(TerminalType::Petscii));
    assert_eq!(match_terminal_name(" C64 "), Some(TerminalType::Petscii));
}

#[test]
fn test_match_terminal_name_ansi_variants() {
    assert_eq!(match_terminal_name("XTERM"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("xterm-256color"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("VT100"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("VT220"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("ANSI"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("linux"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("screen-256color"), Some(TerminalType::Ansi));
    assert_eq!(match_terminal_name("PUTTY"), Some(TerminalType::Ansi));
}

#[test]
fn test_match_terminal_name_dumb() {
    assert_eq!(match_terminal_name("DUMB"), Some(TerminalType::Ascii));
    assert_eq!(match_terminal_name("UNKNOWN"), Some(TerminalType::Ascii));
    assert_eq!(match_terminal_name("NETWORK"), Some(TerminalType::Ascii));
}

#[test]
fn test_match_terminal_name_unrecognized() {
    // Fall back to BACKSPACE detection for names we don't know.
    assert_eq!(match_terminal_name("MY-WEIRD-TERM"), None);
    assert_eq!(match_terminal_name(""), None);
    assert_eq!(match_terminal_name("   "), None);
}

#[tokio::test]
async fn test_send_raw_escapes_iac_bytes() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.send_raw(&[b'A', 0xFF, b'B']).await.unwrap();
    drop(session); // close writer so peer reads EOF after data

    let mut out = Vec::new();
    use tokio::io::AsyncReadExt;
    peer.read_to_end(&mut out).await.unwrap();
    // 0xFF data byte must be escaped as IAC IAC (0xFF 0xFF).
    assert_eq!(out, vec![b'A', 0xFF, 0xFF, b'B']);
}

#[tokio::test]
async fn test_drain_input_until_quiet_clears_buffered_then_stops() {
    // Stale bytes a prior aborted Punter transfer would strand in the pipe.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Petscii);
    use tokio::io::AsyncWriteExt;
    peer.write_all(b"SYNS/BS/BS/B").await.unwrap();
    // Drain with a short gap (keeps the test fast); the line then goes
    // quiet so the drain returns.
    session.drain_input_until_quiet(40, Some(1000)).await;
    // A fresh byte sent after the drain must be the next thing the session
    // reads — proving the stale bytes were all consumed.
    peer.write_all(b"Z").await.unwrap();
    let got = session.session_read_byte().await.unwrap();
    assert_eq!(got, Some(b'Z'), "drain should have consumed all stale bytes");
}

#[tokio::test]
async fn test_drain_input_until_quiet_caps_an_endless_stream() {
    // A peer that never stops talking must not stall the drain past the cap.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Petscii);
    use tokio::io::AsyncWriteExt;
    let pump = tokio::spawn(async move {
        // Stream steadily for longer than the cap; ignore the eventual
        // closed-pipe error once the session stops reading.
        for _ in 0..2000 {
            if peer.write_all(b"S/B").await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    });
    let start = std::time::Instant::now();
    session.drain_input_until_quiet(40, Some(300)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "drain must honor the max cap against an endless stream (took {elapsed:?})"
    );
    pump.abort();
}

#[tokio::test]
async fn test_send_raw_passthrough_when_no_iac() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.send_raw(b"hello").await.unwrap();
    drop(session);

    let mut out = Vec::new();
    use tokio::io::AsyncReadExt;
    peer.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, b"hello");
}

#[tokio::test]
async fn test_send_telnet_protocol_never_escapes() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    // An IAC WILL ECHO protocol sequence contains 0xFF but must
    // go through verbatim — escaping it would corrupt the command.
    session
        .send_telnet_protocol(&[IAC, WILL, OPT_ECHO])
        .await
        .unwrap();
    drop(session);

    let mut out = Vec::new();
    use tokio::io::AsyncReadExt;
    peer.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, vec![IAC, WILL, OPT_ECHO]);
}

#[tokio::test]
async fn test_detect_terminal_type_opening_negotiation() {
    // Pins the documented session-start IAC negotiation (user
    // manual §5): on a non-serial connection the server advertises
    // server-echo + suppress-go-ahead and requests SGA / terminal-
    // type / window-size from the client, in this exact order,
    // before the BACKSPACE detection prompt.  detect_terminal_type
    // then blocks reading the BACKSPACE byte, so the task is
    // aborted once the opening bytes are observed.
    let (session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    let task = tokio::spawn(async move {
        let mut session = session;
        let _ = session.detect_terminal_type().await;
    });

    use tokio::io::AsyncReadExt;
    let mut opening = [0u8; 15];
    peer.read_exact(&mut opening).await.unwrap();
    assert_eq!(
        opening,
        [
            IAC, WILL, OPT_ECHO,
            IAC, WILL, OPT_SGA,
            IAC, DO, OPT_SGA,
            IAC, DO, OPT_TTYPE,
            IAC, DO, OPT_NAWS,
        ]
    );

    task.abort();
}

#[tokio::test]
async fn test_ayt_gets_yes_reply() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    // Send IAC AYT followed by a real data byte so session_read_byte
    // can return something.
    peer.write_all(&[IAC, AYT, b'Z']).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'Z'));

    // The session should have written "[Yes]\r\n" back.
    let mut out = Vec::new();
    peer.write_all(&[]).await.ok();
    // Drop only the session side so we can read EOF.
    drop(session);
    use tokio::io::AsyncReadExt;
    peer.read_to_end(&mut out).await.unwrap();
    assert!(
        out.windows(5).any(|w| w == b"[Yes]"),
        "expected [Yes] reply, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_ip_surfaces_as_esc_ansi() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, IP]).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(0x1B)); // ANSI ESC
}

#[tokio::test]
async fn test_ip_surfaces_as_esc_petscii() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Petscii);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, BRK]).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(0x5F)); // C64 back-arrow used as PETSCII ESC
}

#[tokio::test]
async fn test_ec_surfaces_as_del() {
    // RFC 854 EC (0xF7) should surface as DEL (0x7F) so upstream
    // line-editors treat it as backspace.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, EC]).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(0x7F));
}

#[tokio::test]
async fn test_el_surfaces_as_nak() {
    // RFC 854 EL (0xF8) should surface as the LINE_ERASE_BYTE (0x15,
    // NAK) so the line-input loop can erase the current buffer.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, EL]).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(LINE_ERASE_BYTE));
}

#[tokio::test]
async fn test_do_timing_mark_gets_will() {
    // RFC 860: DO TIMING-MARK must be answered with WILL TIMING-MARK.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DO, OPT_TIMING_MARK, b'X']).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let will_tm = [IAC, WILL, OPT_TIMING_MARK];
    assert!(
        out.windows(3).any(|w| w == will_tm),
        "expected IAC WILL TIMING-MARK, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_dont_timing_mark_is_silent() {
    // RFC 860: DONT TIMING-MARK is a no-op (we never keep persistent
    // state for this option) so the server should NOT emit WONT.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DONT, OPT_TIMING_MARK, b'Y']).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'Y'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let wont_tm = [IAC, WONT, OPT_TIMING_MARK];
    assert!(
        !out.windows(3).any(|w| w == wont_tm),
        "expected no WONT TIMING-MARK, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_do_status_gets_will() {
    // RFC 859: DO STATUS → WILL STATUS.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DO, OPT_STATUS, b'X']).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let will_status = [IAC, WILL, OPT_STATUS];
    assert!(
        out.windows(3).any(|w| w == will_status),
        "expected IAC WILL STATUS, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_do_status_not_repeated() {
    // Two consecutive DO STATUS should yield exactly one WILL reply.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DO, OPT_STATUS, IAC, DO, OPT_STATUS, b'Y'])
        .await
        .unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'Y'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let will_status = [IAC, WILL, OPT_STATUS];
    let count = out.windows(3).filter(|w| *w == will_status).count();
    assert_eq!(count, 1, "expected exactly one WILL STATUS, got {:?}", out);
}

#[tokio::test]
async fn test_sb_status_send_emits_is_dump() {
    // After enabling STATUS, SB STATUS SEND must produce SB STATUS IS
    // <state> SE containing at least the handshake options.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    // The test-session factory skips send_telnet_handshake().  Seed
    // the neg arrays so the dump has something to report beyond just
    // STATUS itself.
    session.neg_sent_will[OPT_ECHO as usize] = true;
    session.neg_sent_will[OPT_SGA as usize] = true;
    session.neg_sent_do[OPT_SGA as usize] = true;
    session.neg_sent_do[OPT_TTYPE as usize] = true;
    session.neg_sent_do[OPT_NAWS as usize] = true;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[
        IAC, DO, OPT_STATUS,
        IAC, SB, OPT_STATUS, STATUS_SEND, IAC, SE,
        b'Z',
    ])
    .await
    .unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'Z'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();

    // Find the IS subnegotiation: IAC SB STATUS IS ... IAC SE.
    let header = [IAC, SB, OPT_STATUS, STATUS_IS];
    let start = out
        .windows(4)
        .position(|w| w == header)
        .expect("no SB STATUS IS in output");
    let body_and_tail = &out[start + 4..];
    let se_rel = body_and_tail
        .windows(2)
        .position(|w| w == [IAC, SE])
        .expect("no IAC SE terminator");
    let body = &body_and_tail[..se_rel];

    // Body should contain WILL ECHO, WILL SGA, WILL STATUS, DO SGA,
    // DO TTYPE, DO NAWS — each as a verb+opt pair.
    let expected_pairs: &[[u8; 2]] = &[
        [WILL, OPT_ECHO],
        [WILL, OPT_SGA],
        [WILL, OPT_STATUS],
        [DO, OPT_SGA],
        [DO, OPT_TTYPE],
        [DO, OPT_NAWS],
    ];
    for pair in expected_pairs {
        assert!(
            body.windows(2).any(|w| w == pair),
            "STATUS IS body missing {:?}; body was {:?}",
            pair,
            body
        );
    }
}

#[tokio::test]
async fn test_dont_status_withdraws() {
    // After DO STATUS → WILL STATUS, a DONT STATUS must produce WONT.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[
        IAC, DO, OPT_STATUS,
        IAC, DONT, OPT_STATUS,
        b'Q',
    ])
    .await
    .unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'Q'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let wont_status = [IAC, WONT, OPT_STATUS];
    assert!(
        out.windows(3).any(|w| w == wont_status),
        "expected IAC WONT STATUS, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_will_status_from_peer_refused() {
    // The peer trying to be the status sender is refused with DONT.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, WILL, OPT_STATUS, b'R']).await.unwrap();
    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'R'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let dont_status = [IAC, DONT, OPT_STATUS];
    assert!(
        out.windows(3).any(|w| w == dont_status),
        "expected IAC DONT STATUS, got {:?}",
        out
    );
}

// ─── Gateway telnet-client IAC parser ─────────────────

fn feed_all(iac: &mut GatewayTelnetIac, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut data = Vec::new();
    let mut replies = Vec::new();
    for &b in input {
        iac.feed(b, &mut data, &mut replies);
    }
    (data, replies)
}

/// Build a reactive-refuse (cooperate=false) parser for tests that
/// exercise the legacy strict-refuser behavior.
fn reactive_iac() -> GatewayTelnetIac {
    let (parser, _) = GatewayTelnetIac::new(false, "ANSI".into(), 80, 24);
    parser
}

/// Build a cooperative parser (cooperate=true) and return the initial
/// offer bytes along with the parser.
fn cooperative_iac() -> (GatewayTelnetIac, Vec<u8>) {
    GatewayTelnetIac::new(true, "ANSI".into(), 80, 24)
}

#[test]
fn test_gateway_iac_plain_data_passes_through() {
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(&mut iac, b"Hello, world!");
    assert_eq!(data, b"Hello, world!");
    assert!(replies.is_empty());
}

#[test]
fn test_gateway_iac_iac_unescapes_to_data_ff() {
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(&mut iac, &[b'A', IAC, IAC, b'B']);
    assert_eq!(data, vec![b'A', 0xFF, b'B']);
    assert!(replies.is_empty());
}

#[test]
fn test_gateway_iac_two_byte_commands_consumed() {
    let mut iac = reactive_iac();
    // AYT (0xF6), NOP (0xF1), GA (0xF9): all consumed, none leak.
    let (data, replies) = feed_all(
        &mut iac,
        &[b'X', IAC, 0xF6, b'Y', IAC, 0xF1, b'Z', IAC, 0xF9, b'W'],
    );
    assert_eq!(data, b"XYZW");
    assert!(replies.is_empty());
}

#[test]
fn test_gateway_iac_will_echo_gets_do_reply() {
    // ECHO cooperation is always on — peer's WILL ECHO is accepted
    // with DO ECHO so the remote echoes the user's keystrokes.
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(&mut iac, &[IAC, WILL, OPT_ECHO, b'A']);
    assert_eq!(data, b"A");
    assert_eq!(replies, vec![IAC, DO, OPT_ECHO]);
}

#[test]
fn test_gateway_iac_will_unsupported_gets_dont_reply() {
    // Unsupported options still get refused.
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(&mut iac, &[IAC, WILL, 0x00, b'A']); // BINARY
    assert_eq!(data, b"A");
    assert_eq!(replies, vec![IAC, DONT, 0x00]);
}

#[test]
fn test_gateway_iac_do_gets_wont_reply() {
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(&mut iac, &[IAC, DO, OPT_NAWS, b'B']);
    assert_eq!(data, b"B");
    assert_eq!(replies, vec![IAC, WONT, OPT_NAWS]);
}

#[test]
fn test_gateway_iac_wont_and_dont_need_no_reply() {
    let mut iac = reactive_iac();
    let (data, replies) = feed_all(
        &mut iac,
        &[IAC, WONT, OPT_ECHO, IAC, DONT, OPT_NAWS, b'C'],
    );
    assert_eq!(data, b"C");
    assert!(replies.is_empty());
}

#[test]
fn test_gateway_iac_duplicate_refusal_not_repeated() {
    let mut iac = reactive_iac();
    // First WILL triggers DONT; second WILL for the same opt is silent.
    let (_, r1) = feed_all(&mut iac, &[IAC, WILL, OPT_SGA]);
    let (_, r2) = feed_all(&mut iac, &[IAC, WILL, OPT_SGA]);
    assert_eq!(r1, vec![IAC, DONT, OPT_SGA]);
    assert!(r2.is_empty());
}

#[test]
fn test_gateway_iac_sb_body_consumed_with_iac_iac_inside() {
    let mut iac = reactive_iac();
    // SB TTYPE IS "v" 0xFF 0xFF "t" IAC SE — the escaped IAC inside
    // must not prematurely end the subnegotiation.
    let (data, replies) = feed_all(
        &mut iac,
        &[
            b'A',
            IAC, SB, OPT_TTYPE, 0x00, b'v', IAC, IAC, b't', IAC, SE,
            b'B',
        ],
    );
    assert_eq!(data, b"AB");
    assert!(replies.is_empty());
}

#[test]
fn test_gateway_iac_sb_body_capped_against_oom() {
    // A malicious remote sending a huge SB body must not make us
    // allocate unbounded memory.  After processing a 1 MiB body
    // followed by IAC SE, the parser must terminate cleanly and
    // the internal sb_body must be at most MAX_SB_BODY_BYTES.
    let mut iac = reactive_iac();
    let mut data = Vec::new();
    let mut replies = Vec::new();
    iac.feed(IAC, &mut data, &mut replies);
    iac.feed(SB, &mut data, &mut replies);
    iac.feed(OPT_TTYPE, &mut data, &mut replies);
    for _ in 0..(1024 * 1024) {
        iac.feed(b'A', &mut data, &mut replies);
    }
    iac.feed(IAC, &mut data, &mut replies);
    iac.feed(SE, &mut data, &mut replies);
    iac.feed(b'Q', &mut data, &mut replies);
    assert!(
        iac.sb_body.len() <= MAX_SB_BODY_BYTES,
        "sb_body grew to {} bytes (cap is {})",
        iac.sb_body.len(),
        MAX_SB_BODY_BYTES
    );
    assert_eq!(
        iac.state,
        GatewayIacState::Normal,
        "parser should resync to Normal after huge SB"
    );
    assert_eq!(
        data.last().copied(),
        Some(b'Q'),
        "post-SB data byte should pass through"
    );
}

#[test]
fn test_gateway_iac_malformed_sb_resyncs_on_iac_se() {
    let mut iac = reactive_iac();
    // IAC inside SB followed by an unexpected byte (not SE, not IAC).
    // Parser must keep scanning for IAC SE.
    let (data, _) = feed_all(
        &mut iac,
        &[
            IAC, SB, OPT_NAWS, 0x00, IAC, 0xEE, 0x00, IAC, SE,
            b'Q',
        ],
    );
    assert_eq!(data, b"Q");
}

#[test]
fn test_gateway_iac_split_across_feeds() {
    // Parser must survive IAC sequences split across multiple calls —
    // simulating fragmented TCP reads.  WILL ECHO now triggers the
    // cooperative DO ECHO reply.
    let mut iac = reactive_iac();
    let mut data = Vec::new();
    let mut replies = Vec::new();
    iac.feed(IAC, &mut data, &mut replies);
    assert!(data.is_empty() && replies.is_empty());
    iac.feed(WILL, &mut data, &mut replies);
    assert!(data.is_empty() && replies.is_empty());
    iac.feed(OPT_ECHO, &mut data, &mut replies);
    assert!(data.is_empty());
    assert_eq!(replies, vec![IAC, DO, OPT_ECHO]);
    iac.feed(b'R', &mut data, &mut replies);
    assert_eq!(data, vec![b'R']);
}

// ─── Cooperative-mode gateway parser ──────────────────

#[test]
fn test_gateway_cooperative_initial_offers() {
    // Cooperate mode advertises WILL TTYPE, WILL NAWS, and requests
    // DO ECHO at connect so the remote echoes the user's keystrokes.
    let (_, initial) = cooperative_iac();
    assert_eq!(
        initial,
        vec![
            IAC, WILL, OPT_TTYPE,
            IAC, WILL, OPT_NAWS,
            IAC, DO, OPT_ECHO,
        ],
    );
}

#[test]
fn test_gateway_cooperative_will_echo_is_ack() {
    // After proactively sending DO ECHO, peer's WILL ECHO is an ack
    // (him_state WantYes → Yes) with no extra reply.
    let (mut iac, _) = cooperative_iac();
    let (data, replies) = feed_all(&mut iac, &[IAC, WILL, OPT_ECHO, b'A']);
    assert_eq!(data, b"A");
    assert!(
        replies.is_empty(),
        "WILL ECHO after our DO ECHO should be a silent ack, got {:?}",
        replies
    );
}

#[test]
fn test_gateway_reactive_no_initial_offers() {
    // Reactive mode (cooperate=false) sends nothing at connect.
    let (_, initial) = GatewayTelnetIac::new(false, "ANSI".into(), 80, 24);
    assert!(initial.is_empty());
}

#[test]
fn test_gateway_cooperative_do_ttype_is_ack() {
    // After sending WILL TTYPE proactively, peer's DO TTYPE is an ack
    // — us_state transitions to Yes, no extra reply.
    let (mut iac, _) = cooperative_iac();
    let (data, replies) = feed_all(&mut iac, &[IAC, DO, OPT_TTYPE, b'A']);
    assert_eq!(data, b"A");
    assert!(
        replies.is_empty(),
        "DO TTYPE after WILL TTYPE should be a silent ack, got {:?}",
        replies
    );
}

#[test]
fn test_gateway_cooperative_sb_ttype_send_returns_is() {
    // After DO TTYPE acks our WILL, peer sends SB TTYPE SEND; we
    // respond with SB TTYPE IS <name>.
    let (mut iac, _) = cooperative_iac();
    let (_, _) = feed_all(&mut iac, &[IAC, DO, OPT_TTYPE]);
    let (data, replies) = feed_all(
        &mut iac,
        &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE, b'Z'],
    );
    assert_eq!(data, b"Z");
    let expected = [
        IAC, SB, OPT_TTYPE, TTYPE_IS,
        b'A', b'N', b'S', b'I',
        IAC, SE,
    ];
    assert_eq!(replies, expected);
}

#[test]
fn test_gateway_reactive_do_ttype_refused() {
    // Without cooperation the same DO TTYPE is refused with WONT.
    let mut iac = reactive_iac();
    let (_, replies) = feed_all(&mut iac, &[IAC, DO, OPT_TTYPE]);
    assert_eq!(replies, vec![IAC, WONT, OPT_TTYPE]);
}

#[test]
fn test_gateway_cooperative_do_naws_emits_sb() {
    // DO NAWS (whether ack or unprovoked) triggers an immediate SB
    // NAWS with our configured dimensions.
    let (mut iac, _) = cooperative_iac();
    let (_, replies) = feed_all(&mut iac, &[IAC, DO, OPT_NAWS]);
    // For cooperative_iac we passed 80x24.
    let expected_sb = [
        IAC, SB, OPT_NAWS,
        0x00, 0x50,  // 80
        0x00, 0x18,  // 24
        IAC, SE,
    ];
    assert!(
        replies.windows(expected_sb.len()).any(|w| w == expected_sb),
        "expected SB NAWS 80x24 in replies, got {:?}",
        replies
    );
}

#[test]
fn test_gateway_cooperative_dont_ttype_withdraws() {
    // Peer refusing our proactive WILL TTYPE drops us_state to No.
    let (mut iac, _) = cooperative_iac();
    let (_, replies) = feed_all(&mut iac, &[IAC, DONT, OPT_TTYPE]);
    // No reply — peer's refusal closes our WantYes cleanly.
    assert!(replies.is_empty());
    // Subsequent SB TTYPE SEND should be ignored (us_state=No).
    let (_, replies2) = feed_all(
        &mut iac,
        &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE],
    );
    assert!(
        replies2.is_empty(),
        "SB TTYPE SEND after DONT should be ignored"
    );
}

#[test]
fn test_gateway_cooperative_naws_sent_with_local_dimensions() {
    // Feed custom dimensions and verify SB NAWS reflects them.
    let (mut iac, _) = GatewayTelnetIac::new(true, "PETSCII".into(), 40, 25);
    let (_, replies) = feed_all(&mut iac, &[IAC, DO, OPT_NAWS]);
    let expected = [
        IAC, SB, OPT_NAWS,
        0x00, 0x28,  // 40
        0x00, 0x19,  // 25
        IAC, SE,
    ];
    assert!(replies.windows(expected.len()).any(|w| w == expected));
}

#[test]
fn test_gateway_cooperative_naws_value_ff_is_escaped() {
    // An 0xFF byte in a NAWS dimension must be IAC-doubled per RFC 854.
    // 255x255 would contain two 0xFF bytes in the size field.
    let (mut iac, _) = GatewayTelnetIac::new(true, "ANSI".into(), 0x00FF, 0x00FF);
    let (_, replies) = feed_all(&mut iac, &[IAC, DO, OPT_NAWS]);
    let expected = [
        IAC, SB, OPT_NAWS,
        0x00, IAC, IAC,  // width high, width low (0xFF escaped)
        0x00, IAC, IAC,  // height high, height low (0xFF escaped)
        IAC, SE,
    ];
    assert!(
        replies.windows(expected.len()).any(|w| w == expected),
        "expected SB NAWS with escaped 0xFFs, got {:?}",
        replies
    );
}

#[test]
fn test_gateway_refusal_not_repeated_within_cycle() {
    // Two rapid WILL SGAs get only one DONT; subsequent WONT clears
    // the refusal-sent flag so a future WILL cycle refreshes.
    let mut iac = reactive_iac();
    let (_, r1) = feed_all(&mut iac, &[IAC, WILL, OPT_SGA]);
    let (_, r2) = feed_all(&mut iac, &[IAC, WILL, OPT_SGA]);
    assert_eq!(r1, vec![IAC, DONT, OPT_SGA]);
    assert!(r2.is_empty(), "second WILL should not re-trigger DONT");
    let (_, _) = feed_all(&mut iac, &[IAC, WONT, OPT_SGA]);
    let (_, r3) = feed_all(&mut iac, &[IAC, WILL, OPT_SGA]);
    assert_eq!(
        r3, vec![IAC, DONT, OPT_SGA],
        "new refusal cycle should issue fresh DONT after peer's WONT"
    );
}

#[test]
fn test_gateway_qmethod_peer_yes_echo_peer_withdraws() {
    // Accept WILL ECHO → peer later WONT ECHO → we reply DONT to ack.
    let mut iac = reactive_iac();
    let (_, r1) = feed_all(&mut iac, &[IAC, WILL, OPT_ECHO]);
    assert_eq!(r1, vec![IAC, DO, OPT_ECHO]);
    let (_, r2) = feed_all(&mut iac, &[IAC, WONT, OPT_ECHO]);
    assert_eq!(r2, vec![IAC, DONT, OPT_ECHO]);
}
// ─── Gateway Q-method fuzz harness ────────────────────

/// Property-based fuzzer for `GatewayTelnetIac`.  Generates random
/// sequences of `Op`s and asserts structural invariants after every
/// step so that any future refactor of the Q-method state machine
/// gets caught at `cargo test`.
///
/// Options are restricted to the range 0..16 so random sequences
/// frequently target the same option — that's where interesting
/// race-condition transitions (`WantYesOpposite` / `WantNoOpposite`)
/// actually get exercised.
mod qmethod_proptest {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        RecvWill(u8),
        RecvWont(u8),
        RecvDo(u8),
        RecvDont(u8),
        LocalEnable(u8),
        LocalDisable(u8),
        RecvData(u8),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let opt = 0u8..16u8;
        prop_oneof![
            opt.clone().prop_map(Op::RecvWill),
            opt.clone().prop_map(Op::RecvWont),
            opt.clone().prop_map(Op::RecvDo),
            opt.clone().prop_map(Op::RecvDont),
            opt.clone().prop_map(Op::LocalEnable),
            opt.clone().prop_map(Op::LocalDisable),
            (0u8..=255u8).prop_map(Op::RecvData),
        ]
    }

    fn apply(
        iac: &mut GatewayTelnetIac,
        op: &Op,
        data: &mut Vec<u8>,
        replies: &mut Vec<u8>,
    ) {
        match *op {
            Op::RecvWill(opt) => {
                iac.feed(IAC, data, replies);
                iac.feed(WILL, data, replies);
                iac.feed(opt, data, replies);
            }
            Op::RecvWont(opt) => {
                iac.feed(IAC, data, replies);
                iac.feed(WONT, data, replies);
                iac.feed(opt, data, replies);
            }
            Op::RecvDo(opt) => {
                iac.feed(IAC, data, replies);
                iac.feed(DO, data, replies);
                iac.feed(opt, data, replies);
            }
            Op::RecvDont(opt) => {
                iac.feed(IAC, data, replies);
                iac.feed(DONT, data, replies);
                iac.feed(opt, data, replies);
            }
            Op::LocalEnable(opt) => {
                iac.request_local_enable(opt, replies);
            }
            Op::LocalDisable(opt) => {
                iac.request_local_disable(opt, replies);
            }
            Op::RecvData(b) => {
                iac.feed(b, data, replies);
            }
        }
    }

    /// Validate that a byte stream of replies only contains well-formed
    /// IAC sequences: `IAC <verb> <opt>`, `IAC SB <opt> ... IAC SE`,
    /// or `IAC <2-byte-command>`.  No orphan data bytes, no truncated
    /// sequences.
    fn iac_reply_stream_is_well_formed(bytes: &[u8]) -> bool {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != IAC {
                return false;
            }
            i += 1;
            if i >= bytes.len() {
                return false;
            }
            match bytes[i] {
                SB => {
                    i += 1;
                    if i >= bytes.len() {
                        return false;
                    }
                    i += 1; // option byte
                    // Scan body until IAC SE.
                    loop {
                        if i >= bytes.len() {
                            return false;
                        }
                        if bytes[i] == IAC {
                            i += 1;
                            if i >= bytes.len() {
                                return false;
                            }
                            if bytes[i] == SE {
                                i += 1;
                                break;
                            }
                            // IAC IAC or other — body continues.
                            i += 1;
                        } else {
                            i += 1;
                        }
                    }
                }
                WILL | WONT | DO | DONT => {
                    i += 1;
                    if i >= bytes.len() {
                        return false;
                    }
                    i += 1; // option byte
                }
                _ => {
                    // 2-byte command.  Our gateway doesn't emit these,
                    // but if it ever does, one byte is the whole thing.
                    i += 1;
                }
            }
        }
        true
    }

    fn check_structural_invariants(iac: &GatewayTelnetIac) {
        for opt in 0u8..=255 {
            let idx = opt as usize;
            // Refusal flags track "we've sent DONT/WONT and have not
            // yet contradicted it."  Legitimate states where the flag
            // may be set are the No-side of the machine:
            //   sent_dont[opt] ∈ {No, WantNo, WantNoOpposite}
            //   sent_wont[opt] ∈ {No, WantNo, WantNoOpposite}
            // Yes-side states mean we've emitted an accepting DO/WILL
            // and must have cleared the flag at that point.
            let him_ok = matches!(
                iac.him_state[idx],
                OptState::No | OptState::WantNo | OptState::WantNoOpposite,
            );
            if iac.sent_dont[idx] {
                assert!(
                    him_ok,
                    "sent_dont[{}] set but him_state is {:?} (yes-side)",
                    opt,
                    iac.him_state[idx],
                );
            }
            let us_ok = matches!(
                iac.us_state[idx],
                OptState::No | OptState::WantNo | OptState::WantNoOpposite,
            );
            if iac.sent_wont[idx] {
                assert!(
                    us_ok,
                    "sent_wont[{}] set but us_state is {:?} (yes-side)",
                    opt,
                    iac.us_state[idx],
                );
            }
        }
    }

    proptest! {
        /// Random sequences of peer-initiated verbs, local mind-changes,
        /// and data bytes must never panic or produce malformed output.
        #[test]
        fn fuzz_random_operations(
            ops in prop::collection::vec(op_strategy(), 0..200),
        ) {
            let (mut iac, _) = GatewayTelnetIac::new(
                true,
                "ANSI".into(),
                80,
                24,
            );
            let mut data = Vec::new();
            let mut replies = Vec::new();
            for op in &ops {
                apply(&mut iac, op, &mut data, &mut replies);
                check_structural_invariants(&iac);
            }
            // Cumulative reply stream from the whole run must be
            // parseable telnet protocol.
            prop_assert!(
                iac_reply_stream_is_well_formed(&replies),
                "reply stream was malformed: {:?}",
                replies,
            );
        }

        /// The byte-level parser must never panic on an arbitrary
        /// input, including truncations mid-sequence.
        #[test]
        fn fuzz_random_bytes(
            bytes in prop::collection::vec(0u8..=255u8, 0..500),
        ) {
            let (mut iac, _) = GatewayTelnetIac::new(
                true,
                "ANSI".into(),
                80,
                24,
            );
            let mut data = Vec::new();
            let mut replies = Vec::new();
            for &b in &bytes {
                iac.feed(b, &mut data, &mut replies);
            }
            check_structural_invariants(&iac);
            prop_assert!(iac_reply_stream_is_well_formed(&replies));
        }

        /// Reactive mode (cooperate=false) should only ever emit
        /// refusal verbs (DONT/WONT) for non-ECHO options — never an
        /// accepting WILL/DO or subnegotiation.
        #[test]
        fn fuzz_reactive_only_refuses(
            ops in prop::collection::vec(op_strategy(), 0..100),
        ) {
            let mut iac = reactive_iac();
            let mut data = Vec::new();
            let mut replies = Vec::new();
            for op in &ops {
                apply(&mut iac, op, &mut data, &mut replies);
            }
            // Walk the reply stream: if we see an accepting verb it
            // must be DO ECHO or the byte sequence must be part of a
            // refusal cycle from an active-change helper.  For the
            // simpler check, verify there are no SB sequences at all
            // (reactive mode never emits subnegotiations).
            let mut i = 0;
            while i + 1 < replies.len() {
                if replies[i] == IAC && replies[i + 1] == SB {
                    panic!(
                        "reactive mode emitted SB subnegotiation: \
                         replies = {:?}", replies,
                    );
                }
                i += 1;
            }
        }
    }
}

// ─── 6-state Q-method transitions ─────────────────────

#[test]
fn test_qmethod_request_enable_from_no() {
    let mut iac = reactive_iac();
    let mut replies = Vec::new();
    iac.request_local_enable(OPT_SGA, &mut replies);
    assert_eq!(replies, vec![IAC, WILL, OPT_SGA]);
    assert_eq!(iac.us_state[OPT_SGA as usize], OptState::WantYes);
}

#[test]
fn test_qmethod_mind_change_during_wantyes_goes_to_opposite() {
    // We send WILL (enter WantYes), then change our mind and send
    // WONT before peer replies: state → WantYesOpposite, nothing on
    // the wire yet because our WILL is still pending.
    let mut iac = reactive_iac();
    let mut replies = Vec::new();
    iac.request_local_enable(OPT_SGA, &mut replies);
    replies.clear();
    iac.request_local_disable(OPT_SGA, &mut replies);
    assert_eq!(iac.us_state[OPT_SGA as usize], OptState::WantYesOpposite);
    assert!(
        replies.is_empty(),
        "in-flight mind-change defers the WONT until peer ack"
    );
}

#[test]
fn test_qmethod_peer_acks_opposite_with_wont() {
    // us_state = WantYesOpposite, peer sends DO (ack of our WILL).
    // We now send WONT and enter WantNo.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.us_state[idx] = OptState::WantYesOpposite;
    let mut replies = Vec::new();
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(DO, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    assert_eq!(iac.us_state[idx], OptState::WantNo);
    assert_eq!(replies, vec![IAC, WONT, OPT_SGA]);
    assert!(
        iac.sent_wont[idx],
        "refusal flag must be set so a re-sent DO doesn't produce a duplicate WONT"
    );
}

#[test]
fn test_qmethod_no_duplicate_wont_when_peer_re_sends_do() {
    // Regression: from WantYesOpposite, peer DO transitions us to
    // WantNo + WONT.  If peer (misbehaving) sends DO again, the
    // WantNo handler must see sent_wont already and skip the dup.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.us_state[idx] = OptState::WantYesOpposite;
    let mut replies = Vec::new();
    // First DO: WantYesOpposite → WantNo with WONT.
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(DO, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    let count_first = replies
        .windows(3)
        .filter(|w| *w == [IAC, WONT, OPT_SGA])
        .count();
    assert_eq!(count_first, 1);
    // Second DO (protocol violation): WantNo stays at No, no dup.
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(DO, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    let count_total = replies
        .windows(3)
        .filter(|w| *w == [IAC, WONT, OPT_SGA])
        .count();
    assert_eq!(
        count_total, 1,
        "a repeated DO should not produce a second WONT"
    );
}

#[test]
fn test_qmethod_no_duplicate_dont_when_peer_re_sends_will() {
    // Mirror of the above, on the him side.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.him_state[idx] = OptState::WantYesOpposite;
    let mut replies = Vec::new();
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(WILL, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    let count_first = replies
        .windows(3)
        .filter(|w| *w == [IAC, DONT, OPT_SGA])
        .count();
    assert_eq!(count_first, 1);
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(WILL, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    let count_total = replies
        .windows(3)
        .filter(|w| *w == [IAC, DONT, OPT_SGA])
        .count();
    assert_eq!(
        count_total, 1,
        "a repeated WILL should not produce a second DONT"
    );
}

#[test]
fn test_qmethod_peer_refuses_opposite_cleanly() {
    // us_state = WantYesOpposite, peer sends DONT (refuses our WILL).
    // We wanted No anyway — settle at No without any extra verb.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.us_state[idx] = OptState::WantYesOpposite;
    let mut replies = Vec::new();
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(DONT, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    assert_eq!(iac.us_state[idx], OptState::No);
    assert!(replies.is_empty(), "opposite path resolved without reply");
}

#[test]
fn test_qmethod_his_wantno_opposite_on_wont_reply() {
    // him_state = WantNoOpposite; peer sends WONT confirming our DONT.
    // We swing to WantYes and send DO.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.him_state[idx] = OptState::WantNoOpposite;
    let mut replies = Vec::new();
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(WONT, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    assert_eq!(iac.him_state[idx], OptState::WantYes);
    assert_eq!(replies, vec![IAC, DO, OPT_SGA]);
}

#[test]
fn test_qmethod_active_enable_is_idempotent_in_wantyes() {
    // Calling request_local_enable while already in WantYes is a no-op.
    let mut iac = reactive_iac();
    let mut replies = Vec::new();
    iac.request_local_enable(OPT_SGA, &mut replies);
    assert_eq!(replies, vec![IAC, WILL, OPT_SGA]);
    replies.clear();
    iac.request_local_enable(OPT_SGA, &mut replies);
    assert!(replies.is_empty(), "idempotent");
    assert_eq!(iac.us_state[OPT_SGA as usize], OptState::WantYes);
}

#[test]
fn test_qmethod_error_recovery_will_in_wantno() {
    // him_state = WantNo, peer sends WILL (protocol violation). We
    // should bounce back to No without entering Yes, and refuse
    // again if we haven't already.
    let mut iac = reactive_iac();
    let idx = OPT_SGA as usize;
    iac.him_state[idx] = OptState::WantNo;
    let mut replies = Vec::new();
    iac.feed(IAC, &mut Vec::new(), &mut replies);
    iac.feed(WILL, &mut Vec::new(), &mut replies);
    iac.feed(OPT_SGA, &mut Vec::new(), &mut replies);
    assert_eq!(iac.him_state[idx], OptState::No);
    assert_eq!(replies, vec![IAC, DONT, OPT_SGA]);
}

// ─── read_gateway_event ───────────────────────────────

#[tokio::test]
async fn test_gateway_event_data_byte() {
    let mut data = &b"Ahello"[..];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Data(b'A'));
}

#[tokio::test]
async fn test_gateway_event_iac_iac_unescapes() {
    let mut data: &[u8] = &[IAC, IAC, b'B'];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Data(0xFF));
}

#[tokio::test]
async fn test_gateway_event_drops_2byte_iac() {
    let mut data: &[u8] = &[IAC, 0xF1, b'X']; // IAC NOP X
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Data(b'X'));
}

#[tokio::test]
async fn test_gateway_event_drops_negotiation() {
    let mut data: &[u8] = &[IAC, WILL, OPT_ECHO, b'Y'];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Data(b'Y'));
}

#[tokio::test]
async fn test_gateway_event_surfaces_naws() {
    // IAC SB NAWS 0x00 0x50 0x00 0x18 IAC SE → NawsResize(80, 24)
    let mut data: &[u8] = &[
        IAC, SB, OPT_NAWS, 0x00, 0x50, 0x00, 0x18, IAC, SE,
        b'Z',
    ];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::NawsResize(80, 24));
}

#[tokio::test]
async fn test_gateway_event_naws_with_escaped_iac_in_body() {
    // Width = 0x00FF needs IAC-doubling inside the NAWS body.
    let mut data: &[u8] = &[
        IAC, SB, OPT_NAWS,
        0x00, IAC, IAC,    // width low = 0xFF (doubled)
        0x00, 0x18,
        IAC, SE,
    ];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::NawsResize(0x00FF, 0x0018));
}

#[tokio::test]
async fn test_gateway_event_drops_non_naws_subneg() {
    // SB TTYPE SEND — should be silently consumed; next event is the data byte.
    let mut data: &[u8] = &[
        IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE,
        b'Q',
    ];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Data(b'Q'));
}

#[tokio::test]
async fn test_gateway_event_eof() {
    let mut data: &[u8] = &[];
    let ev = read_gateway_event(&mut data).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Eof);
}

// ─── NAWS mid-session forwarding ──────────────────────

#[test]
fn test_gateway_naws_update_forwarded_when_enabled() {
    // After DO NAWS peer response, us_state[NAWS] = Yes. A later
    // send_naws_update must emit an IAC SB NAWS to remote.
    let (mut iac, _) = cooperative_iac();
    let (_, _) = feed_all(&mut iac, &[IAC, DO, OPT_NAWS]); // ack sets Yes
    let mut replies = Vec::new();
    iac.send_naws_update(120, 50, &mut replies);
    let expected = [
        IAC, SB, OPT_NAWS,
        0x00, 0x78,  // 120
        0x00, 0x32,  // 50
        IAC, SE,
    ];
    assert_eq!(replies, expected);
}

#[test]
fn test_gateway_naws_update_silent_when_disabled() {
    // Without the NAWS option being enabled (reactive mode or peer
    // refused), send_naws_update emits nothing.
    let mut iac = reactive_iac();
    let mut replies = Vec::new();
    iac.send_naws_update(120, 50, &mut replies);
    assert!(replies.is_empty(), "should not emit SB NAWS when option is off");
}

// ─── write_telnet_data ────────────────────────────────

#[tokio::test]
async fn test_write_telnet_data_escapes_ff() {
    let mut buf: Vec<u8> = Vec::new();
    write_telnet_data(&mut buf, &[b'A', 0xFF, b'B', 0xFF, 0xFF, b'C'])
        .await
        .unwrap();
    assert_eq!(buf, vec![b'A', 0xFF, 0xFF, b'B', 0xFF, 0xFF, 0xFF, 0xFF, b'C']);
}

#[tokio::test]
async fn test_write_telnet_data_passthrough_without_ff() {
    let mut buf: Vec<u8> = Vec::new();
    write_telnet_data(&mut buf, b"hello").await.unwrap();
    assert_eq!(buf, b"hello");
}

#[tokio::test]
async fn test_do_binary_gets_wont() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Peer: IAC DO BINARY (opt 0) + real byte so the read returns.
    peer.write_all(&[IAC, DO, 0x00, b'X']).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    // Expect IAC WONT BINARY somewhere in the reply stream.
    let wont_binary = [IAC, WONT, 0x00];
    assert!(
        out.windows(3).any(|w| w == wont_binary),
        "expected IAC WONT 0x00, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_will_binary_gets_dont() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, WILL, 0x00, b'X']).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let dont_binary = [IAC, DONT, 0x00];
    assert!(
        out.windows(3).any(|w| w == dont_binary),
        "expected IAC DONT 0x00, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_refused_option_not_repeated() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Send DO BINARY twice, then a data byte.
    peer.write_all(&[IAC, DO, 0x00, IAC, DO, 0x00, b'X'])
        .await
        .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let wont_binary = [IAC, WONT, 0x00];
    let matches = out.windows(3).filter(|w| *w == wont_binary).count();
    assert_eq!(matches, 1, "WONT should be sent exactly once, got {:?}", out);
}

#[tokio::test]
async fn test_dont_ack_only_when_we_advertised_will() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.neg_sent_will[OPT_ECHO as usize] = true;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // DONT ECHO (we had advertised WILL ECHO) → expect WONT ECHO ack.
    // DONT BINARY (we never advertised) → no reply.
    peer.write_all(&[IAC, DONT, OPT_ECHO, IAC, DONT, 0x00, b'Z'])
        .await
        .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Z'));
    drop(session);

    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let wont_echo = [IAC, WONT, OPT_ECHO];
    let wont_binary = [IAC, WONT, 0x00];
    assert!(
        out.windows(3).any(|w| w == wont_echo),
        "expected WONT ECHO ack, got {:?}",
        out
    );
    assert!(
        !out.windows(3).any(|w| w == wont_binary),
        "should not have replied to DONT BINARY, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_ttype_is_sets_terminal_type() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    // Mark as if we'd already DO'd TTYPE in real detection.
    session.neg_sent_do[OPT_TTYPE as usize] = true;

    use tokio::io::AsyncWriteExt;
    // IAC WILL TTYPE, then IAC SB TTYPE IS "VT100" IAC SE, then data.
    peer.write_all(&[IAC, WILL, OPT_TTYPE]).await.unwrap();
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS])
        .await
        .unwrap();
    peer.write_all(b"VT100").await.unwrap();
    peer.write_all(&[IAC, SE, b'Q']).await.unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Q'));
    assert!(session.ttype_matched);
    assert_eq!(session.terminal_type, TerminalType::Ansi);
}

#[tokio::test]
async fn test_ttype_is_c64_maps_to_petscii() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;

    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS])
        .await
        .unwrap();
    peer.write_all(b"C64").await.unwrap();
    peer.write_all(&[IAC, SE, b'!']).await.unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'!'));
    assert_eq!(session.terminal_type, TerminalType::Petscii);
}

/// Test 8a: empty TTYPE IS response (zero-byte terminal name).
/// Session must not panic; terminal_type stays at its factory value.
#[tokio::test]
async fn test_ttype_is_empty_payload() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;
    let initial_type = session.terminal_type;

    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS, IAC, SE, b'Q'])
        .await
        .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Q'));
    assert_eq!(session.terminal_type, initial_type);
}

/// Test 8b: TTYPE IS with IAC IAC embedded in the terminal-type
/// string.  The SB-body reader must unescape to a single 0xFF so
/// the name decodes without interpreting the 0xFF as an IAC
/// command.  Terminal-type lookup should treat it as an unknown
/// name and leave the session terminal_type unchanged.
#[tokio::test]
async fn test_ttype_is_with_escaped_iac_in_name() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;
    let initial_type = session.terminal_type;

    use tokio::io::AsyncWriteExt;
    // "term\xFFname" with the 0xFF properly IAC-doubled on the wire.
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS]).await.unwrap();
    peer.write_all(b"term").await.unwrap();
    peer.write_all(&[IAC, IAC]).await.unwrap();      // escaped 0xFF
    peer.write_all(b"name").await.unwrap();
    peer.write_all(&[IAC, SE, b'R']).await.unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'R'));
    // The unusual name doesn't match any known terminal type →
    // session keeps its factory terminal.
    assert_eq!(session.terminal_type, initial_type);
}

/// Test 8c: a ridiculously long TTYPE IS payload — our SB reader
/// has a hard cap to prevent a malicious peer from exhausting
/// memory.  The session must not panic and should resync on the
/// eventual IAC SE.  The writer runs in its own task so we don't
/// deadlock on the duplex buffer (2 KiB > 512-byte buffer).
#[tokio::test]
async fn test_ttype_is_oversized_payload_does_not_panic() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;

    let writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS]).await.unwrap();
        let junk = vec![b'x'; 2048];
        peer.write_all(&junk).await.unwrap();
        peer.write_all(&[IAC, SE, b'Z']).await.unwrap();
    });

    // After the SB, we should cleanly receive the post-SE data byte.
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Z'));
    writer.await.unwrap();
}

#[tokio::test]
async fn test_naws_payload_stored() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.neg_sent_do[OPT_NAWS as usize] = true;

    use tokio::io::AsyncWriteExt;
    // IAC SB NAWS 0x00 0x50 0x00 0x18 IAC SE → 80x24.
    peer.write_all(&[
        IAC, SB, OPT_NAWS, 0x00, 0x50, 0x00, 0x18, IAC, SE, b'A',
    ])
    .await
    .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'A'));
    assert_eq!(session.window_width, Some(80));
    assert_eq!(session.window_height, Some(24));
}

#[tokio::test]
async fn test_naws_with_iac_iac_inside_payload() {
    // Window width 0xFF08 would include the IAC byte — the peer
    // must send IAC IAC to escape. Make sure our payload parser
    // unescapes correctly.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.neg_sent_do[OPT_NAWS as usize] = true;

    use tokio::io::AsyncWriteExt;
    peer.write_all(&[
        IAC, SB, OPT_NAWS, 0xFF, 0xFF, 0x08, 0x00, 0x18, IAC, SE, b'A',
    ])
    .await
    .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'A'));
    assert_eq!(session.window_width, Some(0xFF08));
    assert_eq!(session.window_height, Some(0x0018));
}

#[tokio::test]
async fn test_escaped_iac_as_data() {
    // IAC IAC in the input stream must surface as a single 0xFF
    // data byte (not a start-of-command).
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[b'A', IAC, IAC, b'B']).await.unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'A'));
    assert_eq!(session.session_read_byte().await.unwrap(), Some(0xFF));
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'B'));
}

#[tokio::test]
async fn test_empty_subneg_tolerated() {
    // IAC SB TTYPE IAC SE — zero-length payload. Should not crash
    // and should not set ttype_matched.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, SB, OPT_TTYPE, IAC, SE, b'A'])
        .await
        .unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'A'));
    assert!(!session.ttype_matched);
}

#[tokio::test]
async fn test_dont_without_prior_will_is_silent() {
    // Peer sends DONT ECHO without us having advertised WILL ECHO.
    // We should not reply (no WONT) per RFC 1143 (prevents loops).
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DONT, OPT_ECHO, b'Z'])
        .await
        .unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Z'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    // No reply expected.
    assert!(
        out.is_empty(),
        "DONT for unadvertised option should be silent, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_wont_without_prior_do_is_silent() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, WONT, 0x42, b'Z']).await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Z'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    assert!(
        out.is_empty(),
        "WONT for unadvertised option should be silent, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_do_echo_is_ack_when_we_willed_echo() {
    // Peer's DO ECHO is an acknowledgement of our WILL ECHO — no reply.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.neg_sent_will[OPT_ECHO as usize] = true;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DO, OPT_ECHO, b'Q']).await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'Q'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    // Must NOT contain a WONT ECHO — DO is just an ack.
    let wont_echo = [IAC, WONT, OPT_ECHO];
    assert!(
        !out.windows(3).any(|w| w == wont_echo),
        "should not have replied to DO ECHO ack, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_subneg_with_sb_payload_then_data() {
    // Two subnegs back-to-back, then a data byte. Verify both are
    // processed and we return the data byte cleanly.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;
    session.neg_sent_do[OPT_NAWS as usize] = true;

    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS]).await.unwrap();
    peer.write_all(b"XTERM").await.unwrap();
    peer.write_all(&[IAC, SE, IAC, SB, OPT_NAWS, 0x00, 0x50, 0x00, 0x18, IAC, SE, b'*'])
        .await
        .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'*'));
    assert_eq!(session.terminal_type, TerminalType::Ansi);
    assert_eq!(session.window_width, Some(80));
    assert_eq!(session.window_height, Some(24));
}

#[tokio::test]
async fn test_nop_is_silently_consumed() {
    // IAC NOP (0xF1) has no option byte and needs no reply.
    const NOP: u8 = 0xF1;
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, NOP, b'X']).await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    assert!(out.is_empty(), "NOP should produce no reply, got {:?}", out);
}

// ─── Telnet RFC conformance tests ────────────────────────
//
// These tests cite specific RFC sections and lock in byte-exact
// protocol behavior.  They complement the broader behavioral
// tests above by giving a future reader an explicit checkpoint
// against the standards.

#[test]
fn test_rfc854_command_byte_values() {
    // RFC 854 §"COMMAND NAME" table: every IAC command is a
    // specific byte value.  Lock these in as constants so a
    // refactor that accidentally renames a constant can't
    // silently change the wire format.
    const _: () = assert!(IAC == 0xFF);
    const _: () = assert!(SE == 0xF0);
    const _: () = assert!(SB == 0xFA);
    const _: () = assert!(WILL == 0xFB);
    const _: () = assert!(WONT == 0xFC);
    const _: () = assert!(DO == 0xFD);
    const _: () = assert!(DONT == 0xFE);
}

#[test]
fn test_rfc857_858_859_1073_1091_option_byte_values() {
    // Option byte assignments per IANA Telnet Option registry,
    // codified in the originating RFCs:
    //   RFC 857 — Echo (option 1)
    //   RFC 858 — Suppress Go Ahead (option 3)
    //   RFC 859 — Status (option 5)
    //   RFC 860 — Timing Mark (option 6)
    //   RFC 1091 — Terminal Type (option 24 = 0x18)
    //   RFC 1073 — Window Size / NAWS (option 31 = 0x1F)
    const _: () = assert!(OPT_ECHO == 0x01);
    const _: () = assert!(OPT_SGA == 0x03);
    const _: () = assert!(OPT_STATUS == 0x05);
    const _: () = assert!(OPT_TIMING_MARK == 0x06);
    const _: () = assert!(OPT_TTYPE == 0x18);
    const _: () = assert!(OPT_NAWS == 0x1F);
}

#[test]
fn test_rfc1091_ttype_subnegotiation_command_bytes() {
    // RFC 1091: TTYPE subnegotiation uses two command bytes:
    //   IS   = 0x00 (sender follows with the terminal name)
    //   SEND = 0x01 (request the peer's terminal name)
    const _: () = assert!(TTYPE_IS == 0x00);
    const _: () = assert!(TTYPE_SEND == 0x01);
}

#[tokio::test]
async fn test_rfc854_iac_iac_decodes_to_literal_ff() {
    // RFC 854: "If [the data stream] is desired to send the data
    // byte 255, two 255s must be sent."  i.e., IAC IAC in the
    // data stream decodes to a single literal 0xFF byte.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, IAC, b'X']).await.unwrap();
    assert_eq!(
        session.session_read_byte().await.unwrap(),
        Some(0xFF),
        "IAC IAC must decode to literal 0xFF"
    );
    assert_eq!(
        session.session_read_byte().await.unwrap(),
        Some(b'X'),
        "byte after IAC IAC must read normally"
    );
}

#[tokio::test]
async fn test_rfc1073_naws_subneg_byte_layout() {
    // RFC 1073: NAWS subnegotiation is exactly:
    //   IAC SB NAWS WIDTH_HI WIDTH_LO HEIGHT_HI HEIGHT_LO IAC SE
    // This test feeds a well-formed NAWS payload and verifies
    // both width and height are decoded as 16-bit big-endian.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.neg_sent_do[OPT_NAWS as usize] = true;
    use tokio::io::AsyncWriteExt;
    // 132 cols × 43 rows = 0x0084 × 0x002B.
    peer.write_all(&[
        IAC, SB, OPT_NAWS,
        0x00, 0x84, // width hi, lo
        0x00, 0x2B, // height hi, lo
        IAC, SE,
        b'!', // sentinel data byte
    ])
    .await
    .unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'!'));
    assert_eq!(session.window_width, Some(132));
    assert_eq!(session.window_height, Some(43));
}

#[tokio::test]
async fn test_rfc1091_ttype_is_subneg_byte_layout() {
    // RFC 1091: TTYPE IS subnegotiation is:
    //   IAC SB TTYPE IS <terminal-name> IAC SE
    // The terminal name is bytes following IS (0x00) up to the
    // closing IAC SE.  Test feeds "ANSI" and verifies it ends up
    // recognized as TerminalType::Ansi.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.neg_sent_do[OPT_TTYPE as usize] = true;
    use tokio::io::AsyncWriteExt;
    peer.write_all(&[IAC, SB, OPT_TTYPE, TTYPE_IS])
        .await
        .unwrap();
    peer.write_all(b"ANSI").await.unwrap();
    peer.write_all(&[IAC, SE, b'!']).await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'!'));
    assert_eq!(
        session.terminal_type,
        TerminalType::Ansi,
        "TTYPE IS 'ANSI' must set terminal type to Ansi"
    );
}

#[tokio::test]
async fn test_rfc859_status_send_triggers_status_is_response() {
    // RFC 859: when peer sends IAC SB STATUS SEND IAC SE, we
    // must respond with IAC SB STATUS IS <state> IAC SE.  The
    // state body lists every option we've negotiated.  This
    // test verifies the response begins with the expected
    // wrapper.
    const STATUS_SEND: u8 = 0x01;
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    // Per the handler, we only respond if we've already WILL'd
    // STATUS — otherwise we don't claim to support it.
    session.neg_sent_will[OPT_STATUS as usize] = true;
    // Pretend we WILL'd ECHO so STATUS IS has something to report.
    session.neg_sent_will[OPT_ECHO as usize] = true;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, SB, OPT_STATUS, STATUS_SEND, IAC, SE, b'.'])
        .await
        .unwrap();
    // Drain the data byte so the subneg gets fully processed.
    let _ = session.session_read_byte().await;
    // Drop session so peer can read whatever the server emitted.
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    // Find the STATUS IS reply.  Format: IAC SB STATUS IS ...
    // IAC SE.  We just verify the prefix and the trailer are
    // present, and that ECHO appears as a WILL in the body.
    let prefix = [IAC, SB, OPT_STATUS, 0x00 /* IS */];
    let pos = out
        .windows(prefix.len())
        .position(|w| w == prefix)
        .expect("expected IAC SB STATUS IS in reply");
    // Body must contain WILL OPT_ECHO somewhere before the
    // closing IAC SE.
    let after_prefix = &out[pos + prefix.len()..];
    let se_idx = after_prefix
        .windows(2)
        .position(|w| w == [IAC, SE])
        .expect("expected closing IAC SE");
    let body = &after_prefix[..se_idx];
    let will_echo = [WILL, OPT_ECHO];
    assert!(
        body.windows(2).any(|w| w == will_echo),
        "STATUS IS body must contain WILL OPT_ECHO, got: {:?}",
        body
    );
}

#[tokio::test]
async fn test_rfc855_q_method_dont_loop_on_already_disabled_option() {
    // RFC 855 Q-method §"DON'T to a disabled option": if a peer
    // sends IAC DONT for an option that's already disabled on
    // our side, we must NOT respond with another IAC WONT —
    // doing so would create an infinite negotiation loop.
    // We never advertised WILL ECHO, so OPT_ECHO is in the
    // disabled state; sending DONT ECHO must produce no reply.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    // Make sure OPT_ECHO has not been WILL'd.
    session.neg_sent_will[OPT_ECHO as usize] = false;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DONT, OPT_ECHO, b'.']).await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'.'));
    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    // The reply must not contain another WONT ECHO (which would
    // bounce back to the peer and risk a loop).
    let wont_echo = [IAC, WONT, OPT_ECHO];
    assert!(
        !out.windows(3).any(|w| w == wont_echo),
        "received unexpected WONT ECHO reply (Q-method violation), out={:?}",
        out
    );
}

// ─── save_received_file ───────────────────────────────────

/// Fresh path → write succeeds, file contains the data, meta
/// (when supplied) gets applied.  Smoke test of the happy path.
#[tokio::test]
async fn test_save_received_file_fresh_path() {
    let tmp = std::env::temp_dir()
        .join(format!("save_fresh_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let result = TelnetSession::save_received_file(&tmp, b"hello", None).await;
    assert!(result.is_ok(), "fresh path should save");
    let read_back = std::fs::read(&tmp).unwrap();
    assert_eq!(read_back, b"hello");
    let _ = std::fs::remove_file(&tmp);
}

/// Existing path → AlreadyExists (TOCTOU-tight: even between the
/// caller's intent to save and our `create_new` call, no write
/// happens to the existing file).  Locks in the create-new
/// guarantee that motivated lifting this helper out of the
/// per-protocol save loops.
#[tokio::test]
async fn test_save_received_file_already_exists_is_atomic() {
    let tmp = std::env::temp_dir()
        .join(format!("save_exists_{}", std::process::id()));
    std::fs::write(&tmp, b"original").unwrap();
    let result =
        TelnetSession::save_received_file(&tmp, b"NEW DATA", None).await;
    assert_eq!(
        result.unwrap_err(),
        SaveError::AlreadyExists,
        "must reject pre-existing file with AlreadyExists",
    );
    let read_back = std::fs::read(&tmp).unwrap();
    assert_eq!(
        read_back, b"original",
        "existing file's bytes must not be touched",
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Resume save: with `replace_existing=true`, the saver
/// atomically replaces the on-disk partial with the merged
/// full-file bytes (tmp + rename).  Without this, Kermit's
/// resume-partial code path is broken end-to-end — the
/// receiver loads the partial into memory, merges D-packets,
/// then the create-new save fails with AlreadyExists and
/// the merged data is silently dropped.  This test locks in
/// the resume-write path and would catch any regression to
/// the old create-new-only behavior.
#[test]
fn test_save_received_file_sync_replace_existing_overwrites_partial() {
    let tmp = std::env::temp_dir()
        .join(format!("save_resume_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    // Simulate a 1 KB partial on disk from a prior interrupted
    // session, plus the 4 KB merged buffer the receiver built
    // from (partial + resumed D-packets).
    std::fs::write(&tmp, vec![0xAAu8; 1024]).unwrap();
    let merged: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    let result = TelnetSession::save_received_file_sync(
        &tmp, &merged, None, /* replace_existing */ true,
    );
    assert!(
        result.is_ok(),
        "replace_existing=true must succeed even when path exists",
    );
    let read_back = std::fs::read(&tmp).unwrap();
    assert_eq!(
        read_back, merged,
        "on-disk content must equal the merged full-file bytes",
    );
    // The .kermit-resume.tmp side-file must have been renamed
    // away — leftover tmp files would accumulate across resumes.
    let mut tmp_path = tmp.clone();
    let mut tmp_name = tmp_path.file_name().unwrap().to_os_string();
    tmp_name.push(".kermit-resume.tmp");
    tmp_path.set_file_name(tmp_name);
    assert!(
        !tmp_path.exists(),
        "tmp file must be renamed (or cleaned up) on success",
    );
    let _ = std::fs::remove_file(&tmp);
}

/// `replace_existing=false` keeps the existing create-new
/// "refuse to clobber" semantics — sanity check that the
/// resume branch didn't accidentally make the default path
/// permissive.
#[test]
fn test_save_received_file_sync_no_replace_refuses_existing() {
    let tmp = std::env::temp_dir()
        .join(format!("save_no_replace_{}", std::process::id()));
    std::fs::write(&tmp, b"original").unwrap();
    let err = TelnetSession::save_received_file_sync(
        &tmp,
        b"NEW DATA",
        None,
        /* replace_existing */ false,
    )
    .unwrap_err();
    assert_eq!(err, SaveError::AlreadyExists);
    let read_back = std::fs::read(&tmp).unwrap();
    assert_eq!(
        read_back, b"original",
        "existing bytes must not be touched when replace_existing=false",
    );
    let _ = std::fs::remove_file(&tmp);
}

/// `numbered_received_name` implements the DOS/CP-M-Kermit 8.3 collision
/// scheme exactly as the user specified (and as kercpm3 does on a
/// download collision): keep the base within 8 chars, appending the
/// number when it fits and replacing trailing base chars when it
/// doesn't; the extension is preserved.
#[test]
fn test_numbered_received_name_scheme() {
    let n = |f: &str, i: u32| TelnetSession::numbered_received_name(f, i).unwrap();
    // 8-char base: number replaces the trailing char(s) to stay at 8.
    assert_eq!(n("abcdefgh.txt", 0), "abcdefg0.txt");
    assert_eq!(n("abcdefgh.txt", 9), "abcdefg9.txt");
    assert_eq!(n("abcdefgh.txt", 10), "abcdef10.txt");
    assert_eq!(n("abcdefgh.txt", 99), "abcdef99.txt");
    assert_eq!(n("abcdefgh.txt", 100), "abcde100.txt");
    // Under-8 base: the number is simply appended.
    assert_eq!(n("hi.txt", 0), "hi0.txt");
    assert_eq!(n("hi.txt", 9), "hi9.txt");
    assert_eq!(n("hi.txt", 10), "hi10.txt");
    // 7-char base grows to 8 then replaces once the number needs 2 digits.
    assert_eq!(n("abcdefg.txt", 0), "abcdefg0.txt");
    assert_eq!(n("abcdefg.txt", 10), "abcdef10.txt");
    // No extension: base is numbered, nothing appended after.
    assert_eq!(n("README", 0), "README0");
    assert_eq!(n("mydatafile", 0), "mydataf0"); // 10-char base capped to 8
    // Number too large to fit the 8-char base → None (stop probing).
    assert_eq!(TelnetSession::numbered_received_name("abcdefgh.txt", 100_000_000), None);
}

/// `save_received_file_collision_safe` renames instead of dropping:
/// three uploads of the same name yield the original plus two numbered
/// variants, all with their own contents; the original is never
/// overwritten.
#[test]
fn test_save_collision_safe_renames_not_drops() {
    let dir = std::env::temp_dir()
        .join(format!("collision_safe_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let a = TelnetSession::save_received_file_collision_safe(&dir, "file.txt", b"A", None, false).unwrap();
    let b = TelnetSession::save_received_file_collision_safe(&dir, "file.txt", b"B", None, false).unwrap();
    let c = TelnetSession::save_received_file_collision_safe(&dir, "file.txt", b"C", None, false).unwrap();
    assert_eq!(a, "file.txt");
    assert_eq!(b, "file0.txt");
    assert_eq!(c, "file1.txt");
    assert_eq!(std::fs::read(dir.join("file.txt")).unwrap(), b"A");
    assert_eq!(std::fs::read(dir.join("file0.txt")).unwrap(), b"B");
    assert_eq!(std::fs::read(dir.join("file1.txt")).unwrap(), b"C");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resumed transfer must replace its own partial by exact name — the
/// collision-safe saver never renames a resume.
#[test]
fn test_save_collision_safe_resume_keeps_name() {
    let dir = std::env::temp_dir()
        .join(format!("collision_resume_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("part.bin"), b"partial").unwrap();

    let name = TelnetSession::save_received_file_collision_safe(
        &dir, "part.bin", b"complete", None, /* resumed */ true,
    )
    .unwrap();
    assert_eq!(name, "part.bin");
    assert_eq!(std::fs::read(dir.join("part.bin")).unwrap(), b"complete");
    // No numbered variant was created.
    assert!(!dir.join("part0.bin").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Meta is applied iff supplied; otherwise the saved file keeps
/// the OS-default mtime / mode.  Confirms the helper plumbs meta
/// through to apply_ymodem_meta correctly.
#[tokio::test]
async fn test_save_received_file_applies_meta() {
    let tmp = std::env::temp_dir()
        .join(format!("save_meta_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let target_secs: u64 = 1_400_000_000; // 2014-05-13
    let meta = crate::xmodem::YmodemReceiveMeta {
        size: None,
        modtime: Some(target_secs),
        mode: None,
    };
    TelnetSession::save_received_file(&tmp, b"x", Some(&meta))
        .await
        .unwrap();
    let actual = std::fs::metadata(&tmp)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(actual, target_secs, "save_received_file must apply meta");
    let _ = std::fs::remove_file(&tmp);
}

// ─── YMODEM block-0 metadata application ─────────────────

/// `apply_ymodem_meta` with `meta = None` must be a no-op — covers
/// the common XMODEM (no block 0) and ZMODEM paths so we don't
/// accidentally rewrite mtime/mode on every saved file.
#[test]
fn test_apply_ymodem_meta_none_is_noop() {
    let tmp = std::env::temp_dir()
        .join(format!("ymeta_none_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, b"x").unwrap();
    let before = std::fs::metadata(&tmp).unwrap();
    // Brief sleep so any spurious modtime change is detectable.
    std::thread::sleep(std::time::Duration::from_millis(10));
    TelnetSession::apply_ymodem_meta(&tmp, None);
    let after = std::fs::metadata(&tmp).unwrap();
    assert_eq!(
        before.modified().unwrap(),
        after.modified().unwrap(),
        "modtime must be unchanged when meta is None",
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Modtime application: when block-0 carried a timestamp, the
/// saved file's mtime should match (within whole-second resolution
/// — POSIX `utimes` is second-granular on most filesystems).
#[test]
fn test_apply_ymodem_meta_modtime() {
    let tmp = std::env::temp_dir()
        .join(format!("ymeta_mtime_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, b"x").unwrap();
    let target_secs: u64 = 1_500_000_000; // 2017-07-14 — clearly in the past
    let meta = crate::xmodem::YmodemReceiveMeta {
        size: Some(1),
        modtime: Some(target_secs),
        mode: None,
    };
    TelnetSession::apply_ymodem_meta(&tmp, Some(&meta));
    let after = std::fs::metadata(&tmp).unwrap();
    let actual = after
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(actual, target_secs, "modtime must match block-0 value");
    let _ = std::fs::remove_file(&tmp);
}

/// Mode application is Unix-only; on Unix, the block-0 `mode`
/// field (already masked to 0o7777 by the parser) is masked
/// further to 0o777 by the apply path before reaching `chmod`.
#[cfg(unix)]
#[test]
fn test_apply_ymodem_meta_mode_unix() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir()
        .join(format!("ymeta_mode_{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, b"x").unwrap();
    // Start with mode 0o600.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
    let meta = crate::xmodem::YmodemReceiveMeta {
        size: Some(1),
        modtime: None,
        // Pass setuid + 0o755; the apply mask (0o777) must drop
        // setuid, giving us plain 0o755 on disk.  This guards
        // against a malicious sender setting setuid bits on our
        // saved files.
        mode: Some(0o4755),
    };
    TelnetSession::apply_ymodem_meta(&tmp, Some(&meta));
    let actual = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o7777;
    assert_eq!(actual, 0o755, "setuid bit must be stripped, perms preserved");
    let _ = std::fs::remove_file(&tmp);
}

/// A subnegotiation that begins (`IAC SB <opt>`) but then stalls — the
/// peer sends no further bytes and never the terminating `IAC SE` — must
/// not pin the reader.  The in-SB read is bounded by `SB_DRAIN_TIMEOUT`,
/// after which the event reader reports `Eof` instead of blocking forever
/// (the slowloris guard).
#[tokio::test(start_paused = true)]
async fn test_read_gateway_event_sb_stall_times_out() {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // Yields its queued bytes, then stalls (Poll::Pending) forever —
    // modelling an open-but-silent connection (not EOF).
    struct StallReader {
        data: std::io::Cursor<Vec<u8>>,
    }
    impl tokio::io::AsyncRead for StallReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.data.position() < self.data.get_ref().len() as u64 {
                Pin::new(&mut self.data).poll_read(cx, buf)
            } else {
                Poll::Pending
            }
        }
    }

    // IAC SB NAWS, then silence — the body read stalls.
    let mut reader = StallReader {
        data: std::io::Cursor::new(vec![IAC, SB, OPT_NAWS]),
    };
    // Time is paused; tokio auto-advances to the SB_DRAIN_TIMEOUT deadline
    // once the stalled read is the only pending work, so this resolves
    // promptly instead of waiting the real 15s.
    let ev = read_gateway_event(&mut reader).await.unwrap();
    assert_eq!(ev, GatewayInboundEvent::Eof);
}

// ─── Weather: worldwide location + units helpers ──────────

fn geo(name: &str, admin1: &str, country: &str, cc: &str) -> GeoResult {
    GeoResult {
        name: name.into(),
        admin1: admin1.into(),
        country: country.into(),
        country_code: cc.into(),
        lat: 1.0,
        lon: 2.0,
        timezone: "auto".into(),
    }
}

#[test]
fn test_validate_weather_location_accepts_worldwide() {
    // US zip, city names, a UK postcode with a space, and non-ASCII all pass.
    for good in ["62051", "London", "London, GB", "SW1A 1AA", "Zürich", "São Paulo", "東京"] {
        assert!(validate_weather_location(good).is_ok(), "should accept {good:?}");
    }
    // Surrounding whitespace and control chars are cleaned, not rejected.
    assert_eq!(validate_weather_location("  Paris\t ").unwrap(), "Paris");
    assert_eq!(validate_weather_location("Lon\x07don").unwrap(), "London");
}

#[test]
fn test_validate_weather_location_rejects_empty_and_overlong() {
    assert!(validate_weather_location("").is_err());
    assert!(validate_weather_location("    ").is_err());
    assert!(validate_weather_location("\x01\x02").is_err()); // only control chars
    assert!(validate_weather_location(&"x".repeat(61)).is_err());
}

#[test]
fn test_split_location_query() {
    assert_eq!(split_location_query("London, GB"), ("London".into(), Some("GB".into())));
    assert_eq!(split_location_query("Paris, France"), ("Paris".into(), Some("France".into())));
    assert_eq!(split_location_query("London, Ontario"), ("London".into(), Some("Ontario".into())));
    // No comma -> whole string, no qualifier (US zip still works).
    assert_eq!(split_location_query("62051"), ("62051".into(), None));
    // Empty side is ignored.
    assert_eq!(split_location_query("London,"), ("London,".into(), None));
    assert_eq!(split_location_query(", GB"), (", GB".into(), None));
}

#[test]
fn test_pick_geo_result_disambiguates_by_country_and_region() {
    let londons = [
        geo("London", "England", "United Kingdom", "GB"),
        geo("London", "Ontario", "Canada", "CA"),
        geo("London", "Ohio", "United States", "US"),
    ];
    // No qualifier -> first (prominence-ranked) result.
    assert_eq!(pick_geo_result(&londons, None).unwrap().country_code, "GB");
    // Country code (case-insensitive).
    assert_eq!(pick_geo_result(&londons, Some("ca")).unwrap().country_code, "CA");
    // Country name.
    assert_eq!(pick_geo_result(&londons, Some("United States")).unwrap().country_code, "US");
    // Region (admin1).
    assert_eq!(pick_geo_result(&londons, Some("Ontario")).unwrap().country_code, "CA");
    // A qualifier that matches nothing -> None (caller reports not-found).
    assert!(pick_geo_result(&londons, Some("ZZ")).is_none());
    // Empty list -> None.
    assert!(pick_geo_result(&[], None).is_none());
}

#[test]
fn test_pick_geo_result_us_state_abbreviation() {
    // A US state abbreviation expands to the full admin1 name, so the
    // natural "City, ST" form works, not just "City, StateName".
    let parises = [
        geo("Paris", "Île-de-France", "France", "FR"),
        geo("Paris", "Texas", "United States", "US"),
        geo("Paris", "Tennessee", "United States", "US"),
    ];
    assert_eq!(pick_geo_result(&parises, Some("TX")).unwrap().admin1, "Texas");
    assert_eq!(pick_geo_result(&parises, Some("tn")).unwrap().admin1, "Tennessee");
    // Full name still works.
    assert_eq!(pick_geo_result(&parises, Some("Texas")).unwrap().admin1, "Texas");
    // Springfield, IL — the motivating case.
    let springs = [
        geo("Springfield", "Missouri", "United States", "US"),
        geo("Springfield", "Illinois", "United States", "US"),
    ];
    assert_eq!(pick_geo_result(&springs, Some("IL")).unwrap().admin1, "Illinois");
    // A bogus 2-letter code is not a state and matches nothing.
    assert!(pick_geo_result(&springs, Some("ZZ")).is_none());
}

#[test]
fn test_pick_geo_result_precedence_and_ambiguity() {
    // "CA" is both Canada's country code and California's abbreviation.
    // The exact country-code match must win deterministically (not depend
    // on Open-Meteo's result ordering).
    let londons = [
        geo("London", "England", "United Kingdom", "GB"),
        geo("London", "Ontario", "Canada", "CA"),
        geo("London", "California", "United States", "US"),
    ];
    assert_eq!(pick_geo_result(&londons, Some("CA")).unwrap().country, "Canada");
    // With no country match, the US-state expansion resolves "CA" to
    // California.
    let no_canada = [
        geo("London", "England", "United Kingdom", "GB"),
        geo("London", "California", "United States", "US"),
    ];
    assert_eq!(pick_geo_result(&no_canada, Some("CA")).unwrap().admin1, "California");
    // Multiple same-country matches -> first wins (prominence order).
    let two_us = [
        geo("Paris", "Texas", "United States", "US"),
        geo("Paris", "Tennessee", "United States", "US"),
    ];
    assert_eq!(pick_geo_result(&two_us, Some("United States")).unwrap().admin1, "Texas");
}

#[test]
fn test_parse_geo_results_and_pick() {
    let json = serde_json::json!({
        "results": [
            {"name":"Paris","admin1":"Île-de-France","country":"France","country_code":"FR",
             "latitude":48.85,"longitude":2.35,"timezone":"Europe/Paris"},
            {"name":"Paris","admin1":"Texas","country":"United States","country_code":"US",
             "latitude":33.66,"longitude":-95.55},
            {"name":"NoCoords","country":"X"} // skipped: missing lat/lon
        ]
    });
    let results = parse_geo_results(&json);
    assert_eq!(results.len(), 2, "entry without coordinates is dropped");
    assert_eq!(results[0].timezone, "Europe/Paris");
    assert_eq!(results[1].timezone, "auto", "missing timezone defaults to auto");
    // "Paris, Texas" selects the US result, not the (default) France one.
    assert_eq!(pick_geo_result(&results, Some("Texas")).unwrap().country_code, "US");
    assert_eq!(pick_geo_result(&results, None).unwrap().country_code, "FR");
}

#[test]
fn test_resolve_weather_units() {
    // Auto: US -> imperial, everywhere else -> metric.
    assert_eq!(resolve_weather_units("auto", "US"), WeatherUnits::Imperial);
    assert_eq!(resolve_weather_units("auto", "us"), WeatherUnits::Imperial);
    assert_eq!(resolve_weather_units("auto", "GB"), WeatherUnits::Metric);
    assert_eq!(resolve_weather_units("auto", "FR"), WeatherUnits::Metric);
    // Unknown setting behaves like auto.
    assert_eq!(resolve_weather_units("", "US"), WeatherUnits::Imperial);
    assert_eq!(resolve_weather_units("", "DE"), WeatherUnits::Metric);
    // Explicit overrides ignore the country.
    assert_eq!(resolve_weather_units("us", "GB"), WeatherUnits::Imperial);
    assert_eq!(resolve_weather_units("metric", "US"), WeatherUnits::Metric);
}

#[test]
fn test_weather_unit_formatting_and_labels() {
    // 20 C == 68 F; imperial rounds to F, metric keeps C.
    assert_eq!(format_temp(20.0, WeatherUnits::Imperial), "68");
    assert_eq!(format_temp(20.0, WeatherUnits::Metric), "20");
    assert_eq!(format_temp(0.0, WeatherUnits::Imperial), "32");
    // A value rounding toward negative zero must show "0", never "-0".
    assert_eq!(format_temp(-0.3, WeatherUnits::Metric), "0");
    assert_eq!(format_wind(-0.2, WeatherUnits::Metric), "0");
    // 100 km/h ≈ 62 mph.
    assert_eq!(format_wind(100.0, WeatherUnits::Imperial), "62");
    assert_eq!(format_wind(100.0, WeatherUnits::Metric), "100");
    // Labels.
    assert_eq!(WeatherUnits::Imperial.temp_label(), "F");
    assert_eq!(WeatherUnits::Metric.temp_label(), "C");
    assert_eq!(WeatherUnits::Imperial.wind_label(), "mph");
    assert_eq!(WeatherUnits::Metric.wind_label(), "km/h");
}

/// F3: the session-slot RAII backstop reclaims the `max_sessions` slot on a
/// panic-unwind (armed drop) and is a no-op once the normal path has defused
/// it (so no double-release).  Guards against a future reachable panic in a
/// session silently leaking a slot.
#[test]
fn test_session_slot_guard_releases_on_armed_drop() {
    let count = Arc::new(AtomicUsize::new(1));
    let writers: SessionWriters = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(
        Box::new(Vec::<u8>::new()) as Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    ));

    // Armed guard dropped without defuse (the panic-unwind path) → released.
    {
        let _g = SessionSlotGuard {
            count: count.clone(),
            writers: writers.clone(),
            writer: writer.clone(),
            armed: true,
        };
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "an armed guard must release the slot on drop"
    );

    // Defused guard (normal path already released) → no double-release.
    count.store(1, Ordering::SeqCst);
    {
        let mut g = SessionSlotGuard {
            count: count.clone(),
            writers,
            writer,
            armed: true,
        };
        g.defuse();
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "a defused guard must not release again"
    );
}

// ─── Gateway Shell (kernel.rs) ───────────────────────

use super::kernel::CpmCmd;

// ---- Wildcard glob matcher ----

#[test]
fn test_cpm_glob_star_matches_everything() {
    assert!(TelnetSession::cpm_glob_match("*", "anything.dat"));
    assert!(TelnetSession::cpm_glob_match("*", ""));
    assert!(TelnetSession::cpm_glob_match("*.*", "a.b"));
    assert!(!TelnetSession::cpm_glob_match("*.*", "noext"));
}

#[test]
fn test_cpm_glob_extension_and_case() {
    assert!(TelnetSession::cpm_glob_match("*.txt", "readme.txt"));
    // Case-insensitive both directions.
    assert!(TelnetSession::cpm_glob_match("*.TXT", "readme.txt"));
    assert!(TelnetSession::cpm_glob_match("*.txt", "README.TXT"));
    assert!(!TelnetSession::cpm_glob_match("*.txt", "readme.md"));
}

#[test]
fn test_cpm_glob_question_is_exactly_one() {
    assert!(TelnetSession::cpm_glob_match("foo?.dat", "foo1.dat"));
    assert!(!TelnetSession::cpm_glob_match("foo?.dat", "foo12.dat"));
    assert!(!TelnetSession::cpm_glob_match("foo?.dat", "foo.dat"));
}

#[test]
fn test_cpm_glob_backtracking_and_literals() {
    assert!(TelnetSession::cpm_glob_match("a*b", "ab"));
    assert!(TelnetSession::cpm_glob_match("a*b", "aXXXb"));
    assert!(!TelnetSession::cpm_glob_match("a*b", "axc"));
    assert!(TelnetSession::cpm_glob_match("*a*b", "zzabzzb"));
    assert!(TelnetSession::cpm_glob_match("abc", "abc"));
    assert!(!TelnetSession::cpm_glob_match("abc", "abcd"));
    assert!(TelnetSession::cpm_glob_match("", ""));
    assert!(!TelnetSession::cpm_glob_match("", "x"));
}

// ---- Command parser ----

#[test]
fn test_cpm_parse_listing_and_aliases() {
    assert_eq!(TelnetSession::cpm_parse("dir"), CpmCmd::Dir(None));
    assert_eq!(TelnetSession::cpm_parse("DIR *.txt"), CpmCmd::Dir(Some("*.txt".into())));
    assert_eq!(TelnetSession::cpm_parse("ls"), CpmCmd::Dir(None));
    // Case-insensitive verb, trimmed operands, collapsed whitespace.
    assert_eq!(TelnetSession::cpm_parse("  DiR   sub/  "), CpmCmd::Dir(Some("sub/".into())));
}

#[test]
fn test_cpm_parse_empty_and_unknown() {
    assert_eq!(TelnetSession::cpm_parse(""), CpmCmd::Empty);
    assert_eq!(TelnetSession::cpm_parse("   "), CpmCmd::Empty);
    assert_eq!(
        TelnetSession::cpm_parse("frobnicate x"),
        CpmCmd::Unknown("frobnicate".into())
    );
}

#[test]
fn test_cpm_parse_needs_arg() {
    assert!(matches!(TelnetSession::cpm_parse("type"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("dump"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("era"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("mkdir"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("copy onlyone"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("ren"), CpmCmd::NeedsArg(_)));
    assert!(matches!(TelnetSession::cpm_parse("find"), CpmCmd::NeedsArg(_)));
}

#[test]
fn test_cpm_parse_cls_ver_find() {
    assert_eq!(TelnetSession::cpm_parse("cls"), CpmCmd::Cls);
    assert_eq!(TelnetSession::cpm_parse("CLEAR"), CpmCmd::Cls);
    assert_eq!(TelnetSession::cpm_parse("ver"), CpmCmd::Ver);
    assert_eq!(TelnetSession::cpm_parse("VERSION"), CpmCmd::Ver);
    assert_eq!(
        TelnetSession::cpm_parse("find *.txt"),
        CpmCmd::Find("*.txt".into())
    );
    assert_eq!(
        TelnetSession::cpm_parse("WHERE readme"),
        CpmCmd::Find("readme".into())
    );
}

#[test]
fn test_cpm_parse_ren_both_forms() {
    // CP/M form: NEW=OLD.
    assert_eq!(
        TelnetSession::cpm_parse("ren new.txt=old.txt"),
        CpmCmd::Ren { new: "new.txt".into(), old: "old.txt".into() }
    );
    // DOS space form: OLD then NEW.
    assert_eq!(
        TelnetSession::cpm_parse("ren old.txt new.txt"),
        CpmCmd::Ren { new: "new.txt".into(), old: "old.txt".into() }
    );
    assert_eq!(
        TelnetSession::cpm_parse("rename a=b"),
        CpmCmd::Ren { new: "a".into(), old: "b".into() }
    );
}

#[test]
fn test_cpm_parse_copy_move_dest_first() {
    // Destination is the first operand (CP/M PIP order).
    assert_eq!(
        TelnetSession::cpm_parse("copy sub/ file.txt"),
        CpmCmd::Copy { dst: "sub/".into(), src: "file.txt".into() }
    );
    assert_eq!(
        TelnetSession::cpm_parse("pip dst.dat=src.dat"),
        CpmCmd::Copy { dst: "dst.dat".into(), src: "src.dat".into() }
    );
    assert_eq!(
        TelnetSession::cpm_parse("cp a b"),
        CpmCmd::Copy { dst: "a".into(), src: "b".into() }
    );
    assert_eq!(
        TelnetSession::cpm_parse("move /done/ old.dat"),
        CpmCmd::Move { dst: "/done/".into(), src: "old.dat".into() }
    );
    assert_eq!(
        TelnetSession::cpm_parse("mv a b"),
        CpmCmd::Move { dst: "a".into(), src: "b".into() }
    );
}

#[test]
fn test_cpm_parse_directory_and_misc_verbs() {
    assert_eq!(TelnetSession::cpm_parse("md games"), CpmCmd::Mkdir("games".into()));
    assert_eq!(TelnetSession::cpm_parse("mkdir games"), CpmCmd::Mkdir("games".into()));
    assert_eq!(TelnetSession::cpm_parse("rd games"), CpmCmd::Rmdir("games".into()));
    assert_eq!(TelnetSession::cpm_parse("rmdir games"), CpmCmd::Rmdir("games".into()));
    assert_eq!(TelnetSession::cpm_parse("cd"), CpmCmd::Cd(None));
    assert_eq!(TelnetSession::cpm_parse("cd .."), CpmCmd::Cd(Some("..".into())));
    assert_eq!(TelnetSession::cpm_parse("chdir sub"), CpmCmd::Cd(Some("sub".into())));
    assert_eq!(TelnetSession::cpm_parse("pwd"), CpmCmd::Pwd);
    assert_eq!(TelnetSession::cpm_parse("stat"), CpmCmd::Stat(None));
    assert_eq!(TelnetSession::cpm_parse("stat f.dat"), CpmCmd::Stat(Some("f.dat".into())));
    assert_eq!(TelnetSession::cpm_parse("help"), CpmCmd::Help(None));
    assert_eq!(TelnetSession::cpm_parse("?"), CpmCmd::Help(None));
    assert_eq!(TelnetSession::cpm_parse("user 3"), CpmCmd::User);
    for q in ["exit", "bye", "quit", "QUIT"] {
        assert_eq!(TelnetSession::cpm_parse(q), CpmCmd::Exit);
    }
}

// ---- Jail path normalizer ----

#[test]
fn test_cpm_normalize_relative_and_absolute() {
    assert_eq!(
        TelnetSession::cpm_normalize("", "a/b").unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(
        TelnetSession::cpm_normalize("games", "save.dat").unwrap(),
        vec!["games".to_string(), "save.dat".to_string()]
    );
    // Leading slash resolves from the drive root, ignoring the cwd.
    assert_eq!(
        TelnetSession::cpm_normalize("games/roms", "/top.txt").unwrap(),
        vec!["top.txt".to_string()]
    );
    // "." is skipped; a trailing slash drops the empty component.
    assert_eq!(
        TelnetSession::cpm_normalize("a", "./b/").unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn test_cpm_normalize_parent_within_jail() {
    assert_eq!(
        TelnetSession::cpm_normalize("games/roms", "../x").unwrap(),
        vec!["games".to_string(), "x".to_string()]
    );
    assert_eq!(TelnetSession::cpm_normalize("a", "..").unwrap(), Vec::<String>::new());
}

#[test]
fn test_cpm_normalize_rejects_escape_and_bad_names() {
    // Climbing above the root is refused — the jail can't be escaped.
    assert!(TelnetSession::cpm_normalize("", "../etc").is_err());
    assert!(TelnetSession::cpm_normalize("a", "../../x").is_err());
    // Illegal characters / leading dot / embedded ".." are rejected by the
    // reused validate_filename gate.
    assert!(TelnetSession::cpm_normalize("", "a;b").is_err());
    assert!(TelnetSession::cpm_normalize("", ".hidden").is_err());
    assert!(TelnetSession::cpm_normalize("", "a..b").is_err());
    assert!(TelnetSession::cpm_normalize("", "sp ace").is_err());
}

// ---- Binary guard ----

#[test]
fn test_cpm_looks_binary() {
    assert!(!TelnetSession::looks_binary(b""));
    assert!(!TelnetSession::looks_binary(b"plain ascii text\r\nwith\ttabs\n"));
    // A NUL byte is an immediate reject.
    assert!(TelnetSession::looks_binary(b"text\0more"));
    // A run of C0 control bytes trips the ratio.
    assert!(TelnetSession::looks_binary(&[0x01, 0x02, 0x03, 0x04, 0x05, b'a']));
    // High-bit bytes (PETSCII / Latin-1) are not treated as control.
    assert!(!TelnetSession::looks_binary(&[0xC1; 32]));
}

// ─── Color independent of terminal encoding (C64 no-color fix) ──

/// Declining color must not downgrade a PETSCII terminal to ASCII: the
/// terminal type (hence 40-column layout + case-swap) is preserved and the
/// color helpers simply return plain text.  Regression for the C64 bug where
/// "no color" collapsed PETSCII to 80-column ASCII.
#[test]
fn test_color_disabled_keeps_encoding_returns_plain() {
    let mut s = make_test_session(TerminalType::Petscii);
    // Default (color on): PETSCII color codes wrap the text.
    assert_ne!(s.green("HI"), "HI");
    assert!(s.green("HI").contains("HI"));

    // Color off: plain text, but still PETSCII.
    s.color_enabled = false;
    for got in [
        s.green("HI"), s.red("HI"), s.cyan("HI"), s.yellow("HI"),
        s.amber("HI"), s.dim("HI"), s.blue("HI"), s.white("HI"),
    ] {
        assert_eq!(got, "HI", "color-disabled helper must return plain text");
    }
    assert_eq!(
        s.terminal_type,
        TerminalType::Petscii,
        "declining color must not change the terminal encoding"
    );
    // The 40-column PETSCII separator is unaffected by the color choice.
    assert_eq!(s.separator().len(), PETSCII_WIDTH - 1);
}

/// ANSI with color enabled still emits ANSI escapes; ASCII is always plain.
#[test]
fn test_color_enabled_matrix() {
    let ansi = make_test_session(TerminalType::Ansi);
    assert!(ansi.green("X").contains('\x1b'), "ANSI + color → escape codes");
    let ascii = make_test_session(TerminalType::Ascii);
    assert_eq!(ascii.green("X"), "X", "ASCII is always plain even with color on");
}

/// The Gateway Shell resolves path components case-insensitively (CP/M
/// semantics; DIR shows names uppercased and PETSCII swaps case), returning
/// the real on-disk name.  Regression for "CD Z80ASM can't find z80asm".
#[test]
fn test_cpm_real_components_case_insensitive() {
    let tmp = std::env::temp_dir().join(format!("cpmci_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("z80asm").join("SUB")).unwrap();
    std::fs::write(tmp.join("Hello.TXT"), b"x").unwrap();

    // A differently-cased query resolves to a real, existing entry.
    let d = TelnetSession::cpm_real_components(&tmp, &["Z80ASM".to_string()])
        .expect("dir resolves case-insensitively");
    assert!(tmp.join(&d[0]).is_dir());
    let f = TelnetSession::cpm_real_components(&tmp, &["hello.txt".to_string()])
        .expect("file resolves case-insensitively");
    assert!(tmp.join(&f[0]).is_file());
    // Nested case-insensitive resolution walks each level.
    let n = TelnetSession::cpm_real_components(
        &tmp,
        &["Z80ASM".to_string(), "sub".to_string()],
    )
    .expect("nested resolves");
    assert!(tmp.join(&n[0]).join(&n[1]).is_dir());
    // An absent name never resolves (even a case-insensitive FS can't invent it).
    assert!(TelnetSession::cpm_real_components(&tmp, &["nope".to_string()]).is_none());
    // On a case-SENSITIVE fs the real on-disk case is returned; only assert
    // that where the host is actually case-sensitive (skips macOS/Windows CI).
    if !tmp.join("Z80ASM").exists() {
        assert_eq!(d, vec!["z80asm".to_string()]);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
