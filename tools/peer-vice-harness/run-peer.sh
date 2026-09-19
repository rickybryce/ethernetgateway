#!/usr/bin/env bash
# One peer-to-peer transfer attempt, end to end.
#   run-peer.sh <protocol> [savename]
# Sender sends "puntest"; the receiver saves it under [savename].
#
# **Both ends are addresses, and neither has to be the master.**  These were
# `SLAVE`/`MASTER` with the 141/178 rig baked in, which named a topology
# rather than a role: the master is a *crossbar* here, not an endpoint, so a
# slave can dial another slave (`relay::handle_peer_dial` falls through to
# `claim_remote_peer` when the address is not one of its own ports).  Naming
# them for what they do in the transfer leaves the rig free to move -- and it
# has moved twice.
SENDER="${SENDER:-192.168.1.141}"
RECEIVER="${RECEIVER:-192.168.1.64}"
# The port letter the receiver offers; the dial is <Port>@<host>.
RPORT="${RPORT:-A}"
set -u
export SSH_AUTH_SOCK=/run/user/1000/keyring/ssh
P="${1:?protocol}"; NAME="${2:-peerrecv}"
R() { timeout 300 ssh -o BatchMode=yes ricky@"$1" "cd ~/peer-vice && DISPLAY=:0 python3 $2" 2>&1; }
OUT="$(mktemp -d)"

echo "--- reset both, arm the answering side"
R $RECEIVER "reset.py arm" | tail -3
R $SENDER   "reset.py"     | tail -2
echo "--- dial"
R $SENDER   "peer.py dial \"$RPORT@$RECEIVER\"" | tail -2
echo "--- select $P on both"
( R $RECEIVER "xfer.py proto $P" > $OUT/pm 2>&1 ) &
( R $SENDER   "xfer.py proto $P" > $OUT/ps 2>&1 ) &
wait
# **Arm, start, then LEAVE THE MONITOR ALONE.**  Reading a screen goes through
# VICE's remote monitor, and entering the monitor *pauses the emulated machine*.
# These two steps used to run in parallel with a 10 s offset, so the receiver
# was still being read when the sender began -- a stopped C64 on the far end of
# a handshake.  Measured 2026-09-19 on the two-hop path: with the overlap,
# Punter moved 179 bytes of a 1775-byte payload and stalled with 50-73 s
# silences on both wires; run sequentially, with nothing touching the monitor
# afterwards, the same path moved 1938 bytes and graded byte-identical.
#
# XMODEM survived the overlap and Punter did not, which is why this looked
# like a protocol or a relay defect for an afternoon.  It was the instrument:
# a screen read is not a passive observation here, it is a stop.
echo "--- receiver armed (to completion), then sender (to completion)"
R $RECEIVER "xfer.py recv $P $NAME" > $OUT/r 2>&1
R $SENDER   "xfer.py send $P puntest" > $OUT/s 2>&1
# The transfer runs with no monitor access at all.  Sized for the slowest cell
# measured (2400 baud, ~1.8 KB payload, ~60 s) with room over.
echo "--- quiet window: not touching VICE for ${QUIET:-180}s"
sleep "${QUIET:-180}"
echo "=== RECEIVER ==="; tail -9 $OUT/r
echo "=== SENDER ==="; tail -9 $OUT/s
