#!/usr/bin/env bash
# Stress lane: heavy, realistic load through an xcheckfs mount of two scratch directories. Two healthy file systems
# must never produce a mismatch (false positives), and the data written must survive end to end.
#
#   1. fio with end-to-end verification (sequential, random 4k-256k, two concurrent writers, mixed randrw, mmap,
#      direct=1),
#   2. stress-ng filesystem stressors (metadata and data churn, locks, xattrs, mmap, ...), one after another,
#   3. `xcheckfs ctl status` must report 0 mismatches, then the mount is unmounted and `xcheckfs verify PRIMARY
#      SECONDARY` must report 0 differences.
# The mount runs with --on-mismatch log: mismatches are counted, not repaired.
#
# Environment:
#   XCHECKFS         xcheckfs binary (default: target/debug/xcheckfs of this repo, built with cargo if missing)
#   CHECK            basic | thorough | paranoid (default thorough)
#   STRESS_BASE      directory for the work files (default $TMPDIR or /tmp); a fresh subdirectory is created in it
#   PRIMARY_BASE     put the primary tree in a fresh subdirectory of this directory instead (another file system)
#   SECONDARY_BASE   same for the secondary tree
#   STRESS_SIZE      size of each fio data set (default 32M)
#   STRESS_TIMEOUT   seconds each stress-ng stressor runs (default 10)
#   FIO_TIMEOUT      hard limit in seconds for each fio job (default 300)
#   STRESS_FIO       0: skip the fio part
#   STRESS_NG        0: skip the stress-ng part
#   STRESS_NG_ONLY   space separated stressor names: run only these
#   STRESS_NG_SKIP   space separated stressor names: do not run these
#   STRESS_WORKERS   stress-ng workers per stressor (default 2)
#   STRESS_CROSS_FS  1/0: force "the trees are on different file systems" (default: detected)
#   MOUNT_OPTS       extra arguments for `xcheckfs mount` (e.g. "--threads 4 --direct-io")
#   KEEP             1: keep the work directory (also kept on failure when KEEP=failed)
#
# Exit status: 0 all good, 1 a step failed or xcheckfs recorded a mismatch or `verify` found a difference,
# 2 the environment is unusable. Every step is time-bounded; a hung FUSE connection is aborted.
set -u -o pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
check=${CHECK:-thorough}
size=${STRESS_SIZE:-32M}
ng_secs=${STRESS_TIMEOUT:-10}
fio_secs=${FIO_TIMEOUT:-300}
workers=${STRESS_WORKERS:-2}

die() { echo "ERROR: $*" >&2; exit 2; }

for tool in fusermount3 timeout mountpoint; do command -v "$tool" >/dev/null || die "$tool not found"; done
[ -e /dev/fuse ] || die "/dev/fuse not found"
[ "${STRESS_FIO:-1}" = 0 ] || command -v fio >/dev/null || die "fio not found (STRESS_FIO=0 skips it)"
[ "${STRESS_NG:-1}" = 0 ] || command -v stress-ng >/dev/null || die "stress-ng not found (STRESS_NG=0 skips it)"

bin=${XCHECKFS:-$repo/target/debug/xcheckfs}
if [ ! -x "$bin" ]; then
    [ -z "${XCHECKFS:-}" ] || die "XCHECKFS=$bin is not executable"
    (cd "$repo" && cargo build --quiet) || die "cargo build failed"
fi
[ -x "$bin" ] || die "xcheckfs binary $bin not found"

base=$(mktemp -d "${STRESS_BASE:-${TMPDIR:-/tmp}}/xcheckfs-stress.XXXXXX") || die "cannot create the work directory"
mnt=$base/mnt log=$base/xcheckfs.log
mkdir -p "$mnt"
if [ -n "${PRIMARY_BASE:-}" ]; then
    primary=$(mktemp -d "$PRIMARY_BASE/xcheckfs-stress-primary.XXXXXX") || die "cannot create a directory in PRIMARY_BASE"
else
    primary=$base/primary; mkdir -p "$primary"
fi
if [ -n "${SECONDARY_BASE:-}" ]; then
    secondary=$(mktemp -d "$SECONDARY_BASE/xcheckfs-stress-secondary.XXXXXX") || die "cannot create a directory in SECONDARY_BASE"
