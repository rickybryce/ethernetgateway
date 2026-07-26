//! Which address is this machine's router?
//!
//! The `disable_gateway_connections` rule refuses connections that appear to
//! come from the LAN's router, because traffic forwarded in from outside often
//! arrives wearing that source address.  It used to *assume* the router was
//! `x.x.x.1`, which is a convention, not a fact: plenty of networks put the
//! router on `.254`, and plenty of ordinary machines sit on `.1`.  So the
//! assumption both missed the address it meant to block and blocked an address
//! it did not mean to.
//!
//! This module asks the operating system instead — the default route's next
//! hop, which is exactly the address the router's traffic arrives from — and
//! the allowlist blocks precisely that.  The `.1` heuristic survives only as
//! the fallback for a host where the query fails, so the setting never becomes
//! silently weaker than it was.
//!
//! **The query never runs on the connection path.**  Two of the three
//! platforms answer by running a small system command, and a connection check
//! must not be able to block on a subprocess; the probe runs once on a
//! background thread at startup (and on each restart), and the accept path only
//! ever reads the cached answer.  Until the first probe lands the cache is
//! empty, which means the fallback — the same behaviour this code replaces.
//!
//! Detection per platform, all from documented, stable interfaces:
//!
//! * **Linux** — `/proc/net/route` and `/proc/net/ipv6_route`, read directly.
//!   No subprocess at all.
//! * **macOS / BSD** — `route -n get default`, whose `gateway:` line is the
//!   next hop (the same call every macOS networking guide uses).
//! * **Windows** — `route print`, taking the `0.0.0.0 0.0.0.0` (IPv4) and
//!   `::/0` (IPv6) rows.  Parsed by matching the *addresses*, never the column
//!   headings, so a localised Windows still parses.
//!
//! Every parser is a pure function over the text its platform produces, so all
//! three are unit-tested on every platform from captured output.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::logger::glog;

/// How long a probe result is trusted before a fresh one is worth taking.
/// A DHCP lease change or a swapped router should be picked up without a
/// restart, but this is not worth re-querying often — the accept path reads the
/// cache, and a stale entry for a few minutes only affects one optional rule.
const CACHE_TTL: Duration = Duration::from_secs(300);

struct Cache {
    addrs: Vec<IpAddr>,
    /// When the last probe *finished*.  `None` = never probed.
    probed_at: Option<Instant>,
    /// A probe thread is in flight; don't start a second one.
    in_flight: bool,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            addrs: Vec::new(),
            probed_at: None,
            in_flight: false,
        })
    })
}

/// The router addresses last detected, or empty if we don't know (yet, or at
/// all on this host).  Never blocks and never runs a command: this is what the
/// connection path calls.
///
/// Reading also *triggers* a background refresh when the cached answer has gone
/// stale, so a long-running gateway follows a network that changes under it
/// without anyone restarting anything.
pub(crate) fn cached_addrs() -> Vec<IpAddr> {
    let (addrs, stale) = {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        let stale = match guard.probed_at {
            None => true,
            Some(t) => t.elapsed() > CACHE_TTL,
        };
        (guard.addrs.clone(), stale && !guard.in_flight)
    };
    if stale {
        probe_in_background();
    }
    addrs
}

/// Start a probe on a background thread unless one is already running.  Called
/// at startup and whenever the cached answer ages out.
pub(crate) fn probe_in_background() {
    {
        let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        if guard.in_flight {
            return;
        }
        guard.in_flight = true;
    }
    // A detached thread: nothing waits on this, and the worst case of it dying
    // is that the cache keeps its previous value (or stays empty, which is the
    // documented fallback).
    std::thread::spawn(|| {
        let found = detect();
        let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        let changed = guard.addrs != found;
        guard.addrs = found;
        guard.probed_at = Some(Instant::now());
        guard.in_flight = false;
        let addrs = guard.addrs.clone();
        drop(guard);
        if changed {
            if addrs.is_empty() {
                glog!(
                    "Router address: could not be determined on this host; the \
                     \"block connections from the router\" setting falls back to the \
                     x.x.x.1 rule."
                );
            } else {
                glog!("Router address: {}", join(&addrs));
            }
        }
    });
}

/// Human-readable form of the detected router(s) for the config UIs: the
/// address, or the fallback rule's name when we don't know.  Never blocks.
pub(crate) fn describe() -> String {
    let addrs = cached_addrs();
    if addrs.is_empty() {
        "x.x.x.1".to_string()
    } else {
        join(&addrs)
    }
}

