#!/bin/bash
# shoot-telnet.sh <name> [steps...] : run term.py in an 80x24 xterm and capture it.
set -u
N=$1; shift
H=~/manual-shots; READY=$H/.ready-$N; rm -f "$READY"
xterm -T "shot-$N" -geometry 80x24 -fa 'DejaVu Sans Mono' -fs 14 \
      -bg '#000000' -fg '#d0d0d0' -b 6 +sb \
      -e python3 $H/term.py "$READY" 127.0.0.1 2323 "$@" &
XP=$!
for i in $(seq 1 120); do [ -f "$READY" ] && break; sleep 0.5; done
sleep 1
WID=$(xwininfo -name "shot-$N" | awk '/Window id:/{print $4}')
import -window "$WID" "$H/out/telnet-$N.png" && identify "$H/out/telnet-$N.png" | awk '{print $1, $3}'
kill $XP 2>/dev/null; rm -f "$READY"