else
    secondary=$base/secondary; mkdir -p "$secondary"
fi

# mktemp -d creates 0700; the trees must look alike at their roots.
chmod 755 "$primary" "$secondary" "$mnt"

# Different file systems differ legitimately ([Testing](docs/how-to-guides/development/TESTING.md#pitfalls)); with
# STRESS_CROSS_FS=0|1 the detection (by file system type) can be forced. Then: directory link counts are not compared
# (btrfs always reports 1), and the stressors that depend on fallocate modes (tmpfs refuses ZERO_RANGE and the like),
# on large extended attributes (btrfs: ENOSPC) or on invalid UTF-8 names (ZFS: EILSEQ) are skipped.
pfs=$(stat -f -c %T "$primary" 2>/dev/null) sfs=$(stat -f -c %T "$secondary" 2>/dev/null)
cross=${STRESS_CROSS_FS:-}
[ -n "$cross" ] || { [ "$pfs" != "$sfs" ] && cross=1 || cross=0; }
mount_extra=${MOUNT_OPTS:-} verify_extra=
skip_extra=
if [ "$cross" = 1 ]; then
    mount_extra="$mount_extra --no-dir-nlink" verify_extra=--no-dir-nlink
    skip_extra="fallocate fpunch iomix"
    case "$pfs $sfs" in *btrfs*) skip_extra="$skip_extra xattr" ;; esac     # setxattr of large values: ENOSPC
    case "$pfs $sfs" in *zfs*) skip_extra="$skip_extra filename" ;; esac    # invalid UTF-8 names: EILSEQ
    # security.selinux labels follow the location (tmpfs: user_tmp_t, home: user_home_t); the live check does not see them
    [ -e /sys/fs/selinux/enforce ] && verify_extra="$verify_extra --no-xattrs"
    echo "== different file systems ($pfs, $sfs): --no-dir-nlink; skipping stressors: $skip_extra"
fi
# fpunch --verify reads back a range after FALLOC_FL_ZERO_RANGE and expects zeros; where ZERO_RANGE is refused
# (tmpfs), stress-ng falls back to a plain allocation that zeroes nothing and fails every run, with or without
# xcheckfs. There fpunch runs without --verify: the fallocate traffic through the mount is still checked.
noverify=
zr_ok() { local f=$1/.xcheckfs-zero-range rc; printf 'x' > "$f" && fallocate -z -o 0 -l 1 "$f" 2>/dev/null; rc=$?; rm -f "$f"; return "$rc"; }
if command -v fallocate >/dev/null && ! { zr_ok "$primary" && zr_ok "$secondary"; }; then
    noverify=fpunch
    echo "== FALLOC_FL_ZERO_RANGE not supported ($pfs, $sfs): stress-ng fpunch runs without --verify"
fi


pid=
minor=
failed=0
skipped=0
declare -a summary=()

