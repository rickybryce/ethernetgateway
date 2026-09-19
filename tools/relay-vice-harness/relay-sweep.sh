#!/usr/bin/env bash
# Master/slave transfer sweep, driven from the dev box.
#
#   LINK=serial|telnet ./relay-sweep.sh <proto>:<dir> ...
#
# The device is a real C64 (NovaTerm under VICE) on the SLAVE; the menu it
# drives and the files it moves belong to the MASTER.  So every step is on the
# machine that owns it: the master's transfer directory is cleaned and seeded
# on the master, the emulator is driven on the slave, and the bytes are
# compared HERE against the payload.  Both addresses come from the
# environment -- see below.
#
# A screen that says "complete" is not a result.  Every run ends in a byte
# comparison, and a run with no output file is a failure however cheerful the
# screen was.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"; cd "$HERE"
export SSH_AUTH_SOCK=/run/user/1000/keyring/ssh

# **The rig moves, so these are inputs and not constants.**  They were baked
# in as 141/178, and by the next run those were a machine that had been
# repurposed and one that had changed address -- a sweep that cannot be
# pointed somewhere else is one edit away from grading the wrong box, or from
# grading nothing and saying so cheerfully.  The master paths moved too: the
# data directory used to sit under `target/release/` and now hangs off
# whatever directory the gateway was launched from (0.9.4).  Override any of
# them from the environment.
SLAVE="${SLAVE:-192.168.1.64}"
MASTER="${MASTER:-192.168.1.126}"
MDATA="${MDATA:-/home/ricky/ethernetgateway-data}"
MT="${MT:-$MDATA/transfer}"
MSEED="${MSEED:-/home/ricky/relay-payloads}"
TOOLS="${TOOLS:-/home/ricky/xmodem/tools/punter-vice-harness}"
LINK="${LINK:-serial}"
OUT="${OUT:-$HERE/results/$LINK}"
mkdir -p "$OUT"

# The link belongs in the path: results named only <proto>-<dir> let a serial
# sweep silently overwrite the telnet evidence for the same paths -- disks,
# screens and traces all replaced, with nothing reporting it.  Evidence a later
# run can quietly destroy is not evidence.

seed_master() {
    # Clear the slate; do not pattern-match it.  The gateway saves an upload
    # under the SENDER's own name when the protocol carries one, so a rule that
    # deleted "*up.seq" left files behind that the next run then graded as its
    # own.  Everything removed here is re-seeded immediately below or placed by
    # the gateway itself on first launch.
    ssh $MASTER "find $MT -maxdepth 1 -type f ! -name 'EGT8080.COM' ! -name 'EGT80.COM' -delete; cp -f $MSEED/* $MT/"
}

# Where the master's log stood before this run, so the archive can be an EXACT
# slice of it rather than a guessed tail.  `tail -400` nearly lost the only
# trace that mattered: the first xmodem download's negotiation line was still
# inside the window, but a busier run would have pushed it out and the evidence
# would have been gone with nothing saying so.  A marker cannot overflow.
MLOG="${MLOG:-$MDATA/ethernetgateway.log}"
mark_master() { ssh $MASTER "wc -l < $MLOG" 2>/dev/null | tr -d ' \r'; }

archive() { # proto dir from-line
    local proto="$1" dir="$2" from="${3:-0}"
    # The slave's stdout tee, and -- more importantly -- its OWN rotating log,
    # which is the one that survives a restart on its own.  relay-one.sh
    # restarts the slave every run, so the tee'd copy holds only this run.
    scp -q $SLAVE:/home/ricky/relay-vice/slave.log "$OUT/$proto-$dir.slave.log" 2>/dev/null
    scp -q $SLAVE:/home/ricky/relay-vice/run/ethernetgateway-data/ethernetgateway.log \
        "$OUT/$proto-$dir.slave-own.log" 2>/dev/null
    ssh $MASTER "sed -n '$((from + 1)),\$p' $MLOG" \
        > "$OUT/$proto-$dir.master.log" 2>/dev/null
    return 0
}

