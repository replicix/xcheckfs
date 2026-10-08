#!/usr/bin/env bash
# MySQL/InnoDB under a sysbench OLTP load through xcheckfs (see README.md).
#
#   run.sh images                 pull mysql:8.4, build the sysbench image
#   run.sh baseline               no xcheckfs: the data directory is a plain volume
#   run.sh baselinebuf            like baseline, with buffered I/O (innodb_flush_method=fsync): xcheckfs strips O_DIRECT
#   run.sh basic | thorough       the same workload through xcheckfs --check basic | thorough
#   run.sh compress               page compression (PUNCH_HOLE) on tmpfs volumes: baseline, tmpfs/tmpfs, tmpfs/zfs
#   run.sh crash                  kill -9 the container mid-run, verify, recover through a fresh resync mount
#   run.sh crashloop              ROUNDS (default 4) x { load, SIGKILL, verify, recover through a resync mount }
#   run.sh paranoid               --check paranoid with --direct-io --attr-timeout 0 --entry-timeout 0 (maximal coverage)
#   run.sh validate NAME          start plain mysqld on the SECONDARY (and PRIMARY) volume of scenario NAME
#   run.sh all                    images, baseline, basic, thorough, validate thorough, compress, crash
#   run.sh ci                     short run for CI (see "CI mode" below): writes $OUT/results.json, exit 0 iff all ok
#   run.sh summary                print the result tables collected so far
#   run.sh clean                  remove every xc-mysql-* container, volume and network
#
# Environment (defaults in parentheses):
#   XC_EXTRA     extra `xcheckfs mount` arguments for every xcheckfs scenario
#   XCHECKFS     static xcheckfs binary (target/release/xcheckfs of this repo, else target/debug)
#   OUT          results directory (./results next to this script)
#   TABLES, TABLE_SIZE   sysbench data set (10 x 200000)
#   THREADS      sysbench threads (24)
#   T_RW, T_UPD, T_DEL, T_INS   seconds of oltp_read_write, oltp_update_index, oltp_delete, oltp_insert (180 60 30 60)
#   T_CRW        seconds of each read_write run in the compression phase (60)
#   CRASH_ROUNDS rounds for `crashloop` (4)
#   CRASH_AFTER  seconds into the crash run before SIGKILL (60)
#   MYSQL_CPUS, MYSQL_MEM, SB_CPUS   container limits (8, 6g, 6)
#   TABLES_C, TABLE_SIZE_C   data set for `compress` and `crash` (8 x 100000)
# CI mode (run.sh ci): for each config in CONFIGS (default "baseline basic thorough"; also paranoid and any
# <mode>-strict = that mode with --serialize strict) fresh volumes, server, prepare, four short sysbench runs, clean
# shutdown, final `ctl status`, `xcheckfs verify`, mysqlcheck/CHECK TABLE/row counts/CHECKSUM TABLE of both volumes
# against the live server, volumes dropped. Sized for a 4 vCPU / 16 GB runner; the defaults differ from the above:
#   CONFIGS      configs to run (baseline basic thorough)
#   TABLES, TABLE_SIZE, THREADS   6 x 50000 rows, 16 threads
#   T_RW, T_UPD, T_DEL, T_INS     45 20 15 20 seconds
#   MYSQL_CPUS, SB_CPUS           min(nproc,4), min(nproc,2)
#   XC_EXTRA     appended to the mount arguments of every xcheckfs config
# xcheckfs runs with its defaults (--direct-io auto, --serialize relaxed) and -m log, so every mismatch is counted.
# The "plain" docker volumes live on the file system of /var/lib/docker (here ZFS, 128 KiB blocks: InnoDB page
# compression cannot work there), so `compress` uses tmpfs volumes (4 KiB blocks, hole punching works).
set -u -o pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
OUT=${OUT:-$HERE/results}
if [ "${1:-}" = ci ]; then
    ncpu=$(nproc 2>/dev/null || echo 4)
    D_TABLES=6 D_TABLE_SIZE=50000 D_THREADS=16 D_T_RW=45 D_T_UPD=20 D_T_DEL=15 D_T_INS=20
    D_MYSQL_CPUS=$((ncpu < 4 ? ncpu : 4)) D_MYSQL_MEM=6g D_SB_CPUS=$((ncpu < 2 ? ncpu : 2)) D_WAIT_MAX=240
else
    D_TABLES=10 D_TABLE_SIZE=200000 D_THREADS=24 D_T_RW=180 D_T_UPD=60 D_T_DEL=30 D_T_INS=60
    D_MYSQL_CPUS=8 D_MYSQL_MEM=6g D_SB_CPUS=6 D_WAIT_MAX=600
fi
TABLES=${TABLES:-$D_TABLES}
TABLE_SIZE=${TABLE_SIZE:-$D_TABLE_SIZE}
THREADS=${THREADS:-$D_THREADS}
T_RW=${T_RW:-$D_T_RW}
T_UPD=${T_UPD:-$D_T_UPD}
T_DEL=${T_DEL:-$D_T_DEL}
T_INS=${T_INS:-$D_T_INS}
T_CRW=${T_CRW:-60}
CRASH_AFTER=${CRASH_AFTER:-60}
MYSQL_CPUS=${MYSQL_CPUS:-$D_MYSQL_CPUS}
MYSQL_MEM=${MYSQL_MEM:-$D_MYSQL_MEM}
SB_CPUS=${SB_CPUS:-$D_SB_CPUS}
WAIT_MAX=${WAIT_MAX:-$D_WAIT_MAX}   # seconds to wait for mysqld to answer

IMG=mysql:8.4
SBIMG=xc-mysql-sysbench:local
NET=xc-mysql-net
SRV=xc-mysql-srv
CHK=xc-mysql-chk
SOCK=/run/xc/ctl.sock

if [ -z "${XCHECKFS:-}" ]; then
    for c in "$REPO/target/release/xcheckfs" "$REPO/target/debug/xcheckfs"; do
        [ -x "$c" ] && XCHECKFS=$c && break
    done
fi

MYSQLD_ARGS=(
    mysqld
    --innodb-flush-method=O_DIRECT
    --innodb-use-native-aio=ON
    --innodb-doublewrite=ON
    --innodb-flush-log-at-trx-commit=1
    --sync-binlog=1
    --innodb-buffer-pool-size=256M
    --log-bin=binlog --server-id=1 --max-binlog-size=128M --binlog-expire-logs-seconds=120
    --max-connections=200 --skip-name-resolve
)