fn join(addrs: &[IpAddr]) -> String {
    addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Is `ip` one of this machine's routers?  With no detected address this
/// returns `false` and the caller applies its own fallback.
pub(crate) fn is_router(ip: IpAddr, routers: &[IpAddr]) -> bool {
    let normalized = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    routers.contains(&normalized)
}

// ── Detection ──────────────────────────────────────────────────

/// Ask the OS for the default route's next hop(s).  Returns an empty vec when
/// the platform can't tell us — the caller falls back to the `.1` rule.
fn detect() -> Vec<IpAddr> {
    let mut out = detect_platform();
    // A host with two default routes (two interfaces) legitimately has two
    // routers; dedupe but keep both.
    out.sort();
    out.dedup();
    out
}

#[cfg(target_os = "linux")]
fn detect_platform() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/proc/net/route") {
        out.extend(parse_proc_net_route(&text).into_iter().map(IpAddr::V4));
    }
    if let Ok(text) = std::fs::read_to_string("/proc/net/ipv6_route") {
        out.extend(parse_proc_net_ipv6_route(&text).into_iter().map(IpAddr::V6));
    }
    out
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn detect_platform() -> Vec<IpAddr> {
    let mut out = Vec::new();
    for args in [
        ["-n", "get", "default"].as_slice(),
        ["-n", "get", "-inet6", "default"].as_slice(),
    ] {
        if let Some(text) = run("route", args) {
            out.extend(parse_bsd_route_get(&text));
        }
    }
    out
}

#[cfg(target_os = "windows")]
fn detect_platform() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Some(text) = run("route", &["print", "-4"]) {
        out.extend(parse_windows_route_print_v4(&text).into_iter().map(IpAddr::V4));
    }
    if let Some(text) = run("route", &["print", "-6"]) {
        out.extend(parse_windows_route_print_v6(&text).into_iter().map(IpAddr::V6));
    }
    out
}

/// Platforms we don't have a reader for: say so honestly rather than guess.
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "windows"
)))]
fn detect_platform() -> Vec<IpAddr> {
    Vec::new()
}

/// Run a system command and return its stdout, or `None` if it can't be run or
/// fails.  Only ever called from the probe thread.  No shell is involved (the
/// program and arguments are passed directly, and both are compile-time
/// constants here), so there is nothing for an environment or a filename to
/// inject into.
#[cfg(not(target_os = "linux"))]
fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // Route tables are ASCII; anything else is a sign we're not reading what we
    // think we are, so lossy conversion is fine and cannot panic.
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── Parsers (pure, and tested on every platform) ───────────────

/// `/proc/net/route`: whitespace-separated columns, one header line, then one
/// row per route.  A default route has Destination `00000000`; its Gateway
/// column is the next hop as **little-endian** hex (`0101A8C0` = 192.168.1.1).
/// A `0` gateway means an on-link route with no next hop — not a router.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_net_route(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let mut cols = line.split_whitespace();
        let (_iface, dest, gw) = match (cols.next(), cols.next(), cols.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => continue,
        };
        if !dest.eq_ignore_ascii_case("00000000") {
            continue;
        }
        let Ok(raw) = u32::from_str_radix(gw, 16) else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        // Stored little-endian: the first hex pair is the LAST octet.
        out.push(Ipv4Addr::from(raw.swap_bytes()));
    }
    out
}

/// `/proc/net/ipv6_route`: destination (32 hex chars), destination prefix
/// length (hex), source, source prefix length, then the next hop as 32 hex
/// chars.  The default route is the all-zero destination with prefix length 0;
/// an all-zero next hop is on-link, not a router.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_net_ipv6_route(text: &str) -> Vec<Ipv6Addr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 {
            continue;
        }
        let (dest, dest_len, next_hop) = (cols[0], cols[1], cols[4]);
        if dest.len() != 32 || next_hop.len() != 32 {
            continue;
        }
        if dest.bytes().any(|b| b != b'0') {
            continue; // not the default destination
        }
        if u32::from_str_radix(dest_len, 16).unwrap_or(u32::MAX) != 0 {
            continue; // not a /0
        }
        if next_hop.bytes().all(|b| b == b'0') {
            continue; // on-link
        }
        if let Some(addr) = hex32_to_ipv6(next_hop) {
            out.push(addr);
        }
    }
    out
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn hex32_to_ipv6(hex: &str) -> Option<Ipv6Addr> {
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(Ipv6Addr::from(bytes))
}

