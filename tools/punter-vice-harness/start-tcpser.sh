#!/usr/bin/env bash
# Start tcpser as a virtual Hayes modem bridging VICE (ip232 on TCP 25232) to
# the gateway's telnet port. Dialing 1 (ATDT1) from the C64 connects to the
# gateway. tcpser auto-negotiates telnet + RFC 856 binary so 8-bit Punter
# blocks pass through cleanly.
#
#   VICE ACIA <--ip232/TCP 25232--> tcpser <--telnet/TCP 2323--> gateway
#
# tcpser is third-party (GPL) and is NOT bundled in this repo. Build it once:
#   git clone https://github.com/go4retro/tcpser && (cd tcpser && make)
# then point this script at it via the TCPSER env var, or drop the build in
# ./tcpser/ next to this script.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IP232_PORT="${IP232_PORT:-25232}"
GATEWAY="${GATEWAY_ADDR:-127.0.0.1:2323}"

find_tcpser() {
    for c in "${TCPSER:-}" "$HERE/tcpser/tcpser" "$HOME/claude/punter-vice/tcpser/tcpser"; do
        [ -n "$c" ] && [ -x "$c" ] && { echo "$c"; return 0; }
    done
    command -v tcpser 2>/dev/null && return 0
    return 1
}

if ! TCPSER_BIN="$(find_tcpser)"; then
    cat >&2 <<EOF
tcpser not found. It's third-party (GPL), so it isn't committed here. Build it:
  git clone https://github.com/go4retro/tcpser && (cd tcpser && make)
then re-run as:  TCPSER=/path/to/tcpser/tcpser $0
(or drop the build in $HERE/tcpser/)
EOF
    exit 1
fi

echo "tcpser ($TCPSER_BIN): ip232 127.0.0.1:$IP232_PORT  ->  dial 1 (or 2323) = $GATEWAY"
# -v: virtual RS232 over TCP (ip232) — the side VICE connects to.
# -s/-S: serial speed reported to the C64.
# -l 4 -tSsiI: INFO log + serial/IP traces (handy for debugging).
# -n: phonebook aliases so a numeric dial reaches the gateway.
exec "$TCPSER_BIN" \
    -v "$IP232_PORT" \
    -s 2400 -S 2400 \
    -l 4 -tSsiI \
    -n1="$GATEWAY" \
    -n2323="$GATEWAY"
