#!/usr/bin/env bash
# Drive PostgreSQL under pgbench through xcheckfs (see README.md).
#
#   run.sh bench baseline|basic|thorough|paranoid   one configuration, end to end
#   run.sh all                                      baseline, basic and thorough
#   run.sh crash                                    SIGKILL mid-run, verify, resync recovery
#   run.sh ci                                       short run of every config in CONFIGS, writes
#                                                   $OUT/results.json (see tests/apps/README.md)
#   run.sh clean                                    remove every xc-pg-* container/volume
#
# Environment: XCHECKFS (static binary), IMAGE, SCALE, CLIENTS, JOBS, DURATION,
# OUT (results dir), KEEP=1 (keep volumes after a bench), CRASH_SCALE,
# CRASH_AFTER (seconds into the run to kill), CRASH_ROUNDS (extra kill/recover
# rounds, each killed after a random 5-35 s), XC_EXTRA (extra xcheckfs mount flags).
# ci mode: CONFIGS (default "baseline basic thorough"; also paranoid and any
# <mode>-strict = that mode with --serialize strict). SCALE, CLIENTS, JOBS and
# DURATION default to smaller values (10, 8, 4, 40) than the long scenarios so
# that the three default configurations fit in ~12 minutes on a 4 vCPU runner.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
IMAGE=${IMAGE:-postgres:18}
if [[ ${1:-} == ci ]]; then
    d_scale=10 d_clients=8 d_duration=40
else
    d_scale=50 d_clients=16 d_duration=180
fi
SCALE=${SCALE:-$d_scale}
CLIENTS=${CLIENTS:-$d_clients}
JOBS=${JOBS:-4}
DURATION=${DURATION:-$d_duration}
CRASH_SCALE=${CRASH_SCALE:-20}
CRASH_AFTER=${CRASH_AFTER:-40}
CRASH_ROUNDS=${CRASH_ROUNDS:-1}
KEEP=${KEEP:-0}
OUT=${OUT:-$HERE/out}
XCHECKFS=${XCHECKFS:-}
PGDATA_DIR=/var/lib/postgresql/18/docker
CI=0 # set by `run.sh ci`: run_pgbench then records each phase for results.json
declare -A VALRC=() # validate_vol results per check, for ci

if [[ -z $XCHECKFS ]]; then
    for c in "$ROOT/target/release/xcheckfs" "$(command -v xcheckfs || true)"; do
        if [[ -n $c && -x $c ]]; then XCHECKFS=$c; break; fi
    done
fi

PG_ARGS=(
    -c shared_buffers=128MB
    -c checkpoint_timeout=30s
    -c max_wal_size=256MB
    -c full_page_writes=on
    -c fsync=on
    -c synchronous_commit=on
    -c wal_recycle=on
    -c autovacuum_naptime=5s
    -c log_checkpoints=on
)

