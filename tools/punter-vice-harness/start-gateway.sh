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
# The gateway keeps everything in `ethernetgateway-data` below the directory
# it is launched from, so the seeded config and payloads have to go there or
# it comes up on its defaults instead of this harness's settings.
DATA="$RUN/ethernetgateway-data"
mkdir -p "$DATA/transfer"
# Seed the runtime config once (the gateway rewrites it in place on launch).
[ -f "$DATA/egateway.conf" ] || cp "$HERE/egateway.harness.conf" "$DATA/egateway.conf"
# Seed download samples without clobbering anything already there.
for f in "$HERE"/payloads/*; do
    base="$(basename "$f")"
    [ -f "$DATA/transfer/$base" ] || cp "$f" "$DATA/transfer/$base"
done

cd "$RUN"
echo "Gateway: telnet 127.0.0.1:2323, transfer_dir=$DATA/transfer, verbose on"
echo "Log: $HERE/gateway.log"
# Use process substitution (not a `| tee` pipeline) so `exec` replaces THIS
# shell with the gateway — the script's PID then *is* the gateway, so the
# orchestrator killing that PID actually stops it (a `| tee` pipeline would
# leave the gateway orphaned on :2323). tee still mirrors to stdout + log.
exec "$BIN" > >(tee "$HERE/gateway.log") 2>&1