cleanup() {
    local rc=$?
    if mountpoint -q "$mnt" 2>/dev/null; then
        timeout -k 2 20 fusermount3 -u "$mnt" 2>/dev/null || { abort_conn; timeout -k 2 10 fusermount3 -uz "$mnt" 2>/dev/null; }
    fi
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null
        for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
        kill -9 "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    fi
    if [ "${KEEP:-0}" = 1 ] || { [ "${KEEP:-0}" = failed ] && [ "$failed" != 0 ]; }; then
        echo "work directory kept: $base (primary $primary, secondary $secondary)"
    else
        rm -rf "$base" "$primary" "$secondary"
    fi
    return $rc
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

# Aborts the FUSE connection of OUR mount (it frees processes stuck in uninterruptible sleep on it). Needs the
# connection number recorded right after the mount came up.
abort_conn() {
    [ -n "$minor" ] || return 0
    # Only while our mount is still the owner of that connection number (numbers are reused).
    awk -v m="$mnt" -v d="0:$minor" '$5 == m && $3 == d { f = 1 } END { exit !f }' /proc/self/mountinfo || return 0
    echo "!! aborting FUSE connection $minor" >&2
    echo 1 > "/sys/fs/fuse/connections/$minor/abort" 2>/dev/null || true
}

# bounded SECS CMD...: runs CMD with a time limit; on a timeout aborts the connection (the command may be blocked in
# the kernel on our mount) and returns 124.
bounded() {
    local secs=$1 rc
    shift
    timeout -k 5 "$secs" "$@"
    rc=$?
    if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
        echo "!! TIMEOUT after ${secs}s: $*" >&2
        abort_conn
        return 124
    fi
    return "$rc"
}

ctl() { bounded 30 "$bin" ctl "$mnt" "$@"; }

# status_field NAME: a numeric field of the status object
status_field() {
    ctl status 2>/dev/null | grep -o "\"$1\": *[0-9]*" | grep -o '[0-9]*$'
}

mismatches_now() {
    local m
    m=$(status_field mismatches) || true
    echo "${m:-?}"
}

# stage NAME CMD...: runs a step, records its duration and the mismatches it caused.
stage_no=0
stage() {
    local name=$1 start end rc mb ma ops
    shift
    stage_no=$((stage_no + 1))
    mb=$(mismatches_now)
    start=$(date +%s.%N)
    echo "== [$stage_no] $name"
    "$@"
    rc=$?
    end=$(date +%s.%N)
    ma=$(mismatches_now)
    ops=$(status_field ops)
    local dur result=ok
    dur=$(awk -v a="$start" -v b="$end" 'BEGIN { printf "%.1f", b - a }')
    if [ "$rc" != 0 ]; then result="FAILED($rc)"; failed=1; fi
    if [ "$ma" != "$mb" ]; then result="$result MISMATCHES($mb->$ma)"; failed=1; fi
    echo "== [$stage_no] $name: $result in ${dur}s (ops so far ${ops:-?}, mismatches $ma)"
    summary+=("$(printf '%-34s %-28s %7ss  mismatches %s' "$name" "$result" "$dur" "$ma")")
    return 0
}

# -- mount ---------------------------------------------------------------------------------------------------------
echo "== xcheckfs $bin; check=$check; primary $primary; secondary $secondary; mount $mnt"
# shellcheck disable=SC2086
"$bin" mount --check "$check" --on-mismatch log --log-file "$log" --no-color $mount_extra "$mnt" "$primary" "$secondary" &
pid=$!
for _ in $(seq 1 150); do
    mountpoint -q "$mnt" && break
    kill -0 "$pid" 2>/dev/null || { cat "$log" 2>/dev/null; pid=; die "xcheckfs exited before the mount appeared"; }
    sleep 0.1
done
mountpoint -q "$mnt" || die "mount did not appear within 15 s"
dev=$(stat -c %d "$mnt") && minor=$(( (dev & 255) | ((dev >> 12) & 0xfff00) ))
total_start=$(date +%s)

# -- fio -----------------------------------------------------------------------------------------------------------
# fio_job NAME ARGS...: one fio job in $mnt/fio. Every job writes (verification header + checksum per block) and
# reads everything back; a verification failure is fatal for the job.
fio_job() {
    local name=$1
    shift
    bounded "$fio_secs" fio --name="$name" --directory="$mnt/fio" --size="$size" --verify_fatal=1 --do_verify=1 --verify_state_save=0 \
        --group_reporting --output-format=terse --terse-version=3 --eta=never "$@" > "$base/fio-$name.out" 2>&1
    local rc=$?
    if [ "$rc" != 0 ]; then
        echo "fio $name failed (exit $rc):"
        tail -n 20 "$base/fio-$name.out"
    fi
    return "$rc"
}

run_fio() {
    mkdir -p "$mnt/fio"
    stage "fio sequential write+verify sha1" fio_job seq --rw=write --bs=128k --verify=sha1
    stage "fio random 4k-256k write+verify" fio_job rand --rw=randwrite --bsrange=4k-256k --verify=crc32c --randrepeat=0
    # Two writers into disjoint halves of one file (the file is twice $size), plus a reader verifying afterwards.
    stage "fio 2 concurrent writers+verify" fio_job shared --filename=shared.dat --rw=randwrite \
        --bsrange=4k-64k --verify=sha1 --numjobs=2 --offset_increment="$size" --randrepeat=0
    stage "fio 2 writers, separate files" fio_job sepfiles --rw=write --bs=64k --verify=crc32c --numjobs=2 --new_group
    stage "fio mixed randrw verify" fio_job mixed --rw=randrw --rwmixread=60 --bsrange=4k-128k --verify=crc32c \
        --verify_backlog=32 --randrepeat=0 --ioengine=psync
    stage "fio mmap engine write+verify" fio_job mmapjob --ioengine=mmap --rw=randwrite --bs=16k --verify=crc32c --randrepeat=0
    stage "fio direct=1 write+verify" fio_job direct --direct=1 --rw=write --bs=64k --verify=sha1
    stage "fio direct=1 random write+verify" fio_job directrand --direct=1 --rw=randwrite --bsrange=4k-64k --verify=crc32c \
        --randrepeat=0
    stage "fio overwrite+fsync+verify" fio_job fsync --rw=randwrite --bs=8k --verify=crc32c --fsync=16 --randrepeat=0 \
        --size="$size"
}

# -- stress-ng -----------------------------------------------------------------------------------------------------
# Name|extra options|needs root. `--verify` is added everywhere (stressors without verification ignore it), except to
# fpunch where ZERO_RANGE is refused (see above).
# lockf runs blocking: its workers form lock cycles (answered EDEADLK, like the kernel does) and are stopped with
# signals while waiting (EINTR).
# Not run: stressors about ioctls (chattr, fiemap, file-ioctl, inode-flags, verity: ioctl is not mirrored),
# acl, handle, bind-mount, binderfs, procfs and the ones that do not touch files.
ng_stressors=(
    "dentry||"
    "dir||"
    "dirdeep|--dirdeep-dirs 4 --dirdeep-inodes 5000|"
    "dirmany|--dirmany-bytes 0 --dirmany-ops 3000|"
    "rename||"
    "symlink||"
    "link||"
    "hdd:seq|--hdd-bytes $size --hdd-opts wr-seq,rd-seq,fsync|"
    "hdd:rnd|--hdd-bytes $size --hdd-opts wr-rnd,rd-rnd,fdatasync|"
    "hdd:direct|--hdd-bytes $size --hdd-opts wr-seq,rd-seq,direct,sync|"
    "io||"
    "iomix|--iomix-bytes 8M|"
    "sync-file||"
    "fallocate|--fallocate-bytes 16M|"
    "fpunch||"
    "fsize||"
    "fstat||"
    "getdent||"
    "filename||"
    "touch||"
    "open||"
    "access||"
    "mknod||"
    "metamix||"
    "xattr||"
    "chmod||"
    "chown||"
    "utime||"
    "lockf||"
    "fcntl||"
    "flock||"
    "locka||"
    "lockofd||"
    "mmap:async|--mmap-file --mmap-bytes 16M --mmap-async|"
    "mmap:check|--mmap-file --mmap-bytes 16M --mmap-write-check|"
    "seek|--seek-punch --seek-size 16M|"
    "readahead|--readahead-bytes 16M|"
    "copy-file|--copy-file-bytes 128M|"
    "inotify||"
    "dnotify||"
    "lease||"
)

ng_wanted() {
    local n=$1 s
    if [ -n "${STRESS_NG_ONLY:-}" ]; then
        for s in $STRESS_NG_ONLY; do [ "$s" = "$n" ] && return 0; done
        return 1
    fi
    for s in ${STRESS_NG_SKIP:-} $skip_extra; do [ "$s" = "$n" ] && return 1; done
    return 0
}

# ng_run NAME OPTIONS...: one stressor with $workers workers inside the mount. stress-ng exit codes: 0 ok;
# 3 (could not initialise: missing permission or feature) and 5 (not implemented) are skips; anything else fails.
ng_run() {
    local name=$1 rc verify=--verify
    shift
    [ "$name" = "$noverify" ] && verify=
    # shellcheck disable=SC2086
    bounded $((ng_secs + 120)) stress-ng --"$name" "$workers" --timeout "${ng_secs}s" $verify --temp-path "$mnt/stress-ng" \
        --metrics --times "$@" > "$base/stress-ng-$name.out" 2>&1
    rc=$?
    case "$rc" in
        0) ;;
        3 | 5)
            echo "stress-ng $name: SKIPPED (exit $rc)"
            grep -E 'skip|not implemented|permission|ENOTSUP|EOPNOTSUPP|failed' "$base/stress-ng-$name.out" | head -n 3
            skipped=$((skipped + 1))
            rc=0 ;;
        *) echo "stress-ng $name failed (exit $rc):"; tail -n 25 "$base/stress-ng-$name.out" ;;
    esac
    return "$rc"
}