SCN=""   # current scenario name; results go to $OUT/$SCN
POLLER=""   # pid of the background status poller
PLAIN_OK=0 PLAIN_DETAIL=""

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$OUT/run.log" >&2; }
die() {
    log "FATAL: $*"
    [ -n "$SCN" ] && [ -d "$OUT/$SCN" ] && echo "$*" >>"$OUT/$SCN/fatal.txt"
    exit 1
}

need_bin() {
    [ -n "${XCHECKFS:-}" ] && [ -x "$XCHECKFS" ] || die "set XCHECKFS to a static xcheckfs binary"
    mkdir -p "$OUT/bin"
    cp "$XCHECKFS" "$OUT/bin/xcheckfs"
}

# ---------------------------------------------------------------- docker plumbing

mknet() { docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null; }

# mkvol NAME TYPE: TYPE is "plain" (default driver) or "tmpfs" (4 KiB blocks, supports hole punching)
mkvol() {
    if [ "$2" = tmpfs ]; then
        docker volume create --driver local --opt type=tmpfs --opt device=tmpfs --opt o=size=3g "$1" >/dev/null
    else
        docker volume create "$1" >/dev/null
    fi
}

# Local tmpfs volumes vanish when no container uses them: keep one container around for the whole scenario.
hold() { docker run -d --name "xc-mysql-hold-$1" -v "$2:/p" -v "$3:/s" alpine:3.20 sleep infinity >/dev/null; }
unhold() { docker rm -f "xc-mysql-hold-$1" >/dev/null 2>&1; }

# start_server CHECK MODE VOLP VOLS MEM [QUARANTINE]
#   CHECK=none: VOLP is the data directory itself. Otherwise xcheckfs is mounted over it (entry.sh).
start_server() {
    local check=$1 mode=$2 volp=$3 vols=$4 mem=$5 quar=${6:-}
    local -a volargs
    if [ "$check" = none ]; then
        volargs=(-v "$volp:/var/lib/mysql")
    else
        volargs=(--tmpfs /var/lib/mysql -v "$volp:/xc/p" -v "$vols:/xc/s")
    fi
    docker rm -f "$SRV" >/dev/null 2>&1
    docker run -d --name "$SRV" --network "$NET" --cpus "$MYSQL_CPUS" --memory "$mem" \
        --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined \
        -v "$OUT/bin:/x:ro" -v "$HERE/entry.sh:/entry.sh:ro" "${volargs[@]}" \
        -e MYSQL_ALLOW_EMPTY_PASSWORD=yes -e MYSQL_DATABASE=sbtest \
        -e "XC_CHECK=$check" -e "XC_MODE=$mode" -e "XC_QUARANTINE=$quar" -e "XC_EXTRA=${XC_EXTRA:-}" \
        --entrypoint /entry.sh "$IMG" "${MYSQLD_ARGS[@]}" >/dev/null || die "cannot start $SRV"
}

# wait_up NAME: until mysqld answers on TCP (the temporary init server has networking disabled)
wait_up() {
    local n=$1 i
    for i in $(seq 1 "$WAIT_MAX"); do
        docker exec "$n" mysqladmin --protocol=tcp -h127.0.0.1 -uroot ping >/dev/null 2>&1 && return 0
        [ "$(docker inspect -f '{{.State.Running}}' "$n" 2>/dev/null)" = true ] || { log "$n exited during startup"; return 1; }
        sleep 1
        [ $((i % 60)) = 0 ] && log "waiting for $n ($i s)"
    done
    return 1
}

sql() { docker exec -i "$SRV" mysql --protocol=tcp -h127.0.0.1 -uroot -N -B "$@"; }
xcctl() { docker exec "$SRV" /x/xcheckfs ctl --socket "$SOCK" "$@"; }

# compact one-line status (ops, mismatches, ...) of the running mount
xcstatus() {
    xcctl status 2>/dev/null | tr -d ' \n' | grep -o '"\(ops\|mismatches\|verifications\|resyncs\|resync_failures\|secondary_skipped\|open_files\|state\)":\("[a-z]*"\|[0-9]*\)' | tr '\n' ' '
    echo
}

# poll_status: append a status line every 15 s to $OUT/$SCN/status.log (run in the background)
poll_status() {
    while sleep 15; do
        echo "$(date +%H:%M:%S) $(xcstatus)" >>"$OUT/$SCN/status.log"
    done
}

# ---------------------------------------------------------------- sysbench

SB_COMMON() {
    echo --db-driver=mysql --mysql-host=$SRV --mysql-user=root --mysql-db=sbtest "--tables=$TABLES" "--table-size=$TABLE_SIZE"
}

sb_docker() {
    # shellcheck disable=SC2046
    docker run --rm --network "$NET" --cpus "$SB_CPUS" "$SBIMG" sysbench "$@" $(SB_COMMON)
}

# sb WORKLOAD LABEL SECONDS: run, keep the output, append a row to summary.tsv
sb() {
    local wl=$1 label=$2 secs=$3
    local f="$OUT/$SCN/$wl-$label.txt"
    log "$SCN: sysbench $wl $label ($THREADS threads, $secs s)"
    sb_docker "$wl" "--threads=$THREADS" "--time=$secs" --report-interval=10 --percentile=95 run >"$f" 2>&1
    local tps qps p95 avg err
    tps=$(sed -n 's/.*transactions: *[0-9]* *(\([0-9.]*\) per sec.*/\1/p' "$f")
    qps=$(sed -n 's/.*queries: *[0-9]* *(\([0-9.]*\) per sec.*/\1/p' "$f")
    p95=$(sed -n 's/.*95th percentile: *\([0-9.]*\).*/\1/p' "$f")
    avg=$(sed -n 's/^ *avg: *\([0-9.]*\).*/\1/p' "$f" | head -1)
    err=$(sed -n 's/.*ignored errors: *\([0-9]*\).*/\1/p' "$f")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$SCN" "$label" "${tps:--}" "${qps:--}" "${p95:--}" "${avg:--}" "${err:--}" >>"$OUT/summary.tsv"
    log "$SCN: $label tps=${tps:--} qps=${qps:--} p95=${p95:--}ms $(xcstatus 2>/dev/null)"
}

