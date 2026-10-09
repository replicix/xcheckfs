#!/usr/bin/env bash
# Runs libfuse's test_syscalls (a POSIX syscall conformance test) against an xcheckfs mount of two scratch
# directories and fails if the test fails OR xcheckfs recorded any mismatch between primary and secondary.
#
# Needs network access the first time (libfuse is cloned into the cache directory unless LIBFUSE_SRC points to a
# checkout).
#
# Environment:
#   LIBFUSE_SRC   libfuse source tree (default $XDG_CACHE_HOME/xcheckfs-tests/libfuse, cloned if missing)
#   LIBFUSE_REPO  where to clone from (default https://github.com/libfuse/libfuse)
#   XCHECKFS      xcheckfs binary (default: target/release|debug/xcheckfs of this repo, built if missing)
#   CHECK         basic | thorough | paranoid (default thorough)
#   TEST_ARGS     extra arguments for test_syscalls, e.g. "3 -17" (select/skip tests)
#   UNLINKED_TEST 1: also run the open-but-unlinked tests (test_syscalls -u)
#   KEEP          1: keep the work directory
set -u -o pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
check=${CHECK:-thorough}
cache=${XDG_CACHE_HOME:-$HOME/.cache}/xcheckfs-tests
src=${LIBFUSE_SRC:-$cache/libfuse}

die() { echo "ERROR: $*" >&2; exit 2; }

if [ -z "${LIBFUSE_SRC:-}" ] && [ ! -d "$src/.git" ]; then
    mkdir -p "$cache"
    git clone --depth 1 "${LIBFUSE_REPO:-https://github.com/libfuse/libfuse}" "$src" || die "cloning libfuse failed"
fi

[ -f "$src/test/test_syscalls.c" ] || die "$src/test/test_syscalls.c not found (set LIBFUSE_SRC to a libfuse checkout)"
command -v fusermount3 >/dev/null || die "fusermount3 not found"
command -v cc >/dev/null || die "no C compiler (cc)"

bin=${XCHECKFS:-}
if [ -z "$bin" ]; then
    for c in "$repo/target/release/xcheckfs" "$repo/target/debug/xcheckfs"; do [ -x "$c" ] && { bin=$c; break; }; done
fi
if [ -z "$bin" ]; then
    (cd "$repo" && cargo build --locked --quiet) || die "cargo build failed"
    bin=$repo/target/debug/xcheckfs
fi
[ -x "$bin" ] || die "xcheckfs binary $bin not found"

# test_syscalls.c includes the meson-generated fuse_config.h: a stub with the two feature macros it checks is enough.
mkdir -p "$cache"
stub=$cache/include
mkdir -p "$stub"
printf '#define HAVE_COPY_FILE_RANGE 1\n#define HAVE_STATX 1\n' > "$stub/fuse_config.h"
test_bin=$cache/test_syscalls
if [ ! -x "$test_bin" ] || [ "$src/test/test_syscalls.c" -nt "$test_bin" ]; then
    cc -D_GNU_SOURCE -O1 -Wall -Wno-unused-result -I"$stub" -o "$test_bin" "$src/test/test_syscalls.c" \
        || die "compiling test_syscalls.c failed"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/xcheckfs-libfuse.XXXXXX")
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

"$bin" mount --check "$check" --on-mismatch log --log-file "$log" --no-color "$mnt" "$primary" "$secondary" &
pid=$!
for _ in $(seq 1 150); do
    mountpoint -q "$mnt" && break
    kill -0 "$pid" 2>/dev/null || { cat "$log" 2>/dev/null; die "xcheckfs exited before the mount appeared"; }
    sleep 0.1
done
mountpoint -q "$mnt" || die "mount did not appear within 15 s"

# (The :realdir tests modify the backing directory behind the mount's back, which by definition breaks the mirror:
# they are not usable here. -u, the open-but-unlinked tests, is: UNLINKED_TEST=1.)
args=("$mnt")
if [ "${UNLINKED_TEST:-0}" = 1 ]; then
    args+=(-u)
fi
# shellcheck disable=SC2206
args+=(${TEST_ARGS:-})
echo "== test_syscalls ${args[*]} (check=$check)"
"$test_bin" "${args[@]}"
rc=$?

status=$("$bin" ctl "$mnt" status) || die "xcheckfs ctl status failed"
mism=$(printf '%s\n' "$status" | grep -o '"mismatches": *[0-9]*' | grep -o '[0-9]*$')
echo "== test_syscalls exit code: $rc; xcheckfs mismatches: ${mism:-?}"
if [ "${mism:-1}" != 0 ]; then
    echo "== recorded mismatches:"
    "$bin" ctl "$mnt" mismatches 50
fi

fusermount3 -u "$mnt" || die "unmount failed"
wait "$pid" 2>/dev/null; pid=

[ "$rc" = 0 ] || { echo "FAIL: test_syscalls failed"; exit 1; }
[ "${mism:-1}" = 0 ] || { echo "FAIL: xcheckfs reported mismatches"; exit 1; }
echo "OK"
