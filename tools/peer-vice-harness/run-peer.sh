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
echo "--- receiver, then sender 10s later"
( R $RECEIVER "xfer.py recv $P $NAME" > $OUT/r 2>&1 ) & RP=$!
sleep 10
( R $SENDER   "xfer.py send $P puntest" > $OUT/s 2>&1 ) & SP=$!
wait $RP $SP
echo "=== RECEIVER ==="; tail -9 $OUT/r
echo "=== SENDER ==="; tail -9 $OUT/s
