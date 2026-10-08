#!/usr/bin/env bash
# Container entrypoint for the xcheckfs PostgreSQL test (see README.md).
#
# XC_MODE=baseline  PGDATA's parent (/var/lib/postgresql) is a plain volume.
# XC_MODE=basic|thorough|paranoid
#                   xcheckfs is mounted over /var/lib/postgresql (PRIMARY=/xc/p,
#                   SECONDARY=/xc/s) BEFORE the official entrypoint runs, so
#                   initdb itself goes through xcheckfs.
#
# Unlike a plain `exec`, this script stays PID 1 so that `docker stop`
# (SIGTERM) shuts postgres down with a fast shutdown, collects the final
# xcheckfs status and unmounts cleanly. `docker kill -s KILL` skips all that.
set -euo pipefail

MNT=/var/lib/postgresql
XC_MODE=${XC_MODE:-baseline}
XC_ON_MISMATCH=${XC_ON_MISMATCH:-log}
# Own directory for the socket (xcheckfs chmods the parent to 0700 when it creates it).
XC_SOCK=/run/xc/xc.sock
XC_LOG=${XC_LOG:-/out/xc.log}
OUT=/out

xc() { /x/xcheckfs "$@"; }

if [[ $XC_MODE != baseline ]]; then
    # Same ownership on both sides, as for a fresh docker volume at PGDATA's parent.
    chown postgres:postgres /xc/p /xc/s
    extra=()
    if [[ -n ${XC_EXTRA:-} ]]; then
        read -r -a extra <<<"$XC_EXTRA"
    fi
    xc mount -b --check "$XC_MODE" -m "$XC_ON_MISMATCH" \
        --control-socket "$XC_SOCK" --log-file "$XC_LOG" \
        "${extra[@]}" "$MNT" /xc/p /xc/s
    echo "entry: xcheckfs mounted at $MNT (check=$XC_MODE, on-mismatch=$XC_ON_MISMATCH)"
fi

docker-entrypoint.sh "$@" &
pg=$!
trap 'kill -INT "$pg" 2>/dev/null || true' TERM INT

rc=0
wait "$pg" || rc=$?
# A trapped signal interrupts `wait`; wait again until postgres is really gone.
while kill -0 "$pg" 2>/dev/null; do
    rc=0
    wait "$pg" || rc=$?
done
echo "entry: postgres exited with status $rc"

if [[ $XC_MODE != baseline ]]; then
    xc ctl --socket "$XC_SOCK" status >"$OUT/xc-status-final.json" || true
    xc ctl --socket "$XC_SOCK" stats >"$OUT/xc-stats-final.json" || true
    xc ctl --socket "$XC_SOCK" mismatches 1000 >"$OUT/xc-mismatches-final.json" || true
    for _ in $(seq 1 30); do
        umount "$MNT" 2>/dev/null && break
        sleep 1
    done
    # The daemon exits after the unmount.
    for _ in $(seq 1 30); do
        pgrep -x xcheckfs >/dev/null || break
        sleep 1
    done
    if mountpoint -q "$MNT"; then
        echo "entry: WARNING $MNT is still mounted" >&2
    else
        echo "entry: xcheckfs unmounted"
    fi
fi
chmod -R a+rwX "$OUT" 2>/dev/null || true
exit "$rc"
