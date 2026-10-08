#!/usr/bin/env bash
# Multi-process SQLite stress through xcheckfs, in Docker (see README.md).
#
#   run.sh [-c CONFIGS] [-s SCENARIOS] [-m MODES] [-d SECS] [-n PROCS] [-o OUTDIR] [-x BINDIR] [ci]
#
#   -c  comma list of baseline,basic,thorough[,paranoid], each also as <mode>-strict (= --serialize strict)
#                                                                  (default: baseline,basic,thorough)
#   -s  comma list of normal,multidb,kill,signal         (default: all)
#   -m  comma list of wal,delete,truncate,persist,wal-mmap,wal-full   (default: all valid for the scenario)
#   -d  seconds of load per run                          (default: 120)
#   -n  worker processes                                 (default: 12)
#   -o  output directory for logs and JSON               (default: $TMPDIR/xc-sqlite-out)
#   -x  static xcheckfs binary, or a directory containing it    (default: $XCHECKFS, then $XCHECKFS_DIR)
#   -i  container image with python3 (stdlib sqlite3)    (default: python:3-alpine)
#   -C  CPUs for the container                           (default: 8)
#   -M  extra arguments for `xcheckfs mount`, e.g. "--attr-timeout 0 --entry-timeout 0"
#   -K  keep containers and volumes
#
# "ci" (CI mode, see README.md): short run of a representative scenario/mode subset for each config, results in
# OUT/results.json (schema in tests/apps/README.md).  Configured by environment (flags override):
#   XCHECKFS    path of the static xcheckfs binary          OUT       output directory
#   CONFIGS     space/comma list (default "baseline basic thorough")
#   SCENARIOS   space list of scenario/mode (default "normal/wal normal/delete normal/wal-mmap multidb/wal kill/wal signal/wal")
#   DURATION    seconds of load per scenario (default 25)       PROCS  worker processes (default 12)
#
# Every run gets its own container (xc-sqlite-<id>-...) and its own two volumes (primary, secondary).  With
# "baseline" the workload runs on the primary volume directly; otherwise xcheckfs mounts it with -m log, the
# workload runs in the mount, and afterwards the mount is unmounted, `xcheckfs verify` is run on the two trees and
# both trees are checked directly (integrity_check, balance sum, history, replay) without xcheckfs.
#
# Exit status: 0 all runs clean, 1 some run had problems (see summary.tsv), 2 environment problem.  In ci mode
# problems of any kind (also environment ones) are recorded in results.json and the exit status is 0 or 1.
set -u -o pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
configs=baseline,basic,thorough
scenarios=normal,multidb,kill,signal
modes=
duration=
procs=
out=
bin=${XCHECKFS:-${XCHECKFS_DIR:-}}
image=python:3-alpine
cpus=
keep=0
mount_args=

while getopts "c:s:m:d:n:o:x:i:C:M:Kh" opt; do
    case $opt in
        c) configs=$OPTARG; configs_set=1 ;;
        s) scenarios=$OPTARG ;;
        m) modes=$OPTARG ;;
        d) duration=$OPTARG ;;
        n) procs=$OPTARG ;;
        o) out=$OPTARG ;;
        x) bin=$OPTARG ;;
        i) image=$OPTARG ;;
        C) cpus=$OPTARG ;;
        M) mount_args=$OPTARG ;;
        K) keep=1 ;;
        *) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
ci=0
case ${1:-} in
    ci) ci=1; shift ;;
    '') ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