verify() { # proto dir
    local proto="$1" dir="$2"
    if [ "$dir" = download ]; then
        # Read the image the C64 wrote.  freshdisk.py detached it before the
        # run and attached it after rebuilding, so the only unclosed (splat)
        # entry on it can be this run's download.
        scp -q $SLAVE:/home/ricky/relay-vice/run/xfer.d64 "$OUT/$proto-$dir.d64" || return 1
        python3 "$TOOLS/d64read.py" "$OUT/$proto-$dir.d64" > "$OUT/$proto-$dir.dir" 2>&1
        # **Is this the disk the run was given?**  freshdisk.py writes a marker
        # naming this swap; if the emulator was holding a different disk and
        # wrote its own view back, the marker is what goes missing.  The old
        # check read the image with c1541 -- the wrong SIDE of the swap, which
        # is why a unit number that meant device SIXTEEN went unnoticed for
        # weeks while every "fresh" disk was the previous run's.
        mark="$(grep -oE "marker run[0-9]{5}" "$OUT/$proto-$dir.screen" 2>/dev/null \
                | tail -1 | awk "{print \$2}")"
        if [ -n "$mark" ] && ! grep -qi "$mark" "$OUT/$proto-$dir.dir"; then
            echo "    the disk graded is NOT the disk we built ($mark missing)"
            return 1
        fi
        python3 "$TOOLS/verify-run.py" "$TOOLS/payloads/PUNTEST.SEQ" "$OUT/$proto-$dir.d64"
        return $?
    else
        # Identify the upload by PROVENANCE, never by name: the gateway saves
        # the first file of a batch under the sender's own name, so a ZMODEM or
        # YMODEM upload lands as whatever NovaTerm called it and not as the name
        # typed at the Filename prompt.
        rm -rf "$OUT/.mt-$proto-$dir"; mkdir -p "$OUT/.mt-$proto-$dir"
        scp -q "$MASTER:$MT/*" "$OUT/.mt-$proto-$dir/" 2>/dev/null
        python3 "$TOOLS/verify-upload.py" "$TOOLS/payloads/PUNTEST.SEQ" \
            "$OUT/.mt-$proto-$dir" "$TOOLS/payloads" "$OUT/$proto-$dir."
        return $?
    fi
}

rc=0
for spec in "$@"; do
    proto="${spec%%:*}"; dir="${spec##*:}"
    echo "=============== $proto $dir over $LINK  ($(date +%H:%M:%S))"
    seed_master
    mstart="$(mark_master)"; mstart="${mstart:-0}"
    # A run that did not happen must not be graded: relay-one.sh can abort
    # before it starts anything, and the master's transfer directory and the
    # C64's disk are still sitting there from the run before.  Grading those
    # reads the PREVIOUS run's result as this one's.
    # MASTER_TG travels with the rest of the rig: the telnet leg reaches the
    # master through the slave's Telnet Gateway, and a default baked into
    # relay-one.sh would send the hop to whatever machine held that address
    # last -- a cell that then grades the SLAVE's own files as the master's.
    if ! ssh $SLAVE "cd /home/ricky/relay-vice && MASTER_TG='${MASTER_TG:-$MASTER:2323}' ./relay-one.sh $proto $dir $LINK" \
            > "$OUT/$proto-$dir.screen" 2>&1; then
        tail -20 "$OUT/$proto-$dir.screen"
        archive "$proto" "$dir" "$mstart"
        echo "--- bytes:"
        echo "    FAIL - the run itself did not complete; nothing was graded"
        rc=1
        continue
    fi
    tail -20 "$OUT/$proto-$dir.screen"
    archive "$proto" "$dir" "$mstart"
    echo "--- bytes:"
    if ! verify "$proto" "$dir"; then
        echo "    FAIL"
        rc=1
    fi
done
exit "$rc"