log() { printf '%s %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$OUT/summary.txt"; }
die() {
    echo "run.sh: $*" >&2
    if [[ -n ${CI_FATAL:-} ]]; then echo "$*" >>"$CI_FATAL"; fi
    exit 1
}

# ---- containers -----------------------------------------------------------

# start_pg TAG MODE RUNDIR [ON_MISMATCH] [EXTRA]
# MODE: baseline | basic | thorough | paranoid. Volumes xc-pg-TAG-p / -s.
start_pg() {
    local tag=$1 mode=$2 rundir=$3 onmm=${4:-log} extra=${5:-}
    local cn=xc-pg-$tag
    mkdir -p "$rundir"
    local args=(--name "$cn" --label xc-pg=1 --shm-size 512m
        -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_INITDB_ARGS=--data-checksums
        -e "XC_MODE=$mode" -e "XC_ON_MISMATCH=$onmm" -e "XC_EXTRA=$extra"
        -v "$HERE/entry.sh:/entry.sh:ro" -v "$HERE/work:/work:ro" -v "$rundir:/out"
        --entrypoint /entry.sh)
    if [[ $mode == baseline ]]; then
        args+=(-v "$cn-p:/var/lib/postgresql")
    else
        [[ -x $XCHECKFS ]] || die "set XCHECKFS to the xcheckfs binary"
        args+=(--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined
            -v "$XCHECKFS:/x/xcheckfs:ro" -v "$cn-p:/xc/p" -v "$cn-s:/xc/s")
    fi
    docker run -d "${args[@]}" "$IMAGE" postgres "${PG_ARGS[@]}" >/dev/null
}

wait_ready() { # CONTAINER
    for _ in $(seq 1 900); do
        if docker exec "$1" pg_isready -q -h 127.0.0.1 -U postgres 2>/dev/null; then return 0; fi
        [[ $(docker inspect -f '{{.State.Running}}' "$1") == true ]] || return 1
        sleep 1
    done
    return 1
}

stop_pg() { # CONTAINER  (docker stop -> SIGTERM -> fast shutdown + unmount)
    docker stop -t 600 "$1" >/dev/null
}

px() { local cn=$1; shift; docker exec -u postgres "$cn" "$@"; }

xcctl() { local cn=$1; shift; docker exec "$cn" /x/xcheckfs ctl --socket /run/xc/xc.sock "$@"; }

# xc_snapshot CONTAINER FILE-PREFIX LABEL  (no-op for baseline)
xc_snapshot() {
    local cn=$1 pfx=$2 label=$3
    xcctl "$cn" status >"$pfx-status-$label.json" 2>/dev/null || return 0
    xcctl "$cn" mismatches 200 >"$pfx-mismatches-$label.json" 2>/dev/null || true
    log "  xcheckfs[$label]: $(jq -c '{state,mode,ops,mismatches,allowed,repeats,secondary_skipped,verifications,resyncs,resync_failures,bytes_read,bytes_written,nodes,open_files}' "$pfx-status-$label.json")"
}

# ---- workloads ------------------------------------------------------------

# run_pgbench CONTAINER RUNDIR LABEL ARGS... ; prints the summary lines.
run_pgbench() {
    local cn=$1 rundir=$2 label=$3 dur=${PGB_DURATION:-$DURATION}
    shift 3
    log "pgbench[$label]: -c $CLIENTS -j $JOBS -T $dur $*"
    px "$cn" rm -f /tmp/pgb-"$label".* 2>/dev/null || true
    PGB_RC=0
    px "$cn" pgbench -U postgres -c "$CLIENTS" -j "$JOBS" -T "$dur" -P 15 -l \
        --log-prefix=/tmp/pgb-"$label" --max-tries=5 "$@" postgres >"$rundir/pgbench-$label.txt" 2>&1 || PGB_RC=$?
    if ((PGB_RC != 0)); then log "pgbench[$label] FAILED (exit $PGB_RC), see pgbench-$label.txt"; fi
    grep -E 'transactions actually|failed transactions|latency average|latency stddev|tps =' "$rundir/pgbench-$label.txt" | sed 's/^/  /' | tee -a "$OUT/summary.txt"
    px "$cn" /work/pctl.sh /tmp/pgb-"$label" | tee "$rundir/pctl-$label.txt" | sed 's/^/  pct: /' | tee -a "$OUT/summary.txt"
    px "$cn" rm -f /tmp/pgb-"$label".* 2>/dev/null || true
    if ((CI)); then ci_record_workload "$rundir" "$label" "$PGB_RC"; fi
}

workload_init() { # CONTAINER RUNDIR SCALE
    local cn=$1 rundir=$2 scale=$3 t0
    t0=$(date +%s)
    px "$cn" pgbench -U postgres -i -s "$scale" -q postgres >"$rundir/pgbench-init.txt" 2>&1
    log "pgbench -i -s $scale: $(($(date +%s) - t0))s"
    t0=$(date +%s)
    px "$cn" psql -X -q -U postgres -d postgres -f /work/setup.sql >"$rundir/setup.txt" 2>&1
    log "setup.sql (200 partitions): $(($(date +%s) - t0))s"
}

workload_runs() { # CONTAINER RUNDIR
    local cn=$1 rundir=$2
    run_pgbench "$cn" "$rundir" tpcb
    xc_snapshot "$cn" "$rundir/xc" after-tpcb
    run_pgbench "$cn" "$rundir" N -N
    xc_snapshot "$cn" "$rundir/xc" after-N
    # churn + periodic maintenance in parallel
    px "$cn" /work/maint.sh $((${PGB_DURATION:-$DURATION} + 5)) >"$rundir/maint.txt" 2>&1 &
    local mpid=$!
    run_pgbench "$cn" "$rundir" churn -f /work/churn.pgbench
    wait "$mpid" || true
    log "maintenance cycles: $(grep -c 'VACUUM FULL pgbench_history' "$rundir/maint.txt"), failures: $(grep -c FAILED "$rundir/maint.txt" || true)"
    xc_snapshot "$cn" "$rundir/xc" after-churn
}

db_state() { # CONTAINER RUNDIR
    px "$1" psql -X -At -U postgres -d postgres \
        -c "SELECT 'checkpoints timed=' || num_timed || ' requested=' || num_requested FROM pg_stat_checkpointer" \
        -c "SELECT 'dbsize=' || pg_size_pretty(pg_database_size('postgres'))" \
        -c "SELECT 'relation files (heap/index/toast)=' || count(*) FROM pg_class WHERE relkind IN ('r','i','t')" >"$2/db-state.txt"
    log "  db: $(tr '\n' ' ' <"$2/db-state.txt")"
}

# ---- offline checks ------------------------------------------------------

# verify_vols TAG RUNDIR : xcheckfs verify on the two volumes; sets VERIFY_RC
verify_vols() {
    local tag=$1 rundir=$2
    VERIFY_RC=0
    docker run --rm -v "$XCHECKFS:/x/xcheckfs:ro" -v "xc-pg-$tag-p:/xc/p:ro" -v "xc-pg-$tag-s:/xc/s:ro" \
        alpine:3.20 /x/xcheckfs verify --max-reports 100 /xc/p /xc/s >"$rundir/verify.txt" 2>&1 || VERIFY_RC=$?
    log "xcheckfs verify: exit $VERIFY_RC; $(tail -n 1 "$rundir/verify.txt")"
    if ((VERIFY_RC != 0)); then head -n 20 "$rundir/verify.txt" | sed 's/^/    /' | tee -a "$OUT/summary.txt"; fi
}

# validate_vol TAG SIDE(p|s) RUNDIR : pg_checksums offline, then a plain postgres (no xcheckfs)
validate_vol() {
    local tag=$1 side=$2 rundir=$3 vol=xc-pg-$1-$2 cn=xc-pg-$1-val-$2
    local rc=0
    VALRC[checksums-$side]=1 VALRC[amcheck-$side]=skipped
    docker run --rm -u postgres -v "$vol:/var/lib/postgresql" "$IMAGE" \
        pg_checksums --check -D "$PGDATA_DIR" >"$rundir/val-$side-checksums.txt" 2>&1 || rc=$?
    VALRC[checksums-$side]=$rc
    log "validate[$side] pg_checksums --check: exit $rc; $(tr '\n' ' ' <"$rundir/val-$side-checksums.txt" | cut -c1-200)"
    docker run -d --name "$cn" --label xc-pg=1 --shm-size 512m -e POSTGRES_HOST_AUTH_METHOD=trust \
        -v "$vol:/var/lib/postgresql" "$IMAGE" >/dev/null
    wait_ready "$cn" || { log "validate[$side]: postgres did not start"; docker logs "$cn" >"$rundir/val-$side-postgres.log" 2>&1; return 1; }
    for db in postgres template1; do
        px "$cn" psql -X -q -U postgres -d "$db" -c "CREATE EXTENSION IF NOT EXISTS amcheck" >/dev/null 2>&1 || true
    done
    rc=0
    px "$cn" pg_amcheck -U postgres --all --heapallindexed -j 4 >"$rundir/val-$side-amcheck.txt" 2>&1 || rc=$?
    VALRC[amcheck-$side]=$rc
    log "validate[$side] pg_amcheck --all --heapallindexed: exit $rc; $(wc -l <"$rundir/val-$side-amcheck.txt") output lines"
    if ((rc != 0)); then head -n 10 "$rundir/val-$side-amcheck.txt" | sed 's/^/    /' | tee -a "$OUT/summary.txt"; fi
    px "$cn" psql -X -At -F' ' -U postgres -d postgres \
        -c "SELECT 'accounts', count(*), sum(abalance) FROM pgbench_accounts" \
        -c "SELECT 'history', count(*), sum(delta) FROM pgbench_history" \
        -c "SELECT 'tellers', count(*), sum(tbalance) FROM pgbench_tellers" \
        -c "SELECT 'branches', count(*), sum(bbalance) FROM pgbench_branches" \
        -c "SELECT 'xc_part', count(*), sum(n) FROM xc_part" \
        -c "SELECT 'xc_churn', count(*), max(id) FROM xc_churn" >"$rundir/val-$side-counts.txt" 2>&1 || true
    docker stop -t 120 "$cn" >/dev/null
    docker logs "$cn" >"$rundir/val-$side-postgres.log" 2>&1 || true
    docker rm -f "$cn" >/dev/null
}

compare_counts() { # RUNDIR ; sets COUNTS_RC
    COUNTS_RC=0
    if diff -u "$1/val-p-counts.txt" "$1/val-s-counts.txt" >"$1/val-counts.diff"; then
        log "validate: row counts / sums identical on PRIMARY and SECONDARY: $(tr '\n' ';' <"$1/val-p-counts.txt")"
    else
        COUNTS_RC=1
        log "validate: row counts / sums DIFFER (see val-counts.diff)"
        sed 's/^/    /' "$1/val-counts.diff" | tee -a "$OUT/summary.txt"
    fi
}

validate_both() { # TAG RUNDIR
    validate_vol "$1" p "$2" || true
    validate_vol "$1" s "$2" || true
    compare_counts "$2"
}

drop_pg() { # TAG
    docker rm -fv "xc-pg-$1" >/dev/null 2>&1 || true
    docker volume rm -f "xc-pg-$1-p" "xc-pg-$1-s" >/dev/null 2>&1 || true
}

# ---- scenarios -------------------------------------------------------------

bench() { # MODE [NAME [EXTRA-MOUNT-FLAGS]]
    local mode=$1 name=${2:-$1} xtra=${3:-} tag cn rundir=$OUT/${2:-$1}
    tag=bench-$name cn=xc-pg-bench-$name
    mkdir -p "$rundir"
    drop_pg "$tag"
    log "=== bench $name: scale=$SCALE clients=$CLIENTS jobs=$JOBS duration=${DURATION}s ==="
    docker volume create "$cn-p" >/dev/null
    [[ $mode == baseline ]] || docker volume create "$cn-s" >/dev/null
    start_pg "$tag" "$mode" "$rundir" log "$xtra ${XC_EXTRA:-}"
    wait_ready "$cn" || { docker logs "$cn" | tail -n 30; die "postgres did not become ready"; }
    xc_snapshot "$cn" "$rundir/xc" after-initdb
    workload_init "$cn" "$rundir" "$SCALE"
    xc_snapshot "$cn" "$rundir/xc" after-init
    workload_runs "$cn" "$rundir"
    db_state "$cn" "$rundir"
    px "$cn" psql -X -At -U postgres -d postgres -c "SELECT sum(abalance) FROM pgbench_accounts" >"$rundir/live-sum.txt"
    stop_pg "$cn"
    docker logs "$cn" >"$rundir/container.log" 2>&1 || true
    BENCH_EXIT=$(docker inspect -f '{{.State.ExitCode}}' "$cn")
    log "container exit: $BENCH_EXIT"
    if [[ $mode != baseline ]]; then
        log "final xcheckfs: $(jq -c '{ops,mismatches,allowed,repeats,secondary_skipped,verifications,resyncs}' "$rundir/xc-status-final.json" 2>/dev/null)"
        verify_vols "$tag" "$rundir"
        validate_both "$tag" "$rundir"
    else
        validate_vol "$tag" p "$rundir" || true
    fi
    if ((CI)); then ci_checks "$mode" "$rundir"; fi
    [[ $KEEP == 1 ]] || drop_pg "$tag"
}

crash() {
    local tag=crash cn=xc-pg-crash r1=$OUT/crash/1-run r2=$OUT/crash/2-recover
    mkdir -p "$r1" "$r2"
    drop_pg "$tag"
    log "=== crash test: scale=$CRASH_SCALE, SIGKILL after ${CRASH_AFTER}s ==="
    docker volume create "$cn-p" >/dev/null
    docker volume create "$cn-s" >/dev/null
    start_pg "$tag" thorough "$r1" log "${XC_EXTRA:-}"
    wait_ready "$cn" || die "postgres did not become ready"
    workload_init "$cn" "$r1" "$CRASH_SCALE"
    px "$cn" /work/maint.sh 300 >"$r1/maint.txt" 2>&1 &
    px "$cn" pgbench -U postgres -c "$CLIENTS" -j "$JOBS" -T 300 -P 5 --max-tries=5 -f /work/churn.pgbench postgres >"$r1/pgbench-churn.txt" 2>&1 &
    sleep "$CRASH_AFTER"
    xc_snapshot "$cn" "$r1/xc" before-kill
    log "docker kill -s KILL $cn"
    docker kill -s KILL "$cn" >/dev/null
    wait || true
    docker logs "$cn" >"$r1/container.log" 2>&1 || true
    docker rm -fv "$cn" >/dev/null
    verify_vols "$tag" "$r1"
    cp "$r1/verify.txt" "$r1/verify-after-kill.txt"
    local round
    for ((round = 2; round <= CRASH_ROUNDS; round++)); do
        local rr=$OUT/crash/round-$round
        mkdir -p "$rr"
        log "--- round $round: recover through xcheckfs (resync), run, SIGKILL ---"
        start_pg "$tag" thorough "$rr" resync "--quarantine /out/quarantine ${XC_EXTRA:-}"
        wait_ready "$cn" || { docker logs "$cn" | tail -n 30; die "postgres did not recover"; }
        xc_snapshot "$cn" "$rr/xc" after-recovery
        px "$cn" /work/maint.sh 300 >"$rr/maint.txt" 2>&1 &
        px "$cn" pgbench -U postgres -c "$CLIENTS" -j "$JOBS" -T 300 -P 5 --max-tries=5 -f /work/churn.pgbench postgres >"$rr/pgbench-churn.txt" 2>&1 &
        local after=$((5 + RANDOM % 31))
        sleep "$after"
        xc_snapshot "$cn" "$rr/xc" before-kill
        log "docker kill -s KILL $cn (after ${after}s)"
        docker kill -s KILL "$cn" >/dev/null
        wait || true
        docker logs "$cn" >"$rr/container.log" 2>&1 || true
        docker rm -fv "$cn" >/dev/null
        verify_vols "$tag" "$rr"
    done
    log "--- final restart through a fresh xcheckfs mount in resync mode (quarantine on) ---"
    start_pg "$tag" thorough "$r2" resync "--quarantine /out/quarantine ${XC_EXTRA:-}"
    local t0
    t0=$(date +%s)
    wait_ready "$cn" || { docker logs "$cn" | tail -n 30; die "postgres did not recover"; }
    log "recovery through xcheckfs: ready after $(($(date +%s) - t0))s"
    docker logs "$cn" 2>&1 | grep -E 'recovery|redo|checkpoint (starting|complete).*end-of-recovery|invalid|PANIC|FATAL' | head -n 12 | sed 's/^/  pglog: /' | tee -a "$OUT/summary.txt" || true
    xc_snapshot "$cn" "$r2/xc" after-recovery
    PGB_DURATION=40 run_pgbench "$cn" "$r2" post-recovery
    px "$cn" psql -X -q -U postgres -c CHECKPOINT postgres
    xc_snapshot "$cn" "$r2/xc" after-post-run
    stop_pg "$cn"
    docker logs "$cn" >"$r2/container.log" 2>&1 || true
    log "final xcheckfs: $(jq -c '{ops,mismatches,allowed,repeats,resyncs,resync_failures,resync_giveups,quarantined}' "$r2/xc-status-final.json" 2>/dev/null)"
    verify_vols "$tag" "$r2"
    validate_both "$tag" "$r2"
    [[ $KEEP == 1 ]] || drop_pg "$tag"
}

# ---- CI mode ----------------------------------------------------------------
# `run.sh ci` runs bench() for every config in CONFIGS and writes $OUT/results.json.
# Per config the bench() hooks (run_pgbench -> ci_record_workload, end of bench ->
# ci_checks) leave workloads.jsonl / checks.jsonl / phases.txt in $OUT/<config>/.

CI_DIR= # per-config results dir while a ci run is active

ci_check() { # NAME OK(true|false) [DETAIL]
    jq -nc --arg n "$1" --argjson ok "$2" --arg d "${3:-}" '{name: $n, ok: $ok, detail: $d}' >>"$CI_DIR/checks.jsonl"
    log "  check: $1: $([[ $2 == true ]] && echo ok || echo FAILED) ${3:+($3)}"
}

ci_record_workload() { # RUNDIR LABEL PGBENCH-EXIT
    local rundir=$1 label=$2 rc=$3 f p
    f=$rundir/pgbench-$label.txt p=$rundir/pctl-$label.txt
    local tps count failed avg aborted p50 p95 p99 max
    tps=$(sed -n 's/^tps = \([0-9.]*\).*/\1/p' "$f" | tail -n 1)
    count=$(sed -n 's/^number of transactions actually processed: \([0-9]*\).*/\1/p' "$f" | tail -n 1)
    failed=$(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$f" | tail -n 1)
    avg=$(sed -n 's/^latency average = \([0-9.]*\) ms.*/\1/p' "$f" | tail -n 1)
    aborted=$(grep -c 'aborted' "$f" || true)
    p50=$(sed -n 's/.* p50=\([0-9.]*\).*/\1/p' "$p")
    p95=$(sed -n 's/.* p95=\([0-9.]*\).*/\1/p' "$p")
    p99=$(sed -n 's/.* p99=\([0-9.]*\).*/\1/p' "$p")
    max=$(sed -n 's/.* max=\([0-9.]*\).*/\1/p' "$p")
    if ((rc == 0)) && [[ -n $tps && -n $count ]]; then
        echo "$label ok" >>"$rundir/phases.txt"
    else
        echo "$label failed (pgbench exit $rc)" >>"$rundir/phases.txt"
    fi
    [[ -n $tps ]] || return 0
    jq -nc --arg name "$label" --arg tps "$tps" --arg count "$count" --arg failed "$failed" --arg aborted "$aborted" \
        --arg avg "$avg" --arg p50 "$p50" --arg p95 "$p95" --arg p99 "$p99" --arg max "$max" '
        def n: if . == "" then null else tonumber end;
        {name: $name, unit: "tps", throughput: ($tps | n), count: ($count | n),
         errors: ((($failed | n) // 0) + ($aborted | tonumber)),
         latency_ms: ({avg: ($avg | n), p50: ($p50 | n), p95: ($p95 | n), p99: ($p99 | n), max: ($max | n)}
                      | with_entries(select(.value != null)))}' >>"$rundir/workloads.jsonl"
}

# ci_checks MODE RUNDIR : called at the end of bench() after verify/validate
ci_checks() {
    local mode=$1 rundir=$2 side name d
    for side in p s; do
        [[ $side == s && $mode == baseline ]] && continue
        name=$([[ $side == p ]] && echo primary || echo secondary)
        d=$(grep -E 'Bad checksums|rror' "$rundir/val-$side-checksums.txt" | tr -s ' ' | tr '\n' ';' | sed 's/;$//' | cut -c1-200 || true)
        ci_check "pg_checksums ($name)" "$([[ ${VALRC[checksums-$side]:-1} == 0 ]] && echo true || echo false)" \
            "$([[ ${VALRC[checksums-$side]:-1} == 0 ]] && echo "$d" || echo "exit ${VALRC[checksums-$side]:-?}: $d")"
        case ${VALRC[amcheck-$side]:-skipped} in
        0) ci_check "pg_amcheck ($name)" true "" ;;
        skipped) ci_check "pg_amcheck ($name)" false "postgres did not start on the volume" ;;
        *) ci_check "pg_amcheck ($name)" false "exit ${VALRC[amcheck-$side]}: $(head -n 1 "$rundir/val-$side-amcheck.txt" | cut -c1-200)" ;;
        esac
    done
    if [[ $mode != baseline ]]; then
        ci_check "xcheckfs verify" "$([[ $VERIFY_RC == 0 ]] && echo true || echo false)" "exit $VERIFY_RC: $(tail -n 1 "$rundir/verify.txt" | cut -c1-200)"
        local nlines
        nlines=$(grep -c . "$rundir/val-p-counts.txt" || true)
        if [[ $COUNTS_RC == 0 && $nlines -ge 6 ]] && ! grep -qi error "$rundir/val-p-counts.txt"; then
            ci_check "row counts equal" true "$(tr '\n' ';' <"$rundir/val-p-counts.txt" | cut -c1-200)"
        else
            ci_check "row counts equal" false "differ or unreadable, see val-counts.diff"
        fi
        local mm
        mm=$(jq -r '.mismatches' "$rundir/xc-status-final.json" 2>/dev/null || true)
        if [[ $mm =~ ^[0-9]+$ ]]; then
            ci_check "no mismatches" "$([[ $mm == 0 ]] && echo true || echo false)" "$mm mismatches"
        else
            ci_check "no mismatches" false "no final xcheckfs status"
        fi
    fi
    if [[ ${BENCH_EXIT:-0} != 0 ]]; then ci_check "postgres clean shutdown" false "container exit $BENCH_EXIT"; fi
    local total failed
    total=$(grep -c . "$rundir/phases.txt" || true)
    failed=$(grep -vc ' ok$' "$rundir/phases.txt" || true)
    ci_check "pgbench phases" "$([[ $failed == 0 && $total -gt 0 ]] && echo true || echo false)" \
        "$((total - failed))/$total completed$(grep -v ' ok$' "$rundir/phases.txt" | sed 's/^/; /' | tr -d '\n')"
}

# ci_config CONFIG : one config end to end (runs in a subshell with set -e)
ci_config() {
    local cfg=$1 mode=${1%-strict} xtra=
    [[ $cfg == *-strict ]] && xtra="--serialize strict"
    case $mode in
    baseline | basic | thorough | paranoid) ;;
    *) die "unknown config '$cfg' (baseline|basic|thorough|paranoid, optionally -strict)" ;;
    esac
    [[ $mode != baseline || -z $xtra ]] || die "baseline has no -strict variant"
    bench "$mode" "$cfg" "$xtra"
}

