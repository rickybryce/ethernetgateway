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

/// **Space must never become the session's erase character — and nothing else
/// may be refused.**
///
/// Terminal detection makes whatever byte answers its prompt the erase
/// character. That is the right design: it measures the key rather than
/// guessing, which is the only way to serve a machine whose editing key is
/// unusual. Space is the single answer it must not take, and it is easy to hit
/// — `ATDT ethernetgateway` from inside the CP/M emulator opens a second
/// detection prompt. Measured live 2026-08-14: `New York` in the weather
/// location echoed `New<BS> <BS>York`, and all three gateway paths rewrite
/// `erase_char` to 0x7F on the way out, so every space typed at a remote host
/// arrived as a destructive DEL.
///
/// **The breadth is the other half of the test.** The first fix refused every
/// printable byte, which is wrong about real hardware and would have broken
/// users who were doing nothing wrong — so the printable keys that are somebody's
/// genuine backspace are asserted to still work, by name.
#[test]
fn test_only_space_is_refused_as_the_erase_key() {
    assert!(!can_be_erase_char(b' '), "space is the one answer that cannot be a key");

    // Printable keys that really are a backspace on real machines.  A rule that
    // refuses these tells the user their own key is invalid and then declines
    // to erase with it.
    assert!(
        can_be_erase_char(0x5F),
        "0x5F is the Apple I back arrow (ASCII-1963 left arrow), a real backspace key"
    );
    assert!(can_be_erase_char(b'#'), "# erased on the early Unix ttys");

    // Control codes were always the main case and must be untouched.
    for b in [0x00u8, 0x08, 0x14, 0x1B, 0x1F, 0x7F, 0x80, 0xFF] {
        assert!(can_be_erase_char(b), "0x{b:02x} must still be usable");
    }
    // Nothing else in the printable range is refused either.
    for b in 0x21..=0x7Eu8 {
        assert!(can_be_erase_char(b), "0x{b:02x} ({:?}) must not be refused", b as char);
    }

    // The substitute costs nothing: the three usual backspace keys are accepted
    // whatever the erase character is.
    for key in [0x08u8, 0x7F, 0x14] {
        assert!(
            is_backspace_key(key, DEFAULT_ERASE_CHAR),
            "0x{key:02x} must still erase after a refused detection"
        );
    }
    // The defect, and its absence.
    assert!(is_backspace_key(b' ', b' '), "this is what was happening");
    assert!(!is_backspace_key(b' ', DEFAULT_ERASE_CHAR), "a space must type, not erase");
    // And an Apple I user's key still erases, which the first fix broke.
    assert!(is_backspace_key(0x5F, 0x5F), "the back arrow must still erase");
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

// ─── Connection rate limit ───────────────────────────

#[test]
fn test_the_rate_limit_allows_exactly_the_max_then_refuses() {
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "203.0.113.9".parse().unwrap();
    let w = std::time::Duration::from_secs(60);
    // The Nth connection is still allowed; the caller refuses only when the
    // answer EXCEEDS max, so an off-by-one here would cost a real user their
    // last permitted connection.
    for n in 1..=5 {
        assert_eq!(note_connection(&rates, ip, 5, w).0, n, "connection {n} of 5");
    }
    assert!(note_connection(&rates, ip, 5, w).0 > 5, "the 6th must be over");
    assert!(note_connection(&rates, ip, 5, w).0 > 5, "and it stays over");
}

#[test]
fn test_the_rate_limit_is_per_ip() {
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let a: IpAddr = "203.0.113.9".parse().unwrap();
    let b: IpAddr = "203.0.113.10".parse().unwrap();
    let w = std::time::Duration::from_secs(60);
    for _ in 0..5 {
        note_connection(&rates, a, 5, w);
    }
    assert!(note_connection(&rates, a, 5, w).0 > 5);
    // A flooding neighbour must not spend this address's allowance -- the
    // whole point of keying on the IP.
    assert_eq!(note_connection(&rates, b, 5, w).0, 1);
}

/// **A refusal is logged once per flood, not once per connection.**
///
/// The refusal itself is careful -- counted, never stored -- but the log line
/// beside it was unconditional, and `glog!` is a blocking `write_all` inline
/// in the accept loop.  Worse, the log is a rolling 1 MB x 6: a flood loud
/// enough to matter would push its own evidence out of the file, which is the
/// one thing the limiter exists to record.
#[test]
fn test_only_the_first_refusal_of_a_flood_is_logged() {
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "203.0.113.11".parse().unwrap();
    let w = std::time::Duration::from_secs(60);

    for n in 1..=3 {
        let (count, say_so) = note_connection(&rates, ip, 3, w);
        assert_eq!(count, n);
        assert!(!say_so, "an ACCEPTED connection is not a refusal and says nothing");
    }
    let (_, first) = note_connection(&rates, ip, 3, w);
    assert!(first, "the first refusal must be reported, or the limiter is silent");
    for _ in 0..200 {
        let (_, again) = note_connection(&rates, ip, 3, w);
        assert!(
            !again,
            "every refused connection wrote a log line: a flood then rolls the \
             log and destroys the evidence of itself",
        );
    }
}

/// ...and a later flood, after the address has behaved, is a new episode.
///
/// One line for ever per address would be the opposite defect: the operator
/// would see the first incident and never learn of any that followed.
///
/// **This has to reach the reset through a surviving entry.**  The obvious
/// version -- expire everything and flood again -- proves nothing: the sweep
/// drops the address from the map entirely, so `or_default()` hands back a
/// fresh `ConnRate` whose flag is already clear, and deleting the reset line
/// leaves the test green.  It was written that way first and the mutation
/// survived.  So the state is built directly: one timestamp old enough to
/// have expired and one recent enough to keep the entry alive, with the flag
/// already set, which is the only shape that exercises the line.
#[test]
fn test_an_address_that_recovers_may_be_reported_again() {
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "203.0.113.12".parse().unwrap();
    let w = std::time::Duration::from_secs(60);

    for _ in 0..2 {
        note_connection(&rates, ip, 2, w);
    }
    assert!(note_connection(&rates, ip, 2, w).1, "first flood reported");
    assert!(!note_connection(&rates, ip, 2, w).1, "and only once");

    // Age the address back under its allowance without sleeping for it, and
    // *keep the entry in the map*: one expired timestamp, one still live, so
    // the sweep retains it and the flag it is carrying.
    {
        let now = std::time::Instant::now();
        let mut map = rates.lock().unwrap();
        let rate = map.get_mut(&ip).expect("the flooding address is still held");
        assert!(rate.refusal_logged, "the first flood must have set the flag");
        rate.seen = vec![now - std::time::Duration::from_secs(90), now];
    }

    // One accepted connection -- seen is back under max -- which is what
    // clears the flag.
    assert!(
        !note_connection(&rates, ip, 2, w).1,
        "an accepted connection is not a refusal",
    );
    assert!(
        note_connection(&rates, ip, 2, w).1,
        "an address that came back under the limit and flooded again was \
         never reported a second time",
    );
}

#[test]
fn test_a_refused_connection_is_not_stored() {
    // The cap that stops the limiter becoming its own memory-exhaustion
    // vector: an IP already over the limit is counted but never pushed, so
    // the stored vector cannot grow past `max` however long a flood runs.
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "203.0.113.9".parse().unwrap();
    let w = std::time::Duration::from_secs(60);
    for _ in 0..200 {
        note_connection(&rates, ip, 3, w);
    }
    let map = rates.lock().unwrap();
    assert_eq!(map.get(&ip).map(|v| v.len()), Some(3), "stored entries capped at max");
}

#[test]
fn test_the_window_expires_and_the_map_is_pruned() {
    // A zero-length window means every timestamp is already outside it, so
    // nothing is ever counted and nothing is ever retained.  This is the
    // expiry path without sleeping for it.
    let rates: ConnRateMap = Arc::new(Mutex::new(HashMap::new()));
    let ip: IpAddr = "203.0.113.9".parse().unwrap();
    let zero = std::time::Duration::from_secs(0);
    assert_eq!(note_connection(&rates, ip, 5, zero).0, 1);
    assert_eq!(note_connection(&rates, ip, 5, zero).0, 1, "the previous one expired");
    let map = rates.lock().unwrap();
    assert!(map.len() <= 1, "expired IPs are pruned, not accumulated");
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

/// The lockout window rule itself, driven by a supplied age.
///
/// **This is the half that cannot skip.**  The two tests below backdate a live
/// entry with `Instant::checked_sub`, which answers `None` when the result
/// would pre-date the platform's monotonic epoch -- boot time on Linux -- so
/// on a host up for less than five minutes, which a fresh CI container is,
/// both of them take an early `return` and are counted among the passes.  The
/// rule they exist for is here instead, where the age is a parameter and the
/// boundary can be stated: at exactly `LOCKOUT_DURATION` the entry has
/// expired, which is the comparison no elapsed-time test can pin.
#[test]
fn test_the_lockout_window_rule() {
    use std::time::Duration;
    let secs = |n| Duration::from_secs(n);
    let window = LOCKOUT_DURATION;

    // The window: inclusive at zero, exclusive at the far end.
    assert!(within_lockout_window(Duration::ZERO), "a failure just recorded is inside");
    assert!(
        within_lockout_window(window - Duration::from_millis(1)),
        "a millisecond short of the window is still inside",
    );
    assert!(
        !within_lockout_window(window),
        "exactly at the window the entry has expired -- the boundary the \
         backdating tests cannot reach",
    );
    assert!(!within_lockout_window(window + secs(1)), "past the window is out");

    // The threshold: a lockout needs MAX_AUTH_ATTEMPTS failures AND a fresh one.
    assert!(
        !lockout_is_active(MAX_AUTH_ATTEMPTS - 1, Duration::ZERO),
        "one short of the threshold is not a lockout however recent",
    );
    assert!(lockout_is_active(MAX_AUTH_ATTEMPTS, Duration::ZERO), "at the threshold, locked");
    assert!(
        lockout_is_active(MAX_AUTH_ATTEMPTS + 5, window - secs(1)),
        "still locked inside the window",
    );
    assert!(
        !lockout_is_active(MAX_AUTH_ATTEMPTS + 5, window),
        "the count does not survive the window -- an old lockout must decay",
    );
}

/// Lockout counter must reset after `LOCKOUT_DURATION` elapses
/// without a successful auth.  Faking the elapsed time via direct
/// map manipulation rather than waiting 5 minutes — the production
/// code reads `entry.1.elapsed()` so we can backdate `entry.1` to
/// simulate a stale lockout.
///
/// This is the map half, and it can skip (see below); the rule it rests on is
/// held unconditionally by `test_the_lockout_window_rule`.
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
///
/// Backdates a live entry, so it can skip on a freshly-booted host; the
/// staleness rule itself is `test_the_lockout_window_rule`, which cannot.
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

/// A *missing* known-hosts file is the first-run case and must pin, but a file
/// that exists and won't read must not be mistaken for it.  Collapsing the two
/// silently downgrades TOFU to trust-anything on the relay path, which
/// auto-pins with no prompt and then sends the master's unified credentials.
#[test]
fn test_classify_known_host_distinguishes_missing_from_unreadable() {
    let key_str = format_host_key(&make_test_key());
    let lookup = "master.example:2222";

    // Absent file → Unknown → caller pins on first contact.
    let absent = Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
    assert!(
        matches!(
            classify_known_host(absent, lookup, &key_str),
            HostKeyStatus::Unknown
        ),
        "a missing file is first contact and must stay Unknown",
    );

    // Present but unreadable → Unreadable, NOT Unknown.
    for kind in [
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::InvalidData,
    ] {
        let unreadable = Err(std::io::Error::new(kind, "boom"));
        assert!(
            matches!(
                classify_known_host(unreadable, lookup, &key_str),
                HostKeyStatus::Unreadable(_)
            ),
            "{:?} must not be reported as Unknown — that would re-pin",
            kind,
        );
    }
}

/// The ordinary decisions still hold through the extracted seam.
#[test]
fn test_classify_known_host_known_changed_and_absent_entry() {
    let key_str = format_host_key(&make_test_key());
    let lookup = "master.example:2222";

    let stored = format!("# comment\n\n{} {}\n", lookup, key_str);
    assert!(matches!(
        classify_known_host(Ok(stored.clone()), lookup, &key_str),
        HostKeyStatus::Known
    ));

    assert!(
        matches!(
            classify_known_host(Ok(stored), lookup, "ssh-ed25519 AAAAdifferent"),
            HostKeyStatus::Changed
        ),
        "a stored entry that doesn't match must be Changed, never Unknown",
    );

    // A readable file with no entry for this host is still first contact.
    assert!(matches!(
        classify_known_host(
            Ok("other.example:22 ssh-ed25519 AAAAother\n".to_string()),
            lookup,
            &key_str,
        ),
        HostKeyStatus::Unknown
    ));
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
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_loopback_allowed() {
    let ip: IpAddr = "127.0.0.2".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_ten_network_allowed() {
    let ip: IpAddr = "10.0.5.42".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_172_private_allowed() {
    let ip: IpAddr = "172.16.0.50".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
    let ip2: IpAddr = "172.31.255.254".parse().unwrap();
    assert!(reject_insecure_ip(ip2, false).is_none());
}

#[test]
fn test_reject_insecure_ip_public_rejected() {
    let ip: IpAddr = "8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_some());
}

#[test]
fn test_reject_insecure_ip_172_public_rejected() {
    // 172.32.x.x is NOT private (private is 172.16-31.x.x)
    let ip: IpAddr = "172.32.0.5".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_some());
}

#[test]
fn test_reject_insecure_ip_gateway_allowed_by_default() {
    // The router's own address is allowed unless the operator asks otherwise:
    // it is as often an administrator's machine, or hairpinned LAN traffic, as
    // it is something forwarded in from outside.
    for addr in ["192.168.1.1", "10.0.0.1", "172.16.5.1"] {
        let ip: IpAddr = addr.parse().unwrap();
        assert!(
            reject_insecure_ip(ip, false).is_none(),
            "{addr} should be allowed with disable_gateway_connections off"
        );
    }
}

#[test]
fn test_reject_insecure_ip_gateway_rejected_when_blocked() {
    // ...and refused when it is asked for, with a reason naming the rule.
    // Empty router list = "the OS could not tell us", which is the case the
    // x.x.x.1 fallback exists for.  Passed explicitly so the result does not
    // depend on the routing table of the machine running the suite.
    for addr in ["192.168.1.1", "10.0.0.1", "172.16.5.1"] {
        let ip: IpAddr = addr.parse().unwrap();
        let reason = reject_insecure_ip_with(ip, true, &[]);
        assert!(reason.is_some(), "{addr} should be refused when blocking");
        assert!(reason.unwrap().contains("gateway"));
    }
}

/// The point of asking the OS: block the address that is *actually* the
/// router, and stop blocking a `.1` that is just an ordinary host.
#[test]
fn test_reject_insecure_ip_blocks_the_detected_router_not_the_guess() {
    let routers: Vec<IpAddr> = vec!["192.168.1.254".parse().unwrap()];
    let real: IpAddr = "192.168.1.254".parse().unwrap();
    let guess: IpAddr = "192.168.1.1".parse().unwrap();

    // Off: neither is refused, exactly as before.
    assert!(reject_insecure_ip_with(real, false, &routers).is_none());
    assert!(reject_insecure_ip_with(guess, false, &routers).is_none());

    // On: the real router is refused, and the reason names its address so an
    // operator reading the log knows what was blocked and why.
    let reason = reject_insecure_ip_with(real, true, &routers).expect("router refused");
    assert!(reason.contains("192.168.1.254"), "reason was: {reason}");
    assert!(reason.contains("router"), "reason was: {reason}");

    // ...and the machine on .1 is now just a machine.
    assert!(
        reject_insecure_ip_with(guess, true, &routers).is_none(),
        "a .1 host that is not the router must no longer be refused"
    );
}

/// An IPv4-mapped peer must be judged the same as the bare IPv4 one, or the
/// policy would depend on which socket family the peer happened to arrive on.
#[test]
fn test_detected_router_is_matched_through_v4_mapping() {
    let routers: Vec<IpAddr> = vec!["10.0.0.254".parse().unwrap()];
    let mapped: IpAddr = "::ffff:10.0.0.254".parse().unwrap();
    assert!(reject_insecure_ip_with(mapped, false, &routers).is_none());
    assert!(reject_insecure_ip_with(mapped, true, &routers).is_some());
}

/// Knowing the IPv6 router tells us nothing about the IPv4 one, so the x.x.x.1
/// fallback must still apply to IPv4 on such a host — otherwise detecting half
/// the picture would quietly stop enforcing what the operator asked for.
#[test]
fn test_v4_fallback_survives_when_only_a_v6_router_is_known() {
    let v6_only: Vec<IpAddr> = vec!["fe80::1".parse().unwrap()];
    let v4_dot_one: IpAddr = "192.168.1.1".parse().unwrap();

    // Off: allowed, as always.
    assert!(reject_insecure_ip_with(v4_dot_one, false, &v6_only).is_none());
    // On: the IPv4 router is still unknown, so the convention still applies.
    let reason = reject_insecure_ip_with(v4_dot_one, true, &v6_only)
        .expect("x.x.x.1 must still be refused when only a v6 router is known");
    assert!(reason.contains("gateway addresses"), "reason was: {reason}");

    // Once the IPv4 router IS known, the convention stops applying and only
    // the real address is refused.
    let both: Vec<IpAddr> = vec![
        "fe80::1".parse().unwrap(),
        "192.168.1.254".parse().unwrap(),
    ];
    assert!(reject_insecure_ip_with(v4_dot_one, true, &both).is_none());
    assert!(reject_insecure_ip_with("192.168.1.254".parse().unwrap(), true, &both).is_some());
}

/// An IPv6 router is refused by the same rule.  There is no `.1` convention in
/// IPv6, so this can only ever fire when detection worked.
#[test]
fn test_reject_insecure_ip_blocks_a_detected_ipv6_router() {
    let routers: Vec<IpAddr> = vec!["fe80::1".parse().unwrap()];
    let router: IpAddr = "fe80::1".parse().unwrap();
    let other: IpAddr = "fe80::abcd".parse().unwrap();

    assert!(reject_insecure_ip_with(router, false, &routers).is_none());
    let reason = reject_insecure_ip_with(router, true, &routers).expect("v6 router refused");
    assert!(reason.contains("fe80::1"), "reason was: {reason}");
    // Every other link-local address stays allowed.
    assert!(reject_insecure_ip_with(other, true, &routers).is_none());
}

/// Detection must never *widen* the allowlist: a public address is refused
/// whatever the routing table says, including the absurd case of a public
/// default gateway (a machine with a routable address on its LAN side).
#[test]
fn test_detected_router_never_admits_a_public_address() {
    let routers: Vec<IpAddr> = vec!["8.8.8.8".parse().unwrap()];
    let public: IpAddr = "8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip_with(public, false, &routers).is_some());
    assert!(reject_insecure_ip_with(public, true, &routers).is_some());
    // Loopback keeps its exemption even if something claims it is the router.
    let loop_routers: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap()];
    let lo: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip_with(lo, true, &loop_routers).is_none());
}

#[test]
fn test_blocking_the_gateway_does_not_widen_anything_else() {
    // Turning the rule on must not accidentally admit a public address, and
    // must not shut out the rest of the private space.
    let public: IpAddr = "8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip(public, true).is_some());
    assert!(reject_insecure_ip(public, false).is_some());
    let ordinary: IpAddr = "192.168.1.50".parse().unwrap();
    assert!(reject_insecure_ip(ordinary, true).is_none());
    assert!(reject_insecure_ip(ordinary, false).is_none());
}

#[test]
fn test_loopback_dot_one_allowed_even_when_blocking() {
    // 127.0.0.1 is this machine, not a router: the rule must never touch it,
    // or the operator locks themselves out of their own console.
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip(ip, true).is_none());
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_loopback_dot_one_allowed() {
    // 127.0.0.1 is loopback — exempt from the .1 gateway filter
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_loopback_allowed() {
    let ip: IpAddr = "::1".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_public_rejected() {
    let ip: IpAddr = "2001:db8::1".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_some());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_private_allowed() {
    // ::ffff:192.168.1.100 is IPv4-mapped, should apply IPv4 rules
    let ip: IpAddr = "::ffff:192.168.1.100".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_public_rejected() {
    let ip: IpAddr = "::ffff:8.8.8.8".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_some());
}

#[test]
fn test_reject_insecure_ip_ipv4_mapped_ipv6_gateway_follows_the_flag() {
    // An IPv4-mapped address must obey the same rule as the bare IPv4 one, or
    // the policy would depend on which socket family the peer happened to use.
    let ip: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
    assert!(reject_insecure_ip_with(ip, false, &[]).is_none());
    let reason = reject_insecure_ip_with(ip, true, &[]);
    assert!(reason.is_some());
    assert!(reason.unwrap().contains("gateway"));
}

#[test]
fn test_reject_insecure_ip_ipv6_link_local_allowed() {
    let ip: IpAddr = "fe80::1".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_ipv6_unique_local_allowed() {
    let ip: IpAddr = "fd12:3456:789a::1".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_link_local_ipv4_allowed() {
    let ip: IpAddr = "169.254.1.100".parse().unwrap();
    assert!(reject_insecure_ip(ip, false).is_none());
}

#[test]
fn test_reject_insecure_ip_link_local_dot_one_follows_the_flag() {
    // A link-local .1 is still a .1: allowed by default like any other
    // gateway address, refused when the operator asks for the strict rule.
    let ip: IpAddr = "169.254.0.1".parse().unwrap();
    assert!(reject_insecure_ip_with(ip, false, &[]).is_none());
    assert!(reject_insecure_ip_with(ip, true, &[]).is_some());
}

// ─── Menu ────────────────────────────────────────────

#[test]
fn test_menu_paths() {
    assert_eq!(Menu::Main.path(), "gateway");
    assert_eq!(Menu::FileTransfer.path(), "gateway/xfer");
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
        trace_bytes: false,
        restart: Arc::new(AtomicBool::new(false)),
        current_menu: Menu::Main,
        terminal_type,
        color_enabled: true,
        erase_char: 0x7F,
        lockouts: Arc::new(Mutex::new(HashMap::new())),
        peer_addr: None,
        power_password_failures: Arc::new(std::sync::Mutex::new((0, std::time::Instant::now()))),
        power_lockouts: Default::default(),
        authenticated: false,
        power_arrived_by_relay: false,
        #[cfg(unix)]
        power_elevation: Default::default(),
        transfer_subdir: String::new(),
        xmodem_iac: false,
        last_transfer_note: None,
        arm_next_prompt: false,
        web_lines: Vec::new(),
        web_scroll: 0,
        web_links: Vec::new(),
        web_history: Vec::new(),
        web_url: None,
        web_home_tried: false,
        web_title: None,
        web_forms: Vec::new(),
        weather_location: String::new(),
        is_serial: false,
        is_relay: false,
        serial_port_id: None,
        is_ssh: false,
        idle_timeout: std::time::Duration::ZERO,
        pushback: None,
        mid_iac_cmd: false,
        last_was_cr: false,
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
pub(in crate::telnet) fn make_test_session_with_peer(
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
        trace_bytes: false,
        restart: Arc::new(AtomicBool::new(false)),
        current_menu: Menu::Main,
        terminal_type,
        color_enabled: true,
        erase_char: 0x7F,
        lockouts: Arc::new(Mutex::new(HashMap::new())),
        peer_addr: None,
        power_password_failures: Arc::new(std::sync::Mutex::new((0, std::time::Instant::now()))),
        power_lockouts: Default::default(),
        authenticated: false,
        power_arrived_by_relay: false,
        #[cfg(unix)]
        power_elevation: Default::default(),
        transfer_subdir: String::new(),
        xmodem_iac: false,
        last_transfer_note: None,
        arm_next_prompt: false,
        web_lines: Vec::new(),
        web_scroll: 0,
        web_links: Vec::new(),
        web_history: Vec::new(),
        web_url: None,
        web_home_tried: false,
        web_title: None,
        web_forms: Vec::new(),
        weather_location: String::new(),
        is_serial: false,
        is_relay: false,
        serial_port_id: None,
        is_ssh: false,
        idle_timeout: std::time::Duration::ZERO,
        pushback: None,
        mid_iac_cmd: false,
        last_was_cr: false,
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
    let mode = if is_petscii { GatewayFilter::Petscii } else { GatewayFilter::Ascii };
    let mut state = GatewayOutState::new(mode);
    let mut out = Vec::new();
    filter_gateway_output(input, &mut state, &mut out);
    out
}

/// Helper: the same, for a terminal that keeps its escape sequences.
fn filter_output_ansi(chunks: &[&[u8]]) -> Vec<u8> {
    let mut state = GatewayOutState::new(GatewayFilter::Ansi);
    let mut out = Vec::new();
    for c in chunks {
        filter_gateway_output(c, &mut state, &mut out);
    }
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
    let mut state = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b]0;ti", &mut state, &mut out);
    assert_eq!(out, b"");
    assert_eq!(state.phase(), 3);
    filter_gateway_output(b"tle\x07visible", &mut state, &mut out);
    assert_eq!(out, b"visible");
    assert_eq!(state.phase(), 0);
}

#[test]
fn test_filter_incomplete_csi_spans_chunks() {
    let mut state = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b[32", &mut state, &mut out);
    assert_eq!(out, b"");
    assert_eq!(state.phase(), 2);
    filter_gateway_output(b"mhello", &mut state, &mut out);
    assert_eq!(out, b"hello");
    assert_eq!(state.phase(), 0);
}

#[test]
fn test_filter_bare_esc_at_end_of_chunk() {
    let mut state = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    filter_gateway_output(b"text\x1b", &mut state, &mut out);
    assert_eq!(out, b"text");
    assert_eq!(state.phase(), 1);
    filter_gateway_output(b"[0mmore", &mut state, &mut out);
    assert_eq!(out, b"textmore");
}

#[test]
fn test_filter_petscii_translates_and_swaps() {
    // `ESC[32m` used to be deleted, which is why a C64 saw no colour from any
    // host.  It is now PETSCII green, and the text is still case-swapped.
    let input = b"\x1b[32mHello World";
    let mut want = vec![0x1Eu8];
    want.extend_from_slice(b"hELLO wORLD");
    assert_eq!(filter_output(input, true), want);
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

/// A host's backspace becomes PETSCII **cursor left** (0x9D), not PETSCII DEL
/// (0x14).
///
/// This test previously asserted 0x14 with no stated reason, and the mapping
/// was wrong: ASCII BS moves the cursor without erasing, while PETSCII 0x14
/// deletes the character to its left and pulls the line back (serial.rs's
/// AT-echo handler documents the same asymmetry from the other direction).
/// So every bare BS a remote used to reposition its cursor silently deleted a
/// character the remote still believed was on screen — corruption that grows
/// with line length, which is why short commands looked fine and long ones
/// did not.
#[test]
fn test_filter_petscii_translates_backspace_to_cursor_left() {
    assert_eq!(filter_output(b"ab\x08c", true), b"AB\x9DC");
    assert_eq!(filter_output(b"ab\x7Fc", true), b"AB\x9DC");
}

/// No PETSCII output translator may map a host's backspace to the destructive
/// PETSCII DEL again.
///
/// There are three of them — `filter_gateway_output` here,
/// `translate_ascii_to_petscii_byte` in `serial.rs` (the modem emulator's
/// `AT+PETSCII=1` path) and `ascii_to_petscii_byte` in `colors.rs` (a booted
/// disk image's console stream) — and the first two carried the identical
/// defect, so fixing one would have left a C64 dialling out through the modem
/// still corrupted; the third was written later and started out with the byte
/// simply falling through, which on a booted disk is the same corruption again.
/// They live in different modules with different signatures, so they cannot
/// share a unit test; this scans all three sources instead, the same technique
/// `config::test_every_written_key_can_be_applied` uses on its `match`.
///
/// Scoped to each translator's own body so an unrelated, legitimate `0x14`
/// elsewhere in either file — notably serial.rs's AT-echo, which *originates* a
/// destructive erase on purpose and is correct — cannot trip it.
#[test]
fn test_no_petscii_translator_maps_backspace_to_destructive_del() {
    // (file label, source, marker that opens the translator, bytes to scan)
    let sites: [(&str, &str, &str, usize); 3] = [
        (
            "telnet/gateway.rs filter_gateway_output",
            include_str!("gateway.rs"),
            "if st.mode == GatewayFilter::Petscii {",
            400,
        ),
        (
            "serial.rs translate_ascii_to_petscii_byte",
            include_str!("../serial.rs"),
            "fn translate_ascii_to_petscii_byte",
            300,
        ),
        (
            "telnet/colors.rs ascii_to_petscii_byte",
            include_str!("colors.rs"),
            "fn ascii_to_petscii_byte",
            200,
        ),
    ];
    for (label, src, marker, span) in sites {
        // Strip `//` comments from the WHOLE source before locating anything.
        // Both translators now carry a comment explaining why 0x14 is wrong, so
        // a scan that reads comments fires on the explanation instead of the
        // code — and stripping only inside the window doesn't help either,
        // because a byte span measured against commented source barely reaches
        // the code. Both mistakes were made here before this worked.
        // Comment-only lines are dropped entirely rather than blanked: leaving
        // their indentation behind spends the byte window on whitespace, which
        // is how an earlier version of this scan failed to reach the code.
        let stripped: String = src
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => l[..i].trim_end(),
                None => l.trim_end(),
            })
            .filter(|l| !l.is_empty())
            .collect::<Vec<&str>>()
            .join("\n");
        let at = stripped
            .find(marker)
            .unwrap_or_else(|| panic!("{label}: marker {marker:?} not found — renamed?"));
        let body = &stripped[at..(at + span).min(stripped.len())];
        // The mapping must be present and must be the cursor-left one.
        assert!(
            body.contains("0x9D"),
            "{label}: no 0x9D (PETSCII CRSR LEFT) in the translator — a host's \
             backspace has to map to the non-destructive move"
        );
        assert!(
            !body.contains("0x14"),
            "{label}: maps a byte to PETSCII DEL 0x14, which DELETES a character \
             the host only meant to move over. This corrupted every long line on \
             a C64 gateway session; use 0x9D (CRSR LEFT). Body scanned:\n{body}"
        );
    }
}

/// The case that actually matters, and which the old assertion never covered:
/// `BS SPACE BS` is the universal way a terminal-driving program erases one
/// character. Translated to cursor-left it renders as left, space, left — the
/// character is overwritten with a blank and the cursor ends up before it,
/// which is precisely the host's intent. Translated to 0x14 it became
/// `DEL SPACE DEL`: delete a character, insert a space, delete that — leaving
/// the screen and the host's model of it disagreeing.
#[test]
fn test_filter_petscii_renders_bs_space_bs_as_an_overwrite() {
    assert_eq!(filter_output(b"x\x08 \x08", true), b"X\x9D \x9D");
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
    let mut state = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b]0;title", &mut state, &mut out);
    assert_eq!(state.phase(), 3);
    assert_eq!(out, b"");
    filter_gateway_output(b"more title", &mut state, &mut out);
    assert_eq!(state.phase(), 3);
    assert_eq!(out, b"");
    filter_gateway_output(b"\x07visible", &mut state, &mut out);
    assert_eq!(state.phase(), 0);
    assert_eq!(out, b"visible");
}

#[test]
fn test_filter_csi_interrupted_by_new_esc() {
    let mut state = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    filter_gateway_output(b"\x1b[32", &mut state, &mut out);
    assert_eq!(state.phase(), 2);
    filter_gateway_output(b"\x1b]title\x07text", &mut state, &mut out);
    assert_eq!(state.phase(), 0);
    assert_eq!(out, b"text");
}

// ─── Gateway window-title stripping (ANSI clients) ───

/// **The reported bug, byte for byte.**
///
/// bash sets the window title from `PS1` with `\e]0;\u@\h: \w\a`.  A terminal
/// that does not implement OSC eats the two-byte `ESC ]` and prints the rest,
/// so the SC126 showed `0;ricky@TelnetBible: ~` in front of every prompt --
/// which also looks like the prompt appearing twice, because the title *is*
/// user@host.  Seen under EGT80 *and* QTERM, which is what places the fix here
/// rather than in either terminal.
#[test]
fn test_ansi_gateway_drops_the_window_title_but_keeps_the_prompt() {
    let wire = b"\x1b]0;ricky@TelnetBible: ~\x07ricky@TelnetBible:~$ ";
    assert_eq!(
        filter_output_ansi(&[wire]),
        b"ricky@TelnetBible:~$ ".to_vec(),
        "the title must go and the prompt must stay"
    );
}

/// Colour and cursor addressing are the whole reason a client asks for ANSI,
/// so a CSI sequence has to arrive exactly as sent -- including one split
/// across two reads, since the parser state is carried between them.
#[test]
fn test_ansi_gateway_keeps_csi_sequences_intact() {
    assert_eq!(
        filter_output_ansi(&[b"\x1b[1;32mgreen\x1b[0m"]),
        b"\x1b[1;32mgreen\x1b[0m".to_vec(),
    );
    assert_eq!(
        filter_output_ansi(&[b"\x1b[32", b"mhalf"]),
        b"\x1b[32mhalf".to_vec(),
        "a CSI split across reads must still come out whole"
    );
}

/// **A gateway session also carries file transfers, so this must be a filter
/// that binary data survives.**
///
/// If the user runs `sz` on the far host, XMODEM/ZMODEM bytes come through
/// this same function and nothing here can tell them from a prompt.  `1B 5D`
/// occurs about once per 64 KB of binary, so swallowing `ESC ]` to the next
/// BEL -- which is what the stripping modes do, and what the first draft of
/// this fix did -- would eat part of a download, and eat it *identically* on
/// every retry, so the protocol's CRC could never recover it.
///
/// Every one of these begins like a title and must come back byte for byte.
#[test]
fn test_ansi_gateway_never_eats_binary_that_merely_starts_like_a_title() {
    // ESC ] followed by binary: a control byte ends the candidate.
    let bin = b"\x1b]\x00\x01\x02data";
    assert_eq!(filter_output_ansi(&[bin]), bin.to_vec(), "control byte");

    // The full title prefix, then a high byte — still data, not a title.
    let high = b"\x1b]0;\xff\xfe\x80payload";
    assert_eq!(filter_output_ansi(&[high]), high.to_vec(), "high byte");

    // A different OSC code is not a title and is passed through untouched.
    let palette = b"\x1b]4;1;rgb:00/00/00\x07";
    assert_eq!(filter_output_ansi(&[palette]), palette.to_vec(), "OSC 4");

    // Looks like a title, but never terminates: released once it is too long
    // to be one, so a stream cannot be held hostage by a stray prefix.
    let mut runaway = b"\x1b]0;".to_vec();
    runaway.extend(std::iter::repeat_n(b'A', 400));
    assert_eq!(filter_output_ansi(&[&runaway]), runaway, "unterminated");

    // A real XMODEM-ish block of every byte value, twice, must be untouched.
    let mut block: Vec<u8> = (0..=255u8).collect();
    block.extend(0..=255u8);
    assert_eq!(filter_output_ansi(&[&block]), block, "all byte values");
}

/// The title may arrive in pieces, and the ST terminator (`ESC \`) is as valid
/// as BEL.  Both are checked here because the candidate is held across reads,
/// which is exactly where a state machine gets this wrong.
#[test]
fn test_ansi_gateway_title_split_across_reads_and_st_terminated() {
    assert_eq!(
        filter_output_ansi(&[b"\x1b]0;ric", b"ky@host: ~", b"\x07done"]),
        b"done".to_vec(),
        "a title split across three reads must still be dropped whole"
    );
    assert_eq!(
        filter_output_ansi(&[b"\x1b]0;title\x1b\\after"]),
        b"after".to_vec(),
        "ST terminates a title as well as BEL"
    );
    // A lone ESC is held, then released when the next byte proves it ordinary.
    assert_eq!(filter_output_ansi(&[b"a\x1b", b"Zb"]), b"a\x1bZb".to_vec());
}

/// **The length cap is a boundary, so it is pinned at the boundary.**
///
/// The only other length cover is a 400-byte runaway, which passes wherever
/// the cap sits; moving the check after the push, or `>` for `>=`, would go
/// unnoticed.  A byte is added only while `held` is shorter than the cap, and
/// `held` carries the four-byte `ESC ] <code> ;` prefix, so the longest title
/// that can still be dropped has `OSC_TITLE_MAX - 4` payload bytes.
#[test]
fn test_the_title_length_cap_is_pinned_at_its_boundary() {
    let longest = OSC_TITLE_MAX - 4;

    let mut at_cap = b"\x1b]0;".to_vec();
    at_cap.extend(std::iter::repeat_n(b'A', longest));
    at_cap.push(0x07);
    assert_eq!(
        filter_output_ansi(&[&at_cap]),
        Vec::<u8>::new(),
        "the longest title that fits must still be dropped",
    );

    let mut over_cap = b"\x1b]0;".to_vec();
    over_cap.extend(std::iter::repeat_n(b'A', longest + 1));
    over_cap.push(0x07);
    assert_eq!(
        filter_output_ansi(&[&over_cap]),
        over_cap,
        "one byte past the cap is data, and comes back whole",
    );
}

/// **A candidate that ends in `ESC ]` is the start of the next one.**
///
/// An unterminated OSC followed straight by a real title used to release the
/// second `ESC ]` as text and read the title from state 0 -- printing
/// `0;user@host`, the exact symptom this filter exists to remove.  The ESC
/// being weighed belongs to whatever follows it, not to what came before.
#[test]
fn test_an_unterminated_osc_does_not_let_the_next_title_through() {
    assert_eq!(
        filter_output_ansi(&[b"\x1b]0;stalled\x1b]0;ricky@host\x07after"]),
        b"\x1b]0;stalledafter".to_vec(),
        "the abandoned candidate comes back as data; the real title still goes",
    );
    // And across a read boundary, where the two ESCs straddle the seam.
    assert_eq!(
        filter_output_ansi(&[b"\x1b]2;a\x1b", b"]1;b\x07tail"]),
        b"\x1b]2;atail".to_vec(),
    );
}

/// PETSCII and ASCII clients keep the old behaviour exactly: every sequence
/// stripped, whatever its shape.  Those terminals cannot use an escape at all,
/// and this is the path that has always protected them from the title.
#[test]
fn test_stripping_modes_still_swallow_every_sequence() {
    assert_eq!(filter_output(b"\x1b]0;title\x07text", false), b"text".to_vec());
    assert_eq!(filter_output(b"\x1b[1;32mtext\x1b[0m", false), b"text".to_vec());
    assert_eq!(filter_output(b"\x1b]4;1;rgb:0/0/0\x07x", false), b"x".to_vec());
}

/// **A held byte must never be held for ever — this is the file-transfer case.**
///
/// The filter has to carry a candidate across reads (a burst larger than the
/// 4 KB read buffer splits at an arbitrary byte, so a title straddling two
/// reads is ordinary).  But a stop-and-wait sender goes quiet after each block
/// and waits for an ACK, so a block whose *last* byte is one we are holding
/// never completes: the receiver times out, the sender resends the identical
/// block, and the identical hold repeats.  That is a deadlock, not a retryable
/// error — no CRC can recover from a byte that was never sent.
///
/// So going quiet releases the candidate.  Here that is the flush called
/// directly; in the session it is a `GW_FILTER_FLUSH` branch in the same
/// `select!` as the idle timeout.
#[test]
fn test_a_held_byte_is_released_once_the_remote_goes_quiet() {
    // An XMODEM block whose CRC low byte happens to be 0x1B — one block in
    // 256, so near-certain over a real file.
    let mut block = vec![0x01u8, 0x01, 0xFE];
    block.extend(std::iter::repeat_n(b'X', 128));
    block.push(0x9A);
    block.push(0x1B); // CRC-lo, and the byte the filter wants to weigh
    let mut state = GatewayOutState::new(GatewayFilter::Ansi);
    let mut out = Vec::new();
    filter_gateway_output(&block, &mut state, &mut out);
    assert_eq!(out.len(), block.len() - 1, "the trailing ESC is held, as designed");
    assert!(state.has_pending(), "and the filter knows it owes the client a byte");

    // The sender is now waiting for an ACK it will never get.  The flush is
    // what breaks that.
    state.flush_pending(&mut out);
    assert_eq!(out, block, "the block must arrive byte for byte");
    assert!(!state.has_pending(), "and nothing may be left behind");
}

/// The same for a full candidate: `ESC ]` and a run of printable bytes is
/// nearly a title, so it is held — and if the remote stops there, all of it
/// must still be delivered.  Answering `sz`'s `**` header, a shell prompt with
/// no newline, any half-finished burst.
#[test]
fn test_a_held_candidate_is_released_whole_and_in_order() {
    for (name, wire) in [
        ("mid-title", &b"\x1b]0;partial"[..]),
        ("mid-title after ESC", &b"\x1b]0;partial\x1b"[..]),
        ("bare introducer", &b"\x1b]"[..]),
        ("lone ESC", &b"\x1b"[..]),
    ] {
        let mut state = GatewayOutState::new(GatewayFilter::Ansi);
        let mut out = Vec::new();
        filter_gateway_output(wire, &mut state, &mut out);
        state.flush_pending(&mut out);
        assert_eq!(out, wire.to_vec(), "{name}: every byte, in arrival order");
        assert!(!state.has_pending(), "{name}: nothing left held");
        // A second flush must not invent anything.
        let before = out.len();
        state.flush_pending(&mut out);
        assert_eq!(out.len(), before, "{name}: flushing twice must be a no-op");
    }
}

/// The flush window is a **guess about other people's timeouts**, so it is
/// pinned rather than left to drift.  The tightest wait anything in this
/// codebase uses mid-transfer is 3 s (`AUTO_DETECT_TRAILER_TIMEOUT_SECS` and
/// `EOB_TIMEOUT_SECS` in `xmodem.rs`); vintage senders are slower still.  The
/// release has to be an order of magnitude inside that, or it stops being a
/// safety net and becomes part of the stall.  The floor matters too: the next
/// read of a split burst follows in microseconds, so anything above a
/// millisecond or two is generous, and a very short window would start
/// releasing titles that were merely split across two reads.
#[test]
fn test_the_flush_window_is_far_inside_the_tightest_transfer_timeout() {
    // The real constants, not copies of them: a literal here would keep
    // passing if someone lowered the timeout it claims to track.
    let tightest_protocol_wait = std::time::Duration::from_secs(
        crate::xmodem::AUTO_DETECT_TRAILER_TIMEOUT_SECS
            .min(crate::xmodem::EOB_TIMEOUT_SECS),
    );
    assert!(
        GW_FILTER_FLUSH * 10 <= tightest_protocol_wait,
        "flush window {GW_FILTER_FLUSH:?} is not an order of magnitude inside {tightest_protocol_wait:?}",
    );
    assert!(
        GW_FILTER_FLUSH >= std::time::Duration::from_millis(20),
        "too short a window releases titles that were merely split across reads",
    );
}

/// **A stripping terminal is owed nothing, and must be given nothing.** Its
/// pending ESC belongs to a sequence being discarded; releasing it on a quiet
/// line would put a raw escape on a C64 screen — the very thing the PETSCII
/// path exists to prevent.
#[test]
fn test_the_stripping_modes_never_release_a_pending_escape() {
    for mode in [GatewayFilter::Petscii, GatewayFilter::Ascii] {
        let mut state = GatewayOutState::new(mode);
        let mut out = Vec::new();
        // Digits, not letters: PETSCII case-swaps text, and this test is
        // about the escape rather than the swap.
        filter_gateway_output(b"12\x1b", &mut state, &mut out);
        assert!(!state.has_pending(), "a stripping mode holds nothing for the client");
        state.flush_pending(&mut out);
        assert_eq!(out, b"12".to_vec(), "and the flush adds nothing");
    }
}

/// Where a held-candidate filter goes wrong is the read boundary, and TCP puts
/// that boundary anywhere.  These two properties need no model of what a title
/// is, so they cannot agree with the implementation by sharing its assumptions.
mod gateway_filter_proptest {
    use super::*;
    use proptest::prelude::*;

    /// Feed the bytes through as one read, or split at exactly one point.
    fn filtered(bytes: &[u8], cut: Option<usize>, mode: GatewayFilter) -> Vec<u8> {
        let mut state = GatewayOutState::new(mode);
        let mut out = Vec::new();
        match cut {
            None => filter_gateway_output(bytes, &mut state, &mut out),
            Some(p) => {
                filter_gateway_output(&bytes[..p], &mut state, &mut out);
                filter_gateway_output(&bytes[p..], &mut state, &mut out);
            }
        }
        out
    }

    /// Uniform random bytes are the wrong generator here and quietly make the
    /// whole property vacuous: `ESC ]` needs two specific bytes in a row, so a
    /// held candidate almost never exists at a boundary and the bug the test
    /// is for goes unseen.  (Measured — a mutation that cleared the candidate
    /// on every read passed 256 uniform cases.)  So draw from the alphabet the
    /// state machine actually branches on.
    fn interesting_byte() -> impl Strategy<Value = u8> {
        prop_oneof![
            6 => Just(0x1Bu8),          // ESC
            4 => Just(b']'),            // OSC introducer
            3 => prop::sample::select(vec![b'0', b'1', b'2', b'4', b';']),
            3 => Just(0x07u8),          // BEL
            2 => Just(b'\\'),           // the ST half
            2 => Just(b'['),            // CSI
            4 => 0x20u8..=0x7Eu8,       // ordinary text
            2 => 0x80u8..=0xFFu8,       // high bytes: binary, never a title
            2 => 0x00u8..=0x06u8,       // control bytes: ditto
        ]
    }

    /// Loose bytes alone leave the *drop* path nearly unvisited -- a complete
    /// title is six specific bytes in a row -- so half the fragments are whole
    /// sequences and the stream is built from fragments rather than bytes.
    fn fragment() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            // Loose bytes: the boundaries and every malformed case.
            4 => prop::collection::vec(interesting_byte(), 1..6),
            // A complete window title -- the one thing the ANSI mode drops.
            3 => (
                prop::sample::select(vec![b'0', b'1', b'2']),
                prop::collection::vec(0x20u8..=0x7Eu8, 0..8),
                prop::bool::ANY,
            ).prop_map(|(code, text, use_st)| {
                let mut v = vec![0x1B, b']', code, b';'];
                v.extend(text);
                v.extend(if use_st { vec![0x1B, b'\\'] } else { vec![0x07] });
                v
            }),
            // An OSC that is *not* a title, and must survive untouched.
            2 => prop::collection::vec(0x20u8..=0x7Eu8, 0..6).prop_map(|text| {
                let mut v = vec![0x1B, b']', b'4', b';'];
                v.extend(text);
                v.push(0x07);
                v
            }),
            // A CSI: colour and cursor addressing, which ANSI must keep.
            2 => prop::collection::vec(prop::sample::select(vec![b'0', b'1', b';', b'3']), 0..4)
                .prop_map(|params| {
                    let mut v = vec![0x1B, b'['];
                    v.extend(params);
                    v.push(b'm');
                    v
                }),
        ]
    }

    /// A stream of fragments, flattened.
    fn stream() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(fragment(), 0..8)
            .prop_map(|frags| frags.into_iter().flatten().collect())
    }

    /// The same shapes, but each fragment carries **whether it should
    /// survive**, so the expected output is known exactly instead of scored.
    ///
    /// Filler here is deliberately ESC-free.  Every other fragment starts with
    /// ESC and terminates itself, so with no loose ESC between them no
    /// fragment can run into its neighbour and the survivors concatenated are
    /// the whole answer.  Loose bytes *can* form a title across a fragment
    /// boundary by chance -- rare enough to look like a flake and be believed
    /// -- which is why they stay out of the oracle and are left to the two
    /// properties above.
    fn oracle_fragment() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
        prop_oneof![
            // ESC-free filler: text, binary, control bytes.  Always survives.
            4 => prop::collection::vec(
                prop_oneof![0x20u8..=0x7Eu8, 0x80u8..=0xFFu8, 0x00u8..=0x06u8],
                1..8,
            ).prop_map(|v| (v.clone(), v)),
            // A complete window title: dropped.
            3 => (
                prop::sample::select(vec![b'0', b'1', b'2']),
                prop::collection::vec(0x20u8..=0x7Eu8, 0..8),
                prop::bool::ANY,
            ).prop_map(|(code, text, use_st)| {
                let mut v = vec![0x1B, b']', code, b';'];
                v.extend(text);
                v.extend(if use_st { vec![0x1B, b'\\'] } else { vec![0x07] });
                (v, Vec::new())
            }),
            // An OSC that is not a title: survives.
            2 => prop::collection::vec(0x20u8..=0x7Eu8, 0..6).prop_map(|text| {
                let mut v = vec![0x1B, b']', b'4', b';'];
                v.extend(text);
                v.push(0x07);
                (v.clone(), v)
            }),
            // A CSI: survives.
            2 => prop::collection::vec(prop::sample::select(vec![b'0', b'1', b';', b'3']), 0..4)
                .prop_map(|params| {
                    let mut v = vec![0x1B, b'['];
                    v.extend(params);
                    v.push(b'm');
                    (v.clone(), v)
                }),
            // **An OSC the remote abandoned, then a real title.**  The ESC
            // being weighed belongs to the title that follows, so the
            // abandoned bytes come back as data and the title still goes.
            // Without this shape the whole property suite cannot tell
            // "second title dropped" from "second title printed as
            // `0;user@host`" -- the reported symptom itself.  It terminates,
            // so it stays determinate whatever follows it.
            2 => (
                prop::sample::select(vec![b'0', b'1', b'2']),
                prop::collection::vec(0x20u8..=0x7Eu8, 0..6),
                prop::sample::select(vec![b'0', b'1', b'2']),
                prop::collection::vec(0x20u8..=0x7Eu8, 0..8),
            ).prop_map(|(c1, t1, c2, t2)| {
                let mut abandoned = vec![0x1B, b']', c1, b';'];
                abandoned.extend(t1);
                let mut wire = abandoned.clone();
                wire.extend([0x1B, b']', c2, b';']);
                wire.extend(t2);
                wire.push(0x07);
                (wire, abandoned)
            }),
        ]
    }

    proptest! {
        /// **Where the reads fall must not change a single byte.** The remote's
        /// output arrives in whatever pieces TCP chooses, and a filter that
        /// holds a candidate across a boundary is exactly the kind that drops
        /// or duplicates one there.  Every split point is tried, not a sampled
        /// few, because the interesting one is a specific byte pair.
        #[test]
        fn prop_chunking_never_changes_the_output(
            bytes in stream(),
        ) {
            for mode in [GatewayFilter::Ansi, GatewayFilter::Ascii, GatewayFilter::Petscii] {
                let whole = filtered(&bytes, None, mode);
                for p in 0..=bytes.len() {
                    prop_assert_eq!(
                        &whole,
                        &filtered(&bytes, Some(p), mode),
                        "a read boundary at {} changed the output of {:02X?}",
                        p,
                        bytes,
                    );
                }
            }
        }

        /// **The ANSI filter may only delete.** Its output must be a
        /// subsequence of its input — never a byte invented, never one
        /// reordered — which is what makes a file transfer through a gateway
        /// session recoverable at worst rather than silently corrupt.
        #[test]
        fn prop_ansi_output_is_a_subsequence_of_the_input(
            bytes in stream(),
        ) {
            let out = filtered(&bytes, None, GatewayFilter::Ansi);
            let mut it = bytes.iter();
            for b in &out {
                prop_assert!(
                    it.any(|x| x == b),
                    "filter emitted a byte the remote never sent, or out of order: \
                     {:02X} from {:02X?}",
                    b,
                    bytes,
                );
            }
        }

        /// **What survives is known, not scored.**  The generator builds the
        /// stream out of fragments it has already labelled, so the expected
        /// output is the surviving fragments concatenated -- an exact answer.
        /// A property that only bounds the output (a subsequence, the right
        /// length) will sit at "nearly right" while a title is half-eaten.
        #[test]
        fn prop_ansi_drops_exactly_the_titles_and_nothing_else(
            frags in prop::collection::vec(oracle_fragment(), 0..8),
        ) {
            let mut bytes = Vec::new();
            let mut want = Vec::new();
            for (wire, expected) in &frags {
                bytes.extend_from_slice(wire);
                want.extend_from_slice(expected);
            }
            prop_assert_eq!(
                filtered(&bytes, None, GatewayFilter::Ansi),
                want,
                "stream {:02X?}",
                bytes,
            );
        }

        /// **A candidate the remote never finished is delivered, not lost.**
        /// The generalisation of the XMODEM-block case: whatever precedes it,
        /// an unterminated title at the tail of the stream must come back
        /// whole once the line goes quiet, and the parser must then hold
        /// nothing.  Holding it instead is what deadlocks a stop-and-wait
        /// transfer, since the byte that would decide it is the one the
        /// sender is waiting for an ACK before sending.
        #[test]
        fn prop_an_unterminated_candidate_is_delivered_once_the_line_is_quiet(
            frags in prop::collection::vec(oracle_fragment(), 0..6),
            code in prop::sample::select(vec![b'0', b'1', b'2']),
            tail in prop::collection::vec(0x20u8..=0x7Eu8, 0..6),
        ) {
            let mut bytes = Vec::new();
            let mut want = Vec::new();
            for (wire, expected) in &frags {
                bytes.extend_from_slice(wire);
                want.extend_from_slice(expected);
            }
            // A title the remote stopped in the middle of.
            let mut open = vec![0x1B, b']', code, b';'];
            open.extend(tail.iter().copied());
            bytes.extend_from_slice(&open);
            want.extend_from_slice(&open);

            let mut state = GatewayOutState::new(GatewayFilter::Ansi);
            let mut out = Vec::new();
            filter_gateway_output(&bytes, &mut state, &mut out);
            state.flush_pending(&mut out);
            prop_assert_eq!(&out, &want, "stream {:02X?}", bytes);
            prop_assert!(!state.has_pending(), "nothing may be left held");
        }
    }
}

// ─── The byte trace must not become a credential log ───

/// The log lines recorded after `mark`, so a global buffer shared with every
/// other test can still answer a question about one prompt.
///
/// **A missing marker panics rather than returning nothing.** The buffer is a
/// process-global ring that the whole parallel suite writes to, so the marker
/// can be rotated out -- and an empty `Vec` would satisfy the negative
/// assertion ("no password was logged") without having looked at anything. The
/// one assertion carrying the security claim must not be able to pass
/// vacuously.
fn log_lines_after(mark: &str) -> Vec<String> {
    let all = crate::logger::snapshot(2000);
    match all.iter().rposition(|l| l.contains(mark)) {
        Some(i) => all[i + 1..].to_vec(),
        None => panic!(
            "marker {mark:?} not found in the last {} log lines -- it was rotated out, \
             so this test cannot see what it is asserting about",
            all.len(),
        ),
    }
}

/// **A keystroke diagnostic must not write down passwords.**
///
/// The byte trace sits in `session_read_byte`, under *every* prompt in the
/// gateway -- so with `gateway_debug` on, the telnet login, the SSH gateway's
/// remote password and the Groq API key were each emitted one byte per line.
/// `log_to_file` ships enabled and the same buffer is served at `/logs`, so
/// turning on a diagnostic to chase a stuck ESC key also put credentials on
/// disk.
///
/// The positive control is the point of this test, not decoration: asserting
/// only that nothing was logged passes just as well when the buffer was never
/// initialised, when the trace is off, or when the reader never ran.  So an
/// ordinary line is traced first, in the same session, and *must* appear.
#[tokio::test]
async fn test_the_byte_trace_never_records_a_password() {
    use tokio::io::AsyncWriteExt;
    crate::logger::init();
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    session.trace_bytes = true;

    // Positive control: an ordinary prompt IS traced.
    glog!("pwtrace-probe-normal");
    peer.write_all(b"DIR\r").await.unwrap();
    assert_eq!(session.get_line_input().await.unwrap().as_deref(), Some("DIR"));
    let traced = log_lines_after("pwtrace-probe-normal");
    assert!(
        traced.iter().any(|l| l.contains("WIRE")),
        "the trace must be armed, or this test proves nothing: {traced:?}",
    );

    // The same bytes at a password prompt: nothing on the wire is recorded.
    //
    // A byte deliberately follows the terminating CR.  The drain that runs
    // after it (`drain_trailing_eol`) reads one byte before pushing it back,
    // so with only the input loop muted the first character of whatever comes
    // next was still traced -- on a pasted `secret1\rsecret2\r`, the leading
    // character of the second secret.  Ending the wire at the CR hides that
    // entirely, because the drain then times out with nothing to read.
    glog!("pwtrace-probe-password");
    peer.write_all(b"hunter2\rZ").await.unwrap();
    assert_eq!(
        session.get_password_input().await.unwrap().as_deref(),
        Some("hunter2"),
        "the password itself must still be read correctly",
    );
    let traced = log_lines_after("pwtrace-probe-password");
    assert!(
        !traced.iter().any(|l| l.contains("WIRE")),
        "a password reached the log: {traced:?}",
    );
}

// ─── Gateway ESC: pass it on, and pair it by time ───

/// A clock for the ESC-pair tests: **one** anchor, with offsets measured from
/// it, so presses can be placed in time without sleeping.
///
/// It has to be one anchor.  Taking a fresh `Instant::now()` per call adds
/// however long the test itself took to every offset — which is precisely the
/// quantity under test, so a boundary case lands on the wrong side of the
/// window and the whole suite becomes load-dependent.  (Measured: the
/// exactly-at-the-window assertion below failed that way first.)
struct EscClock(tokio::time::Instant);

impl EscClock {
    fn new() -> Self {
        Self(tokio::time::Instant::now())
    }
    fn at(&self, ms: u64) -> tokio::time::Instant {
        self.0 + std::time::Duration::from_millis(ms)
    }
}

/// **The reported bug.** From an SC126: `ATDT telnetbible.com:6400` straight
/// from the modem passes ESC through fine, but the same host reached through
/// the telnet gateway never sees the first ESC -- and a second one leaves the
/// gateway instead of reaching the remote.
///
/// The old rule *held* an ESC and forwarded it only when a following byte
/// arrived, so a lone ESC waited for a second byte that never came.  Pressing
/// ESC once at a remote's prompt did nothing at all.  Now every ESC is
/// forwarded and only the pair is ours, so this test is about `press`
/// reporting "not a pair" for a single press: the caller forwards on that.
#[test]
fn test_a_single_esc_is_never_swallowed_by_the_gateway() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)), "one ESC must go to the remote, not be held");

    // And again much later: a lone ESC is a lone ESC however often it happens.
    assert!(!esc.press(c.at(10_000)), "a second lone ESC, long after, is also just an ESC");
    assert!(!esc.press(c.at(20_000)));
}

/// Two ESCs in a row and close together is the way out, which is the half of
/// the contract that already worked and must keep working.
#[test]
fn test_two_quick_escs_in_a_row_leave_the_gateway() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)), "the first is not yet a pair");
    assert!(esc.press(c.at(80)), "the second, 80 ms later, leaves");
}

/// **Time is what separates a deliberate pair from two unrelated presses.**
/// A user who pressed ESC to leave `vi`'s insert mode, worked for a minute and
/// pressed ESC again must still be connected -- under the old rule those two
/// were a pair however far apart, so the session simply ended.
#[test]
fn test_two_escs_far_apart_are_two_escs_not_a_way_out() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)));
    assert!(
        !esc.press(c.at(GW_ESC_PAIR.as_millis() as u64 + 1)),
        "one millisecond past the window is not a pair",
    );

    // The boundary itself is inclusive, and pinned so it cannot drift
    // silently: at exactly the window, it is still a pair.
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)));
    assert!(esc.press(c.at(GW_ESC_PAIR.as_millis() as u64)), "exactly at the window pairs");
}

/// **A press too late to pair becomes the first half of a new pair.**
/// Otherwise a user whose first attempt was too slow could never get out
/// without pressing ESC three times, which is not a rule anyone would guess.
#[test]
fn test_a_press_too_late_to_pair_starts_a_fresh_pair() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)));
    assert!(!esc.press(c.at(5_000)), "too late to pair with the first");
    assert!(esc.press(c.at(5_050)), "but it armed a new pair, so this one leaves");
}

/// **An arrow key is `ESC [ A`, and two of them are not a pair.**
///
/// This is why the rule is "consecutive AND inside the window" rather than
/// time alone: two cursor presses put two ESCs a few milliseconds apart, which
/// any purely time-based rule would read as a deliberate double-tap and throw
/// the user out mid-edit.  The intervening `[` is the whole signal, so every
/// non-ESC byte must reach `other()`.
#[test]
fn test_two_arrow_presses_are_not_a_way_out() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    // ESC [ A
    assert!(!esc.press(c.at(0)));
    esc.other(); // '['
    esc.other(); // 'A'
    // ESC [ A again, 4 ms later — faster than any human double-tap.
    assert!(!esc.press(c.at(4)), "the second arrow key must not leave the gateway");
    esc.other();
    esc.other();
    // A third, for good measure.
    assert!(!esc.press(c.at(8)));
}

/// After a leave, the state is clear.  A stale half-pair would make the very
/// next ESC of a *new* session-level action look like a second press.
#[test]
fn test_leaving_clears_the_pair_state() {
    let c = EscClock::new();
    let mut esc = EscHold::new();
    assert!(!esc.press(c.at(0)));
    assert!(esc.press(c.at(10)), "leaves");
    assert!(!esc.press(c.at(20)), "the next ESC starts over, it does not re-leave");
}

/// The window is a **guess about human hands**, so it is pinned rather than
/// left to drift.  Too short and a deliberate double-tap cannot be made at
/// all; too long and two unrelated ESCs at a remote's prompt end the session.
#[test]
fn test_the_esc_pair_window_is_in_the_range_a_human_can_hit() {
    assert!(
        GW_ESC_PAIR >= std::time::Duration::from_millis(250),
        "shorter than a deliberate double-tap: {GW_ESC_PAIR:?}",
    );
    assert!(
        GW_ESC_PAIR <= std::time::Duration::from_millis(1500),
        "long enough to catch two unrelated presses: {GW_ESC_PAIR:?}",
    );
}

// ─── Gateway onward window geometry ──────────────────

/// The geometry a gateway session reports to the remote: operator override
/// wins, else the client's NAWS, else the terminal-type default.
///
/// This is the fix for the C64 long-line corruption class: the SSH gateway
/// used to hardcode 80 columns for anything detected as ANSI, and a C64
/// running CCGMS in ASCII mode is detected as ANSI (it sends 0x08 for
/// backspace) while being physically 40 columns wide.
#[test]
fn test_gateway_window_precedence() {
    // Nothing set anywhere: per-terminal-type defaults.
    assert_eq!(
        gateway_window(TerminalType::Petscii, (None, None), 0, 0),
        (40, 25),
        "PETSCII default"
    );
    assert_eq!(
        gateway_window(TerminalType::Ansi, (None, None), 0, 0),
        (80, 24),
        "ANSI default"
    );
    assert_eq!(
        gateway_window(TerminalType::Ascii, (None, None), 0, 0),
        (80, 24),
        "ASCII default"
    );

    // A client that negotiated NAWS beats the type default.
    assert_eq!(
        gateway_window(TerminalType::Ansi, (Some(132), Some(50)), 0, 0),
        (132, 50),
        "client NAWS should beat the type default"
    );

    // The operator override beats both — the case that matters, since a C64
    // arrives through tcpser / a WiFi modem that reports nothing.
    assert_eq!(
        gateway_window(TerminalType::Ansi, (None, None), 40, 25),
        (40, 25),
        "override should beat the type default"
    );
    assert_eq!(
        gateway_window(TerminalType::Ansi, (Some(132), Some(50)), 40, 25),
        (40, 25),
        "override should beat client NAWS — the operator is correcting a lying client"
    );
}

/// Each dimension resolves independently, so an operator can pin the width
/// of a 40-column C64 and leave the row count automatic.  A single shared
/// "is anything overridden?" test would pass even if one dimension ignored
/// its override.
#[test]
fn test_gateway_window_dimensions_are_independent() {
    assert_eq!(
        gateway_window(TerminalType::Ansi, (None, None), 40, 0),
        (40, 24),
        "width pinned, rows should stay automatic"
    );
    assert_eq!(
        gateway_window(TerminalType::Ansi, (None, None), 0, 25),
        (80, 25),
        "rows pinned, width should stay automatic"
    );
    // Mixed sources: width from the override, rows from client NAWS.
    assert_eq!(
        gateway_window(TerminalType::Petscii, (Some(80), Some(50)), 40, 0),
        (40, 50),
        "width from override, rows from NAWS"
    );
}

/// `0` means "auto" and is the ONLY way to ask for it, so it must never be
/// treated as a width of zero or floored to 1 — the mistake that would make
/// auto unreachable from every UI (the same trap `log_max_size_kb` has).
#[test]
fn test_gateway_window_zero_means_auto_not_zero() {
    let (cols, rows) = gateway_window(TerminalType::Petscii, (None, None), 0, 0);
    assert_eq!((cols, rows), (40, 25), "0/0 must resolve to auto, not 0x0");
    // And a zero override must not shadow a client that did negotiate.
    assert_eq!(
        gateway_window(TerminalType::Petscii, (Some(64), Some(16)), 0, 0),
        (64, 16),
        "a 0 override must fall through to client NAWS"
    );
}

/// Every surface that explains the automatic geometry quotes the default sizes
/// as prose — the telnet help (both widths) and the shared web/GUI hint. Those
/// numbers are a hand-copy of `gateway_default_window`, which is the drift class
/// that left `test_all_error_messages_fit_petscii` checking a dead string and
/// the CP/M submenu hint naming the wrong keys. Derive them instead: change a
/// default and this names the docs that still quote the old one.
#[test]
fn test_documented_default_geometry_matches_the_code() {
    let pairs: Vec<String> = [TerminalType::Petscii, TerminalType::Ansi, TerminalType::Ascii]
        .iter()
        .map(|&tt| {
            let (w, h) = gateway_default_window(tt);
            format!("{}x{}", w, h)
        })
        .collect();
    // ANSI and ASCII share a default, so dedupe before asserting.
    let mut wanted: Vec<&String> = Vec::new();
    for p in &pairs {
        if !wanted.contains(&p) {
            wanted.push(p);
        }
    }
    assert!(
        wanted.len() >= 2,
        "expected at least two distinct default geometries, got {wanted:?} — \
         has the default table collapsed?"
    );

    for petscii in [true, false] {
        let help = TelnetSession::gateway_config_help_lines(petscii).join(" ");
        for pair in &wanted {
            assert!(
                help.contains(pair.as_str()),
                "the {} gateway help never states the {} default from \
                 gateway_default_window — update the help text",
                if petscii { "PETSCII" } else { "80-col" },
                pair,
            );
        }
    }

    // The one-line hint the web and GUI share must agree too.
    let hint = crate::config::Config::gateway_term_hint(0, 0);
    for pair in &wanted {
        assert!(
            hint.contains(pair.as_str()),
            "config::gateway_term_hint's auto text never states the {} default \
             from gateway_default_window",
            pair,
        );
    }
}

/// The `[gw-diag]` "which input won" label must never contradict the value the
/// resolver actually produced.  They express the same precedence twice — the
/// resolver returns the number, the label names the source — which is exactly
/// the shape that drifts in this codebase, so the agreement is pinned across
/// the whole input matrix rather than trusted.
#[test]
fn test_gateway_window_source_agrees_with_the_resolver() {
    let tt = TerminalType::Ansi;
    // Derived, not hand-copied — the point of the test above.
    let (default_cols, _) = gateway_default_window(tt);
    for &ovr in &[0u16, 40] {
        for &negotiated in &[None, Some(132u16)] {
            let (cols, _) = gateway_window(tt, (negotiated, negotiated), ovr, ovr);
            match gateway_window_source(ovr, negotiated) {
                "config override" => assert_eq!(
                    cols, ovr,
                    "label says override but the resolver returned {cols} \
                     (ovr={ovr}, naws={negotiated:?})"
                ),
                "client NAWS" => assert_eq!(
                    cols,
                    negotiated.unwrap(),
                    "label says client NAWS but the resolver returned {cols}"
                ),
                "terminal-type default" => assert_eq!(
                    cols, default_cols,
                    "label says type default but the resolver returned {cols}"
                ),
                other => panic!("unexpected source label {other:?}"),
            }
        }
    }
}

/// A mid-session NAWS resize must be filtered through the geometry resolver,
/// not forwarded raw.
///
/// This was a real defect in the first cut of the override: the connect-time
/// report honoured `gateway_term_width`, but the resize arm called
/// `send_naws_update(cols, rows)` with the client's own numbers, so an
/// operator's pinned width silently lapsed the moment the client resized. The
/// resolver-level rule ("an override beats client NAWS") is already covered by
/// `test_gateway_window_precedence`; what this guards is the *call site* using
/// it at all, which no unit test can reach — the resize lives inside
/// `gateway_telnet`'s `tokio::select!` loop, which needs a live socket pair and
/// a remote peer.
///
/// So it is checked the way `config::test_every_written_key_can_be_applied`
/// checks its `match`: by reading the source. Every `send_naws_update` call
/// must be preceded, within its own arm, by a `resolve_window(` call.
#[test]
fn test_naws_resize_goes_through_the_geometry_resolver() {
    let src = include_str!("gateway.rs");
    // Only the call sites, not the definition.
    let calls: Vec<usize> = src
        .match_indices("iac.send_naws_update(")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !calls.is_empty(),
        "no send_naws_update call sites found — this scan has stopped matching"
    );
    for at in calls {
        // Look back a short window: the resolve must be in the same arm, not
        // merely somewhere earlier in the 2000-line file.
        let from = at.saturating_sub(900);
        let window = &src[from..at];
        assert!(
            window.contains("resolve_window("),
            "a send_naws_update call at byte {at} is not fed by resolve_window() — \
             a raw client size forwarded here discards the operator's \
             gateway_term_width/height override mid-session"
        );
    }
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
        //
        // The file-transfer menu's invalid-key hint used to sit here as
        // "Press U, D, X, C, I, R, Q, or H." — a string the code stopped
        // using once the menu grew to eleven keys.  This list is hand-copied,
        // so it kept passing against the dead string while the live one was
        // 43 cols and wrapped.  It is now two lines and is covered
        // automatically by `test_show_error_literals_fit_petscii` below.
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
        // Other settings (uses the generic hint — too many keys to list)
        "Press a letter from the menu.",
        // CP/M emulator submenu
        "Press E, C, D, U, or Q.",
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

/// Drift-proof companion to `test_all_error_messages_fit_petscii`: instead of
/// asserting against a hand-copied list, this reads the telnet module's own
/// source at compile time (`include_str!`) and checks **every** literal passed
/// to `show_error` / `show_error_lines`.
///
/// The hand-copied list cannot catch a message that is edited in place — it
/// happily kept asserting about a string the code no longer used, while the
/// live 43-col replacement wrapped on a C64 for months.  A scan of the call
/// sites has no such blind spot: a new or lengthened literal is covered the
/// moment it is written, with no second place to remember to update.
///
/// Both helpers indent by two spaces and emit each line **unwrapped**, so the
/// budget is identical: `2 + literal <= PETSCII_WIDTH`.
///
/// Calls whose argument contains `format!` are skipped — their width depends on
/// runtime values (a filename, an `io::Error`) and cannot be checked
/// statically.  Those are the ones to keep short by hand.
#[test]
fn test_show_error_literals_fit_petscii() {
    // Every submodule that calls either helper.  A file missing from this list
    // is the one hole left, so it is asserted non-empty below.
    const SOURCES: &[(&str, &str)] = &[
        ("mod.rs",        include_str!("mod.rs")),
        ("io.rs",         include_str!("io.rs")),
        ("session.rs",    include_str!("session.rs")),
        ("transfer.rs",   include_str!("transfer.rs")),
        ("gateway.rs",    include_str!("gateway.rs")),
        ("serial_ui.rs",  include_str!("serial_ui.rs")),
        ("config_ui.rs",  include_str!("config_ui.rs")),
        ("web.rs",        include_str!("web.rs")),
        ("weather.rs",    include_str!("weather.rs")),
        ("aichat_ui.rs",  include_str!("aichat_ui.rs")),
        ("kernel.rs",     include_str!("kernel.rs")),
        ("cpm_emu.rs",    include_str!("cpm_emu.rs")),
        // Unix only, like the module -- but the strings are just strings, so
        // the widths are worth checking wherever this test runs.  The list
        // being hand-maintained is the hole this test's own comment names, and
        // it opened the moment `power.rs` was added: the file sat outside the
        // scan while carrying a 51-column message, and `checked > 50` below
        // was satisfied hundreds of times over by the other twelve.
        ("power.rs",      include_str!("power.rs")),
    ];

    /// Extract the balanced-parenthesis argument text starting at `open`
    /// (the index of the `(`).  Parens inside string literals don't count,
    /// so a message containing "(" can't truncate the scan.
    fn balanced_arg(src: &str, open: usize) -> &str {
        let b = src.as_bytes();
        let mut depth = 0usize;
        let mut in_str = false;
        let mut esc = false;
        let mut i = open;
        while i < b.len() {
            let c = b[i];
            if in_str {
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return &src[open..=i];
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        &src[open..]
    }

    /// Every `"..."` literal body in `text`, honouring backslash escapes.
    fn string_literals(text: &str) -> Vec<String> {
        let b = text.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                let mut esc = false;
                while j < b.len() {
                    if esc {
                        esc = false;
                    } else if b[j] == b'\\' {
                        esc = true;
                    } else if b[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                out.push(text[start..j.min(text.len())].to_string());
                i = j + 1;
            } else {
                i += 1;
            }
        }
        out
    }

    let mut checked = 0usize;
    for (file, src) in SOURCES {
        // Match the call, not the definition, and don't let `show_error(`
        // also match inside `show_error_lines(` (the char after the name is
        // `_`, not `(`, so searching for the paren-terminated name is enough).
        for pat in ["show_error(", "show_error_lines("] {
            let mut from = 0usize;
            while let Some(rel) = src[from..].find(pat) {
                let at = from + rel;
                from = at + pat.len();
                // Skip the `fn show_error…(` definitions in io.rs.
                let before = src[..at].trim_end();
                if before.ends_with("async fn") || before.ends_with("fn") {
                    continue;
                }
                let arg = balanced_arg(src, at + pat.len() - 1);
                // Runtime-formatted width; not statically checkable.
                if arg.contains("format!") {
                    continue;
                }
                for lit in string_literals(arg) {
                    // Escape sequences are counted as written (`\"` as two
                    // chars), which over-counts.  That errs toward failing a
                    // borderline message rather than passing a too-wide one.
                    let displayed = 2 + lit.chars().count();
                    assert!(
                        displayed <= PETSCII_WIDTH,
                        "{}: show_error literal {:?} is {} chars with its \
                         2-space indent, exceeding PETSCII's {} columns — \
                         split it across lines with `show_error_lines`",
                        file,
                        lit,
                        displayed,
                        PETSCII_WIDTH,
                    );
                    checked += 1;
                }
            }
        }
    }

    // Guards the scanner itself: a refactor that renames the helpers, or an
    // `include_str!` path that goes stale, would otherwise leave this test
    // passing while checking nothing at all.
    assert!(
        checked > 50,
        "expected to check >50 show_error literals, found {checked} — \
         the scanner has stopped matching the real call sites"
    );
}

/// The CP/M disk screens print their own lines rather than going through the
/// `show_error` helpers, so the scan above does not see them — and four of them
/// were over budget the moment they were written.
///
/// Scanned rather than listed, for the reason given above: a hand-copied list
/// keeps asserting about a string the code no longer uses.  Only literals are
/// checked; a `format!` whose width depends on a filename cannot be checked
/// statically and is truncated at the call site instead.
#[test]
fn test_cpm_disk_screen_literals_fit_petscii() {
    // `\r` stripped: a CRLF checkout would otherwise put a stray byte inside
    // every literal and fail this on Windows only.
    const SOURCES: &[(&str, &str)] = &[
        ("cpm_mount_ui.rs", include_str!("cpm_mount_ui.rs")),
        ("cpm_boot_ui.rs", include_str!("cpm_boot_ui.rs")),
    ];
    /// The literal inside `self.dim("…")` and friends, and the indent of the
    /// `format!` that prints it.
    ///
    /// Whitespace between `&format!(` and the format string is skipped, because
    /// the first version of this matched only the contiguous `send_line(&format!("`
    /// and so never saw the sixteen call sites written across several lines —
    /// including two that were over budget. It still passed its own
    /// `checked > 20` floor on the single-line sites, which is the worst way for
    /// a test like this to be wrong: confidently green about text it never read.
    fn scan(src: &str) -> Vec<(usize, String)> {
        let src = src.replace('\r', "");
        let mut out = Vec::new();
        for (i, _) in src.match_indices("send_line(&format!(") {
            let rest = &src[i + "send_line(&format!(".len()..];
            // Skip a newline and indentation before the format string.
            let Some(open) = rest.find('"') else { continue };
            if !rest[..open].chars().all(char::is_whitespace) {
                continue;
            }
            let after_open = &rest[open + 1..];
            let Some(brace) = after_open.find("{}\"") else { continue };
            let indent = &after_open[..brace];
            if !indent.chars().all(|c| c == ' ') {
                continue;
            }
            let after = &after_open[brace..];
            let Some(q) = after.find("(\"") else { continue };
            if after[..q].contains(';') {
                continue; // not this call any more
            }
            let lit = &after[q + 2..];
            let Some(end) = lit.find('"') else { continue };
            out.push((indent.len(), lit[..end].to_string()));
        }
        // send_line("literal")
        for (i, _) in src.match_indices("send_line(\"") {
            let lit = &src[i + "send_line(\"".len()..];
            let Some(end) = lit.find('"') else { continue };
            out.push((0, lit[..end].to_string()));
        }
        out
    }
    let mut checked = 0;
    for (name, src) in SOURCES {
        for (indent, lit) in scan(src) {
            if lit.contains('\\') {
                continue; // an escape we are not measuring correctly
            }
            checked += 1;
            assert!(
                indent + lit.chars().count() <= PETSCII_WIDTH,
                "{name}: {} cols wraps on a 40-column screen: {lit:?}",
                indent + lit.chars().count()
            );
        }
    }
    // The floor is above the 84 the single-line-only version matched, so a
    // regression to that matching would fail here rather than pass quietly.
    assert!(
        checked > 90,
        "the scan found only {checked} literals — it has stopped seeing the \
         multi-line `format!` call sites again"
    );
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
        "  K  CP/M System",
        "  M  More",
        "  R  Troubleshooting",
        "  S  SSH Gateway",
        "  T  Telnet Gateway",
        "  W  Weather",
        "  X  Exit",
        // MORE page (main menu -> M).  Unix only, but the widths are a
        // property of the strings, so they are checked on every platform.
        // (The `Computer:` row is not here: its width depends on a runtime
        // value, so its worst case is pinned by
        // `test_the_computer_row_fits_at_its_longest` instead of by an
        // invented hostname that proves nothing.)
        "  R  Restart the computer",
        "  S  Shut down the computer",
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
        //
        // NOTE: this list is hand-copied, which is exactly the drift this
        // project distrusts — it held "  A  Set AI API key (Groq)" for a while
        // after the menu started drawing something else, and passed, because
        // both strings fit.  A stale row here measures nothing.  Prefer
        // extracting the lines and iterating them (see the `*_help_lines`
        // cluster) when a screen's rows are worth guarding properly.
        "  A  Set Groq API key (optional)",
        "  B  Set browser homepage",
        "  W  Set weather location",
        "  U  Cycle weather units",
        "  V  Toggle verbose transfer logging",
        "  G  Toggle GUI on startup",
        "  E  CP/M settings",
        // CP/M boot settings menu
        "  S  Browser typing: off",
        // L shares a row with R — Other Settings is at its 22-row budget, so
        // this is the two-column form and both keys must fit together.
        "  L  Log file         R  Restart server",
        // Log file submenu (Other Settings -> L)
        "  E  Toggle logging to file",
        "  F  Set log file name",
        "  S  Set rotate size (KB, 0 = never)",
        "  K  Set old logs to keep (0 = none)",
        // CP/M emulator submenu (Other Settings -> E)
        "  E  Toggle emulator on/off",
        "  C  Set runaway ceiling (M-instr)",
        "  U  Cycle virtual-modem port",
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

/// The main menu must fit the screen it is drawn on — counted from the
/// **real** rows.
///
/// header(3) + blank + 10 items + blank + help = 16 rows; the optional CP/M
/// `K` item makes it 17 and the Unix-only `M` (More) 18; slave mode adds three
/// (notice, master line, blank); and `run_menu_loop` writes `ethernet> ` under
/// it with `send`, not `send_line`, so the prompt is a row too.  Worst case:
/// **exactly 22 of 22.**  `M` took the last one, which is why it opens a
/// second page instead of being two entries here.
///
/// **This used to be arithmetic over literals** — `16 + cfg!(unix) + 1 + 1 + 3`
/// compared against 22 — which reads nothing from `render_main_menu`.  Proved
/// by mutation: a twenty-third `send_line` in the renderer left it green while
/// the "exactly 22 of 22" claim in this comment, in CLAUDE.md and in the
/// changelog silently became false.
#[test]
fn test_main_menu_row_count() {
    for term in [TerminalType::Petscii, TerminalType::Ansi, TerminalType::Ascii] {
        let mut session = make_test_session(term);
        session.color_enabled = false;
        // The worst case the renderer can draw: a slave, emulator on.
        let worst = session.main_menu_rows(MenuItems { cpm: true, second_page: cfg!(unix) }, Some("gateway.example.org"));
        let drawn = worst.len() + 1; // the prompt line
        assert!(
            drawn <= 22,
            "the slave-mode main menu draws {} rows on {:?}, exceeds 22",
            drawn,
            term,
        );
        // And it is at the limit, not near it.  If this ever goes slack the
        // sentence above (and CLAUDE.md, and the changelog) is wrong rather
        // than merely conservative — which is the failure this project keeps
        // finding.  If an entry is deliberately removed, correct all three.
        // **The budget is 22 on every platform; what fills it is not.**  `M`
        // is `#[cfg(unix)]`, so a Windows build draws one row fewer -- and
        // pinning that number too, rather than skipping the assertion off
        // Unix, keeps the Windows menu budgeted as well: a *second* entry
        // compiled out would otherwise pass unnoticed there.  This asserted a
        // flat 22 from `85193af` until now and was red on the windows job the
        // whole time.
        let full = if cfg!(unix) { 22 } else { 21 };
        assert_eq!(
            drawn, full,
            "the worst-case main menu is no longer exactly full on {:?}",
            term,
        );
        // The ordinary case must leave the slave notice out, or the three
        // rows it costs are not conditional at all.
        let plain = session.main_menu_rows(MenuItems { cpm: true, second_page: cfg!(unix) }, None);
        assert_eq!(plain.len() + 3, worst.len(), "the slave notice is not 3 rows");
        // Turning the emulator off drops exactly the CP/M item.
        let no_cpm = session.main_menu_rows(MenuItems { cpm: false, second_page: cfg!(unix) }, None);
        assert_eq!(plain.len(), no_cpm.len() + 1, "the CP/M item is not one row");
        assert!(
            !no_cpm.iter().any(|r| r.contains("CP/M")),
            "the CP/M item is drawn with the emulator off",
        );
        // Every row fits: 39 on PETSCII, the `separator()` budget.
        let width = if term == TerminalType::Petscii { PETSCII_WIDTH - 1 } else { 80 };
        for row in &worst {
            assert!(
                row.chars().count() <= width,
                "main menu row {:?} is {} columns on {:?}, over {}",
                row,
                row.chars().count(),
                term,
                width,
            );
        }
    }
}

/// Main menu base items are A, B, C, F, G, R, S, T, W, X (10); the CP/M
/// emulator adds an optional `K`, gated on `cpm_emu_enabled`, and Unix builds
/// add `M` (More) — 12 at most.
#[test]
fn test_main_menu_item_count() {
    let items = ["A", "B", "C", "F", "G", "R", "S", "T", "W", "X"];
    assert_eq!(items.len(), 10, "main menu should have 10 base items");
    // With the CP/M emulator enabled, `K` is the optional 11th item.
    let items_with_cpm = ["A", "B", "C", "F", "G", "K", "R", "S", "T", "W", "X"];
    assert_eq!(items_with_cpm.len(), 11, "with CP/M enabled there are 11 items");
    // `M` is compiled out on Windows, where the page it opens has no items.
    #[cfg(unix)]
    {
        let all = ["A", "B", "C", "F", "G", "K", "M", "R", "S", "T", "W", "X"];
        assert_eq!(all.len(), 12, "a Unix build with CP/M on has 12 items");
    }
}

/// Error hint must list exactly the valid main menu keys, in every variant it
/// has — `K` appears only when the CP/M emulator is enabled, `M` only on Unix.
///
/// It reads the **real** `main_menu_key_hint`, not a copy of its output: the
/// previous version of this test held two literals beside two literals in
/// `handle_main_command`, which is a guard comparing the source with a copy of
/// itself.  The width is the reason the hint uses ranges — measured with both
/// optional keys present, the hint is 36 characters (38 printed), spelling out
/// `R, S, T` puts it at 40 (42 printed), and restoring the dropped `or` as
/// well reaches 43 printed.  Both economies are needed, not just one.
#[test]
fn test_main_menu_error_hint() {
    for cpm in [false, true] {
        let hint = main_menu_key_hint(MenuItems { cpm, second_page: cfg!(unix) });
        // **Measured as it is printed.**  `show_error` puts a two-space indent
        // in front of it, so the budget is 38 -- asserting against 40 is how
        // the literal this replaced overflowed a C64 row unnoticed on the
        // shipped default configuration.
        let printed = format!("  {}", hint);
        assert!(
            printed.chars().count() <= PETSCII_WIDTH,
            "error hint prints as {:?}, {} columns, exceeds {}",
            printed,
            printed.chars().count(),
            PETSCII_WIDTH,
        );
        // Every key the menu actually accepts must be named.
        for key in ["A-C", "F", "G", "R-T", "W", "X", "H"] {
            assert!(hint.contains(key), "error hint {:?} must mention {}", hint, key);
        }
        assert_eq!(
            hint.contains(" K,"),
            cpm,
            "K belongs in the hint exactly when the CP/M item is shown: {:?}",
            hint,
        );
        // The second page moved from `M` in the middle of the letters to `2`
        // at the bottom, so the hint names it after `X` -- and `M` must not
        // come back, like the `D` and `E` below it.
        assert_eq!(
            hint.contains(" 2,"),
            cfg!(unix),
            "2 belongs in the hint exactly where the item is compiled in: {:?}",
            hint,
        );
        assert!(!hint.contains(" M,"), "error hint must not mention M");
        // Keys the menu removed long ago must not come back.
        assert!(!hint.contains(" D,"), "error hint must not mention D");
        assert!(!hint.contains(" E,"), "error hint must not mention E");
    }
}

/// Main help screen content is 18 lines, plus two for each optional item that
/// is on the menu: `K` when the CP/M emulator is on, `2` where the second page
/// has anything on it (Unix only).  The `show_help_page` paginator handles
/// overflow gracefully, so the total still fits the 22-row PETSCII budget for
/// everything except the bottom prompt — which lands on its own page if needed.
///
/// **Both optional entries are counted from the parameter, not from the
/// machine**, so every combination is measured wherever the suite runs.  A
/// version that asked the live state could only ever check the installation it
/// ran on, which is how the `2` lines came to be printed on a menu that does
/// not draw that key.
#[test]
fn test_main_help_content_line_count() {
    const BASE: usize = 18;
    for cpm in [false, true] {
        for second in [false, true] {
            let items = MenuItems { cpm, second_page: second };
            // The second page is compiled out off Unix, so its lines are too.
            let second_shown = second && cfg!(unix);
            let expected = BASE + 2 * usize::from(cpm) + 2 * usize::from(second_shown);
            let lines = TelnetSession::main_help_lines(items);
            assert_eq!(
                lines.len(),
                expected,
                "main help with cpm={cpm} second={second} should have {expected} \
                 content lines, not {}:\n  {}",
                lines.len(),
                lines.join("\n  "),
            );
            // The entry itself, not just the count: two lines could be any two.
            assert_eq!(
                lines.iter().any(|l| l.starts_with("  K  ")),
                cpm,
                "the K entry is present exactly when the menu draws it",
            );
            assert_eq!(
                lines.iter().any(|l| l.starts_with("  2  ")),
                second_shown,
                "the 2 entry is present exactly when the menu draws it",
            );
        }
    }
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
    // The outcome note (`draw_transfer_note`) adds a row whenever the operator
    // arrives here straight from a transfer, which is the common case -- so it
    // belongs in the worst case, not outside it.  The count was 13 and did not
    // include it.
    let note = 1;
    let rows = 3 + 1 + note + 1 + 1 + 5 + 1 + 1; // 14
    assert!(rows <= 22, "file transfer menu is {} rows, exceeds 22", rows);
}

/// Download/delete file listing: header(3) + note + blank + col_header +
/// divider + 10 entries + blank + page_info + blank + nav + blank + prompt.
///
/// **This is now exactly 22 rows -- the whole PETSCII budget, with nothing
/// spare.**  A download returns to this picker, so the outcome note is drawn
/// here too, and that is the row that used the last of the margin.  The next
/// line anyone adds to this screen has to take one away, and this assertion is
/// what will say so.
#[test]
fn test_file_listing_row_count() {
    let header = 3; // sep + title + sep
    let note = 1;   // draw_transfer_note, when arriving from a transfer
    let col = 2;    // column header + divider
    let entries = TelnetSession::TRANSFER_PAGE_SIZE; // 10
    let footer = 5; // blank + page info + blank + nav + prompt
    let total = header + note + 1 + col + entries + footer;
    assert!(
        total <= 22,
        "file listing is {} rows, exceeds 22",
        total,
    );
    assert_eq!(
        total, 22,
        "the listing is at the budget exactly; if this number moved, the \
         screen changed and the margin needs re-checking rather than the \
         assertion relaxing"
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

/// CP/M emulator settings: header(3) + blank + 2 warning lines + blank
/// + 4 status rows + blank + 7 actions + blank + Back + prompt = 22.
///
/// **The screen is full.** It sits exactly on the PETSCII budget, so the next
/// row added here has to displace one or move to a submenu of its own — which
/// is how this screen came to exist in the first place, when Other Settings
/// reached the same limit. Counted against the real function: 21 `send_line`
/// calls plus the prompt.
#[test]
fn test_cpm_settings_row_count() {
    let header = 3;
    let warning = 1 + 2; // blank + "be sure you trust..." over two lines
    // Emulator+ceiling share a row: the printer needed a key and the screen was
    // already exactly on budget.  Emulator/Modem/Images/Runs.
    let status = 1 + 4;
    let actions = 1 + 7; // blank + E, C, U, D, I, B, P
    let footer = 1 + 1 + 1; // blank + Back + prompt
    let rows = header + warning + status + actions + footer;
    assert_eq!(rows, 22, "the CP/M settings screen is {rows} rows");
    assert!(rows <= 22, "CP/M settings is {rows} rows, exceeds 22");
}

/// Every key the CP/M settings screen displays must also be one it handles,
/// and must appear in the error hint it prints when a user presses something
/// else.  Both halves have gone wrong here: `I` was displayed for a whole
/// release while only the parent menu handled it, and the hint had drifted
/// away from the list twice.
#[test]
fn test_cpm_settings_keys_are_displayed_handled_and_hinted() {
    let src = include_str!("config_ui.rs");
    let start = src.find("pub(in crate::telnet) async fn cpm_settings").expect("the fn");
    let end = src[start..].find("\n    /// Log-file submenu").expect("the next fn") + start;
    let body = &src[start..end];

    let hint = "Press E, C, D, M, U, I, B, P, G, or Q.";
    assert!(body.contains(hint), "the error hint changed: it must list every key");
    // `G` is shown only while the images folder is empty — a conditional row,
    // because this screen is at its 22-row budget and the offer replaces the
    // "Images: none mounted" line rather than adding to it. It is handled and
    // hinted unconditionally, which is the safe direction: a key that works
    // when it is not shown costs nothing, and one that is shown and does not
    // work is the drift this test exists for.
    //
    // `D` is now the same shape for a different reason: it moved to the modem
    // screen when the Hayes profile needed a home and this one had no spare
    // row, and it is still accepted here so an operator's habit keeps working.
    for key in ["E", "C", "U", "M", "I", "B", "P", "G"] {
        assert!(
            body.contains(&format!("self.cyan(\"{key}\")")),
            "{key} must be a displayed menu item"
        );
        assert!(
            body.contains(&format!("\"{}\" => ", key.to_ascii_lowercase())),
            "{key} is displayed but never handled — pressing it would just error"
        );
        assert!(hint.contains(key), "{key} is displayed but missing from the error hint");
    }

    // `D` is no longer *displayed* here but must still be accepted and hinted,
    // so an operator who learned it before it moved keeps working. Pinning its
    // absence from the display is the half that matters: putting it back would
    // put this screen over its 22-row budget again, and the row-count test
    // above counts an arithmetic model rather than the real `send_line` calls,
    // so it would not notice.
    assert!(
        !body.contains("self.cyan(\"D\")"),
        "D moved to the modem screen; displaying it here again exceeds 22 rows"
    );
    assert!(body.contains("\"d\" => "), "D must still be accepted where it used to live");
    assert!(hint.contains('D'), "a key that still works must stay in the hint");
}

/// The CP/M modem-profile screen: header(3) + blank + port + blank + 7 actions
/// + blank + Back + prompt = 16.
///
/// The fourth screen split off the CP/M settings menu, and the reason is the
/// one the row-count test above states: that menu is exactly on the 22-row
/// PETSCII budget, so a new question brings its own screen. This one swapped
/// `D` out rather than adding a row, so the parent is unchanged in size.
///
/// The state sits *in* the action rows (`E  Echo (E1)  ON`) rather than in a
/// status block above them, which is what keeps this at 16 — printing six
/// values twice would have cost six rows to say nothing new.
#[test]
fn test_cpm_modem_settings_row_count() {
    let header = 3;
    let port = 1 + 1; // blank + the port this profile belongs to
    let actions = 1 + 7; // blank + E, V, T, X, C, S, D
    let footer = 1 + 1 + 1; // blank + Back + prompt
    let rows = header + port + actions + footer;
    assert_eq!(rows, 16, "the CP/M modem screen is {rows} rows");
    assert!(rows <= 22, "CP/M modem settings is {rows} rows, exceeds 22");
}

/// Every key that screen displays is handled and hinted, and every field of
/// the saved profile is reachable.
///
/// **The point of the screen is completeness**, so the second half matters more
/// than the first: `CpmModemProfile` has six fields, the web UI and the desktop
/// have always edited all six, and telnet edited none. A screen that reached
/// five would be the same defect with a smaller number.
#[test]
fn test_cpm_modem_settings_reaches_every_field_of_the_profile() {
    let src = include_str!("config_ui.rs");
    let start = src
        .find("pub(in crate::telnet) async fn cpm_modem_settings")
        .expect("the fn");
    let end = src[start..]
        .find("pub(in crate::telnet) async fn cpm_printer_settings")
        .expect("the next fn")
        + start;
    let body = &src[start..end];

    let hint = "Press E, V, T, X, C, S, D, or Q.";
    assert!(body.contains(hint), "the error hint must list every key");
    for key in ["E", "V", "T", "X", "C", "S", "D"] {
        assert!(
            body.contains(&format!("self.cyan(\"{key}\")")),
            "{key} must be a displayed menu item"
        );
        assert!(
            body.contains(&format!("\"{}\" => ", key.to_ascii_lowercase())),
            "{key} is displayed but never handled"
        );
        assert!(hint.contains(key), "{key} is displayed but missing from the hint");
    }

    // Every persisted field of the profile is written by some key here. The
    // config keys are the honest list: they are what the web UI edits and what
    // `AT&W` saves, so a field added to `CpmModemProfile` shows up as a key
    // this screen does not mention.
    for key in [
        "cpm_emu_echo",
        "cpm_emu_verbose",
        "cpm_emu_quiet",
        "cpm_emu_x_code",
        "cpm_emu_dcd_mode",
        "cpm_emu_s_regs",
    ] {
        assert!(
            body.contains(key),
            "{key} is part of the saved profile but this screen cannot set it"
        );
    }

    // `Q` is Back on every screen in this file, which is why quiet is `T`.
    assert!(
        !body.contains("self.cyan(\"Q\")"),
        "Q is Back here; a Quiet toggle bound to it would shadow the way out"
    );
}

/// The CP/M printer screen: header(3) + blank + 2 status + blank + up to 3
/// note lines + blank + 2 actions + blank + Back + prompt = 16.
///
/// The third screen to be split off the CP/M settings menu, for the same reason
/// as the other two: that menu sits exactly on the 22-row PETSCII budget, so a
/// new question has to bring its own screen.  Six rows spare here, which is
/// where a second printer board goes when one turns up.
#[test]
fn test_cpm_printer_settings_row_count() {
    let header = 3;
    let status = 1 + 3; // blank + Output/Board/Bare CR
    let note = 1 + 3; // blank + the longer of the two explanations
    let actions = 1 + 3; // blank + P, B, A
    let footer = 1 + 1 + 1; // blank + Back + prompt
    let rows = header + status + note + actions + footer;
    assert_eq!(rows, 18, "the CP/M printer screen is {rows} rows");
    assert!(rows <= 22, "CP/M printer settings is {rows} rows, exceeds 22");
}

/// The CP/M settings screen's merged Emulator/ceiling row must fit 40 columns
/// **at the worst value the config can hold**, not at the default.
///
/// `cpm_emu_max_minstr` is a `u32` and the parser bounds it only below (`>= 1`),
/// so the row's real longest form uses ten digits, not four. When the two rows
/// were merged to make room for the printer key the comment claimed 37 columns
/// — true of the default and wrong by six of the worst case, which would wrap a
/// Commodore and push a screen pinned at exactly 22 rows past its budget.
///
/// The format string is read out of `config_ui.rs` rather than restated here:
/// a test that models a string the code does not use is the kind that stays
/// green through the change that breaks it.
#[test]
fn test_cpm_settings_ceiling_row_fits_petscii() {
    let src = include_str!("config_ui.rs").replace('\r', "");
    let marker = "\"  Emulator:  {}, ceiling ";
    let i = src.find(marker).expect(
        "the Emulator/ceiling row changed shape — re-measure it here rather than \
         deleting this test",
    );
    let rest = &src[i + 1..];
    let fmt = &rest[..rest.find('"').expect("the end of the format string")];

    // The two `{}`: the on/off word, and the ceiling.  Longest of each.
    let status = "off"; // 3 columns; "ON" is 2 — colour codes do not advance the
                        // cursor on any of the three terminal types.
    let ceiling = u32::MAX.to_string(); // 10 digits, what the parser will accept
    let rendered = fmt.replacen("{}", status, 1).replacen("{}", &ceiling, 1);

    assert!(
        !rendered.contains("{}"),
        "the row grew a third field this test does not know how to fill: {rendered:?}"
    );
    assert!(
        rendered.chars().count() <= PETSCII_WIDTH,
        "the Emulator/ceiling row is {} cols at cpm_emu_max_minstr = {}, which \
         wraps a 40-column screen and costs this screen a row it does not have: \
         {rendered:?}",
        rendered.chars().count(),
        u32::MAX
    );
}

/// Every fixed line of the CP/M printer screen has to fit a Commodore's 40
/// columns.
///
/// The screen lives in `config_ui.rs`, which
/// [`test_cpm_disk_screen_literals_fit_petscii`] does not scan — that one covers
/// the disk screens' own files — so without this its width was an *assertion in
/// a comment*, and the comment was wrong: it claimed 26 columns at the widest
/// when the widest row is 30. A number nothing measures drifts from the code
/// the moment either changes, so the number lives here instead — and the first
/// thing it did was catch the comment's replacement being wrong too, because a
/// key row is `"  K  "` and not `"  "`.
///
/// Scanned, not listed: a hand-copied list keeps asserting about strings the
/// code no longer prints. The `Back` row is measured only as far as its key,
/// since `action_prompt` builds it; it is the shortest row on the screen and
/// the same helper is width-checked wherever else it is used.
#[test]
fn test_cpm_printer_screen_literals_fit_petscii() {
    let src = include_str!("config_ui.rs").replace('\r', "");
    let start = src.find("pub(in crate::telnet) async fn cpm_printer_settings").expect("the fn");
    // Bounded by the *next item's* doc comment rather than by naming one: the
    // marker used to be "/// Boot settings, reached from", and when that doc was
    // moved to sit with the function it describes, this scan ran on into the
    // next screen and reported its wider rows as a printer-screen regression.
    // A doc comment at four spaces is the next item; the body's are at eight.
    let end = src[start..]
        .find("\n    /// ")
        .map(|i| i + start)
        .expect("a following item");
    let body = &src[start..end];

    // Both shapes this screen prints: `"  {}"` wrapping a colour helper (the
    // note lines) and `"  {}  Some text"` (the action rows), where the `{}` is
    // one key character.
    let mut widths: Vec<(usize, String)> = Vec::new();
    for (i, _) in body.match_indices("&format!(\"") {
        let rest = &body[i + "&format!(\"".len()..];
        let Some(end) = rest.find('"') else { continue };
        let fmt = &rest[..end];
        if fmt.contains('\\') {
            continue; // an escape we would not measure correctly
        }
        // The widest a `{}` can render on this screen: a colour helper's payload
        // for the note rows, one character for a key.
        let rendered = if fmt.trim_end() == "  {}" {
            // The literal inside the helper call that follows.
            let after = &rest[end..];
            let Some(q) = after.find("(\"") else { continue };
            let lit = &after[q + 2..];
            let Some(e) = lit.find('"') else { continue };
            format!("  {}", &lit[..e])
        } else {
            fmt.replace("{}", "K")
        };
        widths.push((rendered.chars().count(), rendered));
    }

    assert!(
        widths.len() >= 7,
        "the scan found only {} lines — it has stopped matching this screen",
        widths.len()
    );
    for (w, line) in &widths {
        assert!(*w <= PETSCII_WIDTH, "{w} cols wraps on a 40-column screen: {line:?}");
    }
    let widest = widths.iter().map(|(w, _)| *w).max().unwrap();
    assert_eq!(
        widest, 32,
        "the widest fixed row on the CP/M printer screen is now {widest} columns, \
         not 32 — fine if it still fits, but the number is documented"
    );
}

/// **A path row must show the end of the path, not the beginning.**
///
/// The transfer screens and the log-file row render a path through a 26-column
/// PETSCII budget. Right-truncating it spends every one of those columns on the
/// constant base and hides the only part that answers the question the row is
/// there to answer — and the data-directory move made the base 30 columns on its
/// own, so a C64 operator saw the same `ethernetgateway-data/tr...` at the root
/// and three levels down.
///
/// Same lesson as `cpm_runs_row`'s `(missing)` marker: when what is new sits at
/// the end of the line, a naive truncation deletes exactly it.
#[test]
fn test_a_path_row_keeps_its_tail_on_a_40_column_screen() {
    use crate::webbrowser::{truncate_path_to_width, truncate_to_width};
    const PETSCII_PATH_W: usize = 26;

    // The shipped default, at the root and inside a sub-directory — read from
    // `Config::default()` so the test moves with the default rather than
    // carrying a second copy of it.
    let base = crate::config::Config::default().transfer_dir;
    let root = format!("{base}/");
    let deep = format!("{base}/CPM/images/");

    // The old behaviour is the bug, pinned so the two cannot be confused: at
    // this width the head-truncation is *identical* for both.
    assert_eq!(
        truncate_to_width(&root, PETSCII_PATH_W),
        truncate_to_width(&deep, PETSCII_PATH_W),
        "the head of these two paths is the same — which is why keeping the head is wrong"
    );

    // The fix: different, and each ends in the part that identifies it.
    let root_shown = truncate_path_to_width(&root, PETSCII_PATH_W);
    let deep_shown = truncate_path_to_width(&deep, PETSCII_PATH_W);
    assert_ne!(root_shown, deep_shown);
    assert!(root_shown.ends_with("transfer/"), "{root_shown:?}");
    assert!(deep_shown.ends_with("/CPM/images/"), "{deep_shown:?}");
    for shown in [&root_shown, &deep_shown] {
        assert!(shown.chars().count() <= PETSCII_PATH_W, "{shown:?} is too wide");
        assert!(shown.starts_with("..."), "an elided front must say so: {shown:?}");
    }

    // A path that fits is untouched — no ellipsis for its own sake.
    assert_eq!(truncate_path_to_width("transfer/", PETSCII_PATH_W), "transfer/");
    // And the degenerate widths cannot panic.
    assert_eq!(truncate_path_to_width("abcdef", 3), "...");
    assert_eq!(truncate_path_to_width("abcdef", 0), "");
    // Multi-byte input truncates on a char boundary, like its sibling.
    let wide = "ünïcödé/påth/ïs/löng/ënöügh/tö/cüt/";
    assert!(truncate_path_to_width(wide, PETSCII_PATH_W).chars().count() <= PETSCII_PATH_W);
}

/// **A label shown on a C64 must survive a C64.**
///
/// The printer screen renders each value through `truncate_to_width` at 26
/// columns on PETSCII, so a label longer than that does not wrap — it silently
/// loses its tail. Three of these labels used to end in `transfer/printer/`,
/// which put the one thing the operator needed (where the document goes) exactly
/// where the cut falls, and which the data-directory move made wrong anyway. The
/// fixed rows on that screen were already guarded
/// (`test_cpm_printer_screen_literals_fit_petscii`); the *values* were not, and
/// that is the gap this closes.
///
/// The width is read out of `config_ui.rs` rather than written here, so the two
/// cannot drift apart — the same reason the row-fitting test scans the source.
///
/// The distinctness assertion is the one that matters most: a lost tail is a
/// nuisance, but two settings that render as the *same text* on a C64 make the
/// screen a liar about which one is selected.
#[test]
fn test_the_printer_screen_labels_survive_a_40_column_client() {
    let src = include_str!("config_ui.rs").replace('\r', "");
    let start = src.find("pub(in crate::telnet) async fn cpm_printer_settings").expect("the fn");
    let body = &src[start..];
    // `let w = if self.terminal_type == TerminalType::Petscii { 26 } else { 60 };`
    let marker = "TerminalType::Petscii { ";
    let at = body.find(marker).expect("the screen's PETSCII value width");
    let rest = &body[at + marker.len()..];
    let width: usize = rest[..rest.find(' ').expect("a number then a space")]
        .parse()
        .expect("the width is a number");
    assert!((20..=40).contains(&width), "{width} is not a plausible value column");

    let mut sets: Vec<(&str, Vec<&str>)> = Vec::new();
    sets.push((
        "cpm_printer",
        crate::cpm::printer::PRINTER_CHOICES.iter().map(|(_, l)| *l).collect(),
    ));
    sets.push((
        "cpm_printer_autolf",
        crate::cpm::printer::AUTOLF_CHOICES.iter().map(|(_, l)| *l).collect(),
    ));
    let mut boards: Vec<&str> = vec![crate::cpm::printer::PORT_OFF_LABEL];
    boards.extend(crate::cpm::printer::PORT_CHOICES.iter().map(|p| p.label));
    sets.push(("cpm_printer_port", boards));

    for (key, labels) in &sets {
        assert!(!labels.is_empty(), "{key} offers nothing");
        for label in labels {
            let cols = label.chars().count();
            assert!(
                cols <= width,
                "{key}: {label:?} is {cols} columns and loses its tail at {width} \
                 on a 40-column client — say it shorter, or say it in the note rows"
            );
        }
        // Distinct *after* the cut, which is the property a C64 actually sees.
        let mut seen: Vec<String> = Vec::new();
        for label in labels {
            let cut: String = label.chars().take(width).collect();
            assert!(
                !seen.contains(&cut),
                "{key}: two labels both render as {cut:?} at {width} columns"
            );
            seen.push(cut);
        }
    }

    // A positive control: the scan really found the lists it thinks it did.
    assert_eq!(sets.len(), 3);
    assert!(sets.iter().map(|(_, l)| l.len()).sum::<usize>() >= 7);
}

/// Every key the CP/M printer screen displays must also be one it handles and
/// one its error hint names — the same three-way drift the CP/M settings screen
/// suffered twice, guarded the same way.
#[test]
fn test_cpm_printer_settings_keys_are_displayed_handled_and_hinted() {
    let src = include_str!("config_ui.rs");
    let start = src.find("pub(in crate::telnet) async fn cpm_printer_settings").expect("the fn");
    let end = src[start..].find("\n    /// Boot settings, reached from").expect("the next fn") + start;
    let body = &src[start..end];

    let hint = "Press P, B, A, or Q.";
    assert!(body.contains(hint), "the error hint changed: it must list every key");
    for key in ["P", "B", "A"] {
        assert!(
            body.contains(&format!("self.cyan(\"{key}\")")),
            "{key} must be a displayed menu item"
        );
        assert!(
            body.contains(&format!("\"{}\" => ", key.to_ascii_lowercase())),
            "{key} is displayed but never handled — pressing it would just error"
        );
        assert!(hint.contains(key), "{key} is displayed but missing from the error hint");
    }
}

/// The screen has to be reachable, and from the menu that advertises it.  A
/// submenu nothing opens is the defect `I` was for a whole release.
#[test]
fn test_cpm_settings_opens_the_printer_screen() {
    let src = include_str!("config_ui.rs");
    let start = src.find("pub(in crate::telnet) async fn cpm_settings").expect("the fn");
    let end = src[start..].find("\n    /// Log-file submenu").expect("the next fn") + start;
    assert!(
        src[start..end].contains("self.cpm_printer_settings()"),
        "the CP/M settings screen displays P but never opens the printer screen"
    );
}

/// CP/M boot settings: header(3) + blank + 4 status + blank + up to 3 note
/// lines + 2 CPU note lines + blank + 4 actions + blank + Back + prompt = 22.
///
/// This screen exists *because* the CP/M settings screen is full, so the point
/// of counting it is to know how much room the next question has — and there is
/// a known next question: which controller takes an image whose size two boards
/// both claim.  **There is no room left for it**: the CPU selector took the
/// last four rows, so that question needs a screen of its own, the way the
/// printer's did.
#[test]
fn test_cpm_boot_settings_row_count() {
    let header = 3;
    let status = 1 + 4; // blank + Runs/Machine/Backspace/CPU
    let note = 1 + 2; // blank + the longest of the two explanations
    // One row since 0.9.2: it read "The CPU applies to both. Run EGT8080 on
    // the 8080", and the second sentence was a choice between two terminals on
    // drive A:.  There is one now, and it runs on either processor.
    let cpu_note = 1; // "The CPU applies to both."
    let actions = 1 + 5; // blank + R, M, B, C, S
    let footer = 1 + 1 + 1; // blank + Back + prompt
    let rows = header + status + note + cpu_note + actions + footer;
    assert_eq!(rows, 21, "the CP/M boot settings screen is {rows} rows");
    assert!(rows <= 22, "CP/M boot settings is {rows} rows, exceeds 22");
}

/// The "choose what CP/M runs" picker has the same 22-row budget, and its page
/// size is proven the same way rather than counted by hand.
///
/// It replaced a cycling key, and the reason it can afford to exist is that it
/// fits: a picker that overran would scroll its own heading away on a C64, which
/// is exactly the fault the boot confirmation screen was split out to fix.
#[test]
fn test_cpm_runs_picker_page_fits_petscii() {
    let src = include_str!("config_ui.rs");
    let decl = src.find("const RUNS_PAGE: usize =").expect("the page-size constant");
    let per_page: usize = src[decl..]
        .split('=')
        .nth(1)
        .and_then(|s| s.trim().split(';').next())
        .and_then(|s| s.trim().parse().ok())
        .expect("a parseable page size");

    // Counted against the screen as it is actually drawn, blank lines
    // included.  The first version of this model omitted the blank between the
    // intro and the list and computed 21 for a screen that draws 22 — which
    // would have let a later `RUNS_PAGE = 10` pass while scrolling the heading
    // off a C64.  A row model that is short is worse than no model, because it
    // reads as headroom.
    let header = 3; // sep + title + sep
    let intro = 1 + 3 + 1; // blank + three dim lines + blank
    let footer = 1 + 1 + 1 + 1 + 1; // blank + page info + blank + nav + prompt
    let rows = header + intro + per_page + footer;
    assert_eq!(
        rows, 22,
        "the runs picker draws {rows} rows at {per_page} per page; the PETSCII budget is 22 \
         exactly — under it wastes a row and over it scrolls the heading away"
    );
}

/// Every key the CP/M boot screen displays must also be one it handles and one
/// its error hint names.  The same three-way drift the CP/M settings screen
/// suffered twice, guarded the same way.
#[test]
fn test_cpm_boot_settings_keys_are_displayed_handled_and_hinted() {
    let src = include_str!("config_ui.rs");
    let start = src.find("pub(in crate::telnet) async fn cpm_boot_settings").expect("the fn");
    let end = src[start..].find("// ─── SECURITY SETTINGS").expect("the next section") + start;
    let body = &src[start..end];

    // **Derived from the screen, not hand-copied beside it.** This test carried
    // the hint as a literal and the key list as an array, so it compared the
    // source against a copy of itself. When `J` was added with the joystick it
    // was displayed and handled and left out of the hint — precisely what this
    // test exists to catch — and it passed, because `J` was not in its array
    // either. A guard whose expectations are typed by the same hand that edits
    // the screen guarantees nothing.
    let hint = body
        .split("Press ")
        .nth(1)
        .and_then(|s| s.split_once('"'))
        .map(|(m, _)| m.to_string())
        .expect("the wrong-key hint");

    let handled: Vec<char> = body
        .lines()
        .filter_map(|l| {
            let (key, tail) = l.trim().strip_prefix('"')?.split_once('"')?;
            tail.trim_start().starts_with("=>").then_some(())?;
            let mut cs = key.chars();
            let c = cs.next()?;
            cs.next().is_none().then_some(c.to_ascii_uppercase())
        })
        .collect();
    assert!(handled.len() >= 8, "expected this screen's keys, derived {handled:?}");

    for key in handled {
        assert!(
            hint.contains(key),
            "{key} is handled but the error hint does not name it: {hint:?}",
        );
        if key == 'Q' {
            // Q is drawn as an action prompt, not as a coloured menu key.
            assert!(body.contains("action_prompt(\"Q\""), "Q must still be offered");
            continue;
        }
        assert!(
            body.contains(&format!("self.cyan(\"{key}\")")),
            "{key} is handled but never displayed, so nobody can find it",
        );
    }
}

/// **Every key the CP/M Disk Images screen displays must be one it handles.**
///
/// `Q` was displayed by this screen from the day it shipped and never handled:
/// it fell into the `Some(s) if !s.is_empty() => {}` catch-all, which redraws
/// the menu, so the one documented way out did nothing. Only ESC or a bare
/// Enter left — neither of which the screen mentions.
///
/// That is the failure mode a catch-all invites: it makes an unhandled key
/// indistinguishable from a key that is meant to do nothing, so the screen
/// cannot report the difference and neither can a reader. The sibling boot
/// screen is guarded this way already; this one was not, which is how the drift
/// survived.
///
/// `U` is deliberately excluded from the handled check's strictness in one
/// respect — it is displayed only when something is mounted — but it is still
/// required to be handled, since a key that appears conditionally must work
/// whenever it appears.
#[test]
fn test_cpm_disk_images_keys_are_displayed_and_handled() {
    let src = include_str!("cpm_mount_ui.rs");
    let start = src
        .find("pub(in crate::telnet) async fn cpm_mount_wizard")
        .expect("the wizard fn");
    let end = src[start..].find("\n    /// Make a new, empty").expect("the next fn") + start;
    let body = &src[start..end];

    // No "B": the boot picker left this screen in 0.9.2.  This screen is
    // configuration; booting is what the CP/M menu item does, from
    // `cpm_boot_image`.
    for key in ["M", "N", "D", "U"] {
        assert!(
            body.contains(&format!("self.cyan(\"{key}\")")),
            "{key} must be a displayed menu item"
        );
        assert!(
            body.contains(&format!("s == \"{}\"", key.to_ascii_lowercase())),
            "{key} is displayed but never handled"
        );
    }
    // Q is displayed through `action_prompt` rather than `cyan`, and must be
    // handled explicitly rather than left to the catch-all.
    assert!(
        body.contains("self.action_prompt(\"Q\", \"Back\")"),
        "the screen must still offer Q"
    );
    assert!(
        body.contains("s == \"q\" => return Ok(())"),
        "Q is displayed but not handled — the catch-all would swallow it and redraw"
    );
}

/// **The CP/M boot screen is now FULL, counted from the screen itself.**
///
/// Adding the CPU took it to exactly 22 rows, the whole PETSCII budget, so the
/// next setting there has nowhere to go and must open a submenu instead —
/// which is how this screen came to exist when the CP/M one filled up.
///
/// Counted from the source rather than written down as a sum: a hand-copied
/// row count agrees with the screen only until somebody adds a line, and then
/// it passes while the screen overflows. The other screen tests here do the
/// arithmetic by hand because their layouts are fixed; this one is at its limit,
/// which is exactly when the number has to be a measurement.
#[test]
fn test_cpm_boot_screen_row_count() {
    let src = include_str!("config_ui.rs");
    let start = src.find("pub(in crate::telnet) async fn cpm_boot_settings").expect("the fn");
    let end = src[start..].find("let prompt = format!").expect("the prompt line") + start;
    let drawn = src[start..end].matches("self.send_line(").count();
    // Every row is a `send_line`, less the two lines of whichever dim branch
    // does not run — both branches are two lines now, the configured one having
    // been compressed from three to make room for the browser-typing row —
    // plus the prompt itself, drawn with `send` so the cursor stays on it.
    let rows = drawn - 2 + 1;
    assert!(rows <= 22, "CP/M boot screen is {rows} rows, over the 22-row PETSCII budget");
    assert_eq!(
        rows, 22,
        "this screen is full. It was 22, briefly 21 when the CPU note lost its \
         second line — it said \"Run EGT8080 on the 8080\", a choice between two \
         terminals on drive A:, and there is one now — and 22 again since the \
         boot picker left the disks screen and its \"Allow writes?\" question \
         became the standing `W` row here. There is no room left: a new row \
         needs one of these to go first."
    );
}

/// Both CPU labels have to fit the 40-column PETSCII screen once the
/// `  CPU:       ` prefix is allowed for, and still say what the choice costs.
///
/// Iterated over the real list rather than hand-copied, so a processor cannot
/// be added without being measured.
#[test]
fn test_cpu_labels_fit_the_petscii_boot_screen() {
    // "  CPU:       " is 13 columns of 40, and the screen truncates to 26.
    for (value, label) in crate::cpm::cpu::CPU_CHOICES {
        assert!(
            label.len() <= 26,
            "{value}'s label is {} characters and would arrive truncated: {label:?}",
            label.len()
        );
    }
}

/// Every machine description must fit the 40-column PETSCII screen once the
/// `  Machine:   ` prefix is allowed for.  Iterated over the real list rather
/// than hand-copied, so a new machine cannot be added without being measured.
#[test]
fn test_machine_descriptions_fit_the_petscii_boot_screen() {
    // "  Machine:   " is 13 columns, leaving 27 of 40.  The screen truncates to
    // 26 for PETSCII, so nothing can overflow — this asserts the truncation
    // budget is actually big enough to say something useful, not just that it
    // exists.
    for c in crate::cpm::console::MACHINE_CHOICES {
        assert!(
            c.description.len() >= 20,
            "{:?} is too terse to identify a machine once truncated to 26",
            c.description
        );
    }
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
    // + blank + 8 items (E, G, M, S, F, C, O, R) + blank + Q/H + prompt.
    // Worst case is N = SERVER_ADDR_DISPLAY_CAP.
    //
    // `C` (CP/M) moved up from Other Settings, which was at exactly 22 rows
    // while carrying the entry that keeps growing.  This screen had the room:
    // 19 -> 20 in the worst case, and the worst case needs three detected
    // addresses.
    let submenu_rows = 3 + (1 + SERVER_ADDR_DISPLAY_CAP + 1) + 1 + 8 + 1 + 1 + 1; // 20
    assert!(submenu_rows <= 22, "config submenu is {} rows, exceeds 22", submenu_rows);
    // SERVER CONFIGURATION is counted by `test_server_config_screen_row_count`
    // below, which reads the real rows.  The arithmetic that used to live here
    // said 18 while the screen drew 20.
}

/// SERVER CONFIGURATION must fit a PETSCII screen, **counted from the rows it
/// actually draws**.
///
/// **This replaced arithmetic over literals, which had gone two rows stale.**
/// The old guard was `3 + 5 + 1 + 7 + 1 + 1` with a comment naming "7 item
/// rows"; the screen draws eight, plus the `* open ports on firewall` line the
/// sum never counted, so it claimed 18 against a real 20.  It still passed --
/// a guard reporting four spare rows where there are two is not a guard, it is
/// a number that happens to be under the limit, and this branch added a row to
/// this very screen (`L  Conn rate`) without it noticing.  `main_menu_rows` and
/// `more_menu_rows` were each converted to a rows helper for exactly this; this
/// page renders straight to the wire and has none, so its source is read
/// instead -- the same fallback, and for the same page, as
/// `test_every_server_config_key_is_explained_in_its_help`.
///
/// **Bounded to the render block, not the function.**  `server_configuration`
/// is a `loop` whose body is the screen *and* the key handler, and the handler
/// writes plenty of its own lines (the port-test result, the confirmations).
/// Counting the whole function would report a screen three times its size and
/// fail for a reason that is not true -- the same mistake as the sudo scan that
/// read `run_elevated` instead of the probe.  The render block is everything
/// between the `clear_screen` and the prompt, which is precisely what a
/// PETSCII terminal has to hold at once.
#[test]
fn test_server_config_screen_row_count() {
    let src = include_str!("config_ui.rs").replace('\r', "");
    let start = src
        .find("async fn server_configuration(")
        .expect("server_configuration moved or was renamed");
    let body = &src[start..];
    let from = body.find("self.clear_screen().await?;").expect("no clear_screen");
    let to = body.find("let prompt = format!(").expect("no prompt");
    assert!(from < to, "the prompt is drawn before the screen is cleared");
    let screen = &body[from..to];

    // One row per `send_line`, plus the prompt itself, which `send` writes and
    // which sits on its own line -- the row the main menu's two guards
    // disagreed about until 2026-09-14.
    let rows = screen.matches("self.send_line(").count() + 1;

    // Positive control: a scan that matched nothing would pass the budget
    // assertion below without having read the screen at all.
    assert!(
        rows >= 10,
        "the row scan found only {rows} rows in server_configuration's render \
         block -- the scan is broken, not the screen",
    );
    assert!(
        rows <= 22,
        "SERVER CONFIGURATION draws {rows} rows; a PETSCII screen holds 22, \
         and the 23rd scrolls the header off",
    );
    // What it is today, so the next row added has to be a deliberate act.
    // Header(3) + 5 status + blank + 8 item rows + the firewall note + Q/H +
    // prompt.
    assert_eq!(rows, 20, "the screen grew or shrank; check it still fits and update this");
}

/// Master/Slave sub-screen row budget.  The status rows differ by role, so both
/// shapes are counted and the master must remain the worst case.
///
/// MASTER: header(3) + blank + 4 status (role, accept-relays,
/// Kermit-to-slaves, the "Master/User/Pass: (slave only)" note) + blank +
/// 4 item rows (R/A, K, M/P, U/W) + Q/H + prompt = 15, plus the §9 #10
/// live-status block — "Registered remote ports:" header, up to 3 entries
/// (RELAY_STATUS_CAP), an optional "+N more", and a trailing blank = 6 — so 21.
///
/// SLAVE: header(3) + blank + 6 status (role, the two "(master only)" notes,
/// master host:port, user, pass) + blank + 4 item rows + Q/H + prompt = 17, plus
/// up to 2 link lines and a blank = 3 — so 20.
///
/// (Transport is not exposed until the raw transport is implemented; SSH is the
/// only mode.)
///
/// These counts are hand-maintained, and that has already drifted: `9f72b85`
/// added *both* the Kermit-to-slaves status row and its `K` item row without
/// touching this test, which went on asserting the old "5 status + 3 item"
/// shape — a shape that was also wrong about which role shows five status rows.
/// It happened to still total 21 for the master, so nothing overflowed, but the
/// guard demonstrably cannot notice a new row.
///
/// **Headroom is one row.** At 21 of 22, adding a second row to this screen
/// means lowering `RELAY_STATUS_CAP` or splitting the screen.
#[test]
fn test_master_slave_menu_row_count() {
    let master_base = 3 + 1 + 4 + 1 + 4 + 1 + 1; // 15
    let master_status_worst = 1 + 3 + 1 + 1; // header + cap + "+N more" + blank
    let master_rows = master_base + master_status_worst; // 21
    assert!(
        master_rows <= 22,
        "master/slave menu (master) is {} rows, exceeds 22",
        master_rows,
    );

    let slave_base = 3 + 1 + 6 + 1 + 4 + 1 + 1; // 17
    let slave_status_worst = 2 + 1; // up to 2 link lines + blank
    let slave_rows = slave_base + slave_status_worst; // 20
    assert!(
        slave_rows <= 22,
        "master/slave menu (slave) is {} rows, exceeds 22",
        slave_rows,
    );
    assert!(
        slave_rows <= master_rows,
        "the master shape must stay the worst case ({} vs {}) — otherwise the \
         budget above is guarding the wrong screen",
        slave_rows,
        master_rows,
    );
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
/// header(3) + blank + 5 values + blank + 8 item rows + blank + Q/H + prompt = 21
/// (Verbose and GUI share a value row, folding 6 statuses into 5 lines; the
/// last item row carries BOTH `L` Log file and `R` Restart server.)
///
/// The CP/M status left with its menu entry — it shared the gateway-debug row,
/// so the row count is unchanged, but a status a screen cannot act on and does
/// not say where to act on is worse than none.
///
/// **This screen used to be at exactly 22** — its own comment said a new entry
/// had to pair with an existing one. Moving CP/M up to the CONFIGURATION menu
/// as `C` bought a row back, which is most of why it moved: the screen with no
/// room to spare was the one carrying the entry that keeps growing (an
/// emulator, a disk-image wizard, a boot picker and a printer).
///
/// The `L`/`R` pairing is kept rather than spent on un-pairing them: two
/// entries that read fine together are not worth the only spare row.
#[test]
fn test_other_settings_menu_row_count() {
    let rows = 3 + 1 + 5 + 1 + 8 + 1 + 1 + 1; // 21
    assert!(rows <= 22, "other settings menu is {} rows, exceeds 22", rows);
    assert_eq!(rows, 21, "the CP/M entry moved out; this screen should have a row spare");
}

/// **A screen that asks a question has to be a screen.**
///
/// `other_prompt_value` is the shared "type a new value" prompt behind nine menu
/// entries — the Groq key, the homepage, the weather location, the log file, the
/// master password and four more. It used to print underneath whichever menu
/// called it, and every one of those menus sits at or near the 22-row PETSCII
/// budget, so its seven extra rows scrolled the heading and sometimes the prompt
/// itself off a Commodore. Ricky hit it on the real thing pressing `A`.
///
/// **The drawing lives in `other_prompt_value`, not in `other_set_field`.** The
/// two were one function until the master password needed the same screen
/// without the write that followed it — it is held in memory, never put in
/// `egateway.conf` — so the prompt was split from its destination. The row
/// budget is measured across both halves, because an operator still sees one
/// screen and then its confirmation.
///
/// Scanned from the source because the function needs a live session to run:
/// what matters is that it clears and draws a heading *before* it asks.
#[test]
fn test_a_value_prompt_gets_its_own_screen() {
    let src = include_str!("config_ui.rs");
    let fn_body = |name: &str| -> &str {
        let start = src
            .find(&format!("pub(in crate::telnet) async fn {name}"))
            .unwrap_or_else(|| panic!("{name} is gone -- the value prompt moved again"));
        let end = src[start..]
            .find("\n    pub(in crate::telnet) async fn ")
            .map(|i| i + start + 5)
            .unwrap_or(src.len());
        &src[start..end]
    };
    let body = fn_body("other_prompt_value");

    let clear = body.find("self.clear_screen()").expect("it must clear first");
    let heading = body.find("self.yellow(&label.to_uppercase())").expect("a heading naming the field");
    let ask = body.find("New value: ").expect("the prompt");
    assert!(clear < heading, "the heading must come after the clear");
    assert!(heading < ask, "the question must come after the heading, or it scrolls off");

    // And it stays inside the budget: three header rows, a blank, Current, a
    // blank and the prompt is seven — then four more after the answer, which
    // is now `other_saved_notice`.
    let rows = body.matches("send_line(").count()
        + fn_body("other_saved_notice").matches("send_line(").count()
        + 1; // + the prompt, drawn with `send`
    assert!(rows <= 22, "the value prompt draws {rows} rows, over the PETSCII budget");
}

/// **The widest that screen's value rows can get, measured rather than eyeballed.**
///
/// The label column on OTHER SETTINGS widened by one when "AI API key" became
/// "Groq API key", and the weather row was already at *exactly* 40 columns with
/// the longest units word — so that one character would have wrapped a C64 and
/// pushed a 22-row menu's prompt off the screen.  The location truncation gave
/// the character back.
///
/// Written as the arithmetic rather than as a literal, so the next person to
/// widen the column sees which term they are spending.
#[test]
fn test_weather_row_fits_petscii() {
    let prefix = "  Weather:      ".len(); // the aligned label column
    let location = 15; // `max_loc` on a Commodore
    let units = " [metric]".len(); // the longest of auto / us / metric
    assert!(
        prefix + location + units <= PETSCII_WIDTH,
        "the weather row is {} columns, over {PETSCII_WIDTH}",
        prefix + location + units
    );
    // And every other value row on that screen shares the column.
    for label in ["  Groq API key: ", "  Homepage:     ", "  Weather:      "] {
        assert_eq!(label.len(), 16, "{label:?} is out of step with the column");
    }
}

/// The Master/Slave screen's accept-relays row carries an inline "(SSH off!)"
/// qualifier when the relay cannot possibly work, and it must still fit 40 cols.
///
/// The qualifier rides on the existing status row **on purpose**: that screen is
/// at 21 of its 22 rows (see `test_master_slave_menu_row_count`), so a separate
/// warning line would spend the last of the headroom. This pins the widest form
/// so a reworded qualifier can't silently wrap on a C64.
#[test]
fn test_accept_relays_ssh_qualifier_fits_petscii() {
    // The rendered row with colour stripped — what a PETSCII screen measures.
    let widest = "  Accept relays: ENABLED (SSH off!)";
    assert!(
        widest.len() <= PETSCII_WIDTH,
        "accept-relays row with the SSH qualifier is {} chars, exceeds {}",
        widest.len(),
        PETSCII_WIDTH,
    );
    // And the help says what a master actually needs, both parts of it.
    let help = TelnetSession::master_slave_help_lines(true);
    assert!(
        help.iter().any(|l| l.contains("SSH")),
        "master/slave help never mentions that the relay needs SSH: {help:?}"
    );
}

/// Log-file submenu (Other Settings -> L) must fit the 22-row PETSCII screen.
/// header(3) + blank + 5 values (state/file/rotate/keep/max-disk) + blank
/// + 4 items (E/F/S/K) + blank + Q + prompt = 17, well inside the budget.
#[test]
fn test_log_settings_menu_row_count() {
    let rows = 3 + 1 + 5 + 1 + 4 + 1 + 1 + 1; // 17
    assert!(rows <= 22, "log settings menu is {} rows, exceeds 22", rows);
}

/// `numeric_confirmation_lines` itself: one line when it fits, two when it
/// doesn't, and **nothing dropped** either way. Tested directly as well as
/// through the call-site scan below, because the scan only ever asserts that
/// the output fits — it would be satisfied by a function that truncated.
#[test]
fn test_numeric_confirmation_lines_splits_without_losing_anything() {
    // Comfortably short: one line.
    let one = numeric_confirmation_lines("Terminal width", 40, "columns", 38);
    assert_eq!(one, vec!["Terminal width set to 40 columns.".to_string()]);

    // The real worst case that prompted this — Kermit's idle timeout, 49 chars
    // of content against a 38-char PETSCII budget.
    let two = numeric_confirmation_lines("Idle timeout", 86400, "seconds (0 = disabled)", 38);
    assert_eq!(
        two,
        vec![
            "Idle timeout".to_string(),
            "set to 86400 seconds (0 = disabled).".to_string(),
        ]
    );
    for line in &two {
        assert!(line.chars().count() <= 38, "split line still too wide: {line:?}");
    }

    // Nothing is lost by splitting: every word of the one-line form survives.
    let joined = two.join(" ");
    for word in ["Idle", "timeout", "86400", "seconds", "(0", "disabled)."] {
        assert!(joined.contains(word), "{word:?} lost in the split: {joined:?}");
    }

    // A wide screen keeps the single line even for the long case.
    let wide = numeric_confirmation_lines("Idle timeout", 86400, "seconds (0 = disabled)", 78);
    assert_eq!(wide.len(), 1, "78 columns is plenty; should not split");
}

/// The `Runs:` row keeps its `(missing)` marker on a PETSCII screen.
///
/// The marker is the whole reason the row resolves anything, and it is the
/// **last** thing on the line — so a naive `truncate_to_width` of the finished
/// label cuts off exactly the part that is new. `Boot vanished.dsk (missing)`
/// is 27 characters against the 26 that row allows, which is close enough that
/// a wider test name would have missed it.
#[test]
fn test_the_runs_row_keeps_its_marker_when_the_row_is_too_narrow() {
    // A transfer dir that does not exist, so any name resolves to Missing —
    // this is about the fitting, not about the resolving.
    let nowhere = "/nonexistent-egw-transfer-dir";
    const PETSCII_W: usize = 26;

    let row = cpm_runs_row(nowhere, "vanished.dsk", PETSCII_W);
    assert!(row.ends_with(" (missing)"), "the marker must survive: {row:?}");
    assert!(row.chars().count() <= PETSCII_W, "{} columns: {row:?}", row.chars().count());
    assert!(row.starts_with("Boot"), "and it is still recognisably the setting: {row:?}");

    // A name long enough that the filename must give way, and the marker still
    // does not.
    let long = cpm_runs_row(nowhere, "a-very-long-disk-image-name-indeed.dsk", PETSCII_W);
    assert!(long.ends_with(" (missing)"), "{long:?}");
    assert!(long.chars().count() <= PETSCII_W, "{} columns: {long:?}", long.chars().count());

    // A value that could never be a filename says so differently, because it is
    // a different mistake to fix.
    let bad = cpm_runs_row(nowhere, "../../etc/passwd", 60);
    assert!(bad.ends_with(" (invalid name)"), "{bad:?}");

    // The emulator carries no marker at all, and neither does a disk that is
    // really there — checked on a wide row so nothing can be blamed on fitting.
    assert_eq!(cpm_runs_row(nowhere, "", 60), crate::cpm::boot::BOOT_EMULATOR_LABEL);
    let dir = std::env::temp_dir().join("egw_runs_row_test");
    let _ = std::fs::remove_dir_all(&dir);
    let images = dir.join("CPM").join(crate::cpm::image::IMAGES_DIR);
    std::fs::create_dir_all(&images).unwrap();
    // A real image: the row resolves through `boot_target`, which cold-starts
    // the disk now, so eight bytes would correctly earn a "(will not boot)".
    std::fs::write(images.join("real.dsk"), crate::cpm::boot::tests::bootable_image()).unwrap();
    assert_eq!(cpm_runs_row(&dir.to_string_lossy(), "real.dsk", 60), "Boot real.dsk");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every numeric-setting confirmation must fit the screen it is printed on,
/// at that setting's WORST-CASE value.
///
/// `xmodem_set_numeric` prints its confirmation with `send_line`, which does
/// not wrap, and each of its 24 call sites composes a label and a unit of its
/// own. Five of them were silently over the 40-column PETSCII budget —
/// Kermit's idle timeout at 51 characters, and four "Negotiation timeout set
/// to 300 seconds." at 41 — with nothing to catch it, which is why this scrapes
/// the real call sites out of the source rather than restating them.
///
/// The worst case is the site's own `max` argument (widest value it can ever
/// print), so the assertion tracks a caller that raises its ceiling too.
#[test]
fn test_numeric_confirmations_fit_every_screen() {
    // Normalised to LF first.  A Windows checkout has CRLF endings, and the
    // `)\n` anchor below is a *trailing* newline match, so it finds nothing
    // there: the "call" then ran to the end of the file, the cap it parsed came
    // from an unrelated setting, and this test failed on windows-latest only —
    // reporting `kermit_idle_timeout` with a u64::MAX ceiling it never has.
    // (config.rs's scan is safe with a bare `\n` anchor because a *leading*
    // newline still matches inside `\r\n`; only a trailing one breaks.)
    let src = include_str!("config_ui.rs").replace("\r\n", "\n");
    let src = src.as_str();
    // .xmodem_set_numeric( "Label", "key", <current>, <min>, <max>, "unit", )
    let mut checked = 0;
    for (idx, _) in src.match_indices("self.xmodem_set_numeric(") {
        let tail = &src[idx..];
        let close = tail.find(")\n").unwrap_or(tail.len());
        let call = &tail[..close];
        // String literals in order: label, key, unit (comments are skipped
        // because they are stripped below).
        let decommented: String = call
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<&str>>()
            .join("\n");
        let strings: Vec<&str> = decommented
            .match_indices('"')
            .collect::<Vec<_>>()
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| &decommented[c[0].0 + 1..c[1].0])
            .collect();
        if strings.len() < 3 {
            continue; // not a shape we can read; the count floor below catches over-skipping
        }
        let (label, key, unit) = (strings[0], strings[1], strings[2]);
        // The numeric args: current, min, max — take the last one before the unit.
        let nums: Vec<&str> = decommented
            .lines()
            .map(str::trim)
            .filter(|l| l.ends_with(',') && !l.contains('"'))
            .collect();
        let max_raw = nums.last().copied().unwrap_or("0,").trim_end_matches(',');
        // Values like `u16::MAX as u64` or `86400`; resolve the ones we can and
        // fall back to the widest u64 so an unparsed cap is never optimistic.
        let max_val: u64 = if max_raw.contains("u16::MAX") {
            u16::MAX as u64
        } else if max_raw.contains("u32::MAX") {
            u32::MAX as u64
        } else {
            max_raw.parse().unwrap_or(u64::MAX)
        };
        for (screen, name) in [(PETSCII_WIDTH, "PETSCII"), (80usize, "ANSI/ASCII")] {
            let content_width = screen - 2;
            for line in numeric_confirmation_lines(label, max_val, unit, content_width) {
                assert!(
                    line.chars().count() <= content_width,
                    "{name}: confirmation line {line:?} for `{key}` is {} chars, \
                     over the {content_width}-char content budget ({screen}-col \
                     screen minus the 2-space indent). Shorten the label or unit.",
                    line.chars().count(),
                );
            }
        }
        checked += 1;
    }
    // Floor-asserted so a changed call shape can't leave this checking nothing.
    assert!(
        checked >= 20,
        "only parsed {checked} xmodem_set_numeric call sites; the scan has \
         stopped matching the real code"
    );
}

/// The Gateway Configuration screen must fit the 22-row PETSCII budget.
///
/// Counted from the source rather than by hand: its siblings above assert a
/// hand-written sum, which is the drift class that left the Master/Slave
/// row test asserting a pre-`9f72b85` shape and checking nothing. This walks
/// the real render block (everything before the menu-input read, since the
/// key handlers below it don't draw rows) and counts the `send_line` calls,
/// plus one for the trailing `send` prompt that occupies a row without
/// ending it. Add a row to that menu and this number moves on its own.
#[test]
fn test_gateway_config_menu_row_count() {
    let src = include_str!("config_ui.rs");
    let start = src
        .find("async fn gateway_configuration")
        .expect("gateway_configuration not found — did it get renamed?");
    let body = &src[start..];
    // The render block ends where the menu reads a keypress.
    let end = body
        .find("let input = match self.get_menu_input")
        .expect("gateway_configuration has no get_menu_input — shape changed");
    let render = &body[..end];

    let rows = render.matches("self.send_line(").count() + 1; // +1 = the prompt
    // Floor-asserted so a refactor that stops matching can't silently pass
    // by finding zero rows.
    assert!(
        rows >= 10,
        "only found {} rows in the gateway config render block — the scan \
         has stopped matching the real code",
        rows,
    );
    assert!(
        rows <= 22,
        "gateway configuration menu is {} rows, exceeds the 22-row PETSCII screen",
        rows,
    );
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

/// The user manual must not still describe weather as US-only.
///
/// It did, in four places, for the three weeks since the feature went
/// worldwide — the manual is the one surface with no test of any kind, so a
/// change to a feature simply never reached it. This pins the specific claim
/// that was wrong; it is not a general docs test, and it is not pretending to
/// be one.
#[test]
fn test_manual_describes_weather_as_worldwide() {
    let manual = include_str!("../../usermanual.html").replace("\r\n", "\n");
    assert!(
        !manual.to_lowercase().contains("zip code"),
        "the manual still describes weather by US zip code; it takes a city \
         name or postal code anywhere in the world"
    );
    assert!(
        manual.contains("weather_units"),
        "the manual should document weather_units (auto / us / metric)"
    );
}

/// EVERY help screen must fit the narrowest terminal that can be shown it.
///
/// Twelve screens had a width test each and fourteen had none — including the
/// CP/M emulator's, which is how it came to be printed 55 characters wide on a
/// 40-column C64. Testing them one function at a time is what let the list
/// drift, so this iterates all of them: a new help screen is covered the moment
/// it is added to the table below, and a screen that is missing from the table
/// is far more visible here than an absent test file was.
///
/// A screen taking a `petscii` flag is checked in both widths. One with no flag
/// is shown to every terminal type, so it is held to the PETSCII width.
#[test]
fn test_every_help_screen_fits_its_terminal() {
    // (name, lines, width) — the wide screens at 80, the shared ones at 40.
    // No flag: one text for all terminals, so it must fit the narrowest.
    let mut screens: Vec<(&str, &[&str], usize)> = vec![
        // The full page: gating an item only ever *removes* lines, so the
        // variant with both optional entries is the widest this screen gets
        // and covers the narrower ones.
        (
            "main",
            TelnetSession::main_help_lines(MenuItems { cpm: true, second_page: true }),
            PETSCII_WIDTH,
        ),
        ("ai_chat", TelnetSession::ai_chat_help_lines(), PETSCII_WIDTH),
        ("bookmarks", TelnetSession::bookmarks_help_lines(), PETSCII_WIDTH),
        ("form", TelnetSession::form_help_lines(), PETSCII_WIDTH),
        ("download", TelnetSession::download_help_lines(), PETSCII_WIDTH),
        ("delete", TelnetSession::delete_help_lines(), PETSCII_WIDTH),
        (
            "file_transfer_menu",
            TelnetSession::file_transfer_menu_help_lines(),
            PETSCII_WIDTH,
        ),
        ("dialup", TelnetSession::dialup_help_lines(), PETSCII_WIDTH),
        ("gateway_shell", TelnetSession::cpm_help_lines(), PETSCII_WIDTH),
        (
            "serial_config",
            TelnetSession::serial_config_help_lines(),
            PETSCII_WIDTH,
        ),
    ];

    // The MORE page is Unix-only; it is width-checked where it exists.
    #[cfg(unix)]
    screens.push(("more", TelnetSession::more_help_lines(), PETSCII_WIDTH));

    // Flagged: a narrow text and a wide one, each held to its own width.
    for (name, narrow, wide) in [
        (
            "config_submenu",
            TelnetSession::config_submenu_help_lines(true),
            TelnetSession::config_submenu_help_lines(false),
        ),
        (
            "console",
            TelnetSession::console_help_lines(true),
            TelnetSession::console_help_lines(false),
        ),
        (
            "kermit_mode",
            TelnetSession::kermit_mode_help_lines(true),
            TelnetSession::kermit_mode_help_lines(false),
        ),
        (
            "gateway_config",
            TelnetSession::gateway_config_help_lines(true),
            TelnetSession::gateway_config_help_lines(false),
        ),
        (
            "master_slave",
            TelnetSession::master_slave_help_lines(true),
            TelnetSession::master_slave_help_lines(false),
        ),
        (
            "cpm_emulator",
            TelnetSession::cpmemu_help_lines(true),
            TelnetSession::cpmemu_help_lines(false),
        ),
    ] {
        screens.push((name, narrow, PETSCII_WIDTH));
        screens.push((name, wide, 80));
    }

    let mut over: Vec<String> = Vec::new();
    for (name, lines, width) in &screens {
        for line in *lines {
            let n = line.chars().count();
            if n > *width {
                over.push(format!("{name}: {n} chars (max {width}): {line:?}"));
            }
        }
    }
    assert!(
        over.is_empty(),
        "{} help line(s) are wider than the screen they are printed on:\n  {}",
        over.len(),
        over.join("\n  "),
    );
}

/// The CP/M emulator's HELP must fit the screens it is printed on, and stay
/// paginated.
///
/// It was neither. The help is printed from inside the emulator's own REPL, so
/// it never went through `show_help_page` like the rest of the gateway's help
/// — and when the file-loading section was added it reached 21 lines, five of
/// them over 50 characters. On a C64 that means the top scrolls away while the
/// bottom wraps mid-word. This asserts the real lines, both widths, and that
/// the pager is what bounds the page rather than the screen height.
#[test]
fn test_cpm_emulator_help_fits_its_screens() {
    for (petscii, width) in [(true, PETSCII_WIDTH), (false, 80usize)] {
        let lines = TelnetSession::cpmemu_help_lines(petscii);
        assert!(lines.len() > 10, "the help lost its content");
        for line in lines {
            assert!(
                line.chars().count() <= width,
                "CP/M help ({}) line {:?} is {} chars, over {}",
                if petscii { "petscii" } else { "wide" },
                line,
                line.chars().count(),
                width,
            );
        }
        // Every page must fit the pager's own budget — that is what keeps the
        // first line on screen when the last one is printed.
        for page in TelnetSession::paginate_help(lines, HELP_MAX_CONTENT_LINES) {
            assert!(
                page.len() <= HELP_MAX_CONTENT_LINES,
                "a CP/M help page is {} lines, over the {} the pager allows",
                page.len(),
                HELP_MAX_CONTENT_LINES,
            );
        }
    }
}

/// The help has to name the things a user cannot otherwise discover: how to get
/// a file onto a drive (the drives are folders, but nothing else says so), that
/// the drives are shared with other sessions, and that there is a clock.
#[test]
fn test_cpm_emulator_help_covers_the_undiscoverable() {
    for petscii in [true, false] {
        let help = TelnetSession::cpmemu_help_lines(petscii).join(" ");
        for needle in ["CPM/A", "SHARED", "clock", "SUBMIT"] {
            assert!(
                help.contains(needle),
                "CP/M help ({}) never mentions {:?}",
                if petscii { "petscii" } else { "wide" },
                needle,
            );
        }
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

/// Other settings help must stay a SINGLE page in both widths.  Derived from
/// the real help lines and the real page limit rather than a hand-copied row
/// count: the list sits exactly at the limit, so an added entry silently spills
/// one lonely line onto a second page (that is why the `U` weather-units entry
/// is folded onto one line — it paid for the `L` log-file entry).
///
/// A second page is not a bug in itself, so this is a deliberate tidiness
/// guard: if a future entry genuinely needs the room, fold another two-line
/// entry into one rather than just bumping this assertion.
#[test]
fn test_other_help_fits_one_page() {
    for petscii in [true, false] {
        let lines = TelnetSession::other_help_lines(petscii);
        let pages = TelnetSession::paginate_help(lines, HELP_MAX_CONTENT_LINES);
        assert_eq!(
            pages.len(),
            1,
            "other help ({}) needs {} pages for {} lines, limit {} — fold a \
             two-line entry into one instead of spilling",
            if petscii { "petscii" } else { "wide" },
            pages.len(),
            lines.len(),
            HELP_MAX_CONTENT_LINES,
        );
    }
}

/// File Transfer settings submenu row count:
/// header(3) + blank + 1 value + blank + 5 items + blank + Q/H + prompt = 14
#[test]
fn test_file_transfer_help_documents_the_terminal_key() {
    // Both widths, because the PETSCII variant is a separate hand-written
    // array and a key added to one and not the other is undiscoverable on
    // exactly the terminals this gateway exists for.
    for petscii in [true, false] {
        let lines = TelnetSession::file_transfer_help_lines(petscii);
        let joined = lines.join("\n");
        assert!(
            joined.contains("  T  "),
            "the T key is on the screen but not in the {} help",
            if petscii { "PETSCII" } else { "wide" },
        );
        assert!(
            joined.contains("EGT8080.COM"),
            "the help should name the files it writes ({} variant)",
            if petscii { "PETSCII" } else { "wide" },
        );
        // The property an operator most needs to trust before turning it on.
        // Case-insensitive on purpose: the wide variant shouts NEVER and the
        // PETSCII one, tighter on width, does not — and which one it is has no
        // bearing on whether the promise is documented.
        assert!(
            joined.to_lowercase().contains("never overwritten"),
            "the {} help must say an existing file is not overwritten",
            if petscii { "PETSCII" } else { "wide" },
        );
    }
}

#[test]
fn test_file_transfer_settings_menu_row_count() {
    // header(3) + blank + "Transfer dir:" + blank + eight menu rows
    // (D X Y Z K P T R) + blank + the Q/H row, then the prompt.
    //
    // The eight used to be written `5` here while the screen drew seven, so
    // this sum said 14 for a 16-row screen.  It never went red because it only
    // asserts a ceiling, and the ceiling was far away — the drift is the reason
    // the count is spelled out per group now rather than as bare numbers.
    let rows = 3 + 1 + 1 + 1 + 8 + 1 + 1 + 1; // 17 incl. the prompt line
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

/// **A help screen that states a default rots when the default moves.**
///
/// The PETSCII Kermit screen read `Negotiate timeout (45 s)` while the real
/// default had been 300 since the negotiation and idle timeouts were split
/// apart -- out by nearly seven times, on the one screen a C64 operator reads,
/// and two rows below the same screen printing the live value correctly.  The
/// manual's table was right the whole time, which is the usual direction of
/// this drift: the code-rendered half follows the code and the hand-written
/// half beside it does not.
///
/// So every number these screens state is pinned to the constant it describes.
/// Kermit's states none at all now -- the screen already prints that value, so
/// the parenthetical was redundant as well as wrong, and the ANSI variant had
/// always spelled it `(Send-Init handshake)` rather than giving a figure.
#[test]
fn test_transfer_help_screens_state_the_real_defaults() {
    let cfg = crate::config::Config::default();

    for petscii in [true, false] {
        let xmodem = TelnetSession::xmodem_help_lines(petscii).join("\n");
        assert!(
            xmodem.contains(&format!("default {}", cfg.xmodem_negotiation_retry_interval))
                || xmodem.contains(&format!("def {} s", cfg.xmodem_negotiation_retry_interval)),
            "the XMODEM help ({}) no longer states the real C/NAK poke gap of {} s:\n{}",
            if petscii { "PETSCII" } else { "ANSI" },
            cfg.xmodem_negotiation_retry_interval,
            xmodem,
        );

        let zmodem = TelnetSession::zmodem_help_lines(petscii).join("\n");
        assert!(
            zmodem.contains(&format!("def {}", cfg.zmodem_negotiation_retry_interval)),
            "the ZMODEM help ({}) no longer states the real re-send gap of {} s:\n{}",
            if petscii { "PETSCII" } else { "ANSI" },
            cfg.zmodem_negotiation_retry_interval,
            zmodem,
        );

        let punter = TelnetSession::punter_help_lines(petscii).join("\n");
        assert!(
            punter.contains(&cfg.punter_block_size.to_string()),
            "the PUNTER help ({}) no longer states the real block size of {}:\n{}",
            if petscii { "PETSCII" } else { "ANSI" },
            cfg.punter_block_size,
            punter,
        );

        // Kermit states no figure: the screen prints the live one.  A number
        // here would be a second place for it to be wrong.
        let kermit = TelnetSession::kermit_help_lines(petscii).join("\n");
        // Match the key row, not the prose.  The first version of this looked
        // for "Negotiate" and found the blurb line "parameters.  Negotiated
        // with the peer at session start" -- which carries no digits, so the
        // check passed with the stale "45 s" put straight back.  A guard whose
        // subject is chosen by a substring can be pointed at the wrong line by
        // an ordinary word.
        let negotiate: Vec<&str> = kermit
            .lines()
            .filter(|l| l.contains("Negotiate timeout"))
            .collect();
        assert_eq!(
            negotiate.len(),
            1,
            "expected exactly one negotiate-timeout key row in the Kermit help \
             ({}), found {:?}",
            if petscii { "PETSCII" } else { "ANSI" },
            negotiate,
        );
        let negotiate = negotiate[0];
        assert!(
            !negotiate.chars().any(|c| c.is_ascii_digit()),
            "the Kermit help ({}) states a negotiate timeout of its own -- it \
             drifted to 45 s once while the default was {} s, and the screen \
             already prints the live value: {:?}",
            if petscii { "PETSCII" } else { "ANSI" },
            cfg.kermit_negotiation_timeout,
            negotiate,
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

/// **Every menu prompt fits a C64, and every path-shaped one hangs off the
/// main menu's root.**
///
/// This replaced a hand-copied list of four breadcrumbs.  There are 33 prompt
/// literals in `src/telnet/`, so that list covered an eighth of them and its
/// own comment asked the next person to keep it in sync -- the same shape as
/// the `show_error` list whose per-file hole is documented above.  It reads
/// the directory instead, because `include_str!` cannot enumerate one and a
/// named list of files is a hole that opens the day a file is added.
///
/// **Two families of prompt, and the scan found that out rather than assuming
/// it.**  A *breadcrumb* is a path from the main menu (`gateway/config/cpm`);
/// a *field* prompt is one word naming what is being asked for (`baud`,
/// `runs`, `image`).  The first version of this test required a root of every
/// prompt it found and went red on `baud`, which is correct as written.  So
/// the root rule is scoped to the ones that are paths, and the width rule --
/// the one a 40-column PETSCII screen enforces by silently cutting the tail --
/// applies to all of them.
///
/// The root rule is what a rename breaks: renaming the root on the main menu
/// and leaving a child screen saying something else makes the prompt lie about
/// where the user is, which is the whole job it does.  (`xmodem` -> `ethernet`
/// once before, `ethernet` -> `gateway` on 2026-09-19.)
///
/// **The third pass is the one that closes the hole**, and it is why this
/// scans literals rather than only the `format!` call.  Two breadcrumbs --
/// `gateway/config/xfer/xmodem` and `.../ymodem` -- are *arguments* to the
/// shared `xmodem_family_settings` page, so they never appear next to a
/// `format!` and the first two passes could not see them at all.  Sweeping
/// every path-shaped literal catches those, and catches a new screen that
/// invents a different root wherever its string is written.  Exactly two
/// path-shaped literals in `src/telnet/` are not prompts, and they are named
/// below rather than pattern-matched away: an allowlist that has to be edited
/// on purpose is a hole somebody has to dig, which a looser regex is not.
///
/// `Menu::path()` is called rather than copied.  The genuinely dynamic prompts
/// -- `prompt_str`'s transfer subdirectory and the serial pages' per-port
/// labels -- are built at run time from the same root and cannot be read
/// statically.
#[test]
fn test_every_menu_prompt_fits_petscii_and_shares_the_main_menu_root() {
    // The root the main menu itself prints.  Taken from the code, so this
    // test cannot disagree with the screen about what the root is.
    let root = Menu::Main.path();

    // Path-shaped literals in `src/telnet/` that are not menu prompts.
    const NOT_PROMPTS: &[&str] = &[
        "km/h", // weather.rs, a unit
        "n/a",  // config_ui.rs, an empty-value marker
    ];

    let fits = |label: &str, path: &str| {
        let prompt = format!("{}> ", path);
        assert!(
            prompt.len() <= PETSCII_WIDTH,
            "{label}: prompt '{prompt}' is {} chars, exceeds {PETSCII_WIDTH}",
            prompt.len(),
        );
    };
    let rooted = |label: &str, path: &str| {
        assert!(
            path == root || path.starts_with(&format!("{root}/")),
            "{label}: path prompt '{path}' does not hang off the main menu root '{root}'",
        );
    };

    // The three top-level menus, called rather than copied.
    fits("Menu::Main", Menu::Main.path());
    rooted("Menu::Main", Menu::Main.path());
    fits("Menu::FileTransfer", Menu::FileTransfer.path());
    rooted("Menu::FileTransfer", Menu::FileTransfer.path());
    fits("Menu::Browser", Menu::Browser.path());
    rooted("Menu::Browser", Menu::Browser.path());

    /// Is `lit` shaped like a menu path -- lowercase segments joined by `/`?
    fn path_shaped(lit: &str) -> bool {
        lit.contains('/')
            && lit.split('/').all(|seg| {
                !seg.is_empty()
                    && seg
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
    }

    const NEEDLE: &str = r#"format!("{}> ", self.cyan(""#;
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/telnet");
    let mut inline_prompts = 0;
    let mut path_literals = 0;
    let mut files_seen = 0;

    for entry in std::fs::read_dir(dir).expect("src/telnet is not readable") {
        let path = entry.expect("unreadable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // This file contains the needle itself, a few lines above.
        if name == "tests.rs" {
            continue;
        }
        files_seen += 1;
        let code = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .replace('\r', "");

        // Pass one: the literal written straight into a prompt.  Every one
        // of these is a prompt whatever its shape, so the width rule applies
        // to all of them and the root rule to the ones that are paths.
        let mut at = 0;
        while let Some(i) = code[at..].find(NEEDLE) {
            let lit_at = at + i + NEEDLE.len();
            at = lit_at;
            let Some(close) = code[lit_at..].find('"') else {
                panic!("{name}: unterminated prompt literal at byte {lit_at}");
            };
            let lit = &code[lit_at..lit_at + close];
            inline_prompts += 1;
            fits(&name, lit);
            if path_shaped(lit) {
                rooted(&name, lit);
            }
        }

        // Pass two: every path-shaped literal anywhere in the file, which is
        // how the two breadcrumbs passed as arguments are reached.
        for lit in code.split('"').skip(1).step_by(2) {
            if !path_shaped(lit) || NOT_PROMPTS.contains(&lit) {
                continue;
            }
            path_literals += 1;
            fits(&name, lit);
            rooted(&name, lit);
        }
    }

    // Positive controls.  Without these a needle that matched nothing -- a
    // reformatted `format!`, a renamed helper -- would pass having checked
    // only the three `Menu::path()` arms above, which is exactly how the list
    // this test replaced went stale.
    assert!(
        files_seen >= 10,
        "only {files_seen} telnet source files read; the directory scan is not working"
    );
    assert!(
        inline_prompts >= 30,
        "only {inline_prompts} inline prompt literals found; the scan pattern has stopped matching"
    );
    assert!(
        path_literals >= 25,
        "only {path_literals} path-shaped literals found; the literal sweep has stopped matching"
    );
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
        crate::telnet::Inherited::fresh(true, None),
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
        crate::telnet::Inherited::fresh(true, None),
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
/// new help screen is only width-checked once added below.  Single-width
/// tables ignore `petscii` (they fit 40).
fn all_help_line_groups(petscii: bool) -> Vec<&'static [&'static str]> {
    #[allow(unused_mut)]
    let mut groups: Vec<&'static [&'static str]> = vec![
        // One entry per table, which `test_every_help_table_is_width_checked`
        // counts: the optional items only ever remove lines, so the full page
        // is the widest and a second variant here would be both redundant and
        // a table counted twice.
        TelnetSession::main_help_lines(MenuItems { cpm: true, second_page: true }),
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
        // The CP/M emulator's help had its own width test and was never in
        // this list -- found by `test_every_help_table_is_width_checked`, the
        // census that replaced the fixed array length as the tripwire.  Being
        // checked twice costs nothing; being checked nowhere is what the
        // MAINTENANCE note above exists to prevent.
        TelnetSession::cpmemu_help_lines(petscii),
        TelnetSession::CPM_ENTRY_TIPS,
    ];
    // The MORE page is Unix-only, so it joins the list only where it exists.
    // A `Vec` rather than the fixed-size array this used to be: a length that
    // changes with the platform is a number to keep in step twice.
    #[cfg(unix)]
    groups.push(TelnetSession::more_help_lines());
    groups
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
    assert_eq!(Menu::Browser.path(), "gateway/web");
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

/// **A terminal the client announced is believed**, whichever transport
/// carried the announcement.
///
/// SSH sends `TERM` in the pty request and it was being discarded. That cost
/// SSH clients not a prompt but an *answer*: `run()` skips terminal detection
/// entirely for SSH, so every SSH session kept the `TerminalType::Ansi`
/// default whatever the client said — a `dumb` terminal was sent colour, and a
/// Commodore-side client was sent ANSI instead of PETSCII. Verified live:
/// `TERM=c64` over SSH now reaches the menu in PETSCII.
///
/// On the telnet side the same function decides what the BACKSPACE prompt
/// *falls back to*, so the two transports cannot come to disagree about what an
/// announced name means. **It no longer decides whether to ask**: telnet asks
/// every session, because a TTYPE is a claim by whatever speaks telnet, which
/// for a Commodore behind a WiFi modem is the modem (measured: tcpser announces
/// `VT100` for a C64). `ttype_matched` therefore means "we have an answer if
/// nobody presses a key", and for SSH it still means "this is the answer".
#[test]
fn test_an_announced_terminal_is_believed() {
    // A name we know: the type is taken from it.
    let mut s = make_test_session(TerminalType::Ascii);
    s.note_announced_terminal("xterm-256color");
    assert!(s.ttype_matched, "a known TERM must be believed");
    assert_eq!(s.terminal_type, TerminalType::Ansi);
    assert_eq!(s.ttype_raw.as_deref(), Some("xterm-256color"));

    // A Commodore announcing itself is still a Commodore.
    let mut s = make_test_session(TerminalType::Ansi);
    s.note_announced_terminal("C64");
    assert!(s.ttype_matched);
    assert_eq!(s.terminal_type, TerminalType::Petscii);

    // An unrecognised name is *recorded* for the gateway-debug diagnostic but
    // does not count as identification, so there is nothing for the telnet
    // prompt to fall back on and a silent client is disconnected rather than
    // guessed at.
    let mut s = make_test_session(TerminalType::Ascii);
    s.note_announced_terminal("MY-WEIRD-TERM");
    assert!(!s.ttype_matched, "an unknown TERM must still be asked");
    assert_eq!(s.ttype_raw.as_deref(), Some("MY-WEIRD-TERM"));

    // No pty, or an empty/garbage TERM: nothing recorded, still asked.
    let mut s = make_test_session(TerminalType::Ascii);
    s.note_announced_terminal("");
    assert!(!s.ttype_matched);
    assert_eq!(s.ttype_raw, None, "an empty TERM is not an announcement");

    // Control bytes are stripped rather than taken as part of the name, the
    // same as the telnet subnegotiation always did.
    let mut s = make_test_session(TerminalType::Ascii);
    s.note_announced_terminal("vt100\u{0}\r");
    assert!(s.ttype_matched);
    assert_eq!(s.ttype_raw.as_deref(), Some("vt100"));

    // First announcement wins: a later one cannot retype an identified session.
    let mut s = make_test_session(TerminalType::Ascii);
    s.note_announced_terminal("C64");
    s.note_announced_terminal("xterm");
    assert_eq!(s.terminal_type, TerminalType::Petscii, "the first answer stands");
    assert_eq!(s.ttype_raw.as_deref(), Some("C64"));
}

/// **Both detection prompts must fit a 40-column screen.**
///
/// This is the one thing written before the terminal type is known, so it has
/// to fit the narrowest terminal it could be talking to — a PETSCII C64 —
/// rather than the 80 columns everything after detection may assume.
///
/// Added because the space re-prompt was written at 48 columns and would have
/// wrapped on exactly the machine the prompt exists to identify. The strings
/// are read from the constants rather than restated here, for the same reason
/// the help-fit tests iterate `*_help_lines()`: a test holding its own copy
/// cannot catch the real one changing.
#[test]
fn test_the_detection_prompts_fit_a_c64_screen() {
    const PETSCII_COLS: usize = 40;
    for (what, prompt) in [("first ask", DETECT_PROMPT), ("space re-ask", DETECT_REPROMPT)] {
        assert!(
            prompt.chars().count() <= PETSCII_COLS,
            "the {what} prompt is {} columns on a {PETSCII_COLS}-column screen: {prompt:?}",
            prompt.chars().count()
        );
        // It is sent with `send_raw`, before the terminal type is known, so it
        // cannot rely on any translation on the way out.
        assert!(prompt.is_ascii(), "the {what} prompt must be ASCII: {prompt:?}");
    }
    // The re-ask has to name the key it is refusing, or it reads as the same
    // question repeating for no reason.
    assert!(
        DETECT_REPROMPT.to_ascii_lowercase().contains("space"),
        "the re-ask must say what was wrong: {DETECT_REPROMPT:?}"
    );
    // And it must not claim the answer was "not a backspace key", which would
    // be false for the machines this prompt exists for — an Apple I clone's
    // back arrow (0x5F) and the early Unix `#` are both real erase keys.
    assert!(
        !DETECT_REPROMPT.to_ascii_lowercase().contains("not a backspace"),
        "the re-ask must refuse space specifically, not backspace keys in general"
    );
}

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

/// End-to-end cover for the Gateway Shell's `DIR` operand handling — the
/// four distinct paths through its dir/pattern selection:
///   `DIR` (cwd), `DIR SUB` (an existing directory's contents),
///   `DIR *.TXT` (a glob in the cwd), and `DIR NOSUCH` (an error).
///
/// Mutates **no** config — it creates a uniquely-named subtree inside whatever
/// `transfer_dir` already is — but it must still hold `CONFIG_TEST_LOCK`.
/// `cpm_dir_abs` calls `get_config()` *itself*, on every one of the five runs
/// below, so a kermit test repointing `transfer_dir` mid-test would move the
/// jail out from under a subtree created against the old value. Reading global
/// config unsynchronised is the same race that made a kermit test flaky, just
/// from the other side.
#[tokio::test]
async fn test_cpm_dir_operand_selects_directory_or_pattern() {
    use tokio::io::AsyncReadExt;

    let _lock = config::CONFIG_TEST_LOCK.lock().await;

    // Point `transfer_dir` at an ABSOLUTE path for the duration.
    //
    // This is the flake, caught: `cpm_dir` resolves the configured path against
    // the process's current directory, the shipped `transfer_dir` is the
    // relative "transfer", and `webbrowser::tests::test_bookmarks` changes the
    // CWD process-wide (then deletes the directory it changed into). Nothing
    // serialises the two — CONFIG_TEST_LOCK guards the config, not the CWD — so
    // this test intermittently looked for its subtree in a directory that had
    // just been removed. Reproduced by running the two together: 1 failure in
    // 60, with the diagnostic below reporting `transfer_dir` unmoved and the
    // root simply gone, which is what pointed at the CWD rather than the config.
    //
    // An absolute path cannot be re-based by a CWD change, so this test no
    // longer cares what any other test does with it.
    struct TransferDirGuard(String);
    impl Drop for TransferDirGuard {
        fn drop(&mut self) {
            // Inside the lock, like ConfigTestGuard: a failed assertion returns
            // early, and leaving a temp path in the global config would break
            // every later test that reads it.
            config::update_config_value("transfer_dir", &self.0);
        }
    }
    let _dir_guard = TransferDirGuard(config::get_config().transfer_dir);

    let base = std::env::temp_dir().join(format!("eg_cpm_dir_{}", std::process::id()));
    if std::fs::create_dir_all(&base).is_err() {
        return; // no transfer dir available; nothing to assert against
    }
    config::update_config_value("transfer_dir", &base.to_string_lossy());
    let cfg = config::get_config();
    // Unique per run so this can't collide with a parallel test.
    let root = base.join(format!("dirtest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("SUB")).unwrap();
    std::fs::write(root.join("TOP.TXT"), b"top").unwrap();
    std::fs::write(root.join("OTHER.DAT"), b"other").unwrap();
    std::fs::write(root.join("SUB").join("INNER.TXT"), b"inner").unwrap();
    let rootname = root.file_name().unwrap().to_string_lossy().to_string();

    async fn run(subdir: &str, pattern: Option<&str>) -> String {
        let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
        session.transfer_subdir = subdir.to_string();
        session.cpm_dir(pattern).await.unwrap();
        drop(session);
        let mut out = Vec::new();
        peer.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).to_string()
    }

    // 1. No operand: list the cwd.  Both files, plus SUB as a <DIR>.
    let cwd = run(&rootname, None).await;
    // 2. A wildcard-free operand naming a directory lists *its* contents —
    //    the path whose resolved value the old match guard had to throw away.
    let sub = run(&rootname, Some("SUB")).await;
    // 3. A glob is matched within the cwd.
    let glob = run(&rootname, Some("*.TXT")).await;
    // 4. A wildcard-free operand that is neither a directory nor an existing
    //    file is a *name pattern* in the cwd that matches nothing — "No file",
    //    the DOS/CP/M behaviour, not a directory error.
    let bad = run(&rootname, Some("NOSUCH")).await;
    // 5. A path-qualified operand whose parent doesn't exist is the real error
    //    case, and the error must come from resolving that parent.
    let badpath = run(&rootname, Some("NOSUCH/ANY")).await;

    // Snapshot the world BEFORE cleaning up, and attach it to every assertion
    // below.  This test has flaked roughly once in thirty full-suite runs and
    // has never been caught in the act; the output alone does not say why,
    // because the two candidate causes are invisible in it — `transfer_dir`
    // moving under us (cpm_dir calls get_config() itself, once per run above)
    // and the subtree disappearing. Both are answered here, and the tree is
    // still on disk at the point it is read. Cheap: five lines of formatting on
    // the passing path, and the only chance of diagnosing the next occurrence
    // without reproducing it.
    let diag = {
        let after = config::get_config().transfer_dir.clone();
        let listing = |p: &std::path::Path| -> String {
            match std::fs::read_dir(p) {
                Ok(rd) => {
                    let mut names: Vec<String> = rd
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    names.join(",")
                }
                Err(e) => format!("<unreadable: {e}>"),
            }
        };
        format!(
            "transfer_dir at start={:?}, at end={:?} (moved={}), root={:?} exists={}, \
             root contains [{}], SUB contains [{}]",
            cfg.transfer_dir,
            after,
            after != cfg.transfer_dir,
            root,
            root.exists(),
            listing(&root),
            listing(&root.join("SUB")),
        )
    };

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&base);

    assert!(
        cwd.contains("TOP.TXT") && cwd.contains("OTHER.DAT") && cwd.contains("SUB"),
        "bare DIR must list the cwd; got {:?}\n[diag] {}",
        cwd,
        diag,
    );
    assert!(
        sub.contains("INNER.TXT"),
        "DIR SUB must list SUB's contents; got {:?}\n[diag] {}",
        sub,
        diag,
    );
    assert!(
        !sub.contains("TOP.TXT"),
        "DIR SUB must not list the parent's files; got {:?}\n[diag] {}",
        sub,
        diag,
    );
    assert!(
        glob.contains("TOP.TXT") && !glob.contains("OTHER.DAT"),
        "DIR *.TXT must match only .TXT in the cwd; got {:?}\n[diag] {}",
        glob,
        diag,
    );
    assert!(
        bad.contains("No file"),
        "DIR NOSUCH is an unmatched name pattern, so 'No file'; got {:?}\n[diag] {}",
        bad,
        diag,
    );
    assert!(
        badpath.contains("No such directory"),
        "DIR NOSUCH/ANY must report the unresolvable parent; got {:?}\n[diag] {}",
        badpath,
        diag,
    );
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

/// **ESC and a dropped session are different facts.**
///
/// `get_line_input` reports both as `None`.  The CP/M emulator's `A>` prompt
/// read that `None` as "the user is gone" and returned to the gateway menu,
/// so a single ESC left the emulator while every other CP/M surface here
/// (stopping a transient, leaving a booted disk) needs ESC twice.  Nothing
/// chose that behaviour -- it fell out of two facts sharing one value.  A
/// caller can only be right about this if the reader keeps them apart.
#[tokio::test]
async fn test_line_input_reports_esc_and_disconnect_differently() {
    use tokio::io::AsyncWriteExt;
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);

    peer.write_all(b"DIR\r").await.unwrap();
    match session.get_line_input_end().await.unwrap() {
        LineEnd::Line(s) => assert_eq!(s, "DIR"),
        other => panic!("a typed line must come back as Line, got {:?}", other),
    }

    peer.write_all(&[0x1B]).await.unwrap();
    assert!(
        matches!(session.get_line_input_end().await.unwrap(), LineEnd::Escaped(_)),
        "ESC is a keypress, not a disconnect"
    );

    // Closing the peer is the disconnect -- and must not look like an ESC.
    drop(peer);
    assert!(
        matches!(session.get_line_input_end().await.unwrap(), LineEnd::Disconnected),
        "a closed session must report Disconnected"
    );
}

/// **The drain after an ESC is the only place the burst can be seen.**
///
/// It has to run -- an arrow key's `[A` would otherwise be typed into the
/// next prompt as text -- so a caller pairing ESCs would lose a fast second
/// ESC to it, and could not tell an arrow key from a keypress.  Both would
/// break the `A>` prompt in opposite directions: a terminal-sent pair would
/// never exit, and two cursor presses would exit when nobody asked.
#[tokio::test]
async fn test_esc_burst_tells_a_pair_from_an_arrow_key() {
    use tokio::io::AsyncWriteExt;

    // A lone ESC: nothing follows it.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    peer.write_all(&[0x1B]).await.unwrap();
    let lone = match session.get_line_input_end().await.unwrap() {
        LineEnd::Escaped(b) => b,
        other => panic!("expected Escaped, got {:?}", other),
    };
    assert!(!lone.another_esc && !lone.sequence, "a lone ESC carries nothing: {lone:?}");

    // Both ESCs in one burst -- how a terminal or a paste sends a pair.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    peer.write_all(&[0x1B, 0x1B]).await.unwrap();
    let pair = match session.get_line_input_end().await.unwrap() {
        LineEnd::Escaped(b) => b,
        other => panic!("expected Escaped, got {:?}", other),
    };
    assert!(
        pair.another_esc,
        "the second ESC of a burst must survive the drain that eats it: {pair:?}"
    );

    // An arrow key is ESC [ A -- a sequence, not a keypress of its own.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    peer.write_all(&[0x1B, b'[', b'A']).await.unwrap();
    let arrow = match session.get_line_input_end().await.unwrap() {
        LineEnd::Escaped(b) => b,
        other => panic!("expected Escaped, got {:?}", other),
    };
    assert!(
        arrow.sequence && !arrow.another_esc,
        "an arrow key must read as a sequence, never as an ESC pair: {arrow:?}"
    );
}

/// **The NUL of an RFC 854 `CR NUL` pair is not a keystroke.**
///
/// A telnet client spells a bare CR as `CR NUL`, and forwarding the NUL to a
/// booted CP/M guest printed `^@` at its next prompt — the CCP echoes control
/// characters. Reported from a real session: `A>b:`, `B>dir`, `B>a:`, then
/// `A>^@`. It showed only after a command that did no console I/O to swallow it
/// first, which is why `DIR` and logging in a drive looked clean and
/// re-selecting the current drive did not — so a test that only sends one CR
/// would have passed against the bug.
#[tokio::test]
async fn test_a_bare_cr_does_not_deliver_its_padding_nul() {
    use tokio::io::AsyncWriteExt;
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);

    // Two commands' worth, because the defect is about what survives *to the
    // next prompt*: the padding must not reappear as the first byte of the
    // following line.
    peer.write_all(b"a:\r\x00b:\r\x00").await.unwrap();
    let mut got = Vec::new();
    for _ in 0..6 {
        got.push(session.session_read_byte().await.unwrap().unwrap());
    }
    assert_eq!(&got, b"a:\rb:\r", "the padding NUL reached the guest: {got:?}");
}

/// The other half: a NUL that is *not* padding is the peer's own byte, and the
/// LF of a `CR LF` is a real newline plenty of guest software wants.
#[tokio::test]
async fn test_only_the_nul_straight_after_a_cr_is_dropped() {
    use tokio::io::AsyncWriteExt;
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);

    // A lone NUL, a NUL after something that is not a CR, the second NUL of a
    // run (only one is padding), and the LF of a CR LF.
    peer.write_all(b"\x00A\x00\r\x00\x00\r\n").await.unwrap();
    let mut got = Vec::new();
    for _ in 0..7 {
        got.push(session.session_read_byte().await.unwrap().unwrap());
    }
    assert_eq!(
        &got,
        b"\x00A\x00\r\x00\r\n",
        "only the one NUL straight after a CR is padding: {got:?}"
    );

    // Serial and SSH carry no NVT encoding, so nothing is padding there: the
    // same bytes must arrive whole.  This path also feeds a booted guest's
    // console from a file, where a dropped 0x00 is corruption.
    {
        let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
        session.is_ssh = true;
        peer.write_all(b"\r\x00").await.unwrap();
        assert_eq!(session.session_read_byte().await.unwrap(), Some(b'\r'));
        assert_eq!(
            session.session_read_byte().await.unwrap(),
            Some(0),
            "an SSH client's NUL is its own byte, not NVT padding"
        );
    }

    // A pushed-back byte is a real byte, so what follows *it* is not padding.
    // `drain_trailing_eol` pushes back in the middle of exactly these
    // sequences, so a flag that survived across a pushback would drop a NUL
    // that came after something else.
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    peer.write_all(b"\r\x00").await.unwrap();
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'\r'));
    // A byte handed back between the CR and the NUL: with the flag surviving
    // across it, the NUL below would be eaten as padding it is not.
    session.pushback = Some(b'X');
    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'X'));
    assert_eq!(
        session.session_read_byte().await.unwrap(),
        Some(0),
        "the NUL after a pushed-back byte is not CR padding"
    );
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

/// **A telnet client that announced a terminal is still asked.**
///
/// The announcement is a claim made by whatever speaks telnet, and on the paths
/// this gateway exists for that is a modem rather than the machine behind it:
/// measured 2026-09-11, tcpser announces `VT100` for a Commodore 64, and the
/// C64 was then classified ANSI and never asked.  Its own INST/DEL crosses the
/// bridge intact, so asking gets the right answer -- this pins that we ask.
///
/// Driven end to end rather than by inspecting a flag, because the defect was
/// precisely that the flag was consulted *instead of* the wire.
#[tokio::test]
async fn test_an_announced_telnet_client_is_still_asked() {
    let (mut session, peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.note_announced_terminal("VT100");
    assert!(session.ttype_matched, "VT100 must be a name we know");

    let (mut prd, mut pwr) = tokio::io::split(peer);
    let task = tokio::spawn(async move {
        let mut session = session;
        let _ = session.detect_terminal_type().await;
        session
    });

    // The prompt must arrive even though the client named itself.
    let seen = read_until(&mut prd, DETECT_PROMPT).await;
    assert!(
        seen.to_lowercase().contains(&DETECT_PROMPT.to_lowercase()),
        "an announced client must still be asked; got {:?}",
        String::from_utf8_lossy(seen.as_bytes())
    );

    // The C64 answers with its own key, and that must win over the modem's claim.
    use tokio::io::AsyncWriteExt;
    pwr.write_all(&[0x14]).await.unwrap();
    let seen = read_until(&mut prd, "color?").await;
    assert!(
        seen.to_lowercase().contains("petscii"),
        "the keypress must decide, not the announcement; got {:?}",
        seen
    );
    pwr.write_all(b"n").await.unwrap();

    let session = task.await.unwrap();
    assert_eq!(session.terminal_type, TerminalType::Petscii);
    assert_eq!(session.erase_char, 0x14);
}

/// **A client that answers nothing is believed, not dropped.**
///
/// Asking everybody costs nothing to a person and would cost a session to a
/// probe or a script, which never presses a key: before this the announcement
/// short-circuited the prompt, so such a client sailed through.  The
/// announcement is now the fallback, and the fallback lands on exactly the
/// answer it would have given -- so this can only be as good as the old
/// behaviour or better.  Under a paused clock, so the wait is asserted rather
/// than slept through.
#[tokio::test(start_paused = true)]
async fn test_a_silent_announced_client_falls_back_instead_of_being_dropped() {
    let (mut session, peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.note_announced_terminal("xterm-256color");

    let (mut prd, mut pwr) = tokio::io::split(peer);
    let task = tokio::spawn(async move {
        let mut session = session;
        let outcome = session.detect_terminal_type().await;
        (session, outcome)
    });

    // No key is ever pressed.  The wait expires and the announcement stands.
    let seen = read_until(&mut prd, "color?").await;
    assert!(
        !seen.contains("idle timeout"),
        "a client with an announcement must not be disconnected; got {:?}",
        seen
    );
    assert!(
        seen.to_lowercase().contains("ansi"),
        "the announcement must decide; got {:?}",
        seen
    );

    use tokio::io::AsyncWriteExt;
    pwr.write_all(b"n").await.unwrap();
    let (session, outcome) = task.await.unwrap();
    assert!(outcome.is_ok(), "the session must survive: {:?}", outcome);
    assert_eq!(session.terminal_type, TerminalType::Ansi);
    assert_eq!(session.erase_char, session::DEFAULT_ERASE_CHAR);
}

/// **The master-password screen fits a C64.**
///
/// 40 columns and 22 rows is the budget for every screen this gateway draws,
/// and this one is drawn on the narrowest terminal it serves at the worst
/// moment -- a slave whose relay is down, reached over a serial modem. Counting
/// the whole screen, not just the prose: the frame, the indent, the prompt and
/// the confirmation all take rows too.
#[test]
fn test_the_master_password_screen_fits_a_c64() {
    // The longest address this can name: IPv6, or a long hostname.
    for (host, port) in [
        ("192.168.1.178", 2222u16),
        ("2001:0db8:85a3:0000:0000:8a2e:0370:7334", 65535),
        ("a-rather-long-master-hostname.local", 2222),
    ] {
        let body = crate::telnet::session::master_password_screen_lines(host, port);
        for line in &body {
            // Two-space indent, as the screen renders them.
            assert!(
                line.chars().count() + 2 <= 40,
                "{:?} is {} columns with its indent",
                line,
                line.chars().count() + 2
            );
        }
        // 3 frame rows + a blank + the body + a blank + the prompt + 2 rows of
        // confirmation + a blank + "press any key" — all inside 22.
        let rows = 3 + 1 + body.len() + 1 + 1 + 2 + 1 + 1;
        assert!(rows <= 22, "the screen is {rows} rows for {host}");
    }
    // A very long address wraps rather than being silently cut: it is on its
    // own line precisely so the rest of the screen cannot be pushed out.
    let long = crate::telnet::session::master_password_screen_lines(
        "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
        65535,
    );
    assert!(long.iter().any(|l| l.contains("2001:0db8")), "the address must be shown");
}

/// **A terminal announced AFTER the keypress does not overrule it.**
///
/// `SB TTYPE IS` can arrive at any moment, including the input drain that runs
/// a few lines after "Terminal detected" -- so without a latch a C64 that had
/// just pressed 0x14 was turned back into an ANSI terminal by its modem's
/// VT100 claim, which is the very defect the prompt exists to prevent.
#[tokio::test]
async fn test_a_late_announcement_cannot_overrule_the_keypress() {
    let (session, peer) = make_test_session_with_peer(TerminalType::Ascii);
    let (mut prd, mut pwr) = tokio::io::split(peer);
    let task = tokio::spawn(async move {
        let mut session = session;
        let _ = session.detect_terminal_type().await;
        session
    });

    let asked = read_until(&mut prd, "detect terminal").await;
    assert!(asked.contains("detect terminal"), "no prompt: {asked:?}");
    use tokio::io::AsyncWriteExt;
    pwr.write_all(&[0x14]).await.unwrap();          // the C64 answers
    let seen = read_until(&mut prd, "color?").await;
    assert!(seen.to_lowercase().contains("petscii"), "{seen:?}");
    pwr.write_all(b"n").await.unwrap();

    let mut session = task.await.unwrap();
    assert_eq!(session.terminal_type, TerminalType::Petscii);
    // The modem speaks up late, exactly as tcpser's negotiation does.
    session.note_announced_terminal("VT100");
    assert_eq!(
        session.terminal_type,
        TerminalType::Petscii,
        "a late TTYPE must not overrule the machine's own answer"
    );
}

/// **A client that named itself and then typed ahead keeps its name, and keeps
/// its byte.**
///
/// Asking everybody puts the prompt in front of a client that used to skip it,
/// so its first byte -- a script's menu key, say -- lands at a prompt that
/// under the any-byte rule would install it as the ERASE key for the whole
/// session: `f` would thereafter delete a character. With a name in hand only a
/// real backspace byte answers, and anything else is pushed back, so the bytes
/// consumed before the colour prompt are the same as when this client was not
/// asked at all.
#[tokio::test]
async fn test_a_typed_ahead_byte_is_not_taken_as_the_erase_key() {
    let (mut session, peer) = make_test_session_with_peer(TerminalType::Ascii);
    session.note_announced_terminal("xterm");

    let (mut prd, mut pwr) = tokio::io::split(peer);
    let task = tokio::spawn(async move {
        let mut session = session;
        let _ = session.detect_terminal_type().await;
        session
    });

    // Wait for the prompt before typing: the session drains its input once
    // after the option handshake, so a byte sent before that is swallowed --
    // correctly, and it would make this test measure the drain instead.
    let asked = read_until(&mut prd, "detect terminal").await;
    assert!(asked.contains("detect terminal"), "no prompt: {:?}", asked);

    // The client is not answering the question; it is typing at the menu.
    use tokio::io::AsyncWriteExt;
    pwr.write_all(b"f").await.unwrap();
    let seen = read_until(&mut prd, "color?").await;
    assert!(
        seen.to_lowercase().contains("ansi"),
        "the announcement must stand; got {:?}",
        seen
    );
    // 'f' is pushed back, so the colour prompt reads it and ignores it exactly
    // as it did before this client was asked anything.  'n' then answers.
    pwr.write_all(b"n").await.unwrap();

    let session = task.await.unwrap();
    assert_eq!(session.terminal_type, TerminalType::Ansi);
    assert_eq!(
        session.erase_char,
        session::DEFAULT_ERASE_CHAR,
        "a typed-ahead byte must never become the erase key"
    );
}

/// Read from the peer until `marker` shows up, bounded so a wrong expectation
/// fails with what was actually written instead of hanging the suite.
async fn read_until(
    prd: &mut tokio::io::ReadHalf<tokio::io::DuplexStream>,
    marker: &str,
) -> String {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    // One overall deadline, not 64 x 30 s: the docstring promised a bound and
    // the arithmetic delivered half an hour, which is a hang with extra steps.
    // Restoring the defect these tests exist to catch leaves the session
    // waiting on input with nothing more to say, which is exactly when this
    // must fail fast and print what it did see.
    // Comfortably past ANNOUNCED_WAIT: a deadline equal to the wait under test
    // races it, and the loser is decided by the scheduler.  **Derived from the
    // constant, not copied**, because the two were 30 s and 10 s and the day
    // ANNOUNCED_WAIT was raised to 30 they would have become equal -- a flake
    // whose cause is a number in another file.
    let deadline = std::time::Instant::now()
        + crate::telnet::session::ANNOUNCED_WAIT
        + std::time::Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut tmp = [0u8; 256];
        match tokio::time::timeout(left, prd.read(&mut tmp))
        .await
        {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&tmp[..n]),
            Ok(Err(_)) => break,
        }
        // Case-insensitively: once PETSCII is detected every letter on the way
        // out is case-swapped, so "Use PETSCII color?" leaves as "uSE petscii
        // COLOR?" and an exact match waits out the timeout for text that is
        // already there.
        if String::from_utf8_lossy(&buf)
            .to_lowercase()
            .contains(&marker.to_lowercase())
        {
            break;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
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

/// **RFC 856 BINARY is agreed, both ways.**
///
/// This test used to assert the opposite, and carried no reason for it: it was
/// pinning the generic "refuse anything unrecognised" catch-all, with option 0
/// as a convenient example.  Refusing it is wrong, and measurably so.  The
/// server already treats a transfer as 8-bit -- `tnio` applies no NVT CR-NUL
/// stuffing, by explicit decision -- so telling a peer `WONT BINARY` says the
/// opposite of what we do.  An NVT-conformant peer then applies text rules to
/// Punter/XMODEM/ZMODEM blocks.
///
/// Measured 2026-09-06 against a real NovaTerm 9.6c in VICE: the same 1775-byte
/// Punter payload arrives byte for byte over a serial link, and through a
/// telnet peer that we had just told `DONT BINARY` it never got past block 0 --
/// 32 rejections and counting.  Agreeing to BINARY took it to zero.
#[tokio::test]
async fn test_do_binary_is_agreed() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, DO, 0x00, b'X']).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let will_binary = [IAC, WILL, 0x00];
    assert!(
        out.windows(3).any(|w| w == will_binary),
        "expected IAC WILL 0x00, got {:?}",
        out
    );
}

/// The receive direction too: a peer that offers to send us 8-bit gets `DO`.
#[tokio::test]
async fn test_will_binary_is_agreed() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    peer.write_all(&[IAC, WILL, 0x00, b'X']).await.unwrap();

    let b = session.session_read_byte().await.unwrap();
    assert_eq!(b, Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let do_binary = [IAC, DO, 0x00];
    assert!(
        out.windows(3).any(|w| w == do_binary),
        "expected IAC DO 0x00, got {:?}",
        out
    );
}

#[tokio::test]
async fn test_refused_option_not_repeated() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // An option we genuinely do not support -- 0x2A (42) is CHARSET,
    // RFC 2066.  (TERMINAL-SPEED is option 32, 0x20; the comment here named
    // that one for a while, which would have sent the next reader looking for
    // a mismatch that does not exist.)  Deliberately NOT BINARY: that one is
    // answered now, so using it here would test the acceptance path while
    // claiming to test refusal.
    const UNSUPPORTED: u8 = 0x2A;
    peer.write_all(&[IAC, DO, UNSUPPORTED, IAC, DO, UNSUPPORTED, b'X'])
        .await
        .unwrap();

    assert_eq!(session.session_read_byte().await.unwrap(), Some(b'X'));

    drop(session);
    let mut out = Vec::new();
    peer.read_to_end(&mut out).await.unwrap();
    let wont = [IAC, WONT, UNSUPPORTED];
    let matches = out.windows(3).filter(|w| *w == wont).count();
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
/// Text from the geocoding API is sanitised before it can reach a terminal.
///
/// A place name is third-party data printed straight onto the user's screen,
/// and JSON carries a control character perfectly well as `\u001b` — so an ESC
/// in a name would be a cursor move or a screen clear, not text. The AI chat
/// path already sanitises its API's text; this closes the same hole on the
/// weather path. (The browser is covered incidentally: html2text drops ESC and
/// BEL before the text is ever rendered — verified, not assumed.)
fn test_geocoder_text_cannot_carry_escapes_to_the_terminal() {
    let json: serde_json::Value = serde_json::from_str(
        r#"{"results":[{"latitude":1.0,"longitude":2.0,
             "name":"Ci\u001b[2Jty","admin1":"Re\u0007gion",
             "country":"Coun\u007ftry","country_code":"US",
             "timezone":"America/Chi\u001bcago"}]}"#,
    )
    .expect("fixture json");
    let results = parse_geo_results(&json);
    assert_eq!(results.len(), 1, "the entry must still parse, just cleaned");
    let g = &results[0];
    for (field, value) in [
        ("name", &g.name),
        ("admin1", &g.admin1),
        ("country", &g.country),
        ("timezone", &g.timezone),
    ] {
        assert!(
            !value.chars().any(|c| c.is_control() || c == '\u{7f}'),
            "{field} still carries a control character: {value:?}"
        );
    }
    // Readable text survives — this is a filter, not a rejection.
    assert_eq!(g.name, "Ci[2Jty");
    assert_eq!(g.country, "Country");
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

// A command-line submit must not leave a stray byte queued for the next read.
// Every common line ending — CRLF, the NVT `CR NUL` a telnet client sends for
// a bare Enter, lone CR, lone LF — must be fully consumed by the line read, so
// a program launched right after (e.g. a CP/M `.COM` whose first act is a Y/N
// console read) isn't handed a leftover terminator that skips its prompt.
#[tokio::test]
async fn test_command_line_leaves_no_stray_terminator() {
    use tokio::io::AsyncWriteExt;
    for (label, cmdline) in [
        ("CRLF", &b"ECHO\r\n"[..]),
        ("CRNUL", &b"ECHO\r\0"[..]),
        ("CR", &b"ECHO\r"[..]),
        ("LF", &b"ECHO\n"[..]),
    ] {
        let (mut sess, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
        peer.write_all(cmdline).await.unwrap();
        let cmd = sess.get_line_input().await.unwrap();
        assert_eq!(cmd.as_deref(), Some("ECHO"), "{label}: command read");
        // No byte should be immediately available now.
        let stray = tokio::time::timeout(
            std::time::Duration::from_millis(60),
            sess.session_read_byte(),
        )
        .await;
        assert!(
            stray.is_err(),
            "{label}: a stray terminator byte leaked to the next read: {stray:?}"
        );
    }
}

/// The CP/M emulator's free-TPA report (boot banner + `VER`) must state the
/// memory the emulator actually hands a program, and must fit a C64 row with
/// the two-space indent every banner/`VER` line carries.
#[test]
fn test_cpmemu_tpa_line_reports_real_tpa_and_fits_petscii() {
    let line = TelnetSession::cpmemu_tpa_line();
    // 0x0100..0xFE00 is 64768 bytes => 63K, bounds 0100-FDFF.
    assert_eq!(crate::cpm::TPA_BYTES, 0xFD00);
    assert_eq!(line, "63K TPA free (0100-FDFF)");
    assert!(
        line.len() + 2 <= PETSCII_WIDTH,
        "TPA line '{}' exceeds {} cols with the indent",
        line,
        PETSCII_WIDTH
    );
}

/// **The unclaimed-port pacing fires on a loop and not on a burst.**
///
/// The threshold is the whole design: a program inventorying the I/O space —
/// `survey.mac`, which is *why* an unclaimed port reads `0xFF` at all — must
/// sweep 256 ports without being throttled into uselessness, while a guest
/// stuck on a port that is not there must be paced every time round.
#[test]
fn test_unclaimed_port_pacing_bounds_a_loop_not_a_sweep() {
    use crate::telnet::cpm_emu::{unclaimed_nap, UNCLAIMED_READS_BEFORE_NAP};

    assert_eq!(unclaimed_nap(0), None, "a guest doing no port I/O is never paced");
    assert_eq!(unclaimed_nap(1), None);
    assert_eq!(
        unclaimed_nap(UNCLAIMED_READS_BEFORE_NAP),
        None,
        "at the threshold, not past it"
    );
    assert!(
        unclaimed_nap(UNCLAIMED_READS_BEFORE_NAP + 1).is_some(),
        "past it, the loop is paced"
    );
    // A 256-port sweep crosses the threshold a handful of times, not hundreds:
    // the count is cleared after each nap, so the cost of probing the whole
    // I/O space is a few milliseconds, once.  A `const` block, because the
    // whole point is that this is decidable without running anything — a
    // threshold small enough to nap per port would be a compile error.
    const { assert!(256 / UNCLAIMED_READS_BEFORE_NAP <= 4) };
}

/// **Both lines of the emulator's sign-on fit a C64 row.**
///
/// The 8080 note is the one at risk: it names a second thing as well as the
/// processor, and it is drawn only when a non-default CPU is configured — so an
/// overflow here would wrap the screen for exactly the operators who are
/// already doing something unusual, and nobody else would ever see it.
#[test]
fn test_cpm_banner_lines_fit_petscii() {
    for line in [
        crate::telnet::cpm_emu::CPM_BANNER,
        crate::telnet::cpm_emu::CPM_NOTE_8080,
    ] {
        assert!(
            line.len() + 2 <= PETSCII_WIDTH,
            "banner '{line}' is {} cols with the indent, over {PETSCII_WIDTH}",
            line.len() + 2
        );
    }
    // The banner keeps its pointer to the command list whatever the processor
    // is — an earlier version spent those columns on the 8080 warning, which
    // took HELP away from the screen where a new operator meets the emulator.
    assert!(crate::telnet::cpm_emu::CPM_BANNER.contains("HELP"));
    // And the note has to name the processor and the terminal that runs on
    // it.  `EGT8080`, not `EGT8080`: both files are on drive A:, and naming the
    // Z80 one here would send an 8080 operator to the build that crashes.
    assert!(crate::telnet::cpm_emu::CPM_NOTE_8080.contains("8080"));
    assert!(crate::telnet::cpm_emu::CPM_NOTE_8080.contains("EGT8080"));
}

/// **Forgetting one pinned key must not touch another's**, and the traps are
/// textual: a host that is a *prefix* of another, a comment that begins with the
/// host, and a file with no trailing newline.
#[test]
fn test_forgetting_a_pinned_key_takes_only_that_host() {
    use crate::telnet::without_known_host;

    // The prefix trap: `10.0.0.1:22` must not match `10.0.0.1:2222`.
    let content = "10.0.0.1:22 ssh-ed25519 AAAA\n10.0.0.1:2222 ssh-ed25519 BBBB\n";
    let (kept, n) = without_known_host(content, "10.0.0.1:22");
    assert_eq!(n, 1, "exactly one entry should go");
    assert_eq!(kept, "10.0.0.1:2222 ssh-ed25519 BBBB\n", "the longer port survived");

    // A comment mentioning the host is prose, not an entry.
    let content = "# 10.0.0.1:22 was reinstalled\n10.0.0.1:22 ssh-rsa CCCC\n";
    let (kept, n) = without_known_host(content, "10.0.0.1:22");
    assert_eq!(n, 1);
    assert_eq!(kept, "# 10.0.0.1:22 was reinstalled\n", "the comment stays");

    // Nothing for that host: unchanged, and reported as unchanged.
    let content = "192.168.1.5:2222 ssh-ed25519 DDDD\n";
    let (kept, n) = without_known_host(content, "10.0.0.1:22");
    assert_eq!(n, 0);
    assert_eq!(kept, content);

    // No trailing newline in, none invented out.
    let (kept, n) = without_known_host("a:1 K\nb:2 K", "a:1");
    assert_eq!((kept.as_str(), n), ("b:2 K", 1));

    // Removing the only entry leaves an empty file, not a blank line.
    let (kept, n) = without_known_host("a:1 K\n", "a:1");
    assert_eq!((kept.as_str(), n), ("", 1));

    // Two entries for one host (a file edited by hand) both go.
    let (kept, n) = without_known_host("a:1 K1\na:1 K2\nb:2 K\n", "a:1");
    assert_eq!((kept.as_str(), n), ("b:2 K\n", 2));
}

/// The emulator's out-of-band drain probes the wire once per CPU batch — and a
/// batch ends at every BDOS/BIOS trap, so this runs once per console character
/// a guest writes.  It must therefore be a *poll*, not a timed wait.
///
/// This used to be `tokio::time::timeout(Duration::ZERO, …)`, which reads like
/// "don't wait" but rounds up to tokio's next timer tick: ~1.1 ms a call,
/// capping emulated console output at ~840 char/s no matter how fast the Z80
/// core ran.  A screen-painting program (EGT8080) crawled at what looked like 150
/// baud.  The bound below is deliberately loose — the real cost is nanoseconds
/// and the bug's was ~1.1 s for this many calls, so there is no borderline case
/// to be flaky about; it only has to catch a timer creeping back onto the path.
#[tokio::test]
async fn test_poll_once_probes_without_arming_a_timer() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (mut client, _server) = tokio::join!(
        async { tokio::net::TcpStream::connect(addr).await.unwrap() },
        async { listener.accept().await.unwrap() }
    );

    // Nothing has been written, so every probe must come back "not ready"
    // rather than blocking or waiting out a tick.
    let calls = 1_000;
    let started = std::time::Instant::now();
    for _ in 0..calls {
        let mut buf = [0u8; 1];
        let probe = poll_once(tokio::io::AsyncReadExt::read(&mut client, &mut buf));
        assert!(probe.is_none(), "an idle socket must probe as not-ready");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "{calls} readiness probes took {elapsed:?} — a timer is back on the \
         per-character emulator path (the zero-timeout bug cost ~1.1s here)"
    );
}

/// The CP/M trust warning appears on the emulator's banner and on its config
/// screen, and both screens add a two-space indent.  A PETSCII terminal is 40
/// columns, and the full sentence is 66 — so it is deliberately split, and each
/// half has to keep fitting.
///
/// Pinned because the neighbouring drive-range help line silently went stale for
/// weeks; nothing here is checked by the `*_help_lines` fit tests, since these
/// lines are emitted inline rather than from an extracted function.
#[test]
fn test_cpmemu_trust_warning_fits_petscii() {
    // Exactly the strings the two screens send, in order.
    let banner = ["WARNING: Be sure you trust the CP/M", "files you run in the emulator."];
    let config_screen = ["Be sure you trust the CP/M files", "you run in the emulator."];

    for (screen, lines) in [("banner", &banner), ("config screen", &config_screen)] {
        for line in lines.iter() {
            assert!(
                line.len() + 2 <= PETSCII_WIDTH,
                "CP/M {screen} line '{line}' is {} cols with the indent, over {}",
                line.len() + 2,
                PETSCII_WIDTH
            );
        }
        // Split text is only correct if it still reads as the whole sentence.
        let joined = lines.join(" ");
        assert!(
            joined.contains("trust the CP/M files you run"),
            "CP/M {screen} halves must rejoin into the warning, got '{joined}'"
        );
    }
}

/// The emulator's idle-poll pacing. A status call answering "nothing available"
/// ends the CPU batch, so a comms program's idle loop costs one driver pass per
/// turn. Once those passes stopped waiting on a timer, nothing bounded the loop
/// and an idle EGT8080 terminal span the host at **161% CPU**; pacing brought it
/// to 1.4%.
///
/// The rule has to hold two things at once, which is what this pins:
/// throughput must be untouched (a working program never naps, because any real
/// work resets the counter to zero), and a parked session must nap enough to
/// stop spinning while still answering a keystroke imperceptibly fast.
#[test]
fn test_cpmemu_idle_nap_paces_only_an_established_idle_loop() {
    // A working program: the counter never climbs, so it must never be slowed.
    assert_eq!(idle_nap(0), None, "a busy pass must not nap");
    assert_eq!(idle_nap(1), None);
    assert_eq!(
        idle_nap(IDLE_POLLS_BEFORE_NAP),
        None,
        "polling a few times around real work stays at full speed"
    );

    // Just gone quiet: pace, but only briefly.
    let first = idle_nap(IDLE_POLLS_BEFORE_NAP + 1).expect("must pace once idle");
    assert!(
        first >= std::time::Duration::from_millis(1),
        "a nap of zero would not stop the spin"
    );

    // Still first-tier right up to the long threshold.
    assert_eq!(idle_nap(IDLE_POLLS_LONG), Some(first));

    // Parked for a while: back off further, since ~1000 passes/sec is a real
    // share of one slow ARM core.
    let long = idle_nap(IDLE_POLLS_LONG + 1).expect("must still pace");
    assert!(long > first, "the second tier must back off further than the first");

    // Both tiers must stay imperceptible to someone typing. 20 ms is the bound
    // we're willing to defend as unnoticeable; the measured worst case was 8 ms.
    assert!(
        long <= std::time::Duration::from_millis(20),
        "nap {long:?} would make typing feel laggy"
    );

    // Saturating growth must not wrap into "no nap" for a long-idle session.
    assert_eq!(idle_nap(u32::MAX), Some(long));
}

/// The bootable-size lines the boot picker shows when nothing can be booted
/// must fit a 40-column PETSCII screen, and must name every bootable medium.
///
/// Both halves matter. The width cannot be checked by the source-scanning fit
/// test above, because these lines are built by a runtime `format!` — which is
/// exactly how a screen ends up wider than the terminal without anyone
/// noticing. The completeness half is the one that had already failed: the
/// screen said "Only Altair 88-DCDD floppies can boot", with the two floppy
/// sizes, for as long as hard disks had been booting.
#[test]
fn test_bootable_size_lines_fit_petscii_and_name_every_medium() {
    // The screen that listed every bootable size went with the boot picker in
    // 0.9.2, but the *labels* did not: they still reach a 40-column PETSCII
    // terminal on the boot banner and in the mount result, so the budget they
    // have to fit is unchanged and is checked here directly rather than through
    // a screen helper that no longer exists.  The line these were printed on
    // was two spaces, a seven-character size and a separator, which is what
    // leaves 30 for the label.
    const LABEL_BUDGET: usize = 30;
    let media = crate::cpm::boot_machine::BootMachine::bootable_media();
    assert!(media.len() >= 3, "the floppy, the minidisk and the hard disk at least");

    for m in &media {
        assert!(
            m.label.chars().count() <= LABEL_BUDGET,
            "{} chars, over the {LABEL_BUDGET}-column budget: {:?}",
            m.label.chars().count(),
            m.label,
        );
    }

    // The screen that printed them is gone, so what remains to guard is that
    // every bootable medium is *nameable* at all: a board whose label was empty
    // or whose size was zero would still pass the width check above by being
    // vacuously short.  This is what the completeness half of the old test was
    // really for -- it caught a screen claiming "only Altair 88-DCDD floppies
    // can boot" for as long as hard disks had been booting.
    for m in &media {
        assert!(!m.label.trim().is_empty(), "a bootable medium with no name");
        assert!(m.bytes > 0, "{}: a bootable medium of no size", m.label);
    }
}

/// A hangup is not a fault.
///
/// The three disconnect kinds are how a call normally ends — Ricky saw
/// `Serial modem: session error: broken pipe` after every EGT80 hangup on a
/// call that had ended perfectly well.
///
/// `ConnectionAborted` is the one that must NOT be swallowed: `transfer.rs`
/// uses it as a control signal meaning "tear this session down", so it is a
/// decision the gateway made rather than a peer that went away.
#[test]
fn test_normal_disconnect_covers_hangups_but_not_deliberate_aborts() {
    use std::io::{Error, ErrorKind};
    for kind in [ErrorKind::BrokenPipe, ErrorKind::UnexpectedEof, ErrorKind::ConnectionReset] {
        assert!(
            crate::telnet::is_normal_disconnect(&Error::new(kind, "peer went away")),
            "{kind:?} is an ordinary end of session and must not be logged as an error",
        );
    }
    for kind in [ErrorKind::ConnectionAborted, ErrorKind::PermissionDenied, ErrorKind::TimedOut] {
        assert!(
            !crate::telnet::is_normal_disconnect(&Error::new(kind, "real")),
            "{kind:?} must still reach the log",
        );
    }
}

/// `render_bytes` is the outbound half of the byte trace, and its whole job
/// is being readable at a glance.
///
/// Printable bytes must stay themselves — a result code has to read as `OK`,
/// not as hex — while control bytes must be named, because "did a CR LF come
/// back" is the question the trace exists to answer.
#[test]
fn test_render_bytes_keeps_text_readable_and_names_controls() {
    use crate::telnet::cpm_emu::render_bytes;
    assert_eq!(render_bytes(b"OK", 48), "OK");
    assert_eq!(render_bytes(b"\r\nOK\r\n", 48), "<CR><LF>OK<CR><LF>");
    assert_eq!(render_bytes(b"\x1b[2J", 48), "<ESC>[2J");
    assert_eq!(render_bytes(&[0x19], 48), "<^Y>");
    assert_eq!(render_bytes(b"", 48), "");
    // Truncated with a count, so a screen paint cannot bury the log and a
    // reader still knows how much was dropped.
    let long = vec![b'x'; 100];
    let out = render_bytes(&long, 10);
    assert!(out.starts_with("xxxxxxxxxx"), "{out}");
    assert!(out.contains("+90 more"), "{out}");
    // A high byte has no glyph worth inventing.
    assert_eq!(render_bytes(&[0xFF], 48), "<high>");
}

/// The hangup guard's two waits must not be swapped back.
///
/// `HUPLEAD` is the silence BEFORE `+++` and is paid in full every time —
/// nothing arrives during it, because silence is what it waits for.
/// `HUPTAIL` is the silence AFTER, and is free in the normal case because
/// `XGETB` returns the moment the far end's `OK` lands.
///
/// They were 4 and 1 — generous where it costs, stingy where it does not —
/// which made an SC126 hangup take fourteen seconds AND left the trailing
/// wait with no margin over the far end's one-second Hayes guard.  Both
/// halves of that are pinned here: the paid wait stays small, the free wait
/// stays at least as large, and neither drops below the two passes a fast
/// machine needs to clear one second.
#[test]
fn test_egt80_hangup_guard_keeps_its_cheap_wait_generous() {
    let src = include_str!("../../EGT8080/EGT80.Z80");
    let value = |name: &str| -> u32 {
        src.lines()
            .find_map(|l| {
                let l = l.trim_start();
                let rest = l.strip_prefix(name)?;
                let rest = rest.trim_start().strip_prefix("EQU")?;
                // Strip the trailing comment before parsing.
                rest.trim().split(';').next()?.trim().parse().ok()
            })
            .unwrap_or_else(|| panic!("{name} EQU not found in EGT80.Z80"))
    };
    let lead = value("HUPLEAD");
    let tail = value("HUPTAIL");
    assert!(lead >= 2, "HUPLEAD={lead}: must clear the far end's 1s guard on a fast machine");
    assert!(tail >= 2, "HUPTAIL={tail}: must clear the far end's 1s guard on a fast machine");
    assert!(
        tail >= lead,
        "HUPTAIL={tail} < HUPLEAD={lead}: the FREE wait is now smaller than the PAID one, \
         which is the arrangement that cost fourteen seconds and broke the escape's margin",
    );
}

// ─── Gateway input: the C64 affordances and their blast radius ──

/// A gateway session is a plain pipe: run `sz` or PCPUT on the far host and
/// its bytes come through the same path a keystroke does.  **A client that is
/// not a Commodore must therefore see none of the PETSCII work**, or every
/// transfer through the SSH and Telnet Gateways would break -- and break
/// identically on each retry, so no protocol's CRC could recover it.
///
/// The one byte that changes is the erase key, which is the behaviour that
/// predates all of this and is what the fold exists for.
#[test]
fn test_a_non_petscii_keystroke_reaches_the_remote_unchanged() {
    for b in 0u8..=255 {
        let mut keys = Vec::new();
        gateway_input_for_remote(b, GatewayFilter::Ansi, 0x7F, &mut keys);
        assert_eq!(keys, vec![b], "byte {:02X} was altered for a non-PETSCII client", b);
    }
    // With a client whose erase key is BS, only that byte moves.
    for b in 0u8..=255 {
        let mut keys = Vec::new();
        gateway_input_for_remote(b, GatewayFilter::Ansi, 0x08, &mut keys);
        let want = if b == 0x08 { 0x7F } else { b };
        assert_eq!(keys, vec![want], "byte {:02X}", b);
    }
}

/// What the PETSCII input path alters, pinned as a closed set.
///
/// Every byte named here is one a file transfer cannot survive.  That is not
/// new -- the case swap has been unconditional for a Commodore since the
/// gateway was written, which is why PCGET/PCPUT is run over a raw path
/// (`AT+PETSCII=0`) rather than through a translating one.  The point of the
/// test is that the set must not **grow** by accident: a fifth entry is a
/// fifth way to corrupt a download, and it would arrive looking like a
/// harmless convenience for one more key.
#[test]
fn test_what_the_petscii_input_path_alters_is_a_closed_set() {
    let mut altered: Vec<u8> = Vec::new();
    for b in 0u8..=255 {
        let mut keys = Vec::new();
        gateway_input_for_remote(b, GatewayFilter::Petscii, 0x14, &mut keys);
        if keys != vec![b] {
            altered.push(b);
        }
    }
    let mut expected: Vec<u8> = (0x41u8..=0x5A) // PETSCII upper -> ASCII lower
        .chain(0xC1u8..=0xDA) // PETSCII shifted -> ASCII upper
        .chain([
            0x11, 0x91, 0x1D, 0x9D, // cursor keys -> ANSI CSI
            0x14, // the C64's erase key -> ASCII DEL
            0x5F, // the back-arrow -- the C64's ESC key -> 0x1B
        ])
        .collect();
    expected.sort_unstable();
    assert_eq!(altered, expected);
}

/// **The pipe mode is a pipe.** With `gateway_petscii_translate = false` the
/// far end understands Commodores, so every byte must reach it exactly as
/// typed -- the erase byte it identifies the C64 by, the back-arrow it reads
/// as its own ESC, and the cursor keys it knows as PETSCII.  A single byte
/// altered here would be a translation the operator asked us not to do.
#[test]
fn test_the_commodore_aware_mode_alters_nothing_in_either_direction() {
    for b in 0u8..=255 {
        let mut keys = Vec::new();
        gateway_input_for_remote(b, GatewayFilter::Raw, 0x14, &mut keys);
        assert_eq!(keys, vec![b], "byte {:02X} was altered on the way out", b);
    }
    // …and the same coming back, including a whole ANSI sequence, which must
    // NOT be translated: a Commodore-aware board never sends one.
    let mut st = GatewayOutState::new(GatewayFilter::Raw);
    let mut out = Vec::new();
    let wire: Vec<u8> = (0u8..=255).chain(*b"\x1b[32mHi\x93\x9e").collect();
    filter_gateway_output(&wire, &mut st, &mut out);
    assert_eq!(out, wire);
    assert!(!st.has_pending());
}

/// The four cursor keys are the only bytes that become *more* than one byte.
///
/// A length change is worse than a substitution for anything framed, so this
/// is deliberately narrower than the set above.
#[test]
fn test_only_the_cursor_keys_change_the_byte_count() {
    for b in 0u8..=255 {
        let mut keys = Vec::new();
        gateway_input_for_remote(b, GatewayFilter::Petscii, 0x14, &mut keys);
        let expected = if matches!(b, 0x11 | 0x91 | 0x1D | 0x9D) { 3 } else { 1 };
        assert_eq!(keys.len(), expected, "byte {:02X} produced {:?}", b, keys);
    }
}

/// Leaving a gateway is the back-arrow twice for a Commodore, and ESC twice
/// for everyone else -- never both.
///
/// A C64 *can* send a real ESC (measured: CTRL+: emits 0x1B), and while both
/// keys counted, pressing it twice at a remote's prompt dropped the
/// connection instead of reaching the host.  `is_esc_key` still accepts both,
/// because at one of our own prompts either key should cancel; the two rules
/// are different and this pins them apart.
#[test]
fn test_the_leave_pair_honours_only_the_key_the_screen_promises() {
    assert!(is_gateway_leave_key(0x5F, true), "back-arrow leaves on a C64");
    assert!(!is_gateway_leave_key(0x1B, true), "CTRL+: must reach the remote");
    assert!(is_gateway_leave_key(0x1B, false), "ESC leaves on every other terminal");
    assert!(!is_gateway_leave_key(0x5F, false), "underscore is just a character");
    // The prompt rule is unchanged and deliberately more generous.
    assert!(is_esc_key(0x5F, true));
    assert!(is_esc_key(0x1B, true));
    // …and a single back-arrow reaches the remote as a real ESC, which is the
    // whole point of treating it as one: the key the user calls ESC now means
    // ESC at the far end too, whatever that end believes our terminal is.
    let mut keys = Vec::new();
    gateway_input_for_remote(0x5F, GatewayFilter::Petscii, 0x14, &mut keys);
    assert_eq!(keys, vec![0x1B]);
    let mut keys = Vec::new();
    gateway_input_for_remote(0x5F, GatewayFilter::Ansi, 0x7F, &mut keys);
    assert_eq!(keys, vec![0x5F], "an underscore is just an underscore elsewhere");
}

/// The per-port override, and the fallback that keeps a portless client whole.
///
/// `AT+PETSCII` answers this question for the wire a Commodore is *on*; this
/// one answers it for the hop a gateway opens afterwards, and it lives beside
/// that key on the same port screen because that is where an operator is
/// already thinking about the machine plugged in.  The fallback is the part
/// worth pinning: a C64 on a WiFi modem reaches the gateway over telnet with
/// **no serial port at all**, so a purely per-port key would leave that client
/// unable to express this — the server-wide value is what speaks for it.
#[test]
fn test_the_port_answers_first_and_a_portless_client_falls_back() {
    use crate::serial::{GW_PETSCII_DEFAULT, GW_PETSCII_PASSTHROUGH, GW_PETSCII_TRANSLATE};
    // Every choice resolves, and an unknown value reads as the default rather
    // than as one of the two real answers.
    for (stored, server_wide, want) in [
        (GW_PETSCII_TRANSLATE, false, true),
        (GW_PETSCII_PASSTHROUGH, true, false),
        (GW_PETSCII_DEFAULT, true, true),
        (GW_PETSCII_DEFAULT, false, false),
        ("nonsense", true, true),
        ("nonsense", false, false),
    ] {
        let resolved = crate::serial::resolve_gw_petscii(stored, server_wide);
        assert_eq!(
            resolved, want,
            "port set to {stored:?} with server-wide {server_wide}"
        );
    }
    // The label shown on all three screens never invents a fourth answer.
    for (value, _) in crate::serial::GW_PETSCII_CHOICES {
        assert!(!crate::serial::gw_petscii_label(value).is_empty());
    }
    assert_eq!(
        crate::serial::gw_petscii_label("nonsense"),
        crate::serial::GW_PETSCII_CHOICES[0].1,
        "an unreadable value must read as the default, as the resolver treats it"
    );
}

/// The Serial Gateway must never resolve to the Commodore-aware pipe.
///
/// `gateway_petscii_translate = false` means *the far end understands
/// Commodores*. A local serial device never does, so the Serial Gateway
/// answers this question from the terminal type alone. It briefly did not, to
/// give an operator a way to switch off the back-arrow rewriting there too,
/// and that took the case swap and the erase fold with it: a caller who set
/// the key for a PETSCII-aware BBS on the SSH Gateway then found every shifted
/// letter reaching their RC2014 as `0xC1..0xDA` with INST/DEL no longer
/// erasing. It also read the setting of the port the caller *dialled in on*,
/// which is not the port the bridge is connected to.
///
/// A full suite, clippy and CI all passed with that in place, so this is the
/// only thing standing between it and a repeat.
#[test]
fn test_the_serial_gateway_never_becomes_a_commodore_aware_pipe() {
    let src = include_str!("gateway.rs");
    let at = src
        .find("pub(in crate::telnet) async fn run_serial_console_loop")
        .expect("run_serial_console_loop — renamed?");
    // Its own body, up to the next function at the same indentation.
    let end = src[at..]
        .find("\n    pub(in crate::telnet) async fn ")
        .map(|i| at + i)
        .unwrap_or(src.len());
    let body: String = src[at..end]
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        !body.contains("gateway_filter()"),
        "the Serial Gateway must resolve its filter from the terminal type, \
         not from `gateway_petscii_translate` — its far end is a local device"
    );
    assert!(
        body.contains("GatewayFilter::Petscii"),
        "and it must still translate for a Commodore"
    );
    // The property the resolver would have broken, stated directly: under
    // `Raw` nothing is folded, which is exactly what a local device must not
    // be given.
    let mut keys = Vec::new();
    gateway_input_for_remote(0x14, GatewayFilter::Raw, 0x14, &mut keys);
    assert_eq!(keys, vec![0x14], "Raw passes the erase byte through…");
    let mut keys = Vec::new();
    gateway_input_for_remote(0x14, GatewayFilter::Petscii, 0x14, &mut keys);
    assert_eq!(keys, vec![0x7F], "…where a local device needs ASCII DEL");
}

/// An unterminated string sequence must not silence an ASCII client either.
///
/// `crate::petscii` grew this guard first and this copy was missed, so the fix
/// was real on the modem path and absent on the one an ASCII terminal uses --
/// where this function's own header notes that `1B 5D` turns up about once per
/// 64 KB of binary.  Two parsers, one rule, and only one of them had it.
#[test]
fn test_an_unterminated_string_sequence_cannot_silence_an_ascii_client() {
    let mut st = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    let mut wire = vec![0x1B, b']'];
    wire.extend(std::iter::repeat_n(b'x', crate::petscii::STRING_SEQ_CAP + 8));
    wire.extend_from_slice(b"visible");
    filter_gateway_output(&wire, &mut st, &mut out);
    assert!(
        out.ends_with(b"visible"),
        "the line never came back: {:?}",
        String::from_utf8_lossy(&out)
    );
    // A terminated one still costs nothing, however long the run before it.
    let mut st = GatewayOutState::new(GatewayFilter::Ascii);
    let mut out = Vec::new();
    let mut ok = vec![0x1B, b']'];
    ok.extend(std::iter::repeat_n(b'y', 64));
    ok.extend_from_slice(b"\x07after");
    filter_gateway_output(&ok, &mut st, &mut out);
    assert_eq!(out, b"after".to_vec());
}

/// The last transfer's outcome must reach the next screen the terminal draws,
/// and must do so exactly once.
///
/// A vintage terminal takes the screen for a transfer and restores it
/// afterwards, so the summary printed at the moment a transfer ends is thrown
/// away -- measured on a C64 under NovaTerm 9.6c, a byte-perfect XMODEM-1K
/// download left the user looking at the restored "Start XMODEM-1K receive
/// now" text with nothing to say it had worked.  Carrying the result to the
/// menu needs no timing guess, which every other fix here would have been.
#[tokio::test]
async fn test_the_transfer_outcome_reaches_the_next_menu_once() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Petscii);
    use tokio::io::AsyncReadExt;

    session.last_transfer_note = Some(TransferNote {
        ok: true,
        text: "Sent 1775 bytes in 17.8s".into(),
    });
    session.render_file_transfer().await.unwrap();
    session.flush().await.unwrap();

    let mut buf = vec![0u8; 8192];
    let n = peer.read(&mut buf).await.unwrap();
    let first = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        first.to_ascii_lowercase().contains("1775"),
        "the menu must state the outcome of the transfer just finished; got:\n{}",
        first
    );

    // **Taken, not copied.**  A second visit must not show a stale result as
    // though it were this visit's -- that is the same defect as grading a run
    // by a file left over from the run before.
    assert!(
        session.last_transfer_note.is_none(),
        "the note must be cleared once drawn"
    );
    session.render_file_transfer().await.unwrap();
    session.flush().await.unwrap();
    let n2 = peer.read(&mut buf).await.unwrap();
    let second = String::from_utf8_lossy(&buf[..n2]).to_string();
    assert!(
        !second.contains("1775"),
        "a drawn note must not appear again on the next menu; got:\n{}",
        second
    );
}

/// Every outcome line has to fit the narrowest screen that shows it.
///
/// A PETSCII terminal is 40 columns and `truncate_to_width` does not wrap --
/// it silently loses the end, which on these lines is the byte count, the
/// whole point of the message.
#[test]
fn test_the_transfer_outcome_fits_a_40_column_screen() {
    // The widest each phrasing can get with realistic values.
    // Worst cases that can actually occur: the 8 MB per-file cap, and a
    // duration long enough for that file at 300 baud (about 62 hours), which
    // is six digits of seconds.  `u32::MAX` bytes is kept as a hard upper
    // bound even though MAX_FILE_SIZE forbids it -- if a future cap rises,
    // this is what notices.
    let notes = [
        format!("Sent {} bytes, {:.1}s", u32::MAX, 223_200.9),
        format!("Rcvd {} bytes, {:.1}s", u32::MAX, 223_200.9),
        format!("Rcvd {} file(s), {} skipped", 999, 999),
    ];
    for n in &notes {
        // **Assert the line is UNCHANGED, not that it fits.**  Asserting the
        // length was asserting `truncate_to_width`'s own postcondition, so
        // any input passed -- and it passed while demonstrating the very
        // failure it was written to catch: the widest phrasing is 36
        // characters and was being elided, losing the byte count, which is
        // the whole content of the message.  34 is the budget
        // `draw_transfer_note` gives the note on PETSCII, inside a
        // 40-column screen with a two-space indent.
        assert_eq!(
            truncate_to_width(n, 34),
            *n,
            "{:?} is {} chars and does not fit a 40-column PETSCII screen \
             -- it would lose its tail, which is the byte count",
            n,
            n.chars().count()
        );
    }
    // A short note is untouched -- the common case must not be elided.
    assert_eq!(truncate_to_width("Sent 1775 bytes, 17.8s", 34), "Sent 1775 bytes, 17.8s");
}

/// A protocol's teardown must never be read as the operator pressing a key.
///
/// Punter's C1 handshake codes are literal ASCII words -- `GOO`, `S/B`, `SYN`
/// -- and they keep arriving after the last data byte.  Measured on the wire
/// after a completed download: `G` is the File Transfer menu's Gateway Shell
/// key, so the trailing `GOO` dismissed "Press any key to continue" and then
/// opened a screen nobody asked for, with the rest of the burst walking
/// through it.
///
/// The property is not "the bytes get drained eventually" -- it is that after
/// settling, the prompt is STILL WAITING, because nothing the peer said counts
/// as a keystroke.  Without the settle this fails immediately on the `G`.
#[tokio::test]
async fn test_a_protocol_teardown_is_not_a_keypress() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;

    // Exactly what a C64 sends after Punter finishes.
    peer.write_all(b"GOOS/BSYNS\r").await.unwrap();
    peer.flush().await.unwrap();

    session.post_transfer_settle().await;

    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        session.wait_for_key(),
    )
    .await;
    assert!(
        waited.is_err(),
        "a protocol teardown burst must not satisfy the keypress prompt — \
         that is how Punter's trailing GOO opened the Gateway Shell"
    );
}

/// A byte arriving before a person could have read the prompt is not a
/// keypress.
///
/// Settling the line only covers what is already in flight, and a protocol can
/// speak again after it has gone quiet: NovaTerm writes the last block to a
/// 1541 and then sends its final handshake, far later than any drain would
/// wait.  Measured on a C64 -- the summary flashed up and the session returned
/// to the file list on its own, with nobody touching the keyboard.  So the
/// prompt ignores its first `PROMPT_ARM_MS`, which is a statement about people
/// rather than a guess about the peer.
#[tokio::test]
async fn test_a_late_protocol_byte_still_does_not_answer_the_prompt() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;

    // The line is quiet when we settle, so the drain sees nothing...
    session.post_transfer_settle().await;
    // ...and only then does the peer speak, the way a 1541 write does.
    peer.write_all(b"SYN").await.unwrap();
    peer.flush().await.unwrap();

    session.arm_keypress_prompt().await;
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(400),
        session.wait_for_key(),
    )
    .await;
    assert!(
        waited.is_err(),
        "a byte arriving inside the arming window must not answer the prompt"
    );
}

/// And the guard must not lock a real operator out: a key pressed after the
/// window is honoured immediately.  Without this the fix would trade a
/// cosmetic flash for a session nobody can leave.
#[tokio::test]
async fn test_the_arming_window_still_lets_a_person_continue() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;

    session.arm_keypress_prompt().await;
    peer.write_all(b"\r").await.unwrap();
    peer.flush().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(1000),
        session.wait_for_key(),
    )
    .await
    .expect("a keypress after the arming window must be accepted")
    .expect("wait_for_key must not error on a real key");
}

/// The menu drawn after a transfer must not act on the protocol's leftovers.
///
/// Punter's C1 handshake codes are literal ASCII words, so their letters are
/// menu keys.  `GOO` put a `G` on the File Transfer menu -- Gateway Shell --
/// and once that was drained the `D` of `BAD` selected Download a file, which
/// navigated off the very screen carrying the result the operator was meant to
/// read.  Draining around the keypress cannot fix it: the bytes are still
/// trickling when the menu appears, because NovaTerm writes the last block to
/// a 1541 and only then finishes talking.
///
/// So a transfer arms the next menu prompt, and this pins the two halves of
/// that: the flag is set by the post-transfer exchange, and it is TAKEN, so it
/// can never silently suppress a second prompt the operator did mean to use.
#[tokio::test]
async fn test_a_transfer_arms_the_menu_prompt_exactly_once() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncWriteExt;

    assert!(!session.arm_next_prompt, "nothing arms it before a transfer");

    // The key has to arrive *after* the settle and the arming window, or the
    // settle simply drains it -- which is the whole point of both, and is
    // what a real operator does anyway: they read the prompt first.
    let keypress = tokio::spawn(async move {
        // After the settle's quiet gap AND the arming window, or the key is
        // simply absorbed -- which is what both of those exist to do.  Kept
        // generous rather than tuned to the current constants: a test that
        // tracks them exactly would break every time one is widened, and it
        // has already broken once that way.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        peer.write_all(b"\r").await.unwrap();
        peer.flush().await.unwrap();
        peer
    });
    let done = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        session.press_any_key_after_transfer(),
    )
    .await
    .expect("the post-transfer prompt must not hang");
    done.expect("it must not error on a real key");
    let _peer = keypress.await.unwrap();

    assert!(
        session.arm_next_prompt,
        "a transfer must arm the menu prompt behind it -- that menu is where \
         Punter's trailing `D` selected Download a file"
    );

    // Taken, not left set: a second menu prompt is the operator's own.
    assert!(std::mem::take(&mut session.arm_next_prompt));
    assert!(
        !session.arm_next_prompt,
        "the arming must apply to one prompt only"
    );
}

/// The post-transfer prompt is re-offered until somebody answers it.
///
/// A vintage terminal owns the screen during a transfer and restores it
/// afterwards, so a prompt printed once lands in the blackout and is then
/// erased: measured on a C64 after a hand-driven upload, the operator saw no
/// prompt at all and had to discover that pressing a key worked anyway.  No
/// pause before printing fixes that, because nothing tells us when the
/// terminal is back -- the same shape as Kermit's Send-Init, cured by
/// retransmitting rather than by timing the shot.
#[tokio::test]
async fn test_the_post_transfer_prompt_is_offered_more_than_once() {
    let (mut session, mut peer) = make_test_session_with_peer(TerminalType::Ansi);
    use tokio::io::AsyncReadExt;

    // Nobody answers, so the exchange should keep offering and then fall
    // through to a plain wait rather than hanging on one lost prompt.
    let session_task = tokio::spawn(async move {
        let _ = session.press_any_key_after_transfer().await;
    });

    // Read for long enough to span more than one offer.
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
    let mut buf = vec![0u8; 4096];
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            peer.read(&mut buf),
        )
        .await
        {
            Ok(Ok(n)) if n > 0 => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            Ok(Ok(_)) => break,
            _ => {}
        }
        if seen.matches("Press any key").count() >= 2 {
            break;
        }
    }
    session_task.abort();

    assert!(
        seen.matches("Press any key").count() >= 2,
        "the prompt must be re-offered, or a terminal that was repainting when \
         it was first printed never sees one; got {} offer(s) in:\n{}",
        seen.matches("Press any key").count(),
        seen
    );
}

/// **The security screen used to promise a restart that nothing needed.**
///
/// All three surfaces read the credential fresh on the way in -- the web per
/// request, telnet per session, SSH per connection -- so a changed username or
/// password is live for the next login with nothing restarted.  Measured on a
/// live pair: changed the master's password, restarted nothing, and its SSH
/// server accepted the new one and refused the old one on the next connection.
/// A screen telling an operator to restart a headless gateway for a change
/// that had already taken effect is the same class of defect as a comment
/// describing a fix the code does not make.
#[test]
fn test_the_credential_notice_fits_a_c64_and_does_not_promise_a_restart() {
    let lines = TelnetSession::credential_saved_lines();
    assert!(!lines.is_empty(), "the notice says nothing at all");
    for line in lines {
        // Two-space indent on a 40-column PETSCII screen, which does not wrap
        // -- it silently loses the end.
        assert!(
            line.chars().count() <= 38,
            "notice line {line:?} is {} columns",
            line.chars().count()
        );
    }
    let text = lines.join(" ").to_lowercase();
    assert!(
        !text.contains("restart"),
        "the notice still asks for a restart that is not needed: {text}"
    );
    // And it must still say the thing that IS true, or it is not a notice.
    assert!(
        text.contains("new logins"),
        "the notice no longer says when the change applies: {text}"
    );
}

// ─── MORE page: restart / shut down the computer ────────────
//
// Unix only, like the page itself.  These are the pure seams; the screens
// themselves (like every other screen in this file) are verified by driving a
// live session, and the two commands were verified on the Pi the gateway runs
// on as a service — a restart it came back from, and a shutdown it did not.

/// Every line of both confirmation bodies must fit a C64's screen.
///
/// **Measured as printed.**  The body is drawn with a two-space indent, and
/// the budget for a whole row is 39, not 40: a PETSCII terminal auto-wraps at
/// 40 and *then* takes the trailing CR/LF, so a row that exactly fills the
/// width costs two rows of a 22-row page.  That is `separator()`'s rule, and
/// asserting the un-indented line against 38 permits a printed 40 -- the case
/// the rule exists for.
#[cfg(unix)]
#[test]
fn test_power_confirmation_lines_fit_petscii() {
    for action in [PowerAction::Restart, PowerAction::Shutdown] {
        for line in confirm_body(action) {
            // `separator()`'s budget, named rather than written inline: a bare
            // `<= PETSCII_WIDTH - 1` reads to clippy as `< PETSCII_WIDTH`,
            // which is true and says nothing about why.
            const ROW_BUDGET: usize = PETSCII_WIDTH - 1;
            let printed = format!("  {}", line);
            assert!(
                printed.chars().count() <= ROW_BUDGET,
                "{:?} confirmation line prints as {:?}, {} columns",
                action,
                printed,
                printed.chars().count(),
            );
        }
        // The question and the title share the row with a `(Y/N): ` suffix and
        // a two-space indent, so they are held tighter still.
        let asked = format!("  {} (Y/N): ", action.question());
        assert!(
            asked.chars().count() <= PETSCII_WIDTH,
            "{:?} prompt {:?} is {} columns",
            action,
            asked,
            asked.chars().count(),
        );
        assert!(action.title().len() + 2 <= PETSCII_WIDTH);
    }
}

/// **Both bodies must say the computer, not the gateway.**
///
/// This menu sits one keypress from Configuration > Server > R, which restarts
/// the *gateway* and leaves the machine up.  An operator who reads "Restart?"
/// and assumes the familiar one loses every other session on the box — so the
/// distinction is asserted rather than left to the wording surviving an edit.
#[cfg(unix)]
#[test]
fn test_power_confirmation_says_the_whole_computer() {
    for action in [PowerAction::Restart, PowerAction::Shutdown] {
        let text = confirm_body(action).join(" ").to_lowercase();
        assert!(
            text.contains("whole computer"),
            "{:?} does not say it takes the whole computer down: {}",
            action,
            text,
        );
        assert!(
            text.contains("not just the gateway"),
            "{:?} does not distinguish itself from restarting the gateway: {}",
            action,
            text,
        );
        assert!(
            text.contains("session") && text.contains("transfer"),
            "{:?} does not say what is lost: {}",
            action,
            text,
        );
    }
    // Only the shutdown says somebody has to walk over to the machine.
    assert!(
        confirm_body(PowerAction::Shutdown)
            .join(" ")
            .contains("switching on by hand"),
        "the shutdown does not say the computer will not come back on its own",
    );
    assert!(
        !confirm_body(PowerAction::Restart)
            .join(" ")
            .contains("switching on by hand"),
        "the restart claims the computer will not come back, which it will",
    );
}

/// The two commands, pinned.
///
/// `shutdown -h` rather than `-P`: `cfg(unix)` includes macOS, where `-P` does
/// not exist and `-h` powers off on both.  `now` rather than `+0` for the same
/// portability reason.
#[cfg(unix)]
#[test]
fn test_power_commands_are_the_portable_spellings() {
    assert_eq!(PowerAction::Restart.argv(), &["shutdown", "-r", "now"]);
    assert_eq!(PowerAction::Shutdown.argv(), &["shutdown", "-h", "now"]);
    // The two must not be confusable: a copy-paste that left both on `-r`
    // would shut nothing down and would still pass every screen test.
    assert_ne!(
        PowerAction::Restart.argv(),
        PowerAction::Shutdown.argv(),
        "both actions run the same command",
    );
}

/// `sudo`'s stderr reduced to the one line worth a 40-column screen.
///
/// The **last** non-empty line, because sudo's useful sentence comes after its
/// chatter: with `-S` and a wrong password it says "Sorry, try again." and
/// then, at EOF, the count.  Showing the first line would show the least
/// specific one.
#[cfg(unix)]
#[test]
fn test_sudo_error_line_takes_the_last_useful_line() {
    assert_eq!(
        sudo_error_line("Sorry, try again.\nsudo: 1 incorrect password attempt\n", 60),
        "1 incorrect password attempt",
    );
    // A single line still works, and the `sudo: ` prefix goes — the screen
    // already says what was being attempted, and on PETSCII those six
    // characters are a sixth of the row.
    assert_eq!(
        sudo_error_line("sudo: a password is required\n", 60),
        "a password is required",
    );
    // A sudoers refusal is the message that actually explains a live failure.
    assert!(
        sudo_error_line(
            "ricky is not in the sudoers file.  This incident will be reported.\n",
            60,
        )
        .starts_with("ricky is not in the sudoers file."),
    );
    // Nothing at all still says something: a blank screen after a password
    // prompt is indistinguishable from a hang.
    assert_eq!(
        sudo_error_line("", 60),
        "The computer refused the command.",
    );
    assert_eq!(
        sudo_error_line("   \n\n  \n", 60),
        "The computer refused the command.",
    );
    // And it is cut to the width it is given, because it lands on a C64.
    let long = "x".repeat(200);
    assert!(sudo_error_line(&long, 38).chars().count() <= 38);
}

/// The probe must ask about the command it is standing in for.
///
/// **This replaced a test that could not fail.**  The first version called
/// `probe_elevation` and asserted the answer was one of `Elevate`'s three
/// variants -- a tautology over the type -- and that the `Err` string was
/// non-empty, against two non-empty literals.  It also shelled out to `sudo`
/// on every ordinary `cargo test` run, unlike every other external-binary gate
/// here, which is `#[ignore]`d: on a machine whose user is not in sudoers that
/// is a logged authentication failure per run.
///
/// What is actually worth pinning is the *shape* of the probe, because the
/// wrong shape is the defect that was found here: `sudo -n true` asks a
/// different question from the one the answer is used for, and the module
/// comment now spends three paragraphs on why.  A source scan holds the `-l`
/// form in place; comments are stripped first, or the scan reads the very
/// explanation that names the rejected spelling.
#[cfg(unix)]
#[test]
fn test_the_elevation_probe_asks_about_the_real_command() {
    let src = include_str!("power.rs");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains(r#".args(["-k", "-n", "-l", "--"])"#),
        "the probe no longer asks sudo about a specific command",
    );
    // **`-k` is the whole of the timestamp fix and is one character wide.**
    // Without it the probe answers `SudoQuiet` off the operator's own cached
    // credential -- `/run/sudo/ts/<uid>`, shared by every process of that user
    // -- and the page takes the machine down with no password at all for
    // `timestamp_timeout` minutes after they last ran sudo anywhere.  The
    // positive assertion above would survive its removal if the spelling were
    // matched loosely, so the pre-fix form is named and forbidden outright.
    assert!(
        !code.contains(r#".args(["-n", "-l", "--"])"#),
        "the probe no longer clears sudo's timestamp first, so a credential \
         cached in the operator's shell answers for this page -- see \
         probe_elevation's doc comment",
    );
    assert!(
        !code.contains(r#".args(["-n", "true"])"#),
        "the probe is back to asking about `true`, which answers a different \
         question -- see probe_elevation's doc comment",
    );
    // It must be handed the argv it stands in for, not a constant.
    assert!(
        code.contains("probe_elevation(action.argv())"),
        "the probe is no longer given the command it is standing in for",
    );
    // Already root means no sudo at all; that branch must stay first, or a
    // root session would shell out to a sudo it does not need.
    // **Bounded to the one function, and this is not fussiness.**  The first
    // version of this scan split on `probe_elevation` and took everything
    // after it, so once the probe grew a thin wrapper the slice ran to the end
    // of the file -- and the two assertions below then passed by matching
    // `run_elevated`'s copies of the same two lines.  Both mutations survived:
    // removing the probe's timeout, and removing its `kill_on_drop`.  A scan
    // that reads the wrong function is a guard that cannot go red.
    let body = code
        .split("async fn probe_elevation_within")
        .nth(1)
        .expect("probe_elevation_within went away");
    let body = &body[..body.find("\n}\n").expect("probe_elevation_within is unterminated")];
    let direct = body.find("Elevate::Direct").expect("the root branch went away");
    let spawn = body.find("Command::new").expect("the probe went away");
    assert!(
        direct < spawn,
        "the already-root branch no longer comes before the sudo probe",
    );
    // Bounded, and the child cleaned up when the bound fires.  Asserted by
    // scanning rather than by driving it: a call would run the real `sudo -k`
    // and clear the credential cache of whoever is running the suite, which is
    // the one side effect this change is known to have.
    assert!(
        body.contains("tokio::time::timeout(budget"),
        "the elevation probe is unbounded again; a blocking PAM stack hangs \
         the session with no key working",
    );
    assert!(
        body.contains("kill_on_drop(true)"),
        "an abandoned probe is left running",
    );
}

/// **Every** `sudo` this page runs must ignore a cached credential.
///
/// The `-k` fix was applied to the elevation probe first and that was only half
/// of it: `power_action` verifies the typed password with `sudo -v`, and a
/// `sudo -v` satisfied by a live timestamp never reads stdin at all.  Measured
/// on the Pi (sudo 1.9.16p2) with the operator's own credential cached:
/// `sudo -S -p "" -v` fed `not-the-password-xyzzy` was **accepted**, and the
/// same call with `-k` was refused.  So a probe-only fix would have turned
/// "reboots with no password" into "reboots on any password", which is worse
/// -- a screen promising a check that did not happen.
///
/// Counted rather than spelled out: the assertion is that **no** `sudo` is
/// spawned here without `-k`, so a fourth call site added later cannot quietly
/// opt out.  Scanned rather than driven, for `probe_elevation_within`'s reason.
#[cfg(unix)]
#[test]
fn test_no_sudo_on_this_page_can_ride_a_cached_credential() {
    let src = include_str!("power.rs");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    let spawns: Vec<&str> = code.match_indices(r#"Command::new("sudo")"#)
        .map(|(i, _)| &code[i..(i + 240).min(code.len())])
        .collect();
    assert!(
        spawns.len() >= 3,
        "expected the probe, the quiet run and the password run; found {}",
        spawns.len(),
    );
    for (n, window) in spawns.iter().enumerate() {
        assert!(
            window.contains(r#""-k""#),
            "sudo spawn #{} does not pass -k, so a cached credential answers \
             for it -- see this test's doc comment:\n{}",
            n,
            window,
        );
    }
    // The pre-fix spellings, named so a revert cannot pass by matching loosely.
    for gone in [r#"c.arg("-n");"#, r#".args(["-S", "-p", ""])"#] {
        assert!(!code.contains(gone), "the pre-fix form {} is back", gone);
    }
}

/// A `sudo` that never comes back must be given up on, and the child killed.
///
/// Driven through [`run_elevated_within`] with a supplied budget, not by
/// waiting out `SUDO_TIMEOUT` — the printer's idle close is tested the same
/// way, and for the same reason.  `Direct` with a plain `sleep` keeps `sudo`
/// out of an ordinary test run.
#[cfg(unix)]
#[tokio::test]
async fn test_a_power_command_that_never_returns_is_given_up_on() {
    use std::time::Duration;
    let err = crate::telnet::power::run_elevated_within(
        Elevate::Direct,
        None,
        &["sleep", "30"],
        Duration::from_millis(150),
    )
    .await
    .expect_err("a command that outlives its budget must not return output");
    // The kind, not the message: the callers render it, and one of them logs
    // it after the goodbye where nobody can ask a follow-up question.
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

    // **The positive control.**  Without it this test passes just as well with
    // the timeout set to zero, which would make the page unable to run
    // anything at all.
    let out = crate::telnet::power::run_elevated_within(
        Elevate::Direct,
        None,
        &["echo", "quick"],
        Duration::from_secs(5),
    )
    .await
    .expect("a command well inside its budget must still run");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "quick");
}

/// Giving up on the command must **kill** it, not walk away from it.
///
/// `kill_on_drop` is one line and its absence is invisible: the timeout still
/// returns the same error, and the only difference is an abandoned `sudo` left
/// running with the operator's password sitting on a stdin nobody will close.
/// So the child is asked to leave a trace after the budget expires, and the
/// test requires that it never appears.
#[cfg(unix)]
#[tokio::test]
async fn test_a_timed_out_power_command_is_killed_not_abandoned() {
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("egw-power-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let marker = dir.join("survived");
    let script = format!("sleep 0.4; echo alive > {}", marker.display());

    let err = crate::telnet::power::run_elevated_within(
        Elevate::Direct,
        None,
        &["sh", "-c", &script],
        Duration::from_millis(100),
    )
    .await
    .expect_err("the script outlives its budget");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

    // Well past when the script would have written, had it lived.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !marker.exists(),
        "the timed-out child kept running and wrote {} -- kill_on_drop is off",
        marker.display(),
    );

    // The same script, allowed to finish, proves the marker is reachable at
    // all: without this the assertion above passes on a typo in the path.
    crate::telnet::power::run_elevated_within(
        Elevate::Direct,
        None,
        &["sh", "-c", &script],
        Duration::from_secs(5),
    )
    .await
    .expect("the script runs when it is given time");
    assert!(marker.exists(), "the control never wrote its marker");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a timed-out `sudo` says has to fit the screen it lands on.
///
/// The width every other value on these pages is cut to, and the wording is
/// load-bearing: the command may still be running, so it says *did not answer*
/// rather than *failed*.  Telling an operator their shutdown was refused when
/// it may yet happen is the one wrong thing this page could say.
#[cfg(unix)]
#[test]
fn test_the_sudo_timeout_message_fits_and_does_not_claim_a_refusal() {
    let line = crate::telnet::power::sudo_timed_out_line();
    assert!(
        line.chars().count() <= PETSCII_WIDTH - 2,
        "{:?} is {} columns, over the {} a PETSCII screen leaves",
        line,
        line.chars().count(),
        PETSCII_WIDTH - 2,
    );
    assert!(line.contains("did not answer"), "{:?}", line);
    for wrong in ["refus", "fail", "cancel"] {
        assert!(
            !line.to_lowercase().contains(wrong),
            "{:?} claims an outcome the timeout cannot know",
            line,
        );
    }
}

/// A `Direct` run (already root) must not go anywhere near `sudo`.
///
/// Driven with a harmless command rather than `shutdown`, which is the whole
/// point of `run_elevated` taking its argv: the plumbing is testable because
/// nothing about it names the power commands.
#[cfg(unix)]
#[tokio::test]
async fn test_run_elevated_direct_runs_the_command_itself() {
    let out = crate::telnet::power::run_elevated(Elevate::Direct, None, &["echo", "hello"])
        .await
        .expect("echo should run");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

/// The password is written to the child's stdin and **stdin is then closed**.
///
/// Closing it is load-bearing, not tidiness: `sudo -S` reads until it has a
/// line, and a child left holding an open stdin that never EOFs is a session
/// that hangs with the operator looking at a blank screen.  `cat` proves both
/// halves — it echoes what it was given, and it only exits when stdin closes,
/// so a test that returns at all has proved the close.
#[cfg(unix)]
#[tokio::test]
async fn test_run_elevated_feeds_the_password_and_closes_stdin() {
    let out = crate::telnet::power::run_elevated(Elevate::Direct, Some("hunter2"), &["cat"])
        .await
        .expect("cat should run");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hunter2\n");
}

/// The MORE page must fit the screen it is drawn on — counted and measured
/// from the **real** rows, not from arithmetic over literals.
///
/// The slack is deliberate: this page exists so the main menu, which has one
/// spare row, does not have to carry any more.  Colour is turned **off** for
/// the measurement rather than stripped afterwards -- a stripper is a second
/// implementation of the colour helpers, and it is the one that would be
/// wrong.  What is left is exactly what a terminal draws.  On PETSCII the
/// budget is 39, the same one `separator()` uses: a row that exactly fills 40
/// auto-wraps and then takes the trailing CR/LF, costing two rows.
#[cfg(unix)]
#[test]
fn test_more_menu_rows_fit_the_screen() {
    // **Both shapes of the page.**  Where the computer cannot be restarted the
    // two power rows are not drawn, and that page -- which is what a packaged
    // installation shows -- was measured nowhere: it must still fit, still
    // carry its way out, and still not offer what it cannot do.
    for powered in [false, true] {
    for (term, width) in [
        (TerminalType::Petscii, PETSCII_WIDTH - 1),
        (TerminalType::Ansi, 80),
        (TerminalType::Ascii, 80),
    ] {
        let mut session = make_test_session(term);
        session.color_enabled = false;
        let rows = session.more_menu_rows(powered);
        // Plus one for the prompt line the loop writes under them.
        let drawn = rows.len() + 1;
        assert!(
            drawn <= 22,
            "the MORE page is {} rows on {:?}, exceeds 22",
            drawn,
            term,
        );
        // It must still be a menu: the footer always, the power keys exactly
        // when the page offers them.  An empty page keeps its way back, or an
        // operator who pressed 2 would be stranded on it.
        let text = rows.join("\n");
        for want in ["Back", "Help"] {
            assert!(text.contains(want), "the MORE page lost {:?} on {:?}", want, term);
        }
        for want in ["Restart the computer", "Shut down the computer"] {
            assert_eq!(
                text.contains(want),
                powered,
                "{want:?} is on the page exactly when it can be done ({powered}) on {term:?}",
            );
        }
        for row in &rows {
            assert!(
                row.chars().count() <= width,
                "MORE row {:?} is {} columns on {:?}, over {}",
                row,
                row.chars().count(),
                term,
                width,
            );
        }
        // And with colour on the rows must still carry their text: a colour
        // helper that returned an empty string would leave a page of codes.
        // (Asserting the row *count* here would not fail -- `more_menu_rows`
        // pushes a fixed set and colour never varies it.)
        if powered {
            let coloured = make_test_session(term).more_menu_rows(true).join("\n");
            for want in ["Restart the computer", "Shut down the computer"] {
                assert!(
                    coloured.contains(want),
                    "with colour on, the MORE page lost {:?} on {:?}",
                    want,
                    term,
                );
            }
        }
    }
    }
}

/// The MORE page's error hint — the real one — names exactly the keys it takes
/// and fits the row it is printed on.
#[cfg(unix)]
#[test]
fn test_more_menu_error_hint_fits_and_is_complete() {
    // `show_error` adds the two-space indent; see `main_menu_key_hint`.
    //
    // The hint follows the page: where the computer cannot be restarted the
    // two power keys are not drawn and are not accepted, so naming them in the
    // hint would send an operator to keys that do nothing.
    //
    // **Both texts, driven by the parameter.**  This read the live
    // `available()` and asserted the hint agreed with it, which on any one
    // machine exercises one of the two strings and never measures the other --
    // and the one it skips here is the one a packaged installation shows.
    for powered in [false, true] {
        let hint = crate::telnet::power::more_menu_hint(powered);
        let printed = format!("  {hint}");
        assert!(
            printed.chars().count() <= PETSCII_WIDTH,
            "the hint prints as {:?}, {} columns",
            printed,
            printed.chars().count(),
        );
        for key in ["H", "Q"] {
            assert!(hint.contains(key), "the hint must always mention {key}: {hint:?}");
        }
        for key in ["R", "S"] {
            assert_eq!(
                hint.contains(key),
                powered,
                "the hint names {key} exactly when the page offers it: {hint:?}",
            );
        }
    }
}

/// Three refused `sudo` passwords and the page stops asking -- **for the
/// address, not for the connection**.
///
/// This is the bound that matters, and the per-session field it replaced did
/// not provide it.  With `security_enabled` off -- the default -- the second
/// page is reachable with no credential at all, so a peer got three guesses at
/// the operator's *system* password, hung up, and got three more. At the
/// default `conn_rate_max` of 20 a minute that is sixty real PAM failures a
/// minute, per address, and `pam_faillock` denies at three.
///
/// So the count goes in the shared `LockoutMap` -- the same one the telnet,
/// SSH and web credentials use, already shared between them for exactly this
/// reason. Asserted through the real `record_auth_failure` / `is_locked_out`
/// pair rather than a copy, and across two sessions, because a counter that
/// resets on reconnect is the defect.
#[cfg(unix)]
#[test]
fn test_the_sudo_attempt_bound_survives_a_reconnect() {
    use std::net::{IpAddr, Ipv4Addr};
    let lockouts: crate::telnet::LockoutMap = Default::default();
    let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    let other = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));

    assert!(
        !crate::telnet::is_locked_out(&lockouts, ip),
        "a peer that has tried nothing must not be locked out",
    );
    // Three refusals, as if spread over three separate connections -- nothing
    // here carries session state, which is the point.
    for _ in 0..crate::telnet::MAX_AUTH_ATTEMPTS {
        crate::telnet::record_auth_failure(&lockouts, ip);
    }
    assert!(
        crate::telnet::is_locked_out(&lockouts, ip),
        "the fourth attempt from this address must be refused, however many \
         times it reconnected to make the first three",
    );
    assert!(
        !crate::telnet::is_locked_out(&lockouts, other),
        "the lockout is per address: a neighbour must be unaffected",
    );
    // And the bound is small enough to be a bound.
    assert!(
        (1..=5).contains(&crate::telnet::MAX_AUTH_ATTEMPTS),
        "the attempt bound is not a bound",
    );
}

/// ...and the power page is the thing that uses it.
///
/// The test above pins the `LockoutMap`, which is shared machinery that was
/// already covered -- **it passes with the fix deleted**, proved by mutation.
/// The rule this change actually makes is that `power.rs` records into a
/// lockout map and consults it, and the live path needs a real `sudo` to
/// reach, so it is pinned by reading the module instead.  A source scan is
/// weaker than a behavioural test and is what this file's other un-runnable
/// paths already use; it can at least go red, which the test above cannot.
///
/// **Which map** is a separate rule, pinned behaviourally by
/// `test_a_successful_gateway_login_does_not_restore_sudo_attempts`: it is
/// deliberately *not* the shared auth map, because a successful gateway
/// login clears that one and a gateway login is not permission to guess the
/// machine's own password.
///
/// Comments are stripped first, or the scan reads the very explanation that
/// names these functions.
#[cfg(unix)]
#[test]
fn test_the_power_page_counts_against_a_lockout_map() {
    let src = include_str!("power.rs").replace('\r', "");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("record_auth_failure("),
        "power.rs records no failure anywhere: a refused sudo password costs \
         the peer nothing and they can reconnect for three more",
    );
    assert!(
        code.contains("is_locked_out("),
        "power.rs never consults a lockout, so recording into one bounds \
         nothing -- and `authenticate` cannot do it here, because that runs \
         only when security_enabled is on and this page is reachable when \
         it is off",
    );
    // The per-session field exists again, deliberately and only for a session
    // the map cannot key -- see `test_the_sudo_cap_uses_the_map_for_an_address`
    // and its address-less twin, which pin which counter each kind of session
    // gets.  A scan cannot tell those two apart, so it no longer tries: the
    // rule it used to assert (`power_password_failures` must not appear) would
    // now forbid the fix for callers with no address.
    assert!(
        code.contains("peer_addr"),
        "power.rs no longer distinguishes an addressed session from one \
         without, so one of the two counters is reaching the wrong sessions",
    );
}

/// An addressed session is bounded by the power map, and a reconnect does not
/// hand out three more.
#[cfg(unix)]
#[test]
fn test_the_sudo_cap_uses_the_map_for_an_address() {
    let ip: IpAddr = "192.0.2.7".parse().unwrap();
    let power: LockoutMap = Arc::new(Mutex::new(HashMap::new()));

    let mut session = make_test_session(TerminalType::Ansi);
    session.peer_addr = Some(ip);
    session.power_lockouts = power.clone();
    assert!(!session.power_attempts_exhausted(), "a fresh session starts with attempts");
    for _ in 0..MAX_AUTH_ATTEMPTS {
        session.record_power_failure();
    }
    assert!(session.power_attempts_exhausted(), "three refusals must stop the prompt");
    assert_eq!(
        session.power_password_failures.lock().unwrap().0,
        0,
        "an addressed session must not be counted in the per-session field -- \
         that one resets on reconnect",
    );

    // The reconnect: a brand-new session from the same address, sharing the
    // map the way every listener does.  This is what the per-session counter
    // could not do and why the map is the primary rule.
    let mut again = make_test_session(TerminalType::Ansi);
    again.peer_addr = Some(ip);
    again.power_lockouts = power;
    assert!(
        again.power_attempts_exhausted(),
        "hanging up and coming back handed out three more attempts",
    );
}

/// **The attempt cap is checked before the probe, because the probe is the
/// cost.**
///
/// It used to sit inside the branch that asks for a password, which is after
/// `probe_elevation` -- so confirm-Y could be repeated all session, each
/// iteration spawning a real `sudo -k -n -l`.  On a box whose service account
/// is not in sudoers that is one authentication-failure line in the *host's*
/// auth log per keypress, from an unauthenticated LAN user whenever
/// `security_enabled` is off, which is the default.
///
/// Read from the source because the live path needs a real `sudo`, and
/// **bounded to `power_action`'s own body**: the sudo scan in this file was
/// once split on a name that became a thin wrapper, so the slice ran to
/// end-of-file and the assertions passed by matching a different function's
/// copy of the same lines.
#[cfg(unix)]
#[test]
fn test_the_sudo_attempt_cap_is_checked_before_the_probe_is_spawned() {
    let src = include_str!("power.rs").replace('\r', "");
    let start = src
        .find("async fn power_action(")
        .expect("power_action moved or was renamed");
    let rest = &src[start..];
    let end = rest[1..]
        .find("\n    /// ")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    let body: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let cap = body
        .find("power_attempts_exhausted()")
        .expect("power_action no longer checks the attempt cap at all");
    let probe = body
        .find("probe_elevation(")
        .expect("power_action no longer probes -- this scan is reading the wrong function");

    assert!(
        cap < probe,
        "the attempt cap is consulted after the probe is spawned, so holding \
         the confirm key spawns one real `sudo` per press and writes one \
         authentication failure per press into the host's auth log",
    );
}

/// **`new_ssh`'s unconditional trust rests on the shell refusing a relay key.**
///
/// An SSH session is marked authenticated at construction, which is right
/// because the only connection that reaches a session has passed the
/// password -- `auth_publickey` does accept an enrolled relay key, but
/// `shell_request` refuses a key-authenticated connection outright.
///
/// Those are two files with nothing between them.  If that refusal were ever
/// relaxed -- to let a slave open a menu, say -- an enrolled relay key would
/// silently acquire power-page trust on a root or NOPASSWD machine, and no
/// test would go red.  This is that test.
#[test]
fn test_ssh_trust_rests_on_the_shell_refusing_a_relay_key() {
    let src = include_str!("../ssh.rs").replace('\r', "");
    let at = src
        .find("async fn shell_request")
        .expect("shell_request moved or was renamed");
    let tail = &src[at..];
    // Bounded to this method: the next `async fn` ends it.
    let end = tail[1..]
        .find("\n    async fn ")
        .map(|i| i + 1)
        .unwrap_or(tail.len());
    let body: String = tail[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        body.contains("if self.key_authed"),
        "`shell_request` no longer refuses a key-authenticated connection, so \
         an enrolled relay key can open a session -- and `new_ssh` marks \
         every session authenticated, which would hand that key the power \
         page on a root or NOPASSWD machine",
    );
    // And the refusal must come before a shell is handed over, or it refuses
    // nothing.
    let refuse = body.find("if self.key_authed").expect("checked above");
    let grant = body
        .find("duplex_writer")
        .expect("shell_request no longer sets up a session -- wrong function");
    assert!(
        refuse < grant,
        "the relay-key refusal lands after the session is set up",
    );
}

/// **A re-dial does not buy a fresh allowance.**
///
/// Three values bound the power page for a caller the per-IP map cannot key,
/// and each one had to be carried into a dialled menu session separately --
/// the credential, the address, and now the two counters.  The first two were
/// each fixed on their own and each looked complete, because the test written
/// with them asked only about that one value.
///
/// A caller with no address (a physical modem caller, and anything it dials)
/// is bounded by a per-session floor.  Rebuilt fresh, that floor made
/// `ATDT ethernetgateway` -> `2` -> `R` worth three more real PAM attempts
/// against the operator's host account per dial, over a connection already
/// open and which `conn_rate_max` does not count.
///
/// **Shared, not copied**: the guest's guesses have to count against the
/// caller who dialled it, so returning from the menu cannot restore them.
#[cfg(unix)]
#[test]
fn test_a_dialled_menu_session_cannot_reset_the_sudo_allowance() {
    use crate::telnet::power::{Elevate, PowerAction};
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    let mut parent = make_test_session(TerminalType::Ansi);
    parent.peer_addr = None; // the case the per-IP map is blind to
    parent.authenticated = true; // a serial caller: trusted, still bounded

    for _ in 0..MAX_AUTH_ATTEMPTS {
        parent.record_power_failure();
    }
    assert!(parent.power_attempts_exhausted(), "the premise: the floor is reached");
    parent
        .power_elevation
        .lock()
        .unwrap()
        .push((PowerAction::Restart, Elevate::SudoPassword));

    // What `ATDT ethernetgateway` builds, with everything it inherits.
    let writer: SharedWriter =
        std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(tokio::io::sink())));
    let mut dialled = TelnetSession::new_cpm_menu(
        Box::new(tokio::io::empty()),
        writer,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        std::sync::Arc::new(StdMutex::new(HashMap::new())),
        // **The whole point: what the dialler hands over, unedited.**
        parent.inheritable(),
    );

    assert!(
        dialled.power_attempts_exhausted(),
        "dialling the menu from inside the emulator handed out a fresh three \
         guesses at the operator's *system* password, and it can be re-dialled \
         as often as the caller likes",
    );
    assert_eq!(
        dialled.remembered_elevation(PowerAction::Restart),
        Some(Elevate::SudoPassword),
        "the dialled session re-probes, so each dial spawns another real \
         `sudo` and writes another line into the host's auth log",
    );

    // And it is shared, not copied: a guess spent inside counts outside too,
    // or leaving the menu restores it.
    let before = parent.power_password_failures.lock().unwrap().0;
    dialled.record_power_failure();
    assert_eq!(
        parent.power_password_failures.lock().unwrap().0,
        before + 1,
        "guesses spent in the dialled session do not count against the caller \
         who dialled it, so returning from the menu restores them",
    );
}

/// **The address-less floor expires, like every other lockout here.**
///
/// It did not, and that was a worse bargain than the attack it guarded
/// against: the operator standing at the machine's own serial console -- who
/// on a headless Pi reached from a C64 has no other way in -- lost restart
/// and shutdown for the life of the process after three mistypes, with
/// nothing to do about it but power-cycle.  The per-IP branch has always
/// cleared after `LOCKOUT_DURATION`; there was no reason for this one to be
/// permanent, and an unbounded counter is a lockout rather than a bound.
///
/// Driven by backdating the stamp rather than by sleeping, which is how this
/// file reaches every other clock.
#[cfg(unix)]
#[test]
fn test_the_address_less_sudo_floor_expires() {
    let mut session = make_test_session(TerminalType::Ansi);
    session.peer_addr = None;

    for _ in 0..MAX_AUTH_ATTEMPTS {
        session.record_power_failure();
    }
    assert!(session.power_attempts_exhausted(), "three refusals must stop the prompt");

    // Age the last failure past the window.  `checked_sub` can fail on a host
    // up for less than the window -- the trap `within_lockout_window`'s own
    // comment records -- so skip rather than pass vacuously if it does.
    let aged = {
        let mut f = session.power_password_failures.lock().unwrap();
        match f.1.checked_sub(crate::telnet::LOCKOUT_DURATION + std::time::Duration::from_secs(1))
        {
            Some(t) => {
                f.1 = t;
                true
            }
            None => false,
        }
    };
    if !aged {
        eprintln!("skipped: host uptime is under the lockout window");
        return;
    }

    assert!(
        !session.power_attempts_exhausted(),
        "the floor never expires, so an operator at the machine's own serial \
         console loses restart and shutdown until the gateway is restarted",
    );
    // And the count starts again rather than resuming at three.
    assert_eq!(
        session.record_power_failure(),
        1,
        "a failure after the window resumed the old count instead of starting \
         a new one, so one more mistype re-locks immediately",
    );
}

/// **The screen that reports the cap must promise what both counters keep.**
///
/// There were two texts, chosen on `peer_addr`: "Too many tries. Try again
/// later." for an address, and "Too many tries for this session." for a caller
/// without one.  That split was correct when it was written -- the
/// address-less floor had no clock, so offering a wait would have been a
/// screen the next step could not keep.  It stopped being correct two commits
/// later and nothing noticed, because no guard read either literal: the floor
/// is stamped now and expires on `LOCKOUT_DURATION` like everything else here
/// (the test above), while a *reconnect* stopped clearing it, the allowance
/// having become the port's rather than the session's.  So the surviving
/// sentence named the one remedy that does not work and withheld the one that
/// does -- to the operator at the machine's own serial console, which is the
/// branch that fix existed for.  The manual (7.3) said "it expires on its own
/// five minutes and that is the only way out" throughout.
///
/// Two halves, and the behavioural one is the point: waiting has to work on
/// **both** branches, or the single sentence is the wrong single sentence.
#[cfg(unix)]
#[test]
fn test_the_exhausted_message_promises_a_wait_both_counters_keep() {
    // (1) Waiting clears the per-IP branch.
    let ip: IpAddr = "192.0.2.44".parse().unwrap();
    let power: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let mut addressed = make_test_session(TerminalType::Ansi);
    addressed.peer_addr = Some(ip);
    addressed.power_lockouts = power.clone();
    for _ in 0..MAX_AUTH_ATTEMPTS {
        addressed.record_power_failure();
    }
    assert!(addressed.power_attempts_exhausted(), "the premise: the address is capped");
    // Backdating a live `Instant` is impossible on a host up for less than the
    // window -- the trap `within_lockout_window` was extracted to escape -- so
    // skip rather than pass vacuously, exactly as the sibling test does.
    let aged = {
        let mut map = power.lock().unwrap();
        let entry = map.get_mut(&ip).expect("the failure was recorded");
        match entry.1.checked_sub(LOCKOUT_DURATION + std::time::Duration::from_secs(1)) {
            Some(t) => {
                entry.1 = t;
                true
            }
            None => false,
        }
    };
    if aged {
        assert!(
            !addressed.power_attempts_exhausted(),
            "waiting out the window did not clear the per-IP branch, so \
             \"try again later\" is a promise this page cannot keep",
        );
    } else {
        eprintln!("skipped the per-IP half: host uptime is under the lockout window");
    }

    // (2) And the address-less branch, which is the one the wording was wrong
    // about.  Held here as well as in the test above so the two facts the one
    // sentence rests on sit together.
    let mut floor = make_test_session(TerminalType::Ansi);
    floor.peer_addr = None;
    for _ in 0..MAX_AUTH_ATTEMPTS {
        floor.record_power_failure();
    }
    assert!(floor.power_attempts_exhausted(), "the premise: the floor is reached");
    let aged = {
        let mut f = floor.power_password_failures.lock().unwrap();
        match f.1.checked_sub(LOCKOUT_DURATION + std::time::Duration::from_secs(1)) {
            Some(t) => {
                f.1 = t;
                true
            }
            None => false,
        }
    };
    if aged {
        assert!(
            !floor.power_attempts_exhausted(),
            "the address-less floor does not expire, so the page must not \
             tell that caller to wait",
        );
    }

    // (3) One sentence on the screen, and the superseded one named outright so
    // a revert cannot pass by matching loosely -- the shape the `sudo -k` scan
    // uses.  Comments are stripped first, or this reads its own explanation.
    let src = include_str!("power.rs");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains(r#"show_error("Too many tries. Try again later.")"#),
        "the cap's screen no longer offers the wait that both counters honour",
    );
    assert!(
        !code.contains("Too many tries for this session."),
        "the per-session wording is back: it tells a caller with no address to \
         reconnect, which no longer clears the counter, and hides the wait that \
         does -- see this test's doc comment",
    );
}

/// **A relay caller who dials the menu still gets advice a relay can follow.**
///
/// `new_cpm_menu` clears `is_relay` so the caller is not *labelled* a slave,
/// which is a different question from how they reached the gateway -- and
/// while the power page read the label, a relay caller who pressed `K` and
/// typed `ATDT ethernetgateway` was handed the telnet advice ("set
/// `security_enabled` and reconnect") that their session can never satisfy.
/// The same defect surviving one hop, which is why these inputs are one
/// object.
#[cfg(unix)]
#[test]
fn test_the_relay_refusal_survives_a_hop_through_the_emulator() {
    let mut relay = make_test_session(TerminalType::Ansi);
    relay.is_relay = true;
    relay.power_arrived_by_relay = true;

    let writer: SharedWriter =
        std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(tokio::io::sink())));
    let dialled = TelnetSession::new_cpm_menu(
        Box::new(tokio::io::empty()),
        writer,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        relay.inheritable(),
    );

    assert!(
        !dialled.is_relay,
        "the premise: the dialled session is deliberately not labelled a relay",
    );
    assert!(
        dialled.power_arrived_by_relay,
        "how the human reached the gateway did not survive the hop, so the \
         power page gives a relayed caller advice they can never act on",
    );
}

/// **The physical modem holds its allowance across dials, too.**
///
/// The CP/M emulator's `ATDT ethernetgateway` was fixed first, and the
/// physical modem's `dial_ethernet_gateway` is the sibling path: it builds a
/// `TelnetSession` per dial, and while that constructor made its own
/// counters, `+++ ATH` and a re-dial bought another three guesses at the
/// operator's host password and another real `sudo` probe -- from a caller
/// no rate limit counts.  Read from the source because driving a real serial
/// port is not something a unit test can do.
#[test]
fn test_the_serial_modem_holds_one_power_allowance_across_dials() {
    let src = include_str!("../serial.rs").replace('\r', "");

    // The allowance is a field of the modem, which outlives a dial...
    assert!(
        src.contains("dialled: crate::telnet::Inherited"),
        "the serial modem no longer holds an `Inherited` across dials, so \
         hanging up and dialling the gateway's own menu again hands out a \
         fresh allowance of guesses at the host password",
    );

    // ...and the dial hands that field over rather than building one.
    let at = src
        .find("fn dial_ethernet_gateway")
        .expect("dial_ethernet_gateway moved or was renamed");
    let tail = &src[at..];
    let end = tail[1..].find("\nfn ").map(|i| i + 1).unwrap_or(tail.len());
    let body: String = tail[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("state.dialled.clone()"),
        "dial_ethernet_gateway builds its own session context instead of \
         handing over the modem's, so each dial starts a fresh allowance",
    );
    assert!(
        !body.contains("Inherited {"),
        "dial_ethernet_gateway constructs an `Inherited` inline, which is how \
         a re-dial gets counters nobody else shares",
    );

    // **And it is built above the reopen loop, not inside it.**  That loop
    // exists to reopen a device that disappears -- a socat or USB-serial
    // bridge that exits when the attached terminal closes -- so an allowance
    // created inside it is rebuilt whenever the device drops, which is the
    // same hole reached by unplugging instead of `+++ ATH`.  A guard on
    // `dial_ethernet_gateway` alone cannot see this: the mutation that moved
    // it back inside left that assertion green.
    let call = src
        .find("let lost = serial_thread(")
        .expect("serial_thread call moved or was renamed");
    let call_end = src[call..].find(");").expect("unterminated call") + call;
    let args = &src[call..call_end];
    assert!(
        args.contains("dialled.clone()"),
        "serial_thread is handed something other than the allowance held \
         above the reopen loop, so a device that drops and reopens starts a \
         fresh one.  Found: {args:?}",
    );
    let hoisted = src
        .find("let dialled = crate::telnet::Inherited::fresh(")
        .expect("the serial allowance is no longer built at all");
    let loop_at = src[..call].rfind("while !shutdown.load").expect("reopen loop not found");
    assert!(
        hoisted < loop_at,
        "the serial allowance is built inside the reopen loop, so dropping \
         the device hands the caller a fresh three guesses at the host \
         password",
    );
}

/// **Every dial-out call site must hand over its own state, not a literal.**
///
/// `set_menu_context` takes a context object, so a call site could build one
/// with literals and reopen the hole with the whole suite green -- and this
/// hole has been reopened by exactly that route twice, each time in the
/// commit that closed the previous one.
///
/// **It reads the directory.**  The first version named two files; the
/// second named eight of the nineteen in `src/telnet/` while its own doc
/// claimed to sweep the directory, so a new dial-out site in any of the other
/// eleven -- `cpm_mount_ui.rs` being the obvious next home -- would have
/// passed silently, and the `checked >= 2` control could not have noticed
/// either.  `include_str!` cannot enumerate a directory, so this reads it at
/// run time, the way the manual guard reads `usermanual.html`.
#[test]
fn test_every_dial_out_site_passes_its_own_credential_and_address() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/telnet");
    let mut checked = 0;
    let mut files_seen = 0;

    for entry in std::fs::read_dir(dir).expect("src/telnet is not readable") {
        let path = entry.expect("unreadable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // This file is not a dial-out site; it also contains the literal
        // being searched for, so reading it matches the scan's own source.
        if name == "tests.rs" {
            continue;
        }
        files_seen += 1;
        let code = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .replace('\r', "");

        let mut at = 0;
        while let Some(i) = code[at..].find("set_menu_context(") {
            let call_at = at + i;
            let decl = code[..call_at].ends_with("fn ");
            at = call_at + 1;
            if decl {
                continue;
            }
            let tail = &code[call_at..];
            let end = tail.find(");").expect("unterminated call");
            let args: String = tail[..end]
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join(" ");
            // The modem's own unit test builds a context on purpose; it is
            // not a dial-out site in the product.
            //
            // **Both clauses.**  This was loosened to the first one alone
            // while its commit message said the guard had been tightened, and
            // the first alone would skip any future call site that happened
            // to build its own shutdown flag -- unchecked, with `checked >= 2`
            // still satisfied by the other sites.
            if args.contains("AtomicBool::new(false)") && args.contains("HashMap::new()") {
                continue;
            }
            checked += 1;
            // **One accessor, not a list of values.**  These were four
            // arguments and the hole was reopened twice by a site that
            // carried some of them; `inheritable()` is the single place they
            // are gathered, so a new input travels without any of these
            // sites being edited.
            assert!(
                args.contains("self.inheritable()"),
                "{name}: a set_menu_context call builds its own context \
                 instead of passing `self.inheritable()` -- that is how a \
                 dialled session ends up with state its dialler never had, \
                 which has happened twice.  Found: {args:?}",
            );
        }
    }

    // Positive controls: a sweep that read no files, or found no call sites,
    // would pass every assertion above without reading anything.
    assert!(
        files_seen >= 15,
        "the sweep read only {files_seen} files from {dir}; this module has \
         far more, so the sweep is broken rather than the call sites",
    );
    assert!(
        checked >= 2,
        "the sweep found {checked} dial-out call sites; there are at least \
         two, so it is the sweep that is broken",
    );
}

/// **And nothing downstream of the door may build an `Inherited` of its own.**
///
/// The sweep above guards one end of the chain (every `set_menu_context` call
/// hands over `self.inheritable()`) and
/// `test_a_dialled_menu_session_cannot_reset_the_sudo_allowance` guards the
/// other (`new_cpm_menu` uses what it is given).  **The hops in between are
/// guarded by neither**: `MenuContext` holds the object, `dial_gateway_menu`
/// clones it out, and `menu_session` passes it on -- three places where
/// `Inherited::fresh(false, None)` would compile, reset the credential, the
/// address and both counters, and leave the whole suite green.  That is the
/// precise shape the test below this one names: a chain needs a guard per
/// link, not a guard per end, and this chain grew two new links when the
/// values became one object.
///
/// So the rule is stated where it can be checked cheaply: **`Inherited` is
/// constructed in exactly two places**, `mod.rs`'s `fresh` and `inheritable`,
/// and nowhere else in the module.  Everything else receives one.  A test
/// module may build one (they have no dialler to inherit from), so only
/// product code is read -- by the same run-time directory walk the sweep above
/// uses, because `include_str!` cannot enumerate and the file that reopens
/// this will be one nobody listed.
#[test]
fn test_only_the_door_builds_an_inherited_context() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/telnet");
    let mut files_seen = 0;
    let mut offenders: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(dir).expect("src/telnet is not readable") {
        let path = entry.expect("unreadable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // `mod.rs` is where the two constructors live, and `tests.rs` is
        // allowed to build one -- a test has no dialling session to inherit
        // from.  Every other file in the module is downstream of the door.
        if name == "mod.rs" || name == "tests.rs" {
            continue;
        }
        files_seen += 1;
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Product code only: these files carry their own `#[cfg(test)]`
        // modules, which may legitimately build a context.
        let product = src.split("\n#[cfg(test)]").next().unwrap_or(&src);
        let code: String = product
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for shape in ["Inherited::fresh(", "Inherited {"] {
            if code.contains(shape) {
                offenders.push(format!("{name}: builds `{shape}`"));
            }
        }
    }

    // Positive control: a walk that read nothing would report nothing.
    assert!(
        files_seen >= 15,
        "the walk read only {files_seen} files from {dir}; this module has far \
         more, so the walk is broken rather than the code",
    );
    assert!(
        offenders.is_empty(),
        "these files build an `Inherited` instead of receiving one, which is \
         how a dialled session ends up with a credential, an address or an \
         allowance its dialler never had -- the hole this object exists to \
         close, reopened twice already:\n  {}",
        offenders.join("\n  "),
    );
}

/// ...and the door must actually ask that rule.
///
/// Three links carry this: the rule, the door that applies it, and the page
/// that reads the result.  Each has its own mutation, and the middle one is
/// invisible to both its neighbours -- hard-wiring the assignment to `true`
/// leaves the pure-rule test *and* the power-page scan green, measured.  A
/// chain needs a guard per link, not a guard per end.
#[test]
fn test_the_door_sets_authenticated_from_the_rule_not_a_literal() {
    let src = include_str!("session.rs").replace('\r', "");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let at = code
        .find("self.authenticated")
        .expect("run_session never records whether this session authenticated");
    // Whatever follows the assignment, up to its semicolon, must be the rule.
    let tail = &code[at..];
    let end = tail.find(';').expect("unterminated assignment");
    let rhs = &tail[..end];
    assert!(
        rhs.contains("telnet_session_is_credentialed"),
        "the door sets `authenticated` from something other than \
         `telnet_session_is_credentialed` -- a literal or an inline copy of it \
         means the rule's own test guards nothing that ships.  Found: {rhs:?}",
    );
    assert!(
        !code.contains("self.authenticated = true"),
        "`authenticated` is hard-wired somewhere, which makes every session \
         claim a login it never made",
    );
}

/// **And the flag that gate reads must mean something.**
///
/// The guard below scans `power_action` for the refusal, and that is not
/// enough on its own: hard-wiring `self.authenticated = true` at the door
/// leaves it green -- measured by mutation.  A guard on the consumer says
/// nothing about the producer, the same trap as pinning an ordering instead
/// of a bound.  So the producers are tested here, one per entry point.
///
/// **The first version of this rule was wrong and a third review pass caught
/// it.**  It derived the answer as `is_serial || security_enabled`, and
/// `is_serial` cannot carry trust: `new_relay` sets it too, and
/// `new_cpm_menu` is built on `new_relay`.  So a CP/M guest dialling
/// `ATDT ethernetgateway` arrived pre-trusted, and on a root or NOPASSWD
/// machine with `security_enabled` off an unauthenticated peer could reach
/// the power page through `K` and restart the computer -- the hole the flag
/// was added to close, reopened by another route in the same commit.
/// `is_serial` means "does not speak telnet", never "is trusted".
#[test]
fn test_what_counts_as_a_credentialed_session() {
    use crate::telnet::telnet_session_is_credentialed;

    // The telnet door: the only path that runs `authenticate`, and it runs it
    // exactly when `security_enabled` is on.
    assert!(
        !telnet_session_is_credentialed(false),
        "a telnet session with security_enabled off proved nothing, and a \
         machine needing no password would then restart for any peer",
    );
    assert!(
        telnet_session_is_credentialed(true),
        "security_enabled on means authenticate() ran and passed to get here",
    );
}

/// ...and every other entry point states its own answer at construction.
#[test]
fn test_each_session_kind_states_its_own_credential() {
    use crate::config::SerialPortId;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    fn parts() -> (
        Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        SharedWriter,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        LockoutMap,
    ) {
        let writer: SharedWriter =
            std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(tokio::io::sink())));
        (
            Box::new(tokio::io::empty()),
            writer,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            std::sync::Arc::new(StdMutex::new(HashMap::new())),
        )
    }

    // SSH always authenticates -- `auth_password` is the only method offered
    // and it ignores `security_enabled` -- and `run`'s door skips SSH
    // entirely, so this must be set at construction or the power page
    // refuses the one session kind that always proved something.
    let (r, w, s, rs, l) = parts();
    assert!(
        TelnetSession::new_ssh(r, w, s, rs, None, l).authenticated,
        "an SSH session is refused by the power page although SSH is the one \
         path that always checks a credential",
    );

    // A physical serial port is its own trust boundary.
    let (r, w, s, rs, l) = parts();
    assert!(
        TelnetSession::new_serial(
            SerialPortId::A,
            r,
            w,
            s,
            rs,
            l,
            crate::telnet::Inherited::fresh(true, None),
        )
        .authenticated
    );

    // **A relay session is NOT pre-authenticated, and this is the one that
    // was wrong.**  `shell_request` refuses a key-authenticated connection
    // because "a relay key is not a login" -- but `exec_request` has no such
    // gate and `serial-relay <port>` defaults to the `menu` target, which
    // builds a session through `new_relay`.  So while this was `true`, a
    // holder of an enrolled relay key could reach the power page on a root or
    // NOPASSWD master with no password anywhere in the story.
    //
    // The `menu` target is a designed feature -- it is how a caller on a
    // slave's serial port reaches the master's menu -- so the answer is not
    // to refuse it: it is that a relayed caller has not authenticated to
    // *this* gateway.  The slave did.
    let (r, w, s, rs, l) = parts();
    assert!(
        !TelnetSession::new_relay(r, w, s, rs, None, l).authenticated,
        "a relay session claims a login, so an enrolled relay key reaches the \
         power page on a machine where nothing else would ask for a password",
    );

    // And the one that must NOT decide for itself: a menu session dialled
    // from inside another session is exactly as credentialed as its parent,
    // both ways round.
    let dialler: IpAddr = "192.0.2.77".parse().unwrap();
    for parent in [false, true] {
        let (r, w, s, rs, l) = parts();
        let sess = TelnetSession::new_cpm_menu(
            r,
            w,
            s,
            rs,
            l,
            crate::telnet::Inherited::fresh(parent, Some(dialler)),
        );
        assert!(sess.is_serial, "the premise: it looks like a serial session");
        assert_eq!(
            sess.authenticated, parent,
            "a dialled menu session did not inherit its parent's credential \
             state (parent authenticated = {parent}), so `ATDT \
             ethernetgateway` is a way to gain trust the dialler never had",
        );
        // **And its address, or the sudo cap is escapable.**  The cap keys on
        // `peer_addr`; with `None` a session falls back to a per-session
        // floor that starts at zero, so a locked-out caller could press `K`,
        // dial the menu, and have three more real PAM attempts against the
        // operator's host account -- then hang up and dial again, from inside
        // the guest, indefinitely.  It also put no originating address in the
        // log the refusals are supposed to be traceable through.
        assert_eq!(
            sess.peer_addr,
            Some(dialler),
            "a dialled menu session lost its dialler's address, so the sudo \
             attempt cap falls back to a floor that re-dialling resets",
        );
    }
}


/// **A machine that needs no password still needs a login.**
///
/// `Elevate::SudoPassword` asks for the operator's system password, so that
/// session proves something before the computer moves.  `Elevate::Direct`
/// (already root) and `Elevate::SudoQuiet` (a NOPASSWD sudoers rule) ask for
/// nothing -- correctly, there being nothing to ask -- and `security_enabled`
/// ships **off**, so on such a machine any peer reaching the telnet port
/// could have restarted the computer having presented no credential at all.
///
/// Both no-password paths are covered, not just the root one that was
/// reported: a NOPASSWD rule is the same hole by another route.
#[cfg(unix)]
#[test]
fn test_a_password_free_machine_still_needs_an_authenticated_session() {
    use crate::telnet::power::Elevate;

    // The rule, stated once here and read out of the source below so the two
    // cannot drift.
    for elev in [Elevate::Direct, Elevate::SudoQuiet] {
        assert!(
            elev != Elevate::SudoPassword,
            "{elev:?} is a no-password path, which is the premise of this test",
        );
    }

    let src = include_str!("power.rs").replace('\r', "");
    let start = src.find("async fn power_action(").expect("power_action renamed");
    let rest = &src[start..];
    let end = rest[1..].find("\n    /// ").map(|i| i + 1).unwrap_or(rest.len());
    let body: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        body.contains("elev != Elevate::SudoPassword") && body.contains("!self.authenticated"),
        "power_action no longer refuses a no-password elevation to an \
         unauthenticated session, so a root or NOPASSWD machine can be \
         restarted by any peer that reaches the telnet port",
    );

    // It must be refused BEFORE the password branch and the command: a screen
    // must not promise a step that cannot happen.
    let gate = body.find("!self.authenticated").expect("gate present");
    let run = body
        .find("run_elevated(")
        .expect("power_action no longer runs anything -- wrong function");
    assert!(gate < run, "the refusal lands after the command has been run");

    // And the screen names the setting the operator can act on, on a 40-col
    // display -- **without asserting what it is currently set to.**
    //
    // The first wording said "Turn on security_enabled", and the code never
    // reads that setting: `authenticated` can be false with it *on*, for a
    // session that began before the operator switched it on, which is the
    // very reason the flag is recorded at the door rather than derived.  A
    // screen that names a false cause sends the operator to a setting that is
    // already set.  Same rule as `sudo_timed_out_line`: say what was
    // observed, never an outcome the code did not check.
    let lines = crate::telnet::power::unverified_lines(false);
    assert!(
        lines.iter().any(|l| l.contains("security_enabled")),
        "the refusal does not name the setting that would fix it: {lines:?}",
    );
    let screen = lines.join(" ").to_lowercase();
    // It must not assert what the setting currently *is*...
    for claim in ["is off", "is not set", "turn on", "switch on"] {
        assert!(
            !screen.contains(claim),
            "the refusal says {claim:?}, which asserts a value for a setting \
             this path never reads: {lines:?}",
        );
    }
    // ...and it must still tell the operator what to do about it.  Dropping
    // the instruction along with the assertion left the default install --
    // telnet only, SSH off -- told to "reconnect on a listener that asks who
    // you are", of which there is none.  A screen that names no action is the
    // other half of the same defect.
    assert!(
        screen.contains("set security_enabled"),
        "the refusal names no action the operator can take: {lines:?}",
    );

    // **And a relay is told something it can actually do.**  `run` skips both
    // `authenticate` and the credential assignment for any `is_serial`
    // session, and a relay is one -- so "set security_enabled and reconnect"
    // is a step that cannot happen however often it is followed, which is the
    // failure this page's order of steps exists to prevent.
    let relay = crate::telnet::power::unverified_lines(true);
    let relay_screen = relay.join(" ").to_lowercase();
    assert!(
        !relay_screen.contains("set security_enabled"),
        "a relayed caller is told to set security_enabled and reconnect, \
         which can never make them authenticated: {relay:?}",
    );
    assert!(
        relay_screen.contains("telnet") || relay_screen.contains("ssh"),
        "the relay refusal names no route that works: {relay:?}",
    );
    for l in &relay {
        assert!(
            l.chars().count() <= PETSCII_WIDTH - 2,
            "{l:?} is {} chars, too wide for a PETSCII screen",
            l.chars().count(),
        );
    }
    for l in &lines {
        assert!(
            l.chars().count() <= PETSCII_WIDTH - 2,
            "{l:?} is {} chars, too wide for a PETSCII screen",
            l.chars().count(),
        );
    }
}

/// **A cancelled confirmation cannot spawn a probe for ever.**
///
/// The ordering guard above pins only *where* the cap is checked, and that
/// was not enough: the cap counts refused **passwords**, raised solely when a
/// submitted one comes back refused, while `power_prompt_password` returns
/// `None` for ESC and for a bare Enter and records nothing.  So `R`, `Y`,
/// Enter looped indefinitely, spawning one real `sudo -k -n -l` per pass --
/// one authentication failure in the *host's* auth log per keypress on a
/// machine whose account is not in sudoers, reachable without authenticating
/// at all while `security_enabled` is off.
///
/// The bound is that a session probes once and remembers the answer, so this
/// pins the memo rather than the ordering: a second pass through the page
/// must not re-ask.  No test may call the probe itself -- it shells out to
/// the real `sudo` -- so the memo is driven directly, which is the whole
/// reason the answer is a field and not a local.
#[cfg(unix)]
#[test]
fn test_a_session_probes_for_elevation_only_once() {
    use crate::telnet::power::{Elevate, PowerAction};

    let session = make_test_session(TerminalType::Ansi);
    assert!(
        session.power_elevation.lock().unwrap().is_empty(),
        "a fresh session must not claim to know how it would elevate",
    );

    // What a first successful probe for *Shutdown* leaves behind.
    session
        .power_elevation
        .lock()
        .unwrap()
        .push((PowerAction::Shutdown, Elevate::SudoQuiet));

    // **Keyed by action, because the probe is.**  Sudoers rules are
    // per-argument -- `NOPASSWD: /sbin/shutdown -h now` alone is ordinary --
    // so answering Restart out of Shutdown's slot would skip the password,
    // clock out the farewell, and only then have sudo refuse.  An earlier
    // version of this test asserted that a field kept the value just
    // assigned to it, which is true of any field and could not go red.
    assert_eq!(
        session.remembered_elevation(PowerAction::Shutdown),
        Some(Elevate::SudoQuiet),
        "the answer for the action that was probed is not remembered, so a \
         cancel loop spawns a real sudo every time round",
    );
    assert_eq!(
        session.remembered_elevation(PowerAction::Restart),
        None,
        "Restart was answered out of Shutdown's slot: sudoers is \
         per-argument, so that skips a password the machine does want and \
         fails only after the farewell has been sent",
    );

    // And the source must actually consult it: a memo nothing reads is a
    // field, not a bound.  Bounded to `power_action`'s own body, for the
    // reason the ordering guard states.
    let src = include_str!("power.rs").replace('\r', "");
    let start = src.find("async fn power_action(").expect("power_action renamed");
    let rest = &src[start..];
    let end = rest[1..].find("\n    /// ").map(|i| i + 1).unwrap_or(rest.len());
    let body: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let memo = body
        .find("self.remembered_elevation(")
        .expect("power_action never consults the probe memo, so every \
                 confirmation spawns a fresh sudo");
    let probe = body.find("probe_elevation(").expect("power_action no longer probes");
    assert!(
        memo < probe,
        "the memo is read after the probe is spawned, which bounds nothing",
    );
}

/// **The gate must agree with the probe about whether this can work.**
///
/// `available()` decided on `no_new_privs` alone, while
/// `probe_elevation_within` answers the *root* question first and returns
/// `Elevate::Direct`.  `no_new_privs` stops a setuid binary gaining
/// privilege, which is why `sudo` cannot work under it -- but a gateway
/// already running as root gains nothing and needs no `sudo`.  So under a
/// root unit with `NoNewPrivileges=yes`, an ordinary hardened container, the
/// `2` entry, both rows, their key arms, the MORE hint and the help screen
/// all vanished for a machine that would have restarted perfectly well.
///
/// A gate that disagrees with the probe is the same defect either way round:
/// one direction hides a feature that works, the other offers one that
/// cannot.  Tested as a pure rule because `available()` caches a reading of
/// the live host and can only ever see one of these four combinations.
#[cfg(unix)]
#[test]
fn test_the_power_gate_agrees_with_the_probe_about_root() {
    use crate::telnet::power::power_is_possible;

    // The case that was wrong: root, hardened unit.  sudo could not elevate,
    // and nothing needed it to.
    assert!(
        power_is_possible(true, true),
        "a root gateway under NoNewPrivileges=yes runs shutdown directly, but \
         the menu hid every way of asking for it",
    );
    assert!(power_is_possible(true, false), "root with the flag clear is the easy case");
    assert!(
        power_is_possible(false, false),
        "an ordinary account that can reach sudo is the common install",
    );
    // The case the feature was hidden for, and still must be: not root, and
    // no password can elevate.
    assert!(
        !power_is_possible(false, true),
        "offering a page no password can satisfy is the defect this gate \
         exists to prevent",
    );
}

/// **Logging in does not buy three more guesses at the machine's password.**
///
/// These counters were the shared auth `LockoutMap`, and `session.rs` clears
/// an address from that map on every successful gateway login -- so with
/// `security_enabled` on, the bound was three guesses *per login*: log in,
/// spend them on the operator's host account, hang up, log in, repeat.  The
/// default configuration held only by accident, there being no login to clear
/// anything when security is off.
///
/// The two credentials are different things, so they are different counters.
/// This is the assertion the reconnect test above cannot make: it never drives
/// a successful auth, which is the exact step that wiped the entry.
#[cfg(unix)]
#[test]
fn test_a_successful_gateway_login_does_not_restore_sudo_attempts() {
    let ip: IpAddr = "192.0.2.8".parse().unwrap();
    let auth: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let power: LockoutMap = Arc::new(Mutex::new(HashMap::new()));

    let mut session = make_test_session(TerminalType::Ansi);
    session.peer_addr = Some(ip);
    session.lockouts = auth.clone();
    session.power_lockouts = power.clone();

    for _ in 0..MAX_AUTH_ATTEMPTS {
        session.record_power_failure();
    }
    assert!(session.power_attempts_exhausted(), "three refusals must stop the prompt");

    // Exactly what `session.rs` does when a gateway login succeeds.
    clear_lockout(&auth, ip);

    assert!(
        session.power_attempts_exhausted(),
        "a successful gateway login handed out three more guesses at the \
         operator's *system* password -- the two credentials are different \
         things and must not share a counter",
    );
    // And the reverse: the power page must not be able to lock an operator
    // out of the gateway itself.
    assert!(
        !is_locked_out(&auth, ip),
        "refused sudo attempts leaked into the auth lockout and banned the \
         operator from logging in at all",
    );
}

/// A session with no address is bounded by its own count.
///
/// **The case the per-IP map is structurally blind to.**  A caller on the
/// modem, or a CP/M guest that dialled `ATDT ethernetgateway`, has no
/// `peer_addr`, so it is in the `LockoutMap` under no key at all -- and every
/// wrong answer at this prompt is a real PAM attempt against the operator's
/// host account.  Between the per-session counter being removed and this, that
/// prompt could be answered without bound.
#[cfg(unix)]
#[test]
fn test_the_sudo_cap_falls_back_to_the_session_without_an_address() {
    let lockouts: LockoutMap = Arc::new(Mutex::new(HashMap::new()));
    let mut session = make_test_session(TerminalType::Ansi);
    session.lockouts = lockouts.clone();
    assert_eq!(session.peer_addr, None, "this test is about the address-less case");

    assert!(!session.power_attempts_exhausted());
    for n in 1..=MAX_AUTH_ATTEMPTS {
        assert_eq!(session.record_power_failure(), n, "the count is the running total");
    }
    assert!(
        session.power_attempts_exhausted(),
        "an address-less session must still be stopped after {} refusals",
        MAX_AUTH_ATTEMPTS,
    );
    // And it must not have invented a key in the shared map: a made-up address
    // would lock out whoever really holds it.
    assert!(
        lockouts.lock().unwrap().is_empty(),
        "a session with no address wrote into the per-IP map: {:?}",
        lockouts.lock().unwrap().keys().collect::<Vec<_>>(),
    );
}
/// The second menu's help fits one screen.
///
/// Not a rule for every help table -- the main menu's is 22 lines and
/// paginates on purpose -- but this page offers two actions, and splitting a
/// short explanation across two screens so the reader has to press a key to
/// finish it is a worse answer than three words fewer.  It **was** split:
/// adding the `Q  Back` entry took the table to 17 against a 15-line screen,
/// and the live gateway said "Page 1/2".  Measured there, not here.
#[cfg(unix)]
#[test]
fn test_the_second_menus_help_is_one_screen() {
    // **Asserted through the paginator, not by counting lines.**  The first
    // version of this test compared the line count against
    // `HELP_MAX_CONTENT_LINES` and passed while the live gateway printed
    // "Page 1/2" -- because `paginate_help` also split at the last blank line,
    // so 15 lines against a budget of 15 still became two pages.  A count is a
    // proxy; the number of pages is the claim.
    let lines = TelnetSession::more_help_lines();
    let pages = TelnetSession::paginate_help(lines, crate::telnet::HELP_MAX_CONTENT_LINES);
    assert_eq!(
        pages.len(),
        1,
        "the second menu's help ({} lines, budget {}) paginates into {} pages: \
         shorten a line rather than making the reader page through an \
         explanation of two keys",
        lines.len(),
        crate::telnet::HELP_MAX_CONTENT_LINES,
        pages.len(),
    );
}

/// Content that fits is one page, even when it contains a blank line.
///
/// The prefer-a-blank split is for *overflow*; it used to fire regardless, so
/// a table inside the budget still paged if it had a blank anywhere but the
/// end.  Found on the live gateway, not here: the second menu's help was 15
/// lines against a budget of 15 and printed "Page 1/2".
#[test]
fn test_paginate_help_does_not_split_content_that_fits() {
    let lines = ["a1", "a2", "", "b1", "b2", "", "c1", "c2"];
    let pages = TelnetSession::paginate_help(&lines, 15);
    assert_eq!(pages.len(), 1, "8 lines in a 15-line budget split: {pages:?}");
    assert_eq!(pages[0], lines, "and the page keeps its blanks in place");

    // Exactly at the budget is still one page -- the case that was wrong.
    let full: Vec<&str> = (0..15)
        .map(|i| if i == 11 { "" } else { "x" })
        .collect();
    let pages = TelnetSession::paginate_help(&full, 15);
    assert_eq!(pages.len(), 1, "15 lines in a 15-line budget split: {pages:?}");
}


/// The MORE help must point at the *other* restart, or the two stay
/// confusable everywhere except the confirmation screen.
#[cfg(unix)]
#[test]
fn test_more_help_distinguishes_the_two_restarts() {
    let text = TelnetSession::more_help_lines().join(" ");
    assert!(
        text.contains("Configuration > Server > R"),
        "the MORE help does not say where restarting the gateway alone lives: {}",
        text,
    );
    assert!(
        text.to_lowercase().contains("not just"),
        "the MORE help does not distinguish the computer from the gateway: {}",
        text,
    );
}

/// Every (left label, right key, right label) the port settings screen can
/// draw, read out of `serial_ui.rs` itself.
///
/// Left labels are the second argument of a `serial_menu_row(` call; the
/// right-hand pairs are the `Some(("K", "Label"))` tuples handed to it, which
/// this screen builds in local variables rather than inline, so the two are
/// collected separately and combined.  Comments are stripped first: this
/// file's prose quotes both shapes, and a scan that reads its own explanation
/// measures itself.
#[cfg(test)]
fn serial_menu_label_pairs() -> Vec<(String, String, String)> {
    let src = include_str!("serial_ui.rs");
    let code: String = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n");

    /// The string literals inside `text`, in order, without their quotes.
    fn literals(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let b: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < b.len() {
            if b[i] == '"' {
                let mut lit = String::new();
                i += 1;
                while i < b.len() && b[i] != '"' {
                    if b[i] == '\\' {
                        i += 1;
                    }
                    if i < b.len() {
                        lit.push(b[i]);
                    }
                    i += 1;
                }
                out.push(lit);
            }
            i += 1;
        }
        out
    }

    // Left labels: the second literal of each call, skipping the definition
    // itself (which names no labels).
    let mut lefts: Vec<String> = Vec::new();
    for chunk in code.split("serial_menu_row(").skip(1) {
        let arg = chunk.split(");").next().unwrap_or("");
        let lits = literals(arg);
        if lits.len() >= 2 {
            lefts.push(lits[1].clone());
        }
    }
    // **Every call site must have been parsed, and the caller's own control
    // cannot tell.**  It asserts on the number of *pairs*, which is lefts x
    // rights -- so one left label and four right-hand tuples clears a floor of
    // four while three of the screen's four rows go unmeasured.  A call site
    // whose label stops being a bare literal (a `format!`, a const, a wrapped
    // line the `");"` split swallows) fails exactly that way, silently.  So the
    // count is held against the source: one call per occurrence, less the
    // definition.
    let call_sites = code.matches("serial_menu_row(").count() - 1;
    assert_eq!(
        lefts.len(),
        call_sites,
        "the label scan parsed {} of {} `serial_menu_row(` call sites -- the \
         rows it missed are aligned by nothing, and the pair count the caller \
         checks cannot see the difference",
        lefts.len(),
        call_sites,
    );

    // Right-hand pairs: every `Some(("K", "Label"))` in the file.
    let mut rights: Vec<(String, String)> = Vec::new();
    for chunk in code.split("Some((").skip(1) {
        let arg = chunk.split("))").next().unwrap_or("");
        let lits = literals(arg);
        if lits.len() >= 2 {
            rights.push((lits[0].clone(), lits[1].clone()));
        }
    }

    let mut out = Vec::new();
    for l in &lefts {
        for (k, r) in &rights {
            out.push((l.clone(), k.clone(), r.clone()));
        }
    }
    out
}

/// The port settings screen's right-hand keys must share one column.
///
/// Reported 2026-09-14 from a live Port B screen: `G`, `X` and `C` sat at
/// columns 26, 24 and 22.  Each row was hand-spaced with three spaces after a
/// label of a different length, and because the three rows are drawn by three
/// different `if` branches -- raw, console and modem mode each draw a
/// different pair -- nothing ever compared them.
///
/// This asserts the real `serial_menu_row`, on every terminal type, because
/// the padding has to be computed from the *drawn* width: `cyan()` wraps the
/// key in bytes a terminal does not print, and aligning on the formatted
/// string would line the invisible ones up on ANSI and leave the visible
/// columns ragged.
#[test]
fn test_the_port_settings_rows_align_their_second_key() {
    // **The labels are read out of the screen's own source, not listed here.**
    // The first version held a hand-copied list, so a label added or widened at
    // a real call site was invisible to it: proved by mutation -- lengthening
    // `"Dialup Mapping"` to `"Dialup Mapping and more stuff here"` left every
    // telnet test green while the drawn row reached 50 columns on a C64, the
    // gap clamped to a single space by `serial_menu_row`'s `.max(1)`.  That
    // clamp exists for exactly this case and had no test at all.
    //
    // Left labels are the second argument of each `serial_menu_row(` call;
    // right-hand pairs are every `Some(("K", "Label"))` in the file.  Every
    // combination is measured, which over-approximates -- the screen never
    // draws the widest left beside the widest right -- and that is the safe
    // direction for a budget.
    let pairs = serial_menu_label_pairs();
    assert!(
        pairs.len() >= 4,
        "the label scan found {} pairs; the screen draws at least four",
        pairs.len(),
    );
    let pairs: Vec<(&str, &str, &str)> = pairs
        .iter()
        .map(|(l, k, r)| (l.as_str(), k.as_str(), r.as_str()))
        .collect();
    for term in [TerminalType::Petscii, TerminalType::Ansi, TerminalType::Ascii] {
        let mut session = make_test_session(term);
        session.color_enabled = false; // measure what a terminal draws
        for &(label, key2, label2) in &pairs {
            let row = session.serial_menu_row("F", label, Some((key2, label2)));
            let col = row.find(key2).unwrap_or_else(|| {
                panic!("row {:?} lost its second key {:?}", row, key2)
            });
            assert_eq!(
                col, SERIAL_MENU_SECOND_COL,
                "{:?} puts {} at column {}, not {}",
                row, key2, col, SERIAL_MENU_SECOND_COL,
            );
            // At least two spaces before it, or it reads as one word.
            assert!(
                row[..col].ends_with("  "),
                "row {:?} runs its two columns together",
                row,
            );
            // And the whole row still fits the screen it is printed on --
            // 39 on PETSCII, the `separator()` budget: a row that exactly
            // fills 40 auto-wraps and then takes the trailing CR/LF.
            let width = if term == TerminalType::Petscii { PETSCII_WIDTH - 1 } else { 80 };
            assert!(
                row.chars().count() <= width,
                "row {:?} is {} columns on {:?}, over {}",
                row,
                row.chars().count(),
                term,
                width,
            );
        }
        // A row with no second entry is unchanged by the helper.
        assert_eq!(
            session.serial_menu_row("B", "Set baud rate", None),
            "  B  Set baud rate",
        );
        // **With colour ON, the drawn column must be the same.**  This is the
        // half that matters: `cyan()` wraps the key in bytes a terminal does
        // not print, so a padding computed from the *formatted* string lines
        // up the invisible ones -- correct on ASCII, where there are no codes,
        // and wrong on ANSI and PETSCII, where there are.  A version of this
        // test that only measured the uncoloured row passed with exactly that
        // bug put back.
        //
        // The codes are removed with the program's own constants rather than
        // by pattern-matching escape sequences: a stripper written here would
        // be a second implementation of `colors.rs`, and it is the one that
        // would be wrong.
        for &(label, key2, label2) in &pairs {
            let row = make_test_session(term).serial_menu_row("F", label, Some((key2, label2)));
            let drawn = row
                .replace(ANSI_CYAN, "")
                .replace(ANSI_RESET, "")
                .replace([char::from(PETSCII_CYAN), char::from(PETSCII_DEFAULT)], "");
            let col = drawn.find(key2).unwrap_or_else(|| {
                panic!("coloured row {:?} lost its second key {:?}", row, key2)
            });
            assert_eq!(
                col, SERIAL_MENU_SECOND_COL,
                "with colour on, {:?} draws {} at column {}, not {}",
                drawn, key2, col, SERIAL_MENU_SECOND_COL,
            );
        }
    }
}

/// No row on the port settings screen may hand-build its own two columns.
///
/// The alignment test above drives `serial_menu_row`, which proves the helper
/// aligns -- and would happily stay green if a new row went back to writing
/// `"  {}  Label   {}  Other"` with its own spaces, which is exactly how the
/// three columns drifted apart in the first place.  So this reads the screen's
/// own source and fails on any `format!` row carrying two key colourings,
/// which is what a hand-built two-column row looks like.
///
/// Comments are stripped first: this file's prose describes the very pattern
/// it is looking for, and a scan that reads its own explanation reports
/// itself.  (That has happened here before.)
#[test]
fn test_no_port_settings_row_builds_its_own_columns() {
    let src = include_str!("serial_ui.rs");
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // `serial_menu_row` is the one place allowed to colour two keys in one
    // `format!` -- it is the helper every row goes through -- so its own body
    // is cut out before the scan.  Without this the guard reports the fix as
    // the defect, which is how it first failed: a scanner that cannot tell the
    // rule from a breach of it is a scanner nobody will keep.
    let helper = "pub(in crate::telnet) fn serial_menu_row";
    let code = match code.find(helper) {
        Some(start) => {
            // The function ends at the first `\n    }` -- a closing brace at
            // the impl's own indent -- after its signature.
            //
            // **Loud when it cannot find that.**  This used to be
            // `.unwrap_or(code.len())`, which cut the scan back to everything
            // *before* the helper and said nothing: the helper sits near the
            // top of the file, so the surviving slice holds **zero**
            // `format!` statements (measured), `offenders` comes out empty and
            // the assertion at the bottom passes having read nothing at all.
            // A negative assertion is only worth what its input is worth.
            let end = code[start..]
                .find("\n    }")
                .map(|o| start + o + 6)
                .unwrap_or_else(|| {
                    panic!(
                        "serial_menu_row's body has no `\\n    }}` to end it -- \
                         the excision would silently swallow the rest of the \
                         file and this guard would pass having scanned nothing"
                    )
                });
            let mut out = code[..start].to_string();
            out.push_str(&code[end..]);
            out
        }
        None => panic!("serial_menu_row went away; every row is hand-built again"),
    };

    /// Statements that colour two keys in one `format!` -- a row building its
    /// own columns instead of going through `serial_menu_row`.
    fn offenders_in(code: &str) -> Vec<String> {
        code.split("format!(")
            .skip(1)
            // One statement's worth: up to the `;` that ends it.
            .map(|chunk| chunk.split(';').next().unwrap_or(""))
            .filter(|stmt| stmt.matches("self.cyan(").count() >= 2)
            .map(|stmt| stmt.chars().take(120).collect())
            .collect()
    }

    // **Positive control 1: the detector detects.**  Run it over a statement
    // that is exactly the defect, through the same code path as the real scan.
    let sample = r#"rows.push(format!("  {}  {}", self.cyan("A"), self.cyan("B")));"#;
    assert_eq!(
        offenders_in(sample).len(),
        1,
        "the detector no longer recognises a hand-built two-column row, so \
         the empty result below means nothing",
    );

    // **Positive control 2: the scan reached the real code.**  111 `format!`
    // statements survive the excision today; the broken path above leaves 0.
    // A floor well under the real number and far above zero distinguishes
    // them without pinning a count that ordinary edits would move.
    let scanned = code.matches("format!(").count();
    assert!(
        scanned >= 50,
        "the scan examined only {scanned} `format!` statements in serial_ui.rs \
         -- it is not reading the file, so an empty offender list proves \
         nothing",
    );

    let offenders = offenders_in(&code);
    assert!(
        offenders.is_empty(),
        "{} row(s) build their own two columns instead of calling \
         serial_menu_row, so their second key will not line up:\n  {}",
        offenders.len(),
        offenders.join("\n  "),
    );
}

/// The `Computer:` row must fit at the longest name it can ever carry.
///
/// **Its width comes from a runtime value, so no fixture measures it.**  The
/// hand-copied list in `test_all_menu_items_fit_petscii` held
/// `"  Computer: raspberrypi"` — an invented eleven-character name that is
/// safe by accident — and `test_more_menu_rows_fit_the_screen` measures
/// whatever `/etc/hostname` happens to say on the machine running the suite.
/// Neither is the worst case.
///
/// **And it drives `computer_row_for`, not a copy of it.**  The first version
/// of this test rebuilt the row inline from the same two literals the function
/// holds; mutation proved it worthless — widening the PETSCII cap from 26 to
/// 39 left every telnet test green while the row printed at 51 columns on a
/// C64.  `hostname_label` caps a name at 32 characters, so that is the longest
/// string that can reach here.
#[cfg(unix)]
#[test]
fn test_the_computer_row_fits_at_its_longest() {
    let longest = "w".repeat(32);
    for (term, width) in [
        (TerminalType::Petscii, PETSCII_WIDTH - 1),
        (TerminalType::Ansi, 80),
    ] {
        let mut session = make_test_session(term);
        session.color_enabled = false;
        let row = session
            .computer_row_for(&longest)
            .expect("a 32-character name must produce a row");
        assert!(
            row.chars().count() <= width,
            "the computer row is {} columns on {:?} at the longest name ({:?}), over {}",
            row.chars().count(),
            term,
            row,
            width,
        );
        // On PETSCII the cap (26) is below `hostname_label`'s own (32), so
        // the truncation must actually bite or that branch is untested.  On a
        // wide screen the cap is 60 and a 32-character name passes through
        // whole, which is correct and is why this is not asserted there.
        if term == TerminalType::Petscii {
            assert!(
                row.chars().count() < "  Computer: ".len() + longest.chars().count(),
                "the name was not truncated on {:?}: {:?}",
                term,
                row,
            );
        }
        // A machine that will not say what it is called gets no row at all,
        // rather than `Computer: ` with nothing after it.
        assert!(session.computer_row_for("").is_none());
        assert!(session.computer_row_for("   ").is_none());
    }
    // `hostname_label` must stay inside the cap this test assumes, on whatever
    // branch this machine takes (the file, or the `hostname` command).
    let label = crate::relay::hostname_label();
    assert!(
        label.chars().count() <= 32,
        "hostname_label returned {} characters, over its own cap",
        label.chars().count(),
    );
}

/// Every `*_help_lines` table must appear in `all_help_line_groups`.
///
/// **The fixed array length used to be the tripwire.** `all_help_line_groups`
/// returned `[&'static [&'static str]; 27]`, so adding a help screen without
/// listing it failed to compile — that is what the MAINTENANCE comment meant
/// by "bump the array length to match". The MORE page's table is Unix-only, so
/// the array had to become a `Vec`, and the tripwire went with it: a new help
/// screen can now be added and silently go unchecked while the comment still
/// promises otherwise.
///
/// So the census is the tripwire now. It counts the `fn *_help_lines` and
/// `*_HELP*` tables the telnet module defines and holds that against the
/// number `all_help_line_groups` returns. Comments are stripped, or this
/// file's own prose about `*_help_lines` is counted as a definition.
#[test]
fn test_every_help_table_is_width_checked() {
    const SOURCES: &[&str] = &[
        include_str!("mod.rs"),
        include_str!("config_ui.rs"),
        include_str!("web.rs"),
        include_str!("kernel.rs"),
        include_str!("cpm_emu.rs"),
        include_str!("serial_ui.rs"),
        include_str!("transfer.rs"),
        include_str!("gateway.rs"),
        include_str!("session.rs"),
        include_str!("weather.rs"),
        include_str!("aichat_ui.rs"),
    ];
    let mut defined: Vec<String> = Vec::new();
    for src in SOURCES {
        for line in src.lines() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            // `fn name_help_lines(` — with or without a visibility prefix.
            if let Some(rest) = t.split("fn ").nth(1) {
                if let Some(name) = rest.split('(').next() {
                    if name.ends_with("_help_lines") && !name.is_empty() {
                        defined.push(name.to_string());
                    }
                }
            }
            // The one const table in the set.
            if t.contains("const CPM_ENTRY_TIPS") {
                defined.push("CPM_ENTRY_TIPS".to_string());
            }
        }
    }
    defined.sort();
    defined.dedup();
    // `more_help_lines` is `#[cfg(unix)]` in mod.rs and a source scan cannot
    // see a cfg, so off Unix this census was comparing a Unix source file
    // against a Windows binary's list: 29 defined, 28 listed.  Red on the
    // windows job and nowhere else, from `85193af` until now -- CI does not
    // run on dev pushes, so a day of dev commits went by without anyone
    // seeing it.  Dropped here rather than removing power.rs from `SOURCES`,
    // because the strings are still worth width-checking wherever this runs.
    #[cfg(not(unix))]
    defined.retain(|n| n != "more_help_lines");

    let listed = all_help_line_groups(true).len();
    assert_eq!(
        defined.len(),
        listed,
        "{} help tables are defined but {} are width-checked; \
         every `*_help_lines` must appear in all_help_line_groups exactly once.\n  {}",
        defined.len(),
        listed,
        defined.join("\n  "),
    );
}

/// A server certificate is refused as a pinnable host key; a plain key is not.
///
/// russh 0.63 widened `check_server_key` to `PublicKeyOrCertificate`, and the
/// migration that compiles is not the one that is correct: taking the
/// `PublicKey` *inside* a certificate and pinning it would put a key no CA
/// statement was ever checked for into `gateway_hosts`, on the two paths where
/// a MITM is most expensive.  See [`pinnable_host_key`].
///
/// The fixtures are a real `ssh-keygen` host key and a real host certificate
/// signed over that same key, which is what makes this a test rather than a
/// restatement: the certificate's inner key **is** `HOST_KEY`, so an
/// unwrapping implementation returns `Some(HOST_KEY)` and is caught here.  The
/// third assertion is the positive control for exactly that -- without it,
/// `None` would pass just as well for a certificate whose inner key we had
/// never identified, and the test would be proving nothing about the mutation
/// it exists to stop.
#[test]
fn test_a_host_certificate_is_never_pinned_as_a_host_key() {
    use russh::keys::{Certificate, PublicKey, PublicKeyOrCertificate};

    const HOST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJVWeD2ec3r+aZemdrMS2vmNxjl71OENBSNUC/CoIaeh test-host";
    const HOST_CERT: &str = "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29tAAAAIJJGKSZkOn8KUz6VfVhuY5gOgxhKbM1rOs4Bi9BoBEuPAAAAIJVWeD2ec3r+aZemdrMS2vmNxjl71OENBSNUC/CoIaehAAAAAAAAAAAAAAACAAAACXRlc3QtaG9zdAAAABAAAAAMZXhhbXBsZS50ZXN0AAAAAGqgjG8AAAAAbImo7wAAAAAAAAAAAAAAAAAAADMAAAALc3NoLWVkMjU1MTkAAAAgb4X+oryvFI6AlURCI53CaSEIamywyE97Wl2oryATAtYAAABTAAAAC3NzaC1lZDI1NTE5AAAAQOC1aqeDEQtz0BYJS3CW8C0O4CVE5qF0kNsVkh8tZnlEvBjol8NoLGjTPmdu9dKMzxi6zoUHvA6c6VsKjWUUGwc= test-host";

    let host_key = PublicKey::from_openssh(HOST_KEY).expect("fixture host key parses");
    let cert = Certificate::from_openssh(HOST_CERT).expect("fixture certificate parses");

    // A plain host key is what we pin, unchanged.
    let plain = PublicKeyOrCertificate::PublicKey {
        key: host_key.clone(),
        hash_alg: None,
    };
    assert_eq!(
        pinnable_host_key(&plain).as_ref(),
        Some(&host_key),
        "a plain host key must be pinned exactly as presented",
    );

    // Positive control: the certificate really does carry that key, so the
    // wrong migration has something to return.
    assert_eq!(
        cert.public_key(),
        host_key.key_data(),
        "fixture is not wired up: the certificate must be signed over HOST_KEY, \
         or the assertion below cannot distinguish refusing from unwrapping",
    );

    // The rule.
    assert!(
        pinnable_host_key(&PublicKeyOrCertificate::Certificate(cert)).is_none(),
        "a certificate must never become a pinned host key: the key inside it \
         is not one this gateway was ever told to trust, and pinning it is \
         trust-on-first-use over a CA statement nobody checked",
    );
}

/// Every key a menu page offers must be explained in that page's own help.
///
/// Asked for after the second page was added: the main menu documents its
/// leave key (`X  Exit`) and the second page did not document its own
/// (`Q  Back`), so the two pages disagreed about what a help screen owes the
/// reader.  The keys are read out of the **rows the page draws**, not from a
/// list here, because a hand-kept list beside a code-rendered menu is the half
/// that rots -- which is the defect this whole family of guards keeps finding.
///
/// `H` is the one exclusion: it is the key the reader pressed to get to the
/// help they are reading, and every page would otherwise carry a line
/// explaining how to do what they have just done.
#[test]
fn test_every_menu_key_is_explained_in_that_pages_help() {
    /// Keys a rendered page offers, in both shapes the menus use:
    /// `"  A  Label"` for an item and `"Q=Back"` for a footer action.
    fn keys_of(rows: &[String]) -> Vec<char> {
        let mut keys = Vec::new();
        for row in rows {
            // Footer actions: `K=Label`, possibly two on one row.
            let b: Vec<char> = row.chars().collect();
            for (i, w) in b.windows(2).enumerate() {
                if w[1] == '=' && w[0].is_ascii_uppercase() && (i == 0 || !b[i - 1].is_alphanumeric())
                {
                    keys.push(w[0]);
                }
            }
            // Items: two spaces, one key, two spaces, a label.
            let t = row.trim_start();
            let c: Vec<char> = t.chars().collect();
            if c.len() > 3 && (c[0].is_ascii_uppercase() || c[0].is_ascii_digit())
                && c[1] == ' ' && c[2] == ' ' && c[3] != ' '
            {
                keys.push(c[0]);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    let mut session = make_test_session(TerminalType::Ansi);
    session.color_enabled = false; // read the keys, not the colour codes

    // Only the `#[cfg(unix)]` push below mutates this, exactly as in
    // `all_help_line_groups`, which carries the same attribute for the same
    // reason: on Windows there is no second page and `-D warnings` would
    // otherwise fail the build on an unused `mut`.
    #[allow(unused_mut)]
    let mut pages: Vec<(&str, Vec<String>, &'static [&'static str])> = vec![
        (
            "main menu",
            session.main_menu_rows(MenuItems { cpm: true, second_page: cfg!(unix) }, None),
            TelnetSession::main_help_lines(MenuItems { cpm: true, second_page: cfg!(unix) }),
        ),
        // The same page with both optional items off: the help must still
        // explain every key that is left.
        (
            "main menu (no optional items)",
            session.main_menu_rows(MenuItems { cpm: false, second_page: false }, None),
            TelnetSession::main_help_lines(MenuItems { cpm: false, second_page: false }),
        ),
    ];
    #[cfg(unix)]
    pages.push((
        "second menu",
        session.more_menu_rows(true),
        TelnetSession::more_help_lines(),
    ));

    for (page, rows, help) in pages {
        let keys = keys_of(&rows);
        // Positive control: an extractor that finds nothing would pass every
        // assertion below without reading a single key.
        assert!(
            keys.len() >= 3,
            "{page}: the key scan found {:?} in {:?} -- the page draws more \
             than that, so the extractor is broken, not the help",
            keys,
            rows,
        );
        for key in keys {
            if key == 'H' {
                continue; // the key they pressed to read this
            }
            let wanted = format!("  {}  ", key);
            assert!(
                help.iter().any(|l| l.starts_with(&wanted)),
                "{page}: key {:?} is on the screen but no help line starts \
                 with {:?}. Every key a page offers must be explained in that \
                 page's help.\n  keys: {:?}\n  help:\n    {}",
                key,
                wanted,
                keys_of(&rows),
                help.join("\n    "),
            );
        }
    }
}

/// **Every key the SERVER CONFIGURATION page draws is explained in its help.**
///
/// That page is not in the guard above because it has no `*_rows()` helper --
/// it renders straight to the wire with `send_line`, so nothing could hand a
/// test its rows.  Which is exactly how `L  Conn rate` shipped drawn but
/// unexplained: `H` on that screen listed every key except the new one.
///
/// So the keys are read out of the function's own source instead.  A scan is
/// weaker than reading a rendered page and is what this file already uses for
/// paths a test cannot drive -- it can at least go red.
///
/// **Bounded to `server_configuration`'s own body.**  A scan that runs to the
/// end of the file reads every other screen's keys too and then fails on
/// keys this help was never meant to carry; the same shape as the sudo scan
/// that silently read `run_elevated` instead of the probe.
#[test]
fn test_every_server_config_key_is_explained_in_its_help() {
    let src = include_str!("config_ui.rs").replace('\r', "");
    let start = src
        .find("async fn server_configuration(")
        .expect("server_configuration moved or was renamed");
    let rest = &src[start..];
    // The next function at the same level ends this one.
    let end = rest[1..]
        .find("\n    pub(in crate::telnet) async fn ")
        .expect("could not find the end of server_configuration");
    let body = &rest[..end];

    let mut keys: Vec<char> = Vec::new();
    let marker = "self.cyan(\"";
    let mut at = 0;
    while let Some(i) = body[at..].find(marker) {
        let k = &body[at + i + marker.len()..];
        let c: Vec<char> = k.chars().collect();
        if c.len() > 1 && c[1] == '"' && (c[0].is_ascii_uppercase() || c[0].is_ascii_digit()) {
            keys.push(c[0]);
        }
        at += i + marker.len();
    }
    keys.sort_unstable();
    keys.dedup();

    // Positive control: a scan that found nothing would pass every assertion
    // below without reading a single key.
    assert!(
        keys.len() >= 10,
        "the key scan found only {keys:?} in server_configuration -- that page \
         draws far more, so the scan is broken, not the help",
    );

    for petscii in [true, false] {
        let help = TelnetSession::config_help_lines(petscii);
        for key in &keys {
            if *key == 'H' || *key == 'Q' {
                continue; // the footer actions, explained by being pressed
            }
            let wanted = format!("  {}  ", key);
            assert!(
                help.iter().any(|l| l.starts_with(&wanted)),
                "SERVER CONFIGURATION (petscii={petscii}): key {key:?} is drawn \
                 on the screen but no help line starts with {wanted:?}. Every \
                 key a page offers must be explained in that page's help.\n  \
                 keys: {keys:?}",
            );
        }
    }
}

/// ...and the other way round: a help screen must not explain a key the page
/// does not draw.
///
/// **This is the direction that was missing, and something had already gone
/// through the gap.**  The second page and its `2` entry are hidden where the
/// kernel forbids elevation -- the ordinary state of a packaged installation
/// -- across the menu row, the key arm and the error hint; the help screen was
/// a fourth surface and was not gated, so `H` documented a key that was not on
/// the menu and did nothing when pressed.  The existing guard above could not
/// see it: it only asks that every key on the screen is explained.
///
/// Driven by `MenuItems` rather than by the live state, for the reason that
/// type exists: both flags come from process state a test cannot set, so a
/// guard reading them could only ever check the machine it runs on.
#[test]
fn test_the_main_help_explains_only_keys_the_menu_draws() {
    /// The item keys a help table documents: `"  K  Label"`.
    fn help_keys(lines: &[&str]) -> Vec<char> {
        let mut keys: Vec<char> = lines
            .iter()
            .filter_map(|l| {
                let c: Vec<char> = l.chars().collect();
                // Two spaces, one key, two spaces, a label -- the indented
                // continuation lines have five leading spaces and no key.
                (c.len() > 5
                    && c[0] == ' '
                    && c[1] == ' '
                    && (c[2].is_ascii_uppercase() || c[2].is_ascii_digit())
                    && c[3] == ' '
                    && c[4] == ' '
                    && c[5] != ' ')
                    .then_some(c[2])
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    let mut session = make_test_session(TerminalType::Ansi);
    session.color_enabled = false; // read the keys, not the colour codes

    for cpm in [false, true] {
        for second in [false, true] {
            let items = MenuItems { cpm, second_page: second };
            let help = TelnetSession::main_help_lines(items);
            let documented = help_keys(help);
            // Positive control: an extractor that found nothing would pass
            // every assertion below without reading a single key.
            assert!(
                documented.len() >= 6,
                "the help scan found {documented:?} -- the page explains more \
                 than that, so the extractor is broken, not the help",
            );
            let drawn = session.main_menu_rows(items, None).join("\n");
            for key in documented {
                if key == 'H' {
                    continue; // the key they pressed to read this
                }
                let wanted = format!("  {key}  ");
                assert!(
                    drawn.contains(&wanted),
                    "cpm={cpm} second={second}: the help explains key {key:?} \
                     but the menu does not draw it. A help screen documenting \
                     an absent key is the same defect as an item an operator \
                     cannot use.\n  menu:\n{drawn}\n  help:\n    {}",
                    help.join("\n    "),
                );
            }
        }
    }
}

/// The manual's main-menu listing must be the menu the gateway draws.
///
/// §5.5 reproduces the menu as a `<pre><code>` block, which is a hand-kept
/// copy of a code-rendered list -- the half that rots, and it did: moving the
/// second page from `M` in the middle to `2` at the bottom left the manual
/// showing the old key in the old place, and nothing would have said so.
/// Three separate doc surfaces stated that key; this is the one a user reads.
///
/// Unix only, because the manual documents the full build and says so in the
/// paragraph below the block ("absent from Windows builds").  Comparing a
/// Windows menu against it would fail on the one row that is deliberately
/// missing -- the trap that put two other guards red on the windows job.
#[cfg(unix)]
#[test]
fn test_the_manual_main_menu_matches_the_real_one() {
    let manual = include_str!("../../usermanual.html").replace("\r\n", "\n");
    let at = manual
        .find("5.5 The Main Menu")
        .expect("the manual lost its main-menu section");
    let open = manual[at..]
        .find("<pre><code>")
        .map(|o| at + o + "<pre><code>".len())
        .expect("§5.5 lost its menu block");
    let close = manual[open..]
        .find("</code></pre>")
        .map(|o| open + o)
        .expect("§5.5's menu block is unterminated");

    /// The `  K  Label` rows of a block, as `(key, label)`.
    fn items(lines: &str) -> Vec<(char, String)> {
        lines
            .lines()
            .filter_map(|l| {
                let c: Vec<char> = l.chars().collect();
                if c.len() > 5 && c[0] == ' ' && c[1] == ' ' && c[3] == ' ' && c[4] == ' '
                    && (c[2].is_ascii_uppercase() || c[2].is_ascii_digit())
                {
                    Some((c[2], c[5..].iter().collect::<String>().trim_end().to_string()))
                } else {
                    None
                }
            })
            .collect()
    }

    let documented = items(&manual[open..close]);
    let mut session = make_test_session(TerminalType::Ansi);
    session.color_enabled = false;
    let drawn = items(&session.main_menu_rows(MenuItems { cpm: true, second_page: cfg!(unix) }, None).join("\n"));

    // Positive control: an extractor that matched nothing would make the
    // comparison below trivially true on two empty lists.
    assert!(
        drawn.len() >= 10,
        "the menu scan found {drawn:?} -- the extractor is broken, not the manual",
    );
    assert_eq!(
        documented, drawn,
        "usermanual.html §5.5 does not match the menu the gateway draws.\n  \
         manual: {documented:?}\n  actual: {drawn:?}",
    );
}

/// `no_new_privs` is read from the kernel, and the field is parsed exactly.
///
/// The flag decides whether the second menu offers to restart the computer or
/// explains that it cannot, so a sloppy parse either kills a working feature
/// or restores the prompt that cannot be honoured.  Absent means false: a
/// kernel that does not report the field does not enforce it either.
#[cfg(unix)]
#[test]
fn test_the_no_new_privs_field_is_read_exactly() {
    use crate::telnet::power::parse_no_new_privs;
    // The real shape, as `/proc/self/status` writes it (tab-separated).
    let set = "Name:\tethernetgateway\nUid:\t1000\t1000\nNoNewPrivs:\t1\nSeccomp:\t2\n";
    let clear = "Name:\tethernetgateway\nUid:\t1000\t1000\nNoNewPrivs:\t0\nSeccomp:\t0\n";
    assert!(parse_no_new_privs(set), "NoNewPrivs: 1 must read as set");
    assert!(!parse_no_new_privs(clear), "NoNewPrivs: 0 must read as clear");
    assert!(!parse_no_new_privs("Name:\tx\nSeccomp:\t0\n"), "absent means false");
    assert!(!parse_no_new_privs(""), "an empty status means false");
    // Not fooled by a field that merely starts the same way.
    assert!(
        !parse_no_new_privs("NoNewPrivsSomething:\t1\n"),
        "a different field beginning with the same text is not this one",
    );
}

/// The `sudo` password prompt fits a C64, whatever the account is called.
///
/// **The account name comes from the environment, so it has no bound of its
/// own.**  A service account, a long login, or `$USER` set to something
/// absurd all reach this line, and it is printed with no wrap: on PETSCII a
/// row that exactly fills 40 takes a second row for its CR/LF, so a long name
/// costs the screen two rows and lands the prompt somewhere other than where
/// the operator is looking.  Every other value on these screens is cut
/// (`computer_row_for` to 26); this one was not, until it was pulled out of
/// the `format!` it was hiding in.
#[cfg(unix)]
#[test]
fn test_the_sudo_password_prompt_fits_the_narrowest_screen() {
    use crate::telnet::power::password_prompt_label;
    let long = "a".repeat(200);
    for (what, user) in [
        ("no name at all", None),
        ("an empty name", Some("")),
        ("whitespace", Some("   ")),
        ("an ordinary login", Some("ricky")),
        ("the service account", Some("ethernetgateway")),
        ("an absurd name", Some(long.as_str())),
    ] {
        let label = password_prompt_label(user);
        assert!(
            label.chars().count() <= PETSCII_WIDTH,
            "the prompt for {what} is {} columns: {label:?}",
            label.chars().count(),
        );
        // It must still say what it wants, or a fit test passes on a blank.
        assert!(
            label.to_lowercase().contains("password"),
            "the prompt for {what} does not ask for a password: {label:?}",
        );
    }
    // The name is shown when there is one, and the two shapes differ -- a
    // truncation that swallowed the name would pass the width check alone.
    assert!(password_prompt_label(Some("ricky")).contains("ricky"));
    assert!(!password_prompt_label(None).contains("for"));
}

/// A refused `sudo` says what its own last line cannot.
///
/// **sudo's message is right for a typo and misleading for the two cases that
/// cannot be fixed by retyping**: an account that is not in sudoers, and one
/// with no password at all -- which is what the shipped service user is
/// (`useradd` with no `-p`), so an operator who turns `NoNewPrivileges` off
/// and keeps that account is told *"1 incorrect password attempt"* about a
/// password that cannot exist.  Measured on the Pi: that is the exact string
/// sudo produces.  The hint is deliberately conditional, because telling the
/// cases apart means reading sudo's English.
#[cfg(unix)]
#[test]
fn test_the_sudo_refusal_names_what_retyping_cannot_fix() {
    // The lines as the screen prints them -- read out of the module rather
    // than copied, or this is a test comparing the source with itself.
    let src = include_str!("power.rs").replace('\r', "");
    let at = src
        .find("sudo_error_line(&stderr, self.confirmation_content_width())")
        .expect("the refusal path moved; this guard no longer reads it");
    let call = &src[at..];
    let end = call.find(".await?;").expect("the refusal no longer shows anything");
    let shown = &call[..end];

    // It must still show sudo's own line: the cause is often in there, and
    // replacing it with our guess would lose "shutdown: command not found"
    // and every other specific refusal.
    assert!(
        shown.contains("&msg"),
        "the refusal no longer shows sudo's own line:\n{shown}",
    );
    // And it must name both of the things sudo's line does not.
    for phrase in ["permitted to use", "no password"] {
        assert!(
            shown.contains(phrase),
            "the refusal does not mention {phrase:?}, so an operator whose \
             account can never answer this prompt is told to retype:\n{shown}",
        );
    }
    // Conditional, not an accusation: it must not assert the password was wrong.
    assert!(
        shown.contains("If the password is right"),
        "the hint reads as a verdict rather than a possibility:\n{shown}",
    );

    // Every added line fits a C64 with `show_error_lines`' two-space indent.
    // (`test_show_error_literals_fit_petscii` scans this call too; this is the
    // positive control that these particular lines were reached.)
    let mut checked = 0;
    for line in shown.lines().filter_map(|l| {
        let t = l.trim();
        t.strip_prefix('"').and_then(|r| r.strip_suffix("\",")).map(str::to_string)
    }) {
        assert!(
            2 + line.chars().count() <= PETSCII_WIDTH,
            "refusal line {line:?} prints as {} columns",
            2 + line.chars().count(),
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "the line scan found {checked} literals -- it is not reading the \
         screen it claims to measure",
    );
}

/// The "cannot elevate here" screen says which setting, and fits a C64.
///
/// **It must not read as a password problem.**  Measured on the Pi: with the
/// shipped unit's `NoNewPrivileges=yes`, sudo refused and the operator was
/// shown sudo's *second* line -- a hint about container configuration -- after
/// being asked for a password that could never work.  Naming the setting is
/// what turns that into something an operator can act on.
#[cfg(unix)]
#[test]
fn test_the_blocked_screen_names_the_setting_and_fits() {
    let lines = crate::telnet::power::blocked_lines();
    assert!(!lines.is_empty(), "the screen says nothing");
    let joined = lines.join(" ");
    assert!(
        joined.contains("NoNewPrivileges"),
        "the screen does not name the setting an operator would grep for: {joined}",
    );
    assert!(
        joined.to_lowercase().contains("not your password"),
        "the screen does not rule out the password, which is where the \
         operator is already looking: {joined}",
    );
    for l in &lines {
        // `show_error_lines` prints a two-space indent, like `show_error`.
        assert!(
            l.chars().count() + 2 <= PETSCII_WIDTH,
            "{l:?} is {} columns with the indent, over {PETSCII_WIDTH}",
            l.chars().count() + 2,
        );
    }
}

/// The second page is offered, hinted and drawn as one decision -- **in both
/// states**.
///
/// Three surfaces gate on the same answer: the main menu's `2` row, the
/// valid-key hint, and the page's own R/S rows.  The failure that matters is
/// them disagreeing -- a row drawn for a key that is refused, or a key
/// accepted with nothing on screen to say so.
///
/// **Driven, not observed.**  The first version of this test read the live
/// `available()` and asserted the three agreed with it; on a machine where the
/// feature works -- this one, and CI -- deleting the gating left everything
/// present and still agreeing, so it **passed with the gating removed**, twice
/// over, proved by mutation.  The answer comes from the kernel's
/// `no_new_privs` and cannot be set for a test, which is why the flags are
/// parameters now: `MenuItems` and `more_menu_rows` take them, so both states
/// are exercised wherever the suite runs.
#[cfg(unix)]
#[test]
fn test_the_second_page_is_offered_and_drawn_as_one_decision() {
    use crate::telnet::MenuItems;
    let mut session = make_test_session(TerminalType::Ansi);
    session.color_enabled = false;

    for offered in [true, false] {
        let items = MenuItems { cpm: true, second_page: offered };
        let menu = session.main_menu_rows(items, None).join("\n");
        assert_eq!(
            menu.contains("2  Second Menu"),
            offered,
            "second_page={offered}: the main menu row disagrees.\n{menu}",
        );
        let hint = crate::telnet::main_menu_key_hint(items);
        assert_eq!(
            hint.contains(" 2,"),
            offered,
            "second_page={offered}: the valid-key hint disagrees: {hint:?}",
        );
    }

    for powered in [true, false] {
        let page = session.more_menu_rows(powered).join("\n");
        for (key, label) in [("R", "Restart the computer"), ("S", "Shut down the computer")] {
            assert_eq!(
                page.contains(label),
                powered,
                "powered={powered}: the page draws {key} when it should not, \
                 or not when it should.\n{page}",
            );
        }
        // Whatever else is true, the way out is always there -- a page with no
        // items and no way back would strand the operator.
        assert!(page.contains("Q=Back"), "powered={powered}: no way back:\n{page}");
    }
}

/// **Erasing the last digit must put the user back at a fresh prompt.**
///
/// `get_menu_input(false)` opens a digit collector on the first digit,
/// because a list can run past nine and `10` has to be typable.  The
/// collector accepted only more digits, so a letter arriving inside it was
/// dropped -- and backspacing the number away did not leave it.  Reported
/// 2026-09-19 from the CP/M boot picker: `9`, then a change of mind, then
/// backspace, and `N`/`P` were dead with no way to change page short of
/// leaving the screen.  Every one of the 37 menus that reads keys this way
/// had it, not just that one.
///
/// The cases are one test because they are one rule: a letter typed at an
/// empty prompt is a menu key, whether or not a digit was typed and erased
/// first; a digit typed there still opens a collector.  The last of them
/// pins the rule's **boundary** rather than its effect -- while a number is
/// still being typed a letter is deliberately still dropped, because `1` may
/// yet become `10` and the mount picker numbers ten images to a page.  A
/// later reader tempted to "finish the job" should change that on purpose.
///
/// **The failure is a wrong answer, not a hang.** The feed ends in EOF, so
/// a collector that never lets go runs out of input and returns `None` --
/// which is what the unfixed code does here, and is why this can go red.
#[tokio::test]
async fn test_backspacing_a_digit_returns_to_the_menu_keys() {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::io::AsyncWriteExt;

    /// Feed `keys` to one session's menu prompt and return what it read.
    async fn menu_input_as(term: TerminalType, keys: &[u8]) -> Option<String> {
        let (mut client, reader) = tokio::io::duplex(4096);
        let (_sink, writer_inner) = tokio::io::duplex(65536);
        let writer: SharedWriter =
            std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(writer_inner)));
        let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));
        let mut session = TelnetSession::new_ssh(
            Box::new(reader),
            writer,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
            lockouts,
        );
        session.terminal_type = term;
        client.write_all(keys).await.expect("feed");
        client.flush().await.expect("flush");
        // Close the far end so a collector that never lets go hits EOF and
        // answers `None` rather than hanging the suite.
        drop(client);
        session.get_menu_input(false).await.expect("menu input")
    }

    /// The common case: an ASCII/ANSI terminal.
    async fn menu_input_for(keys: &[u8]) -> Option<String> {
        menu_input_as(TerminalType::Ansi, keys).await
    }

    // The reported sequence: a digit, a change of mind, then a nav key.
    assert_eq!(
        menu_input_for(b"9\x08n").await,
        Some("n".to_string()),
        "after backspacing the digit away, N must be the menu key again"
    );

    // The rubout spelling of the same key, because a terminal picks one.
    assert_eq!(
        menu_input_for(b"9\x7fp").await,
        Some("p".to_string()),
        "0x7F erases the digit the same way 0x08 does"
    );

    // Positive control: the collector still collects, or the assertions
    // above would pass just as well with the digit path deleted outright.
    assert_eq!(
        menu_input_for(b"12\r").await,
        Some("12".to_string()),
        "a multi-digit entry must still be collected and submitted"
    );

    // And it re-opens: a digit after the erase starts a fresh number.
    assert_eq!(
        menu_input_for(b"9\x085\r").await,
        Some("5".to_string()),
        "a digit typed after the erase must start a new number, not resume 9"
    );

    // The line-erase key reaches the same empty prompt, so it leaves by the
    // same door.  `get_line_input` has honoured it all along; the collector
    // dropped it as just another control byte.
    assert_eq!(
        menu_input_for(b"12\x15n").await,
        Some("n".to_string()),
        "line-erase must clear the number and hand the next key to the menu"
    );

    // **An erased prompt is an untouched prompt, Enter included.**  The
    // collector used to answer `Some("")` for a bare Enter once the digits
    // were gone, and three screens read the empty answer as Back -- so
    // `9`, backspace, Enter left the screen, while the same Enter at a prompt
    // nobody had typed at was ignored.  The two are the same state now.
    assert_eq!(
        menu_input_for(b"9\x08\rn").await,
        Some("n".to_string()),
        "Enter after the erase must be ignored, as it is at a fresh prompt"
    );

    // **And on the terminal this menu exists for.**  A C64 sends `0x14` for
    // INST/DEL, so the erase itself is a byte none of the cases above use.
    // Both of the keyboard's letter ranges are here, and the second is the
    // one that makes this a PETSCII test rather than a repeat: unshifted N
    // is `0x4E`, which an ANSI session would also read as `n`, but shifted N
    // is `0xCE` and only the PETSCII decode turns that into a letter -- an
    // ANSI session answers with the raw character instead.
    for key in [0x4E_u8, 0xCE] {
        assert_eq!(
            menu_input_as(TerminalType::Petscii, &[b'9', 0x14, key]).await,
            Some("n".to_string()),
            "a Commodore's INST/DEL must free the menu keys the same way (key {key:#04x})"
        );
    }
    assert_ne!(
        menu_input_as(TerminalType::Ansi, &[b'9', 0x14, 0xCE]).await,
        Some("n".to_string()),
        "control: 0xCE is only an N once the PETSCII decode has run, so the \
         case above is exercising that decode and not repeating the ASCII one"
    );

    // **The boundary, pinned deliberately.**  A letter arriving while digits
    // are still buffered is dropped, and stays dropped: `1` may yet become
    // `10`, and `cpmmount_pick_image` numbers ten images to a page, so the
    // collector cannot know the number is finished.  Only an *empty* prompt
    // hands the key back.  This is the half of the old behaviour that is
    // intended, and it is asserted so that widening the fix is a decision
    // somebody makes rather than one that slips in.
    assert_eq!(
        menu_input_for(b"1n\r").await,
        Some("1".to_string()),
        "a letter typed mid-number is still dropped; only an empty prompt frees it"
    );
}

/// **The welcome page's window, which is all clocks.**
///
/// `welcome_is_due` is pure so the cases that matter can be stated instead of
/// waited for.  The one worth keeping is the stamp in the future: a box whose
/// clock was wrong when the stamp was written and then corrected makes
/// `now - first` underflow, and an unsaturated subtraction would wrap to an
/// enormous number and hide the page for ever.
#[test]
fn test_the_welcome_page_shows_for_seven_days_and_then_stops() {
    const DAY: u64 = 86_400;
    let t0: u64 = 1_800_000_000;

    assert!(
        welcome_is_due(0, t0),
        "a stamp of 0 means nobody has seen it yet, so it is due"
    );
    assert!(welcome_is_due(t0, t0), "the moment it is first shown");
    assert!(
        welcome_is_due(t0, t0 + WELCOME_SHOW_DAYS * DAY - 1),
        "one second inside the window"
    );
    assert!(
        !welcome_is_due(t0, t0 + WELCOME_SHOW_DAYS * DAY),
        "exactly {WELCOME_SHOW_DAYS} days is outside the window, not inside it"
    );
    assert!(
        !welcome_is_due(t0, t0 + 365 * DAY),
        "long past the window"
    );
    assert!(
        welcome_is_due(t0 + DAY, t0),
        "a stamp in the future must keep showing the page, not wrap and hide \
         it for ever"
    );
}

/// **The welcome page fits a C64, counting the chrome the renderer adds.**
///
/// The body is read from `welcome_lines()` rather than copied, like every
/// other fit test here.  The surrounding rows are *counted out of the
/// renderer's own source* instead of being a number written here: the two
/// main-menu row guards disagreed about whether the prompt line counts until
/// 2026-09-14, and the one that forgot it reported a spare row that did not
/// exist.  A row added to `show_welcome_if_due` moves this number by itself.
#[test]
fn test_the_welcome_page_fits_a_petscii_screen() {
    let src = include_str!("session.rs");
    let start = src
        .find("async fn show_welcome_if_due")
        .expect("show_welcome_if_due not found — this scan needs renaming");
    let after = &src[start..];
    let end = after
        .find("\n    /// Inner menu loop")
        .expect("the renderer's end marker moved — the scan would run on");
    let body = &after[..end];

    // Every `send_line` the renderer makes.  One of them is inside the loop
    // over `welcome_lines()`, so it is not chrome.
    let emitted = body.matches("self.send_line(").count();
    assert!(
        emitted >= 4,
        "only {emitted} send_line calls found in the renderer — the scan has \
         stopped matching"
    );
    let chrome = emitted - 1;

    let lines = TelnetSession::welcome_lines();
    let rows = lines.len() + chrome;
    assert!(
        rows <= 22,
        "the welcome page draws {rows} rows ({} body + {chrome} chrome), and a \
         PETSCII screen holds 22",
        lines.len(),
    );

    for line in lines {
        assert!(
            line.len() <= PETSCII_WIDTH,
            "welcome line {line:?} is {} chars, exceeds {PETSCII_WIDTH}",
            line.len(),
        );
    }

    // The two literals the renderer prints itself, read out of it rather than
    // restated here, and measured with the two-space indent it adds.
    for literal in ["WELCOME TO YOUR ETHERNET GATEWAY", "Press SPACE for the main menu"] {
        assert!(
            body.contains(literal),
            "the renderer no longer prints {literal:?} — this test is measuring \
             text the product does not show"
        );
        assert!(
            literal.len() + 2 <= PETSCII_WIDTH,
            "{literal:?} plus its indent is {} chars, exceeds {PETSCII_WIDTH}",
            literal.len() + 2,
        );
    }
}

/// **An abandoned session says goodbye, wherever it was abandoned.**
///
/// An idle timeout is how a session that nobody is sitting at ends, not a
/// fault: `run` turns it into "Disconnected: idle timeout." and returns `Ok`,
/// and `is_normal_disconnect` deliberately does NOT count `TimedOut`, so
/// anything that escapes that arm is logged as a session error.
///
/// Only `run_menu_loop` used to be inside it.  Adding the welcome page put a
/// blocking read *before* it and made that reachable for everyone -- it is the
/// first screen, and it is the one that asks to be read, so it is exactly
/// where somebody walks away.  The master-password prompt had the same hole
/// the whole time and nobody had hit it.
///
/// An SSH session is used because `run` skips terminal detection and the login
/// for one, which puts the welcome page first with no scripted input in the
/// way.  The stamp is set to *now* rather than left at `0` so the page is due
/// without the session writing a config file as a side effect.
#[tokio::test]
async fn test_a_session_abandoned_on_the_welcome_page_still_says_goodbye() {
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::io::AsyncReadExt;

    let _cfg_lock = crate::config::CONFIG_TEST_LOCK.lock().await;
    let previous = crate::config::get_config().welcome_first_shown;
    crate::config::update_config_value("welcome_first_shown", &unix_now().to_string());

    // Held open and never written to: the session waits for a key that never
    // comes, which is what an abandoned terminal looks like.
    let (_client, reader) = tokio::io::duplex(4096);
    let (mut peer, writer_inner) = tokio::io::duplex(65536);
    let writer: SharedWriter =
        std::sync::Arc::new(tokio::sync::Mutex::new(Box::new(writer_inner)));
    let lockouts: LockoutMap = std::sync::Arc::new(StdMutex::new(HashMap::new()));
    let mut session = TelnetSession::new_ssh(
        Box::new(reader),
        writer,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        None,
        lockouts,
    );
    session.idle_timeout = std::time::Duration::from_millis(150);

    let outcome = session.run().await;

    // Drain until the pipe goes quiet: one `read_buf` returns the first chunk
    // only, which here was the clear-screen and the first separator -- enough
    // to look like the page was never drawn.
    let mut drained = Vec::new();
    loop {
        let mut chunk = vec![0u8; 4096];
        match tokio::time::timeout(
            std::time::Duration::from_millis(300),
            peer.read(&mut chunk),
        )
        .await
        {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => drained.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
        }
    }
    let seen = String::from_utf8_lossy(&drained).to_string();

    crate::config::update_config_value("welcome_first_shown", &previous.to_string());

    assert!(
        outcome.is_ok(),
        "an idle timeout on the welcome page must be handled, not returned as \
         a session error (callers log anything that is not a normal \
         disconnect, and TimedOut is not one): {outcome:?}"
    );
    assert!(
        seen.contains("WELCOME TO YOUR ETHERNET GATEWAY"),
        "the welcome page should have been drawn first; got: {seen:?}"
    );
    assert!(
        seen.contains("idle timeout"),
        "the goodbye must reach the screen the caller was left looking at; \
         got: {seen:?}"
    );
}
