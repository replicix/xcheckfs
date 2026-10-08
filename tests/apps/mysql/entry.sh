#!/usr/bin/env bash
# Entry point wrapper for the mysql:8.4 image: mounts xcheckfs over /var/lib/mysql (PRIMARY=/xc/p, SECONDARY=/xc/s) and
# then runs the image's original docker-entrypoint.sh with the given arguments (mysqld ...).
#
# Environment:
#   XC_CHECK       none (default: no xcheckfs, plain pass-through) | basic | thorough | paranoid
#   XC_MODE        --on-mismatch mode (default log)
#   XC_QUARANTINE  directory (inside the container) for --quarantine, optional
#   XC_EXTRA       extra `xcheckfs mount` arguments, word-split, optional
#   XC_BIN         xcheckfs binary (default /x/xcheckfs)
#
# Unlike a plain `exec`, this script stays PID 1 so that it can stop xcheckfs cleanly (SIGTERM = unmount + summary)
# after mysqld has exited, and collect the final `ctl` output in /run (read with `docker cp` after the container exits).
set -u

if [ "${XC_CHECK:-none}" = none ]; then
    exec docker-entrypoint.sh "$@"
fi

bin=${XC_BIN:-/x/xcheckfs}
mnt=/var/lib/mysql
# xcheckfs chmods the parent directory of --control-socket to 0700 if it creates it; use a dedicated directory anyway
sock=/run/xc/ctl.sock

args=(mount --check "$XC_CHECK" --on-mismatch "${XC_MODE:-log}" --control-socket "$sock" --log-file /run/xc.log)
[ -n "${XC_QUARANTINE:-}" ] && args+=(--quarantine "$XC_QUARANTINE")
if [ -n "${XC_EXTRA:-}" ]; then
    read -ra extra <<<"$XC_EXTRA"
    args+=("${extra[@]}")
fi

ulimit -n 1048576 2>/dev/null || true
"$bin" "${args[@]}" "$mnt" /xc/p /xc/s >/run/xc.stdout 2>&1 &
xcpid=$!

up=0
for _ in $(seq 1 300); do
    if grep -q ' - fuse xcheckfs:' /proc/self/mountinfo; then up=1; break; fi
    kill -0 "$xcpid" 2>/dev/null || break
    sleep 0.1
done
if [ "$up" != 1 ]; then
    echo "entry.sh: xcheckfs did not mount" >&2
    cat /run/xc.stdout /run/xc.log >&2 2>/dev/null
    exit 1
fi
echo "entry.sh: xcheckfs mounted over $mnt (check=$XC_CHECK mode=${XC_MODE:-log} pid=$xcpid)"

docker-entrypoint.sh "$@" &
child=$!
trap 'kill -TERM "$child" 2>/dev/null' TERM INT
wait "$child"
rc=$?
while kill -0 "$child" 2>/dev/null; do
    wait "$child"
    rc=$?
done
trap - TERM INT

"$bin" ctl --socket "$sock" status >/run/xc-final-status.json 2>&1
"$bin" ctl --socket "$sock" mismatches 1000 >/run/xc-final-mismatches.json 2>&1
"$bin" ctl --socket "$sock" stats >/run/xc-final-stats.json 2>&1
kill -TERM "$xcpid" 2>/dev/null
wait "$xcpid"
echo "$?" >/run/xc.exit
echo "entry.sh: mysqld exited $rc, xcheckfs exited $(cat /run/xc.exit)"
exit "$rc"