ci() {
    local cfg rc t0 t1 mode xargs rundir runs=() allok=1
    CI=1
    jq --version >/dev/null || die "jq is required"
    # shellcheck disable=SC2206
    local configs=(${CONFIGS:-baseline basic thorough})
    for cfg in "${configs[@]}"; do
        mode=${cfg%-strict}
        rundir=$OUT/$cfg
        rm -rf "$rundir"
        mkdir -p "$rundir"
        touch "$rundir/workloads.jsonl" "$rundir/checks.jsonl" "$rundir/phases.txt"
        CI_DIR=$rundir CI_FATAL=$rundir/fatal.txt
        export CI_FATAL
        log "##### ci config $cfg"
        t0=$(date +%s.%N)
        # No `||`/`if` around the subshell: that would disable set -e inside it.
        set +e
        (
            set -eE
            trap 'echo "line $LINENO: $BASH_COMMAND" >>"$CI_FATAL"' ERR
            ci_config "$cfg"
        )
        rc=$?
        set -e
        t1=$(date +%s.%N)
        if ((rc != 0)); then
            ci_check "run completed" false "aborted (exit $rc): $(tail -n 1 "$rundir/fatal.txt" 2>/dev/null | cut -c1-300)"
            drop_pg "bench-$cfg"
        fi
        xargs=
        if [[ $mode != baseline ]]; then
            xargs="--check $mode -m log"
            [[ $cfg != *-strict ]] || xargs+=" --serialize strict"
            [[ -z ${XC_EXTRA:-} ]] || xargs+=" $XC_EXTRA"
        fi
        jq -n --arg config "$cfg" --arg args "$xargs" --argjson wall "$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b-a}')" \
            --slurpfile wl "$rundir/workloads.jsonl" --slurpfile ck "$rundir/checks.jsonl" \
            --argjson xc "$(jq -c . "$rundir/xc-status-final.json" 2>/dev/null || echo null)" \
            --argjson phases_bad "$(grep -vc ' ok$' "$rundir/phases.txt" || true)" '
            {config: $config, xcheckfs_args: $args,
             ok: (($ck | length) > 0 and ($ck | all(.ok)) and $phases_bad == 0),
             wall_s: $wall, workloads: $wl,
             xcheckfs: (if $config | startswith("baseline") then null else $xc end),
             checks: $ck}' >"$rundir/run.json"
        [[ $(jq .ok "$rundir/run.json") == true ]] || allok=0
        runs+=("$rundir/run.json")
        log "##### ci config $cfg: ok=$(jq .ok "$rundir/run.json") wall=$(jq .wall_s "$rundir/run.json")s"
        # Rewritten after every config so a later crash still leaves results.
        ci_write_results "${runs[@]}"
    done
    ci_write_results "${runs[@]}"
    ((allok)) || { log "ci: at least one config FAILED"; return 1; }
    log "ci: all configs ok"
}