prepare() {
    local t0=$SECONDS
    log "$SCN: sysbench prepare ($TABLES x $TABLE_SIZE rows)"
    sb_docker oltp_read_write "--threads=$TABLES" prepare >"$OUT/$SCN/prepare.txt" 2>&1 || die "prepare failed (see $OUT/$SCN/prepare.txt)"
    local d=$((SECONDS - t0))
    printf '%s\tprepare(%ss)\t-\t-\t-\t-\t-\n' "$SCN" "$d" >>"$OUT/summary.tsv"
    log "$SCN: prepare took ${d}s $(xcstatus 2>/dev/null)"
}

# table_report CONTAINER: "table rows checksum" lines for the sysbench tables, sorted
table_report() {
    local c=$1 i q="" tl=""
    for i in $(seq 1 "$TABLES"); do
        q+="SELECT 'sbtest$i', COUNT(*) FROM sbtest.sbtest$i;"
        tl+="sbtest.sbtest$i,"
    done
    {
        docker exec -i "$c" mysql --protocol=tcp -h127.0.0.1 -uroot -N -B -e "$q" | awk '{print $1" rows="$2}'
        docker exec -i "$c" mysql --protocol=tcp -h127.0.0.1 -uroot -N -B -e "CHECKSUM TABLE ${tl%,};" | awk '{print $1" checksum="$2}'
    } | sort
}

# ibd_report CONTAINER: size and allocated bytes of the .ibd files (holes show as allocated < size)
ibd_report() {
    docker exec "$1" bash -c 'stat -c "%n size=%s allocated=%b*%B" /var/lib/mysql/sbtest/*.ibd' | sed 's#/var/lib/mysql/sbtest/##' | sort -V
}

tablespaces() {
    sql -e "SELECT NAME, ROW_FORMAT, FS_BLOCK_SIZE, FILE_SIZE, ALLOCATED_SIZE FROM INFORMATION_SCHEMA.INNODB_TABLESPACES WHERE NAME LIKE 'sbtest/%' ORDER BY NAME;"
}

# ---------------------------------------------------------------- finishing a run

# stop_server: clean mysqld shutdown, collect xcheckfs logs from the stopped container, verify P against S
stop_server() {
    local check=$1 volp=$2 vols=$3 d="$OUT/$SCN"
    log "$SCN: mysqladmin shutdown"
    timeout 300 docker exec "$SRV" mysqladmin --protocol=tcp -h127.0.0.1 -uroot shutdown >>"$d/shutdown.txt" 2>&1
    if ! timeout 300 docker wait "$SRV" >"$d/exit-code.txt"; then
        log "$SCN: $SRV did not stop within 300 s, killing it"
        docker kill "$SRV" >/dev/null 2>&1
        echo 137 >"$d/exit-code.txt"
    fi
    docker logs "$SRV" >"$d/mysqld.log" 2>&1
    if [ "$check" != none ]; then
        local f
        for f in xc.log xc.stdout xc.exit xc-final-status.json xc-final-mismatches.json xc-final-stats.json; do
            docker cp "$SRV:/run/$f" "$d/$f" >/dev/null 2>&1
        done
        log "$SCN: final status: $(tr -d ' \n' <"$d/xc-final-status.json" | grep -o '"\(ops\|mismatches\|verifications\|resyncs\|resync_failures\|secondary_skipped\|repeats\|bytes_written\)":[0-9]*' | tr '\n' ' ') xcheckfs exit=$(cat "$d/xc.exit" 2>/dev/null)"
        verify_vols "$volp" "$vols" >"$d/verify.txt" 2>&1
        local rc=$?
        echo "verify exit code: $rc" >>"$d/verify.txt"
        log "$SCN: xcheckfs verify exit code $rc: $(tail -2 "$d/verify.txt" | head -1)"
    fi
    docker rm -f "$SRV" >/dev/null 2>&1
}

# verify_vols VOLP VOLS: offline comparison of the two trees, in a throw-away container
verify_vols() {
    docker run --rm -v "$OUT/bin:/x:ro" -v "$1:/xc/p:ro" -v "$2:/xc/s:ro" alpine:3.20 /x/xcheckfs verify --max-reports 200 /xc/p /xc/s
}

# ---------------------------------------------------------------- scenarios

# scenario NAME CHECK MODE PTYPE STYPE PROFILE
scenario() {
    local name=$1 check=$2 mode=$3 pt=$4 st=$5 profile=$6
    SCN=$name
    local volp="xc-mysql-$name-p" vols="xc-mysql-$name-s" mem=$MYSQL_MEM
    mkdir -p "$OUT/$SCN"
    : >"$OUT/$SCN/status.log"
    echo "$TABLES $TABLE_SIZE" >"$OUT/$SCN/dataset.txt"
    docker volume rm "$volp" "$vols" >/dev/null 2>&1
    mkvol "$volp" "$pt"
    [ "$check" != none ] && mkvol "$vols" "$st"
    [ "$check" = none ] && vols=$volp
    hold "$name" "$volp" "$vols"
    log "$SCN: start (check=$check mode=$mode primary=$pt secondary=$st profile=$profile)"
    start_server "$check" "$mode" "$volp" "$vols" "$mem"
    wait_up "$SRV" || die "$SRV did not come up"
    local poller=""
    if [ "$check" != none ]; then
        poll_status &
        poller=$!
        POLLER=$poller
    fi

    prepare
    if [ "$profile" = ci ]; then
        ci_workloads
    elif [ "$profile" = main ]; then
        sb oltp_read_write rw "$T_RW"
        sb oltp_update_index update_index "$T_UPD"
        sb oltp_delete delete "$T_DEL"
        sb oltp_insert insert "$T_INS"
        log "$SCN: compression probe on a 16 KiB-page table"
        sql -e "ALTER TABLE sbtest.sbtest1 COMPRESSION='zlib'; OPTIMIZE TABLE sbtest.sbtest1;" >"$OUT/$SCN/compress-probe.txt" 2>&1
        tablespaces >>"$OUT/$SCN/compress-probe.txt"
    else
        sb oltp_read_write rw-plain "$T_CRW"
        ibd_report "$SRV" >"$OUT/$SCN/ibd-before-compress.txt"
        log "$SCN: ALTER TABLE ... COMPRESSION='zlib' + OPTIMIZE TABLE on $TABLES tables"
        local i t0=$SECONDS
        for i in $(seq 1 "$TABLES"); do
            sql -e "ALTER TABLE sbtest.sbtest$i COMPRESSION='zlib'; OPTIMIZE TABLE sbtest.sbtest$i;" >>"$OUT/$SCN/compress.txt" 2>&1
        done
        log "$SCN: compression took $((SECONDS - t0)) s; $(grep -c 'Operation failed\|Compression failed' "$OUT/$SCN/compress.txt") failures"
        tablespaces >"$OUT/$SCN/tablespaces-after-compress.txt"
        ibd_report "$SRV" >"$OUT/$SCN/ibd-after-compress.txt"
        sb oltp_read_write rw-compressed "$T_CRW"
        sb oltp_update_index update_index-compressed 30
        tablespaces >"$OUT/$SCN/tablespaces-end.txt"
        ibd_report "$SRV" >"$OUT/$SCN/ibd-end.txt"
    fi

    table_report "$SRV" >"$OUT/$SCN/report-live.txt"
    sql -e "CHECK TABLE sbtest.sbtest1" >"$OUT/$SCN/check-live.txt" 2>&1
    [ -n "$poller" ] && { kill "$poller" 2>/dev/null; wait "$poller" 2>/dev/null; }
    [ "$check" != none ] && xcctl mismatches 200 >"$OUT/$SCN/mismatches-live.json" 2>&1
    stop_server "$check" "$volp" "$vols"
    # tmpfs volumes are gone once nothing holds them: validate before releasing them
    [ "$profile" = compress ] && [ "$check" != none ] && validate_vols "$name"
    [ "$profile" = ci ] && ci_finish "$name" "$check"
    unhold "$name"
}

