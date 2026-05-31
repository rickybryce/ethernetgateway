#!/usr/bin/env bash
# Stop any harness processes left running (gateway on :2323, tcpser, VICE).
# Uses precise matches so it won't disturb an unrelated gateway you run
# normally.
set -uo pipefail

pkill -f "punter-vice.*tcpser/tcpser" 2>/dev/null && echo "stopped tcpser" || true
pkill -f "x64sc .*novaterm" 2>/dev/null && echo "stopped VICE" || true

# Stop the gateway by the port it owns (don't pkill the binary by name — that
# could hit another instance, and pkill races its tokio shutdown).
gw_pid=$(ss -ltnp 2>/dev/null | awk '/:2323 /{print}' \
    | grep -oE 'pid=[0-9]+' | head -1 | cut -d= -f2)
if [ -n "${gw_pid:-}" ]; then
    kill "$gw_pid" 2>/dev/null && echo "stopped gateway (pid $gw_pid on :2323)" || true
fi
echo "done"
