#!/usr/bin/env bash
# One protocol, one direction, against an ALREADY-RUNNING VICE.
#   ./relay-one.sh <protocol> <download|upload> <serial|telnet>
#
# The EMULATOR is deliberately not restarted (see start-vice.sh) -- what a
# restart used to buy is bought two cheaper ways: freshdisk.py swaps the floppy
# so a download's splat entry can only be this run's, and run-transfer.py
# re-establishes NovaTerm's state at the top of every run.
#
# The SLAVE GATEWAY is restarted, and that is not the same concession.  A PTY
# pair never drops carrier: socat holds both ends open, so a session that ended
# leaves the modem ONLINE and the next run types ATDT at a still-connected
# remote -- which is exactly how the first trial here failed, with NovaTerm's
# `at` landing in the master's file picker as "Select #: t".  Real hardware gets
# a clean line from being unplugged; this rig has to ask for one.  Restarting
# also gives each run its own slave.log (truncating a file a live `tee` holds
# open leaves a sparse hole, not an empty file) and re-exercises the relay
# connect, which is the thing under test.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$HERE"
PROTO="${1:?protocol}"; DIR="${2:?download|upload}"; LINK="${3:-serial}"

# The dial string differs per link and getting it wrong looks like a hang: on
# serial the C64 talks to our own modem emulator (which resolves the name and,
# on a slave, relays it to the master); on telnet it talks to tcpser, whose
# phonebook start-tcpser.sh seeded.
if [ "$LINK" = serial ]; then DIALNO=ethernetgateway; else DIALNO=1; fi

pgrep -x x64sc >/dev/null || { echo "FATAL: no VICE running -- start-vice.sh first" >&2; exit 1; }

echo "--- restarting the slave for a clean line"
# Empty the log BEFORE launching.  start-slave.sh kills the old gateway, sleeps,
# and rebuilds the socat pair before its `tee` truncates this file -- ten-odd
# seconds during which it still holds the PREVIOUS run's "REGISTERED with
# master".  The poll below would match that on its first iteration and declare a
# slave ready that is not up: the exact failure the poll was written to prevent.
: > slave.log
(nohup ./start-slave.sh "$LINK" > /dev/null 2>&1 < /dev/null &)
# Wait for the thing this run actually needs, not for a fixed sleep: on the
# serial link that is the port having registered with the master, on telnet the
# listener being up.  A run started before either is a failure that reads like
# a protocol fault.
want="Telnet server listening"
[ "$LINK" = serial ] && want="REGISTERED with master"
ready=no
for _ in $(seq 1 60); do
    if grep -aq "$want" slave.log 2>/dev/null; then ready=yes; break; fi
    sleep 1
done
[ "$ready" = yes ] || { echo "FATAL: slave never reported '$want'; see slave.log" >&2; exit 1; }
echo "--- slave ready ($want)"

# Report PYTHON's status, not grep's.  `cmd | grep -v ... || FATAL` tests the
# filter, and grep answers 0 whenever it printed a line -- so freshdisk.py's own
# "FATAL: monitor refused ..." was passed through and the run continued against
# the previous run's disk.  The same trap, with the same fix, is three lines
# below for run-transfer.py; this call never got it.
DISPLAY=:0 timeout 120 python3 freshdisk.py 2>&1 | grep -v "X protocol\|Xlib"
[ "${PIPESTATUS[0]}" -eq 0 ] || {
    echo "FATAL: could not swap in a fresh transfer disk" >&2; exit 1; }

# Report the transfer's status, not the filter's: ending on a pipe would make
# this script exit with grep's status, and grep answers 0 when it printed
# something -- so a failed run that happened to print a line would look clean.
# On the telnet link the gateway the C64 dials is the SLAVE's own telnet
# server, and the files under test belong to the MASTER -- so the run takes the
# slave's Telnet Gateway out to it.  Without this the telnet leg would test the
# slave against itself and pass while proving nothing about a relay.
TG=()
# The master's telnet port for the Telnet Gateway hop.  An address, not a
# constant -- see relay-sweep.sh, which passes this in; the default is only
# for driving one cell by hand.
[ "$LINK" = telnet ] && TG=("tg=${MASTER_TG:-192.168.1.126:2323}")

DISPLAY=:0 timeout 660 python3 run-transfer.py "$PROTO" "$DIR" "$DIALNO" "${TG[@]}" 2>&1 \
    | grep -v "X protocol\|Xlib"
exit "${PIPESTATUS[0]}"
