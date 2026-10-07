# External test suites

These scripts are opt-in: `cargo test` never runs them. Both mount two scratch directories (a primary and a secondary on
the same file system) through the `xcheckfs` binary with `--check thorough --on-mismatch log`, run a third-party
POSIX conformance suite inside the mount, ask the running mount for its mismatch count (`xcheckfs ctl <mnt> status`),
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