if [ "$ci" = 1 ]; then
    [ -n "${configs_set:-}" ] || configs=${CONFIGS:-baseline basic thorough}
    ci_scenarios=${SCENARIOS:-normal/wal normal/delete normal/wal-mmap multidb/wal kill/wal signal/wal}
    ci_scenarios=${ci_scenarios//,/ }
    : "${duration:=${DURATION:-25}}"
    : "${procs:=${PROCS:-12}}"
    : "${out:=${OUT:-}}"
    [ -n "$out" ] || out=${TMPDIR:-/tmp}/xc-sqlite-out
    # the container gets at most 4 CPUs: that is what a standard GitHub runner has
    : "${cpus:=$(n=$(nproc 2>/dev/null || echo 4); [ "$n" -gt 4 ] && n=4; echo "$n")}"
    grace=60
    run_timeout=$((duration + 400))
else
    : "${duration:=120}"
    : "${procs:=12}"
    : "${out:=${OUT:-${TMPDIR:-/tmp}/xc-sqlite-out}}"
    : "${cpus:=8}"
    grace=90
    run_timeout=$((duration + 900))
fi
configs=${configs//,/ }

results_written=0
# ci: write results.json from whatever is in $out (also after a fatal error)
ci_finish() {
    local rc=0
    python3 "$here/ci_results.py" --out "$out" --configs "$configs" --scenarios "$ci_scenarios" \
        --duration "$duration" --procs "$procs" --image "$image" --cpus "$cpus" \
        --mount-args "$mount_args" >"$out/ci_results.log" 2>&1 || rc=$?
    cat "$out/ci_results.log"
    results_written=1
    return $rc
}
die() {
    echo "ERROR: $*" >&2
    if [ "$ci" = 1 ]; then
        mkdir -p "$out" 2>/dev/null && echo "$*" >"$out/fatal.txt"
        [ "$results_written" = 1 ] || ci_finish
        exit 1
    fi
    exit 2
}
if [ -d "$bin" ]; then bin=$bin/xcheckfs; fi
[ -n "$bin" ] && [ -x "$bin" ] && [ -f "$bin" ] || die "need an executable xcheckfs binary: XCHECKFS=FILE (or -x FILE|DIR, or XCHECKFS_DIR)"
command -v docker >/dev/null || die "docker not found"
mkdir -p "$out" || die "cannot create $out"
out=$(cd "$out" && pwd)
[ "$ci" = 1 ] && rm -f "$out/fatal.txt" "$out/results.json" "$out/summary.tsv"
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
id=$(date +%H%M%S)-$$
summary=$out/summary.tsv
[ -s "$summary" ] || printf 'config\tscenario\tmode\tcommits_s\treads_s\tp50_ms\tp99_ms\tbusy\tviolations\tmismatches\tmax_lock_waiters\tops\tstuck\tverify_rc\tsec_ok\tprimary_ok\tdigest_eq\tok\n' >"$summary"

declare -a cleanup_names=()
# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
    [ "$ci" = 1 ] && [ "$results_written" = 0 ] && ci_finish
    [ "$keep" = 1 ] && return
    local n
    for n in "${cleanup_names[@]}"; do
        docker rm -f "$n" >/dev/null 2>&1
        docker volume rm "$n-p" "$n-s" >/dev/null 2>&1
    done
}
trap cleanup EXIT
[ "$ci" = 1 ] && trap 'exit 143' TERM INT

# valid modes per scenario
modes_for() {
    local sc=$1
    if [ -n "$modes" ]; then echo "${modes//,/ }"; return; fi
    case $sc in
        normal) echo "wal delete truncate persist wal-mmap wal-full" ;;
        *) echo "wal delete" ;;
    esac
}

dx() { docker exec "$name" "$@"; }

# fatal_rc RDIR MESSAGE: record that a scenario could not be run (ci_results.py turns it into a failed run)
fatal_rc() {
    echo "FATAL: $2" >&2
    printf '%s\n' "$2" >"$1/fatal.txt"
}