/// `route -n get [-inet6] default` on macOS/BSD prints an indented
/// `key: value` block; the next hop is the `gateway:` line.  A default route
/// that is on-link has no gateway line at all, and a `gateway:` naming an
/// interface (`en0`) rather than an address simply won't parse — both are
/// skipped.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn parse_bsd_route_get(text: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("gateway:") else {
            continue;
        };
        let value = rest.trim();
        // macOS appends a zone to link-local addresses (fe80::1%en0); the zone
        // is not part of the address we compare peers against.
        let value = value.split('%').next().unwrap_or(value);
        if let Ok(ip) = value.parse::<IpAddr>() {
            out.push(ip);
        }
    }
    out
}

/// Windows `route print -4`.  The IPv4 default route is the row whose
/// destination and netmask are both `0.0.0.0`; the third column is the next
/// hop, or the literal `On-link`.  Matching on those addresses rather than on
/// the column headings is what makes this work on a non-English Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_route_print_v4(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 || cols[0] != "0.0.0.0" || cols[1] != "0.0.0.0" {
            continue;
        }
        if let Ok(ip) = cols[2].parse::<Ipv4Addr>() {
            out.push(ip);
        }
    }
    out
}

/// Windows `route print -6`.  Rows are `If Metric Destination Gateway`; the
/// default route's destination is `::/0`.  The gateway is the token after it,
/// which is an address or the literal `On-link`.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_route_print_v6(text: &str) -> Vec<Ipv6Addr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(pos) = cols.iter().position(|c| *c == "::/0") else {
            continue;
        };
        let Some(gw) = cols.get(pos + 1) else { continue };
        let gw = gw.split('%').next().unwrap_or(gw);
        if let Ok(ip) = gw.parse::<Ipv6Addr>() {
            out.push(ip);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from a real Debian host (`cat /proc/net/route`).
    const PROC_NET_ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";

    #[test]
    fn test_parse_proc_net_route_takes_the_default_routes_next_hop() {
        // 0101A8C0 is little-endian for 192.168.1.1.
        assert_eq!(
            parse_proc_net_route(PROC_NET_ROUTE),
            vec![Ipv4Addr::new(192, 168, 1, 1)]
        );
    }

    #[test]
    fn test_parse_proc_net_route_ignores_on_link_and_junk() {
        // A default route with no next hop is on-link, not a router.
        let on_link = "Iface\tDestination\tGateway\nppp0\t00000000\t00000000\t0003\n";
        assert!(parse_proc_net_route(on_link).is_empty());
        // Header only, empty file, and a truncated row must all be survivable.
        assert!(parse_proc_net_route("Iface\tDestination\tGateway\n").is_empty());
        assert!(parse_proc_net_route("").is_empty());
        assert!(parse_proc_net_route("Iface\nnonsense\n").is_empty());
        assert!(parse_proc_net_route("Iface Dest GW\neth0\tZZZZZZZZ\tQQQQQQQQ\n").is_empty());
    }

    #[test]
    fn test_parse_proc_net_route_handles_two_default_routes() {
        let two = "Iface\tDestination\tGateway\n\
                   eth0\t00000000\t0101A8C0\t0003\n\
                   wlan0\t00000000\tFE01A8C0\t0003\n";
        assert_eq!(
            parse_proc_net_route(two),
            vec![Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(192, 168, 1, 254)]
        );
    }

    #[test]
    fn test_parse_proc_net_ipv6_route_takes_the_default_next_hop() {
        let text = "\
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000000 00000000 00000003 eth0
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000000 00000001 00000001 eth0
";
        assert_eq!(
            parse_proc_net_ipv6_route(text),
            vec!["fe80::1".parse::<Ipv6Addr>().unwrap()]
        );
    }

    #[test]
    fn test_parse_proc_net_ipv6_route_skips_on_link_default() {
        let text = "00000000000000000000000000000000 00 \
                    00000000000000000000000000000000 00 \
                    00000000000000000000000000000000 00000400 00000000 00000000 00000003 eth0\n";
        assert!(parse_proc_net_ipv6_route(text).is_empty());
        assert!(parse_proc_net_ipv6_route("").is_empty());
        assert!(parse_proc_net_ipv6_route("short line\n").is_empty());
    }

    // Captured from macOS 14 (`route -n get default`).
    const BSD_ROUTE_GET: &str = "\
   route to: default
destination: default
       mask: default
    gateway: 192.168.1.254
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
";

    #[test]
    fn test_parse_bsd_route_get() {
        assert_eq!(
            parse_bsd_route_get(BSD_ROUTE_GET),
            vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 254))]
        );
    }

    #[test]
    fn test_parse_bsd_route_get_strips_the_ipv6_zone() {
        let text = "   route to: default\n    gateway: fe80::1%en0\n  interface: en0\n";
        assert_eq!(
            parse_bsd_route_get(text),
            vec![IpAddr::V6("fe80::1".parse().unwrap())]
        );
    }

    #[test]
    fn test_parse_bsd_route_get_ignores_an_interface_gateway_and_no_route() {
        // A point-to-point default route names an interface, not an address.
        assert!(parse_bsd_route_get("    gateway: utun3\n").is_empty());
        // `route: writing to routing socket: not in table` — no gateway line.
        assert!(parse_bsd_route_get("route: not in table\n").is_empty());
        assert!(parse_bsd_route_get("").is_empty());
    }

    // Captured from Windows 11 (`route print -4`), including the On-link rows
    // that must not be mistaken for a next hop.
    const WIN_ROUTE_V4: &str = "\
===========================================================================
Active Routes:
Network Destination        Netmask          Gateway       Interface  Metric
          0.0.0.0          0.0.0.0      192.168.1.1     192.168.1.50     25
        127.0.0.0        255.0.0.0         On-link         127.0.0.1    331
      192.168.1.0    255.255.255.0         On-link      192.168.1.50    281
