#!/usr/bin/env bash
# One-shot orchestrator: start the gateway and tcpser in the background, then
# launch VICE/NovaTerm in the foreground. Closing VICE stops everything. Logs
# land in gateway.log and tcpser.log next to this script.
#
# Order matters: gateway first (tcpser dials it), tcpser second (VICE connects
# to its ip232 port), VICE last.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

pids=()
cleanup() {
    echo
    echo "Shutting down harness..."
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    sleep 1
    for pid in "${pids[@]}"; do kill -9 "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT INT TERM

echo "=== [1/3] gateway (telnet 2323, headless, verbose) ==="
"$HERE/start-gateway.sh" >/dev/null 2>&1 &
pids+=($!)
sleep 2

echo "=== [2/3] tcpser (ip232 25232 -> gateway) ==="
"$HERE/start-tcpser.sh" >"$HERE/tcpser.log" 2>&1 &
pids+=($!)
sleep 1

echo "=== [3/3] VICE / NovaTerm (foreground) ==="
echo "Logs:  gateway -> $HERE/gateway.log   tcpser -> $HERE/tcpser.log"
echo "Close the VICE window (or Ctrl-C here) to stop everything."
echo
"$HERE/start-vice.sh"
