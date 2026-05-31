#!/usr/bin/env bash
# Launch VICE (x64sc) running NovaTerm with a SwiftLink ACIA wired to tcpser
# over ip232. NovaTerm speaks Punter; this is the real C64 peer for the
# gateway.
#
# ACIA: SwiftLink @ $DE00, NMI, on RS232 device 1 = tcpser's ip232 endpoint.
#
# The NovaTerm disk is user-supplied (not in this repo). Point NOVATERM_DISK at
# it, or drop it at the default path below.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DISK="${NOVATERM_DISK:-$HOME/Documents/NovaTerm/novaterm_9.6c.d64}"
ACIA_BASE="${ACIA_BASE:-0xDE00}"   # use 0xDF00 with the *-df00 NovaTerm disk
IP232_PORT="${IP232_PORT:-25232}"

if [ ! -f "$DISK" ]; then
    echo "NovaTerm disk not found: $DISK" >&2
    echo "Set NOVATERM_DISK=/path/to/novaterm.d64 and re-run." >&2
    exit 1
fi

echo "VICE: NovaTerm = $DISK"
echo "      SwiftLink ACIA @ $ACIA_BASE (NMI) -> ip232 127.0.0.1:$IP232_PORT"
echo "      In NovaTerm: pick the SwiftLink/$ACIA_BASE interface, then dial 1."

exec x64sc \
    -acia1 -acia1base "$ACIA_BASE" -acia1irq 1 -acia1mode 1 -myaciadev 0 \
    -rsdev1 "127.0.0.1:$IP232_PORT" -rsdev1ip232 -rsdev1baud 2400 \
    -autostart "$DISK"