# plain_check VOL LABEL [MEM]: plain mysql:8.4 on the volume (no xcheckfs): crash recovery if needed, mysqlcheck, report
plain_check() {
    local vol=$1 label=$2 d="$OUT/$SCN" mem=${3:-4g}
    PLAIN_OK=0 PLAIN_DETAIL="mysqld did not start"   # result for ci_validate
    docker rm -f "$CHK" >/dev/null 2>&1
    docker run -d --name "$CHK" --network "$NET" --cpus 4 --memory "$mem" -v "$vol:/var/lib/mysql" \
        -e MYSQL_ALLOW_EMPTY_PASSWORD=yes "$IMG" "${MYSQLD_ARGS[@]}" >/dev/null || die "cannot start $CHK"
    if ! wait_up "$CHK"; then
        docker logs "$CHK" >"$d/plain-$label.log" 2>&1
        log "$SCN: plain mysqld on $label FAILED to start (see plain-$label.log)"
        docker rm -f "$CHK" >/dev/null 2>&1
        return 1
    fi
    {
        echo "== mysqlcheck --all-databases --check --extended"
        docker run --rm --network "$NET" "$SBIMG" mysqlcheck -h"$CHK" -uroot --all-databases --check --extended
        echo "mysqlcheck exit code: $?"
        echo "== CHECK TABLE ... EXTENDED"
        local i
        for i in $(seq 1 "$TABLES"); do
            docker exec "$CHK" mysql --protocol=tcp -h127.0.0.1 -uroot -N -B -e "CHECK TABLE sbtest.sbtest$i EXTENDED"
        done
    } >"$d/plain-$label-check.txt" 2>&1
    table_report "$CHK" >"$d/report-$label.txt"
    ibd_report "$CHK" >"$d/ibd-$label.txt" 2>&1
    docker exec "$CHK" mysqladmin --protocol=tcp -h127.0.0.1 -uroot shutdown >/dev/null 2>&1
    docker wait "$CHK" >/dev/null
    docker logs "$CHK" >"$d/plain-$label.log" 2>&1
    docker rm -f "$CHK" >/dev/null 2>&1
    local mrc nbad nok nerr
    mrc=$(sed -n 's/^mysqlcheck exit code: //p' "$d/plain-$label-check.txt")
    nbad=$(grep -P '\tcheck\t' "$d/plain-$label-check.txt" | grep -vc 'OK$')
    nok=$(grep -Pc '\tcheck\tstatus\tOK$' "$d/plain-$label-check.txt")
    nerr=$(grep -c '\[ERROR\]' "$d/plain-$label.log")
    PLAIN_DETAIL="mysqlcheck exit ${mrc:-?}, CHECK TABLE EXTENDED ok on $nok of $TABLES tables ($nbad not ok), $nerr [ERROR] lines in mysqld log"
    [ "${mrc:-1}" = 0 ] && [ "$nbad" = 0 ] && [ "$nok" = "$TABLES" ] && PLAIN_OK=1
    log "$SCN: plain $label: $(grep -c ' OK$\| OK ' "$d/plain-$label-check.txt") OK lines, $(grep -ci 'error\|corrupt\|warning' "$d/plain-$label-check.txt") error/warning lines in check output"
}

# validate_vols NAME: plain mysqld on the SECONDARY and on the PRIMARY volume, compared with the live report
validate_vols() {
    SCN=$1
    local d="$OUT/$SCN" l
    plain_check "xc-mysql-$SCN-s" secondary || true
    plain_check "xc-mysql-$SCN-p" primary || true
    for l in secondary primary; do
        if diff -u "$d/report-live.txt" "$d/report-$l.txt" >"$d/report-diff-$l.txt" 2>&1; then
            log "$SCN: $l: rows/CHECKSUM TABLE identical to the live report"
        else
            log "$SCN: $l: rows/CHECKSUM TABLE DIFFER from the live report (see report-diff-$l.txt)"
        fi
    done
}

validate() {
    [ -f "$OUT/$1/dataset.txt" ] || die "no results for $1"
    read -r TABLES TABLE_SIZE <"$OUT/$1/dataset.txt"
    validate_vols "$1"
}

