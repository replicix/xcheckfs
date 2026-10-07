# External test suites

These scripts are opt-in: `cargo test` never runs them. All of them mount two scratch directories (a primary and a secondary, by default on
the same file system) through the `xcheckfs` binary with `--check thorough --on-mismatch log`, run a third-party
POSIX conformance suite (or, for the stress lane, a load) inside the mount, ask the running mount for its mismatch count (`xcheckfs ctl <mnt> status`),
unmount it, and exit non-zero if the suite failed or xcheckfs recorded any mismatch. The environment variables
(`XCHECKFS`, `CHECK`, `KEEP`, ...) are listed at the top of each script.

`run-libfuse-syscalls.sh` compiles `test/test_syscalls.c` from a libfuse source tree (`$LIBFUSE_SRC`, default
`/tmp/libfuse`; the meson-generated `fuse_config.h` is replaced by a two-line stub) into the cache directory and runs it
against the mount as the current user; no root and no network are needed. `UNLINKED_TEST=1` additionally runs the
open-but-unlinked tests, `TEST_ARGS="3 -17"` selects or skips individual test numbers.

`run-pjdfstest.sh` needs root (the suite changes users and creates device nodes). It clones and builds pjdfstest into
`$XDG_CACHE_HOME/xcheckfs-tests/pjdfstest` unless `$PJDFSTEST_DIR` already holds a built copy, mounts with
`-o suid,dev`, and runs `prove -rv` over the whole suite (or `$PJD_TESTS`, e.g. `"chmod rename"`) with the mount as the
working directory. Failures that the primary file system itself causes (unsupported features such as ACLs) show up as
prove failures; what xcheckfs adds is the mismatch check between the two trees.

`run-stress.sh` is the stress lane: a load test that hunts false positives (two healthy file systems must never
mismatch) and checks data integrity end to end. It mounts with `--check thorough --on-mismatch log` and runs, inside
the mount, (1) `fio` jobs that write and verify (sequential sha1, random 4k-256k crc32c, two concurrent writers, a
mixed randrw job, the `mmap` engine, `direct=1`) and (2) one `stress-ng` filesystem stressor after another (dentry,
dir, dirdeep, rename, symlink, link, hdd, io, fallocate, xattr, chmod, chown, utime, lockf, fcntl, flock, mmap, seek,
readahead, copy-file, ...; `--verify`, 2 workers, `$STRESS_TIMEOUT` seconds each), then a mixed run of several at
once. Afterwards `ctl status` must show 0 mismatches (otherwise `ctl mismatches 50` and the log lines are printed),
and after the unmount `xcheckfs verify PRIMARY SECONDARY` must report 0 differences. The summary lists every stage with
its duration and mismatch count. Needs `fio` and `stress-ng` (`STRESS_FIO=0` / `STRESS_NG=0` skip a part). Knobs:
`STRESS_SIZE` (fio data set, default 32M), `STRESS_TIMEOUT` (per stressor, default 10 s), `STRESS_NG_ONLY` /
`STRESS_NG_SKIP`, `MOUNT_OPTS="--attr-timeout 0 --entry-timeout 0"` (more operations reach xcheckfs), and
`PRIMARY_BASE` / `SECONDARY_BASE` to put the two trees on different file systems (e.g. `/dev/shm` against `/var/tmp`).
When primary and secondary are on different file systems (detected by type, or `STRESS_CROSS_FS=1`) the lane mounts
with `--no-dir-nlink` and leaves out the stressors whose outcome legitimately depends on the file system
(`fallocate`, `fpunch`, `iomix`: fallocate modes; `xattr` with btrfs; `filename` with ZFS) and, on SELinux hosts, the
xattr comparison of the final `verify`. Every step is time-bounded; a hung step aborts the FUSE connection of its own mount through
`/sys/fs/fuse/connections/N/abort` and fails the run. `lockf` runs blocking: its workers form lock cycles (answered
`EDEADLK`) and are stopped by signals while they wait (`EINTR`). Roughly 3 minutes at `STRESS_TIMEOUT=3`, 8 at the default;
CI runs it with `STRESS_SIZE=16M STRESS_TIMEOUT=5`.
