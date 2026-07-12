#!/usr/bin/env bash
# Start the Ethernet Gateway headless on telnet 2323, verbose, from a runtime
# dir seeded from this harness's config template + sample payloads. Logs to
# stderr, tee'd to gateway.log so the Punter per-block trace is captured.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"

BIN="${GATEWAY_BIN:-$REPO_ROOT/target/release/ethernetgateway}"
[ -x "$BIN" ] || BIN="$REPO_ROOT/target/debug/ethernetgateway"
if [ ! -x "$BIN" ]; then
    echo "Gateway binary not found." >&2
    echo "Build it:  (cd $REPO_ROOT && cargo build --release)" >&2
    exit 1
fi

RUN="$HERE/run"
mkdir -p "$RUN/transfer"
# Seed the runtime config once (the gateway rewrites it in place on launch).
[ -f "$RUN/egateway.conf" ] || cp "$HERE/egateway.harness.conf" "$RUN/egateway.conf"
# Seed download samples without clobbering anything already there.
for f in "$HERE"/payloads/*; do
    base="$(basename "$f")"
    [ -f "$RUN/transfer/$base" ] || cp "$f" "$RUN/transfer/$base"
done

cd "$RUN"
echo "Gateway: telnet 127.0.0.1:2323, transfer_dir=$RUN/transfer, verbose on"
echo "Log: $HERE/gateway.log"
# Use process substitution (not a `| tee` pipeline) so `exec` replaces THIS
# shell with the gateway — the script's PID then *is* the gateway, so the
# orchestrator killing that PID actually stops it (a `| tee` pipeline would
# leave the gateway orphaned on :2323). tee still mirrors to stdout + log.
exec "$BIN" > >(tee "$HERE/gateway.log") 2>&1