# crash: SIGKILL the container mid-run, verify, then recover through a fresh resync mount
crash() {
    SCN=crash
    local d="$OUT/crash" volp=xc-mysql-crash-p vols=xc-mysql-crash-s
    mkdir -p "$d"
    : >"$d/status.log"
    echo "$TABLES $TABLE_SIZE" >"$d/dataset.txt"
    docker volume rm "$volp" "$vols" xc-mysql-crash-p0 xc-mysql-crash-s0 >/dev/null 2>&1
    mkvol "$volp" plain
    mkvol "$vols" plain
    log "crash: start (thorough, log mode)"
    start_server thorough log "$volp" "$vols" "$MYSQL_MEM"
    wait_up "$SRV" || die "$SRV did not come up"
    poll_status &
    local poller=$!
    prepare
    log "crash: oltp_read_write, SIGKILL after $CRASH_AFTER s"
    (sb_docker oltp_read_write "--threads=$THREADS" --time=600 --report-interval=10 --mysql-ignore-errors=all run >"$d/rw-killed.txt" 2>&1) &
    local sbpid=$!
    sleep "$CRASH_AFTER"
    echo "$(date +%H:%M:%S) before kill: $(xcstatus)" >>"$d/status.log"
    xcctl mismatches 200 >"$d/mismatches-before-kill.json" 2>&1
    docker kill -s KILL "$SRV" >/dev/null
    log "crash: container killed"
    kill "$poller" 2>/dev/null; wait "$poller" 2>/dev/null
    docker ps -q --filter "ancestor=$SBIMG" | xargs -r docker kill >/dev/null 2>&1
    wait "$sbpid" 2>/dev/null
    local f
    for f in xc.log xc.stdout; do docker cp "$SRV:/run/$f" "$d/$f" >/dev/null 2>&1; done
    docker logs "$SRV" >"$d/mysqld-killed.log" 2>&1
    docker rm -f "$SRV" >/dev/null 2>&1

    log "crash: xcheckfs verify after the kill"
    verify_vols "$volp" "$vols" >"$d/verify-after-kill.txt" 2>&1
    local rc=$?
    log "crash: verify exit code $rc ($(tail -1 "$d/verify-after-kill.txt"))"
    # copies of the diverged trees: what mysqld would do with each side on its own
    local v
    for v in p s; do
        mkvol "xc-mysql-crash-${v}0" plain
        docker run --rm -v "xc-mysql-crash-$v:/from:ro" -v "xc-mysql-crash-${v}0:/to" alpine:3.20 cp -a /from/. /to/
    done

    log "crash: recovery through a fresh xcheckfs mount (thorough, resync, quarantine)"
    start_server thorough resync "$volp" "$vols" "$MYSQL_MEM" /run/xc-quarantine
    wait_up "$SRV" || log "crash: recovery start FAILED"
    docker logs "$SRV" >"$d/mysqld-recovery.log" 2>&1
    xcctl status >"$d/status-after-recovery.json" 2>&1
    xcctl mismatches 1000 >"$d/mismatches-recovery.json" 2>&1
    log "crash: after recovery: $(xcstatus)"
    poll_status &
    poller=$!
    sql -e "CHECK TABLE sbtest.sbtest1" >"$d/check-after-recovery.txt" 2>&1
    sb oltp_read_write rw-after-recovery 30
    table_report "$SRV" >"$d/report-live.txt"
    kill "$poller" 2>/dev/null; wait "$poller" 2>/dev/null
    docker exec "$SRV" bash -c 'cd /run/xc-quarantine 2>/dev/null && find . -type f | head -200' >"$d/quarantine-files.txt" 2>&1
    xcctl mismatches 1000 >"$d/mismatches-live.json" 2>&1
    stop_server thorough "$volp" "$vols"
    plain_check "$vols" secondary
    plain_check "$volp" primary
    plain_check xc-mysql-crash-p0 p0-plain-recovery
    plain_check xc-mysql-crash-s0 s0-plain-recovery
    log "crash: done"
}

# diverge_detail VOLP VOLS VERIFYFILE: for every file whose content differs, list the differing 16 KiB pages and the
# first 40 bytes plus the trailer (checksum, LSN) of each side's version
diverge_detail() {
    local f
    sed -n 's#^\(/[^:]*\): content:.*#\1#p' "$3" | while read -r f; do
        echo "== $f"
        docker run --rm -v "$1:/xc/p:ro" -v "$2:/xc/s:ro" alpine:3.20 sh -c '
            f=$1
            cmp -l "/xc/p$f" "/xc/s$f" | awk "{print int((\$1-1)/16384)}" | uniq -c
            for pg in $(cmp -l "/xc/p$f" "/xc/s$f" | awk "{print int((\$1-1)/16384)}" | uniq | head -20); do
                for side in p s; do
                    echo "page $pg side $side header:"
                    dd if="/xc/$side$f" bs=16384 skip="$pg" count=1 2>/dev/null | od -A d -t x1 -N 40
                    echo "page $pg side $side trailer:"
                    dd if="/xc/$side$f" bs=16384 skip="$pg" count=1 2>/dev/null | od -A d -t x1 -j 16376 -N 8
                done
            done' sh "$f"
    done
}

