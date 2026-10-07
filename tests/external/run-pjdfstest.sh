#!/usr/bin/env bash
# Runs the pjdfstest POSIX file system test suite (`prove -rv`) inside an xcheckfs mount of two scratch directories
# and fails if prove fails OR xcheckfs recorded any mismatch between primary and secondary. Needs root (the suite
# switches users, creates device nodes, ...) and network access the first time (pjdfstest is cloned and built into
# the cache directory unless PJDFSTEST_DIR already holds a built copy).
#
# Environment:
#   PJDFSTEST_DIR   a built pjdfstest checkout (default $XDG_CACHE_HOME/xcheckfs-tests/pjdfstest, cloned if missing)
#   PJDFSTEST_REPO  where to clone from (default https://github.com/pjd/pjdfstest)
#   XCHECKFS        xcheckfs binary (default: target/release|debug/xcheckfs of this repo, built if missing)
#   CHECK           basic | thorough | paranoid (default thorough)
#   PJD_TESTS       tests to run, relative to the pjdfstest tests/ directory (default: all), e.g. "chmod rename"
#   KEEP            1: keep the work directory
set -u -o pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
check=${CHECK:-thorough}
cache=${XDG_CACHE_HOME:-$HOME/.cache}/xcheckfs-tests
pjd=${PJDFSTEST_DIR:-$cache/pjdfstest}

die() { echo "ERROR: $*" >&2; exit 2; }

[ "$(id -u)" = 0 ] || die "pjdfstest must run as root (try: sudo -E $0)"
command -v fusermount3 >/dev/null || die "fusermount3 not found"
command -v prove >/dev/null || die "prove (perl) not found"

bin=${XCHECKFS:-}
if [ -z "$bin" ]; then
    for c in "$repo/target/release/xcheckfs" "$repo/target/debug/xcheckfs"; do [ -x "$c" ] && { bin=$c; break; }; done
fi
if [ -z "$bin" ]; then
    (cd "$repo" && cargo build --offline --quiet) || die "cargo build failed"
    bin=$repo/target/debug/xcheckfs
fi
[ -x "$bin" ] || die "xcheckfs binary $bin not found"

if [ ! -x "$pjd/pjdfstest" ]; then
    mkdir -p "$(dirname "$pjd")"
    [ -d "$pjd/.git" ] || git clone --depth 1 "${PJDFSTEST_REPO:-https://github.com/pjd/pjdfstest}" "$pjd" || die "cloning pjdfstest failed"
    (cd "$pjd" && autoreconf -ifs && ./configure && make pjdfstest) || die "building pjdfstest failed (needs autoconf, automake, make, cc)"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/xcheckfs-pjd.XXXXXX")
mnt=$work/mnt primary=$work/primary secondary=$work/secondary log=$work/xcheckfs.log
mkdir -p "$mnt" "$primary" "$secondary"
pid=

cleanup() {
    if mountpoint -q "$mnt" 2>/dev/null; then
        fusermount3 -u "$mnt" 2>/dev/null || fusermount3 -uz "$mnt" 2>/dev/null
    fi
    [ -n "$pid" ] && { kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; }
    if [ "${KEEP:-0}" = 1 ]; then echo "work directory kept: $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

# suid/dev: the suite tests set-id bits and device nodes. Root mounts get allow_other by default.
"$bin" mount --check "$check" --on-mismatch log --log-file "$log" --no-color -o suid,dev "$mnt" "$primary" "$secondary" &
pid=$!
for _ in $(seq 1 150); do
    mountpoint -q "$mnt" && break
    kill -0 "$pid" 2>/dev/null || { cat "$log" 2>/dev/null; die "xcheckfs exited before the mount appeared"; }
    sleep 0.1
done
mountpoint -q "$mnt" || die "mount did not appear within 15 s"

# pjdfstest finds its helper binary and misc.sh relative to the tests directory; it runs in the file system under test.
tests=()
if [ -n "${PJD_TESTS:-}" ]; then
    for t in $PJD_TESTS; do tests+=("$pjd/tests/$t"); done
else
    tests=("$pjd/tests")
fi
echo "== prove -rv ${tests[*]} (in $mnt, check=$check)"
(cd "$mnt" && prove -rv "${tests[@]}")
rc=$?

status=$("$bin" ctl "$mnt" status) || die "xcheckfs ctl status failed"
mism=$(printf '%s\n' "$status" | grep -o '"mismatches": *[0-9]*' | grep -o '[0-9]*$')
echo "== prove exit code: $rc; xcheckfs mismatches: ${mism:-?}"
if [ "${mism:-1}" != 0 ]; then
    echo "== recorded mismatches:"
    "$bin" ctl "$mnt" mismatches 50
fi

fusermount3 -u "$mnt" || die "unmount failed"
wait "$pid" 2>/dev/null; pid=

[ "$rc" = 0 ] || { echo "FAIL: pjdfstest failed"; exit 1; }
[ "${mism:-1}" = 0 ] || { echo "FAIL: xcheckfs reported mismatches"; exit 1; }
echo "OK"