run_one() {
    local cfg=$1 sc=$2 mode=$3
    local tag="$cfg-$sc-$mode" rdir t0=$SECONDS
    local base=${cfg%-strict} serialize=''
    [ "$base" = "$cfg" ] || serialize='--serialize strict'
    name="xc-sqlite-$id-$cfg-$sc-$mode"
    rdir=$out/$tag
    rm -rf "$rdir"; mkdir -p "$rdir"
    cleanup_names+=("$name")
    echo "=== $tag ($(date +%T)) ==="

    if ! { docker volume create "$name-p" >/dev/null && docker volume create "$name-s" >/dev/null; }; then
        fatal_rc "$rdir" "docker volume create failed"
        return 2
    fi
    docker run -d --name "$name" --cpus "$cpus" --memory 6g \
        --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
        -v "$bin:/x/xcheckfs:ro" -v "$here:/w:ro" -v "$rdir:/out" \
        -v "$name-p:/data/p" -v "$name-s:/data/s" \
        "$image" sleep 86400 >"$rdir/docker-run.txt" 2>&1 || { fatal_rc "$rdir" "docker run failed: $(tail -n 3 "$rdir/docker-run.txt")"; return 2; }

    local work=/data/p/db status_cmd='' dbs=1 hold=0
    if [ "$cfg" != baseline ]; then
        work=/mnt/xc/db
        status_cmd='/x/xcheckfs ctl --socket /run/xc.sock status'
        dx mkdir -p /mnt/xc /run
        dx /x/xcheckfs verify /data/p /data/s >"$rdir/verify-pre.txt" 2>&1 || echo "pre-verify rc=$?"
        # shellcheck disable=SC2086  # $mount_args and $serialize are word lists on purpose
        dx /x/xcheckfs mount -b --check "$base" -m log --threads 8 $serialize $mount_args --control-socket /run/xc.sock \
            --log-file /run/xc.log --pid-file /run/xc.pid /mnt/xc /data/p /data/s >"$rdir/mount.txt" 2>&1 \
            || { echo "mount failed:"; cat "$rdir/mount.txt"; fatal_rc "$rdir" "mount failed: $(tail -n 3 "$rdir/mount.txt")"; return 2; }
    fi
    case $sc in
        multidb) dbs=5 ;;
        kill | signal) hold=10 ;;
        *) ;;
    esac

    timeout "$run_timeout" docker exec "$name" python3 /w/workload.py run \
        --dir "$work" --mode "$mode" --scenario "$sc" --procs "$procs" --dbs "$dbs" --duration "$duration" \
        --grace "$grace" --hold-ms "$hold" --label "$tag" --status-cmd "$status_cmd" --out /out/result.json \
        >"$rdir/workload.log" 2>&1
    local wrc=$?
    tail -n 12 "$rdir/workload.log" | cut -c1-400
    dx sh -c 'cp /dev/shm/xcw/*.log /out/ 2>/dev/null; tar -C /dev/shm/xcw -cf /out/worker-state.tar . 2>/dev/null; true'

    local vrc='-' sec_ok='-' pri_ok='-' digest_eq='-' mm='-'
    if [ "$cfg" != baseline ]; then
        dx /x/xcheckfs ctl --socket /run/xc.sock status >"$rdir/status-final.json" 2>&1
        dx /x/xcheckfs ctl --socket /run/xc.sock stats >"$rdir/stats-final.json" 2>&1
        dx /x/xcheckfs ctl --socket /run/xc.sock mismatches 200 >"$rdir/mismatches.json" 2>&1
        # shellcheck disable=SC2016  # expanded inside the container
        dx sh -c 'p=$(cat /run/xc.pid); grep -E "VmHWM|VmRSS|Threads" /proc/$p/status; awk "{print \"utime_ticks=\" \$14 \" stime_ticks=\" \$15}" /proc/$p/stat' \
            >"$rdir/xc-proc.txt" 2>&1
        dx umount /mnt/xc 2>"$rdir/umount.txt" || { sleep 3; dx umount -l /mnt/xc 2>>"$rdir/umount.txt"; }
        for _ in $(seq 1 40); do dx test -e /run/xc.pid || break; sleep 0.5; done
        dx test -e /run/xc.pid && echo "WARNING: xcheckfs daemon still running after unmount"
        docker cp "$name:/run/xc.log" "$rdir/xc.log" 2>/dev/null
        dx /x/xcheckfs verify /data/p /data/s >"$rdir/verify.txt" 2>&1
        vrc=$?
        dx python3 /w/workload.py check /data/p/db/bank*.db --json /out/check-primary.json >"$rdir/check-primary.txt" 2>&1
        pri_ok=$?
        dx python3 /w/workload.py check /data/s/db/bank*.db --json /out/check-secondary.json >"$rdir/check-secondary.txt" 2>&1
        sec_ok=$?
        # logical content of every database must be identical on both sides
        if [ "$(grep -o 'digest=[0-9a-f]*' "$rdir/check-primary.txt")" = "$(grep -o 'digest=[0-9a-f]*' "$rdir/check-secondary.txt")" ]; then
            digest_eq=1
        else
            digest_eq=0
        fi
        mm=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["mismatches"])' "$rdir/status-final.json" 2>/dev/null || echo '?')
        echo "verify rc=$vrc primary-check rc=$pri_ok secondary-check rc=$sec_ok digests-equal=$digest_eq mismatches=$mm"
        grep -c MISMATCH "$rdir/xc.log" 2>/dev/null | sed 's/^/MISMATCH log lines: /'
    else
        dx python3 /w/workload.py check "$work"/bank*.db >"$rdir/check-primary.txt" 2>&1
        pri_ok=$?
    fi
    docker exec "$name" chown -R "$(id -u):$(id -g)" /out >/dev/null 2>&1

    python3 - "$rdir/result.json" "$summary" "$cfg" "$sc" "$mode" "$vrc" "$sec_ok" "$pri_ok" "$digest_eq" "$wrc" "$((SECONDS - t0))" <<'PYEOF'