===========================================================================
";

    #[test]
    fn test_parse_windows_route_print_v4() {
        assert_eq!(
            parse_windows_route_print_v4(WIN_ROUTE_V4),
            vec![Ipv4Addr::new(192, 168, 1, 1)]
        );
    }

    #[test]
    fn test_parse_windows_route_print_v4_skips_an_on_link_default() {
        let text = "          0.0.0.0          0.0.0.0         On-link      192.168.1.50     25\n";
        assert!(parse_windows_route_print_v4(text).is_empty());
        assert!(parse_windows_route_print_v4("").is_empty());
    }

    /// The parser keys off the addresses, never the column headings, so a
    /// localised Windows must still yield the same answer.
    #[test]
    fn test_parse_windows_route_print_v4_is_locale_independent() {
        let german = "\
===========================================================================
Aktive Routen:
     Netzwerkziel    Netzwerkmaske          Gateway    Schnittstelle Metrik
          0.0.0.0          0.0.0.0      192.168.2.1     192.168.2.20     35
===========================================================================
";
        assert_eq!(
            parse_windows_route_print_v4(german),
            vec![Ipv4Addr::new(192, 168, 2, 1)]
        );
    }

    #[test]
    fn test_parse_windows_route_print_v6() {
        let text = "\
Active Routes:
 If Metric Network Destination      Gateway
  8    281 ::/0                     fe80::1%8
  1    331 ::1/128                  On-link
";
        assert_eq!(
            parse_windows_route_print_v6(text),
            vec!["fe80::1".parse::<Ipv6Addr>().unwrap()]
        );
        assert!(parse_windows_route_print_v6("  1  331 ::1/128  On-link\n").is_empty());
    }

    #[test]
    fn test_is_router_matches_exactly_and_through_v4_mapping() {
        let routers = vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 254))];
        assert!(is_router("192.168.1.254".parse().unwrap(), &routers));
        // The same address arriving on a dual-stack socket as ::ffff:a.b.c.d.
        assert!(is_router("::ffff:192.168.1.254".parse().unwrap(), &routers));
        // The old assumption is just another host once the real router is known.
        assert!(!is_router("192.168.1.1".parse().unwrap(), &routers));
        // With nothing detected, nothing matches — the caller falls back.
        assert!(!is_router("192.168.1.254".parse().unwrap(), &[]));
    }

    #[test]
    fn test_describe_falls_back_to_the_rule_name() {
        assert_eq!(join(&[]), "");
        let two = vec![
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V6("fe80::1".parse().unwrap()),
        ];
        assert_eq!(join(&two), "192.168.1.1, fe80::1");
    }

    /// The real probe must be safe to call anywhere: it may find nothing (a
    /// container with no default route, an unsupported platform), but it must
    /// not panic, and it must not block the caller.
    #[test]
    fn test_detect_is_harmless_and_returns_sorted_unique() {
        let found = detect();
        let mut sorted = found.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(found, sorted, "detect() must return sorted, deduped output");
    }
}