ci_write_results() { # RUN-JSON-FILES...
    local ver=${IMAGE#*:}
    jq -s --arg title "PostgreSQL ${ver}, pgbench" --argjson scale "$SCALE" --argjson clients "$CLIENTS" \
        --argjson jobs "$JOBS" --argjson dur "$DURATION" '
        {schema: 1, app: "postgres", title: $title,
         params: {scale: $scale, clients: $clients, jobs: $jobs, duration_s: $dur},
         runs: .}' "$@" >"$OUT/results.json.tmp"
    mv "$OUT/results.json.tmp" "$OUT/results.json"
}

clean() {
    local c v
    for c in $(docker ps -aq --filter label=xc-pg=1); do docker rm -fv "$c" >/dev/null; done
    for v in $(docker volume ls -q --filter name=xc-pg-); do docker volume rm -f "$v" >/dev/null; done
    echo "removed xc-pg-* containers and volumes"
}

mkdir -p "$OUT"
printf '*\n' >"$OUT/.gitignore"
case "${1:-}" in
bench) bench "${2:?baseline|basic|thorough|paranoid}" ;;
all) for m in baseline basic thorough; do bench "$m"; done ;;
crash) crash ;;
ci) ci ;;
clean) clean ;;
*) die "usage: run.sh bench MODE | all | crash | ci | clean" ;;
esac