run_ng() {
    local entry label name opts root
    mkdir -p "$mnt/stress-ng"
    for entry in "${ng_stressors[@]}"; do
        label=${entry%%|*}
        name=${label%%:*}
        opts=${entry#*|}
        root=${opts##*|}
        opts=${opts%|*}
        ng_wanted "$name" || continue
        if [ -n "$root" ] && [ "$(id -u)" != 0 ]; then echo "== skipping $name (needs root)"; continue; fi
        # shellcheck disable=SC2086
        stage "stress-ng $label" ng_run "$name" $opts
    done
    # A few at once: metadata and data churn racing each other in the same tree.
    if ng_wanted mix; then
        local mixargs=(--dentry 2 --rename 2 --link 1 --symlink 1 --hdd 2 --hdd-bytes "$size" --lockf 2)
        ng_wanted xattr && mixargs+=(--xattr 2)
        stage "stress-ng mix" bounded $((ng_secs + 120)) stress-ng "${mixargs[@]}" --timeout "$((ng_secs * 2))s" --verify \
            --temp-path "$mnt/stress-ng" --metrics
    fi
    rm -rf "${mnt:?}/stress-ng" 2>/dev/null
}

[ "${STRESS_FIO:-1}" = 0 ] || run_fio
[ "${STRESS_NG:-1}" = 0 ] || run_ng

# -- result --------------------------------------------------------------------------------------------------------
echo "== load finished after $(( $(date +%s) - total_start ))s"
status=$(ctl status) || { failed=1; echo "ERROR: xcheckfs ctl status failed"; }
mism=$(printf '%s\n' "$status" | grep -o '"mismatches": *[0-9]*' | grep -o '[0-9]*$')
ops=$(printf '%s\n' "$status" | grep -o '"ops": *[0-9]*' | grep -o '[0-9]*$')
echo "== xcheckfs: ${ops:-?} operations, ${mism:-?} mismatches"
if [ "${mism:-1}" != 0 ]; then
    failed=1
    echo "== recorded mismatches:"
    ctl mismatches 50
    echo "== log: $log"
    grep 'MISMATCH' "$log" | head -n 50
fi

# Unmount (bounded; an unresponsive mount is aborted).
if ! timeout -k 2 30 fusermount3 -u "$mnt"; then
    echo "!! unmount failed or hung"; failed=1
    abort_conn
    timeout -k 2 10 fusermount3 -uz "$mnt" 2>/dev/null
fi
if [ -n "$pid" ]; then
    for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$pid" 2>/dev/null; then echo "!! xcheckfs did not exit after the unmount"; failed=1; kill "$pid" 2>/dev/null; fi
    wait "$pid" 2>/dev/null
    mrc=$?
    pid=
    # Exit 3: xcheckfs recorded a mismatch (already counted above); anything else non-zero is an error.
    if [ "$mrc" != 0 ] && [ "$mrc" != 3 ]; then echo "!! xcheckfs mount exited with status $mrc"; failed=1; fi
fi
minor=

echo "== xcheckfs verify"
vstart=$(date +%s)
# shellcheck disable=SC2086
bounded 600 "$bin" verify $verify_extra --max-reports 50 "$primary" "$secondary"
vrc=$?
echo "== verify exit code $vrc in $(( $(date +%s) - vstart ))s"
[ "$vrc" = 0 ] || failed=1

echo
echo "== summary (check=$check, size=$size, stress-ng ${ng_secs}s x $workers workers)"
printf '%s\n' "${summary[@]}"
echo "== ops: ${ops:-?}; mismatches: ${mism:-?}; verify: exit $vrc; stress-ng stressors skipped: $skipped; total $(( $(date +%s) - total_start ))s"
if [ "$failed" != 0 ]; then echo "FAIL"; exit 1; fi
echo "OK"