# crashloop: ROUNDS times { load, SIGKILL, verify, fresh resync mount (InnoDB crash recovery) } on the same volumes
crashloop() {
    SCN=loop
    local d="$OUT/loop" volp=xc-mysql-loop-p vols=xc-mysql-loop-s r f rc sbpid
    mkdir -p "$d"
    : >"$d/status.log"
    echo "$TABLES $TABLE_SIZE" >"$d/dataset.txt"
    docker volume rm "$volp" "$vols" >/dev/null 2>&1
    mkvol "$volp" plain
    mkvol "$vols" plain
    start_server thorough resync "$volp" "$vols" "$MYSQL_MEM" /run/xc-quarantine
    wait_up "$SRV" || die "$SRV did not come up"
    prepare
    for r in $(seq 1 "${CRASH_ROUNDS:-4}"); do
        log "loop: round $r: oltp_read_write"
        (sb_docker oltp_read_write "--threads=$THREADS" --time=600 --report-interval=10 --mysql-ignore-errors=all run >"$d/rw-round$r.txt" 2>&1) &
        sbpid=$!
        sleep $((CRASH_AFTER + RANDOM % 20))
        echo "$(date +%H:%M:%S) round $r before kill: $(xcstatus)" >>"$d/status.log"
        xcctl mismatches 200 >"$d/mismatches-round$r-before-kill.json" 2>&1
        docker kill -s KILL "$SRV" >/dev/null
        docker ps -q --filter "ancestor=$SBIMG" | xargs -r docker kill >/dev/null 2>&1
        wait "$sbpid" 2>/dev/null
        for f in xc.log xc.stdout; do docker cp "$SRV:/run/$f" "$d/$f-round$r" >/dev/null 2>&1; done
        docker rm -f "$SRV" >/dev/null 2>&1
        verify_vols "$volp" "$vols" >"$d/verify-round$r.txt" 2>&1
        rc=$?
        log "loop: round $r: verify after kill: exit $rc: $(tail -1 "$d/verify-round$r.txt")"
        [ "$rc" = 3 ] && diverge_detail "$volp" "$vols" "$d/verify-round$r.txt" >"$d/diverged-round$r.txt" 2>&1
        start_server thorough resync "$volp" "$vols" "$MYSQL_MEM" /run/xc-quarantine
        wait_up "$SRV" || log "loop: round $r: recovery start FAILED"
        docker logs "$SRV" >"$d/mysqld-recovery-round$r.log" 2>&1
        xcctl status >"$d/status-recovery-round$r.json" 2>&1
        xcctl mismatches 1000 >"$d/mismatches-recovery-round$r.json" 2>&1
        docker cp "$SRV:/run/xc-quarantine" "$d/quarantine-round$r" >/dev/null 2>&1
        log "loop: round $r: after recovery: $(xcstatus)"
    done
    sb oltp_read_write rw-final 30
    table_report "$SRV" >"$d/report-live.txt"
    xcctl mismatches 1000 >"$d/mismatches-live.json" 2>&1
    stop_server thorough "$volp" "$vols"
    validate_vols loop
}

# ---------------------------------------------------------------- CI mode

# ci_check NAME ok|fail DETAIL: append a check to $OUT/$SCN/checks.tsv (becomes "checks" in results.json)
ci_check() {
    printf '%s\t%s\t%s\n' "$1" "$2" "$(printf '%s' "$3" | tr '\t\n' '  ')" >>"$OUT/$SCN/checks.tsv"
    [ "$2" = ok ] || log "$SCN: CHECK FAILED: $1: $3"
}

# sb_ci WORKLOAD SECONDS: one sysbench run; a row in workloads.tsv (name tps count errors avg p95 max), or a failed check
sb_ci() {
    local wl=$1 secs=$2 f="$OUT/$SCN/$1.txt" name="xc-mysql-sb-$SCN" rc
    log "$SCN: sysbench $wl ($THREADS threads, $secs s)"
    # shellcheck disable=SC2046
    timeout $((secs + 120)) docker run --rm --name "$name" --network "$NET" --cpus "$SB_CPUS" "$SBIMG" sysbench "$wl" \
        --threads="$THREADS" --time="$secs" --report-interval=10 --percentile=95 $(SB_COMMON) run >"$f" 2>&1
    rc=$?
    [ "$rc" = 0 ] || docker rm -f "$name" >/dev/null 2>&1
    local tps cnt err avg p95 max
    tps=$(sed -n 's/.*transactions: *[0-9]* *(\([0-9.]*\) per sec.*/\1/p' "$f")
    cnt=$(sed -n 's/.*transactions: *\([0-9]*\) .*/\1/p' "$f")
    err=$(sed -n 's/.*ignored errors: *\([0-9]*\).*/\1/p' "$f")
    avg=$(sed -n 's/^ *avg: *\([0-9.]*\).*/\1/p' "$f" | head -1)
    p95=$(sed -n 's/.*95th percentile: *\([0-9.]*\).*/\1/p' "$f")
    max=$(sed -n 's/^ *max: *\([0-9.]*\).*/\1/p' "$f" | head -1)
    if [ "$rc" != 0 ] || [ -z "$tps" ] || [ -z "$cnt" ]; then
        ci_check "workload $wl" fail "sysbench exit code $rc (see $wl.txt)"
        return 1
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$wl" "$tps" "$cnt" "${err:-0}" "${avg:--}" "${p95:--}" "${max:--}" >>"$OUT/$SCN/workloads.tsv"
    log "$SCN: $wl tps=$tps p95=${p95:--}ms errors=${err:-0}"
    [ "$SCN" = baseline ] || log "$SCN: $(xcstatus 2>/dev/null)"
}

# the four workloads in order; stops at the first one that fails (the failure is recorded as a check)
ci_workloads() {
    sb_ci oltp_read_write "$T_RW" && sb_ci oltp_update_index "$T_UPD" && sb_ci oltp_delete "$T_DEL" && sb_ci oltp_insert "$T_INS"
}

# ci_validate NAME VOL LABEL: plain mysqld on the volume (plain_check), then checks against the live server's report
ci_validate() {
    local name=$1 vol=$2 label=$3 d="$OUT/$1"
    plain_check "$vol" "$label" || true
    if [ "$PLAIN_OK" = 1 ]; then ci_check "mysqlcheck ($label)" ok "$PLAIN_DETAIL"; else ci_check "mysqlcheck ($label)" fail "$PLAIN_DETAIL"; fi
    if [ "$(wc -l <"$d/report-live.txt")" -ne $((2 * TABLES)) ]; then
        ci_check "checksums equal live ($label)" fail "the live report is incomplete (see report-live.txt)"
    elif diff -u "$d/report-live.txt" "$d/report-$label.txt" >"$d/report-diff-$label.txt" 2>&1; then
        ci_check "checksums equal live ($label)" ok "$TABLES tables: rows and CHECKSUM TABLE"
    else
        ci_check "checksums equal live ($label)" fail "rows/CHECKSUM TABLE differ (see report-diff-$label.txt)"
    fi
}