import json, sys
res, summ, cfg, sc, mode, vrc, sec, pri, deq, wrc, wall = sys.argv[1:]
try:
    r = json.load(open(res))
except Exception:
    r = {}
x = r.get("xcheckfs", {})
f = x.get("final", {})
ok = (r.get("ok") is True and wrc == "0" and vrc in ("-", "0") and sec in ("-", "0") and pri == "0"
      and deq in ("-", "1") and (f.get("mismatches") in (None, 0)))
row = [cfg, sc, mode, r.get("commits_per_s"), r.get("reads_per_s"), r.get("lat_p50_ms"), r.get("lat_p99_ms"),
       r.get("busy_errors"), r.get("violations"), f.get("mismatches"), x.get("max_lock_waiters"), f.get("ops"),
       len(r.get("stuck", [])), vrc, sec, pri, deq, "OK" if ok else "FAIL(wrc=%s)" % wrc]
with open(res.replace("result.json", "rc.json"), "w") as fh:
    json.dump(dict(workload_rc=int(wrc), verify_rc=vrc, secondary_rc=sec, primary_rc=pri, digest_eq=deq,
                   wall_s=int(wall), ok=bool(ok)), fh)
with open(summ, "a") as fh:
    fh.write("\t".join("" if v is None else str(v) for v in row) + "\n")
print("SUMMARY", "\t".join("" if v is None else str(v) for v in row))
sys.exit(0 if ok else 1)
PYEOF
    local rc=$?
    if [ "$keep" != 1 ]; then
        docker rm -f "$name" >/dev/null 2>&1
        docker volume rm "$name-p" "$name-s" >/dev/null 2>&1
    fi
    return $rc
}

bad=0
if [ "$ci" = 1 ]; then
    docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image" >/dev/null 2>&1 \
        || echo "WARNING: cannot pull $image"
    for cfg in $configs; do
        for entry in $ci_scenarios; do
            run_one "$cfg" "${entry%%/*}" "${entry#*/}" || bad=1
        done
    done
    ci_finish || bad=1
    echo "results: $out/results.json"
    exit $bad
fi
for sc in ${scenarios//,/ }; do
    for mode in $(modes_for "$sc"); do
        for cfg in $configs; do
            run_one "$cfg" "$sc" "$mode"
            rc=$?
            [ "$rc" = 2 ] && die "environment problem in $cfg-$sc-$mode"
            [ "$rc" = 0 ] || bad=1
        done
    done
done
echo "summary: $summary"
exit $bad