# ci_finish NAME CHECK: the checks after the server is stopped (called by scenario for the ci profile)
ci_finish() {
    local name=$1 check=$2 d="$OUT/$1" rc vrc
    rc=$(cat "$d/exit-code.txt" 2>/dev/null)
    if [ "$rc" = 0 ]; then ci_check "mysqld clean shutdown" ok "exit code 0"; else ci_check "mysqld clean shutdown" fail "exit code ${rc:-?}"; fi
    if [ "$check" != none ]; then
        rc=$(cat "$d/xc.exit" 2>/dev/null)
        if [ "$rc" = 0 ]; then ci_check "xcheckfs clean exit" ok "exit code 0"; else ci_check "xcheckfs clean exit" fail "exit code ${rc:-?}"; fi
        vrc=$(sed -n 's/^verify exit code: //p' "$d/verify.txt")
        if [ "$vrc" = 0 ]; then
            ci_check "xcheckfs verify" ok "$(grep -v '^verify exit code' "$d/verify.txt" | tail -1)"
        else
            ci_check "xcheckfs verify" fail "exit code ${vrc:-?}: $(grep -v '^verify exit code' "$d/verify.txt" | tail -1)"
        fi
        ci_validate "$name" "xc-mysql-$name-s" secondary
    fi
    ci_validate "$name" "xc-mysql-$name-p" primary
}

# ci_run CONFIG DIR: one configuration, in a subshell (die only ends this config). Writes the files that ci_report reads.
ci_run() {
    local cfg=$1 dir=$2 check mode extra=""
    SCN=$dir
    trap '[ -n "${POLLER:-}" ] && kill "$POLLER" 2>/dev/null' EXIT
    case $cfg in
        baseline) check=none ;;
        *)
            mode=${cfg%-strict}
            check=$mode
            [ "$cfg" != "$mode" ] && extra="--serialize strict"
            [ "$mode" = paranoid ] && extra="--direct-io --attr-timeout 0 --entry-timeout 0 $extra"
            ;;
    esac
    [ -n "$CI_USER_EXTRA" ] && extra="$extra $CI_USER_EXTRA"
    extra=$(echo "$extra" | xargs)
    XC_EXTRA=$extra
    export XC_EXTRA
    if [ "$check" = none ]; then : >"$OUT/$dir/xcheckfs_args.txt"; else echo "--check $check -m log${extra:+ $extra}" >"$OUT/$dir/xcheckfs_args.txt"; fi
    [ -n "$CI_PREFATAL" ] && [ "$check" != none ] && die "$CI_PREFATAL"
    scenario "$dir" "$check" log plain plain ci
}

ci_report() {
    CI_OUT=$OUT CI_DIRS="$*" CI_TABLES=$TABLES CI_TABLE_SIZE=$TABLE_SIZE CI_THREADS=$THREADS \
        CI_T="$T_RW $T_UPD $T_DEL $T_INS" CI_CPUS="$MYSQL_CPUS $SB_CPUS" python3 - <<'PY'
import json, os, sys

out = os.environ["CI_OUT"]
tables, size, threads = (int(os.environ[k]) for k in ("CI_TABLES", "CI_TABLE_SIZE", "CI_THREADS"))
t_rw, t_upd, t_del, t_ins = (int(x) for x in os.environ["CI_T"].split())
WORKLOADS = ["oltp_read_write", "oltp_update_index", "oltp_delete", "oltp_insert"]


def read(path, default=""):
    try:
        with open(path) as f:
            return f.read()
    except OSError:
        return default


def num(s):
    if s in ("", "-"):
        return None
    f = float(s)
    return int(f) if f.is_integer() and "." not in s else f


runs = []
for d in os.environ["CI_DIRS"].split():
    base = os.path.join(out, d)
    cfg = read(os.path.join(base, "config.txt")).strip() or d
    checks = []
    for line in read(os.path.join(base, "checks.tsv")).splitlines():
        name, ok, detail = (line.split("\t") + ["", ""])[:3]
        checks.append({"name": name, "ok": ok == "ok", "detail": detail})
    workloads = []
    for line in read(os.path.join(base, "workloads.tsv")).splitlines():
        name, tps, count, errors, avg, p95, mx = line.split("\t")
        lat = {k: num(v) for k, v in (("avg", avg), ("p95", p95), ("max", mx)) if num(v) is not None}
        workloads.append({"name": name, "unit": "tps", "throughput": num(tps), "count": num(count),
                          "errors": num(errors), "latency_ms": lat})
    xc = None
    fatal = read(os.path.join(base, "fatal.txt")).strip()
    if cfg != "baseline":
        try:
            xc = json.loads(read(os.path.join(base, "xc-final-status.json")))
        except ValueError:
            xc = None
        if isinstance(xc, dict):
            n = xc.get("mismatches")
            checks.append({"name": "no mismatches", "ok": n == 0, "detail": str(n)})
        elif not fatal:
            xc = None
            checks.append({"name": "xcheckfs final status", "ok": False,
                           "detail": "no ctl status output (see xc.log, xc.stdout)"})
    if fatal:
        checks.append({"name": "run completed", "ok": False, "detail": fatal.replace("\n", "; ")})
    elif len(workloads) < len(WORKLOADS) and not any(c["name"].startswith("workload ") for c in checks):
        checks.append({"name": "run completed", "ok": False,
                       "detail": "%d of %d workloads completed" % (len(workloads), len(WORKLOADS))})
    ok = bool(checks) and all(c["ok"] for c in checks) and len(workloads) == len(WORKLOADS)
    try:
        wall = round(float(read(os.path.join(base, "wall_s.txt")).strip()), 1)
    except ValueError:
        wall = None
    runs.append({"config": cfg, "xcheckfs_args": read(os.path.join(base, "xcheckfs_args.txt")).strip(),
                 "ok": ok, "wall_s": wall, "workloads": workloads, "xcheckfs": xc, "checks": checks})

res = {
    "schema": 1,
    "app": "mysql",
    "title": "MySQL 8.4 / InnoDB, sysbench",
    "params": {"tables": tables, "table_size": size, "threads": threads,
               "oltp_read_write_s": t_rw, "oltp_update_index_s": t_upd, "oltp_delete_s": t_del,
               "oltp_insert_s": t_ins, "innodb_flush_method": "O_DIRECT", "innodb_buffer_pool": "256M",
               "mysql_cpus": os.environ["CI_CPUS"].split()[0], "sysbench_cpus": os.environ["CI_CPUS"].split()[1]},
    "runs": runs,
}
with open(os.path.join(out, "results.json"), "w") as f:
    json.dump(res, f, indent=2)
    f.write("\n")

for r in runs:
    tps = " ".join("%s=%s" % (w["name"].replace("oltp_", ""), w["throughput"]) for w in r["workloads"])
    print("%-16s %-4s %7ss  %s" % (r["config"], "ok" if r["ok"] else "FAIL", r["wall_s"], tps), file=sys.stderr)
    for c in r["checks"]:
        if not c["ok"]:
            print("    FAILED %s: %s" % (c["name"], c["detail"]), file=sys.stderr)
sys.exit(0 if runs and all(r["ok"] for r in runs) else 1)
PY
}

# ci: every config of CONFIGS in turn; always writes $OUT/results.json; exit 0 iff all runs ok
ci() {
    local cfg dir t0 rc dirs=() cfgs=() n=0
    CI_USER_EXTRA=${XC_EXTRA:-}
    CI_PREFATAL=""
    read -ra cfgs <<<"${CONFIGS:-baseline basic thorough}"
    if [ -z "${XCHECKFS:-}" ] || [ ! -x "$XCHECKFS" ]; then
        CI_PREFATAL="XCHECKFS is not an executable file"
    else
        mkdir -p "$OUT/bin"
        cp "$XCHECKFS" "$OUT/bin/xcheckfs"
    fi
    if ! (mknet && ci_images); then
        CI_PREFATAL="${CI_PREFATAL:+$CI_PREFATAL; }cannot prepare the Docker network/images"
        CI_ALLFATAL=1
    fi
    for cfg in "${cfgs[@]}"; do
        n=$((n + 1))
        dir=${cfg//[^A-Za-z0-9_-]/_}
        dirs+=("$dir")
        rm -rf "${OUT:?}/$dir"
        mkdir -p "$OUT/$dir"
        echo "$cfg" >"$OUT/$dir/config.txt"
        t0=$(date +%s.%N)
        if ! [[ $cfg =~ ^(baseline|(basic|thorough|paranoid)(-strict)?)$ ]]; then
            echo "unknown config '$cfg' (baseline, basic, thorough, paranoid, <mode>-strict)" >"$OUT/$dir/fatal.txt"
        elif [ "${CI_ALLFATAL:-0}" = 1 ]; then
            echo "$CI_PREFATAL" >"$OUT/$dir/fatal.txt"
        else
            log "ci: config $cfg ($n of ${#cfgs[@]})"
            (ci_run "$cfg" "$dir")
            rc=$?
            [ "$rc" = 0 ] || [ -s "$OUT/$dir/fatal.txt" ] || echo "config run ended with exit code $rc" >"$OUT/$dir/fatal.txt"
        fi
        date +%s.%N | awk -v t0="$t0" '{printf "%.1f\n", $1 - t0}' >"$OUT/$dir/wall_s.txt"
        log "ci: config $cfg took $(cat "$OUT/$dir/wall_s.txt") s"
        docker rm -f "$SRV" "$CHK" "xc-mysql-hold-$dir" "xc-mysql-sb-$dir" >/dev/null 2>&1
        docker volume rm "xc-mysql-$dir-p" "xc-mysql-$dir-s" >/dev/null 2>&1
    done
    ci_report "${dirs[@]}"
}

# ci_images: pull/build only what is missing
ci_images() {
    docker image inspect "$IMG" >/dev/null 2>&1 || docker pull -q "$IMG" >/dev/null || return 1
    docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull -q alpine:3.20 >/dev/null || return 1
    docker image inspect "$SBIMG" >/dev/null 2>&1 || docker build -q -f "$HERE/Dockerfile.sysbench" -t "$SBIMG" "$HERE" >/dev/null || return 1
}

images() {
    docker pull "$IMG" >/dev/null || die "pull $IMG"
    docker build -q -f "$HERE/Dockerfile.sysbench" -t "$SBIMG" "$HERE" >/dev/null || die "build $SBIMG"
}

summary() {
    [ -f "$OUT/summary.tsv" ] || die "no results in $OUT"
    printf '%-14s %-24s %10s %12s %10s %10s %6s\n' scenario phase tps qps p95_ms avg_ms errors
    awk -F'\t' '{printf "%-14s %-24s %10s %12s %10s %10s %6s\n", $1, $2, $3, $4, $5, $6, $7}' "$OUT/summary.tsv"
}

clean() {
    local x
    for x in $(docker ps -aq --filter 'name=xc-mysql-'); do docker rm -fv "$x" >/dev/null; done
    for x in $(docker volume ls -q --filter 'name=xc-mysql-'); do docker volume rm "$x" >/dev/null; done
    docker network rm "$NET" >/dev/null 2>&1
    docker rmi "$SBIMG" >/dev/null 2>&1
    log "removed xc-mysql-* containers, volumes, network and image"
}

main() {
    mkdir -p "$OUT"
    local cmd=${1:-}
    case "$cmd" in
        images) images ;;
        ci) ci; exit $? ;;
        baseline) need_bin; mknet; scenario baseline none log plain plain main ;;
        baselinebuf) need_bin; mknet; MYSQLD_ARGS+=(--innodb-flush-method=fsync); scenario baselinebuf none log plain plain main ;;
        paranoid) need_bin; mknet; XC_EXTRA=${XC_EXTRA:---direct-io --attr-timeout 0 --entry-timeout 0}; export XC_EXTRA
            scenario paranoid paranoid log plain plain main ;;
        basic) need_bin; mknet; scenario basic basic log plain plain main ;;
        thorough) need_bin; mknet; scenario thorough thorough log plain plain main ;;
        compress)
            need_bin; mknet
            TABLES=${TABLES_C:-8}; TABLE_SIZE=${TABLE_SIZE_C:-100000}
            scenario cbaseline none log tmpfs tmpfs compress
            scenario cthorough thorough log tmpfs tmpfs compress
            scenario cmixed thorough log tmpfs plain compress ;;
        crashloop) need_bin; mknet; TABLES=${TABLES_C:-8}; TABLE_SIZE=${TABLE_SIZE_C:-100000}; crashloop ;;
        crash) need_bin; mknet; TABLES=${TABLES_C:-8}; TABLE_SIZE=${TABLE_SIZE_C:-100000}; crash ;;
        validate) need_bin; mknet; validate "${2:?scenario name}" ;;
        all)
            "$0" images && "$0" baseline && "$0" basic && "$0" thorough && "$0" validate thorough &&
                "$0" compress && "$0" crash && "$0" crashloop && "$0" summary ;;
        summary) summary ;;
        clean) clean ;;
        *) sed -n '2,/^set /p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 2 ;;
    esac
}

main "$@"
