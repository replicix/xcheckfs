# Testing

The suite has to prove two things: xcheckfs reports **every** divergence
(no false negatives) and **only** divergences (no false positives, also under
heavy concurrency). Healthy tree pairs prove the second; a fault-injecting
secondary proves the first.

## Lanes

| Command | What runs | Needs |
|---|---|---|
| `cargo test --lib` | Unit tests: comparison rules, histograms, policy, rules files, verify, TUI helpers | nothing |
| `cargo test --test engine_healthy` | The engine over two healthy trees: broad and random workloads at every check level, many-thread stress (hot directories, rename trees, appends, overlapping writers), delayed halves, two different file systems (`/dev/shm` vs `/tmp`) — zero mismatches required | nothing |
| `cargo test --test engine_relaxed` | Relaxed serialization ([Design](../../explanation/DESIGN.md#concurrent-data-operations)): disjoint in-place writers, readers and `fallocate` really reach both file systems concurrently (observed with gates inside the backends, not inferred), operations that must stay exclusive wait, every interleaving of the halves of two writers with a racing `getattr` / `lookup` / `read` gives no false positive, and faults on one of two concurrent writers are still detected | nothing |
| `cargo test --test engine_probe` | The mount-time probe ([Design](../../explanation/DESIGN.md#mount-time-probe)): a secondary made to behave like another kind of file system by fault effects in place before the engine starts, so the probe sees them. A moved or exchanged directory, a truncate to the current size and a hole punched into a hole are adapted to (no mismatch, `aligned_mtimes` counted; without the probe the same difference is reported), a neighboring behavior that is a defect (a size-changing truncate, a hole punched into data) is still reported, missing `fallocate` modes are listed with the allow rule, two file systems of one kind need no adaptation, and the probe leaves no scratch directory, restores the roots' times and copes with a root it cannot write to | nothing |
| `cargo test --test engine_faults` | Every injected fault is detected at the levels where it must be, and not where it can't be | nothing |
| `cargo test --test engine_policy` | log / fail / freeze (every operator action) / detach, allow rules, de-duplication | nothing |
| `cargo test --test engine_locks` | Mirrored record locks: conflicts, FIFO waiters, release on flush/close, lock faults, races with I/O | nothing |
| `cargo test --test fuse_mount` | Real in-process mounts driven through the kernel: all operation types, forked processes for POSIX locks, mmap, stress, faults through the mount | `/dev/fuse` and `fusermount3`; skips otherwise |
| `tests/apps/<app>/run.sh ci` | Real applications on a mount in Docker: PostgreSQL (pgbench), MySQL/InnoDB (sysbench), multi-process SQLite. Zero mismatches, identical trees and a consistent database on each side required; also measures throughput and latency against a plain volume ([tests/apps/README.md](../../../tests/apps/README.md)). Runs in CI as the `Applications` workflow | Docker with `/dev/fuse`, a static `xcheckfs` binary, opt-in |
| `tests/external/run-libfuse-syscalls.sh` | libfuse `test_syscalls` through the `xcheckfs` binary | a libfuse checkout, opt-in |
| `tests/external/run-pjdfstest.sh` | pjdfstest through the `xcheckfs` binary | root, network, opt-in |
| `tests/external/run-stress.sh` | Stress lane: fio with end-to-end verification and stress-ng filesystem stressors through the `xcheckfs` binary over two healthy directories; zero mismatches and a clean `xcheckfs verify` required. Runs in CI as the `stress` job | `/dev/fuse`, `fusermount3`, `fio`, `stress-ng` |

`cargo test` runs everything except the external scripts and the application
tests in about 15 s.
`XCHECKFS_SOAK=N` makes the stress tests run N times longer. FUSE tests have
a watchdog that aborts a hung connection after 120 s
(`XCHECKFS_WATCHDOG_SECS`).

The stress and property tests in `engine_healthy.rs` and
`engine_proptest.rs` run under both `--serialize strict` and `relaxed`.
`XCHECKFS_SERIALIZE=strict` runs every other test in strict mode.

The trees of the engine and mount tests live in `/dev/shm` (or the temporary
directory). `XCHECKFS_TEST_PRIMARY=DIR` and `XCHECKFS_TEST_SECONDARY=DIR` put
them on other file systems, for example two loop-mounted ones, to see which
of their differences the tests trip over. Tests that compare the trees at the
end expect two file systems of the same kind; on a mixed pair, failures are
expected where the file systems legitimately differ
([Known differences](../../reference/fs-differences.md)). The mount-time
probe adapts to the differences it knows; the rest, and the unsupported
`fallocate` modes, are what such a run shows. Running the suite and
fio / stress-ng over every ordered pair of ext4, xfs, btrfs, f2fs and tmpfs
(loop-mounted) is how those differences were found.

## Application tests

`tests/apps/` runs the applications' own benchmarks on two healthy
directories mirrored by a static `xcheckfs` binary, for each configuration
in `CONFIGS` (default `baseline basic thorough`; add `-strict` to a name for
`--serialize strict`). They find what synthetic tests do not: the I/O
patterns of real database engines (`O_DIRECT` with native AIO, concurrent
in-place writes to one file, hole punching, `fcntl` locks, shared `mmap`).
`run.sh ci` is sized for a CI runner and writes `results.json`;
`tests/apps/report.py` renders it as Markdown tables (throughput relative to
`baseline`, latencies, xcheckfs counters, checks) and, with `--base`, the
change against earlier results. Usage and the `results.json` format:
[tests/apps/README.md](../../../tests/apps/README.md); the workflow:
[Run in CI](../run-in-ci.md#application-tests-of-xcheckfs).

## Building blocks

- `src/backend/fault.rs` (feature `fault-injection`): `FaultBackend` wraps
  any backend and injects faults selected by operation, name/path and trigger
  (`once`, `nth`, `after`, `every`). Effects: errno, applied-then-errno,
  silently skipped operation, delay, corrupted/short/dropped reads and
  writes, lying stats, dropped/added/retyped directory entries, wrong link
  targets, xattr lies, lock lies, wrong offsets. Two effects model a file
  system that makes a different legitimate choice: `StampMtime` (`rename`: a
  directory that moved to another parent, both for `RENAME_EXCHANGE`, gets its
  mtime set to now) and `RestoreTimes` (`pwrite`, `truncate`, `fallocate`: the
  call happens, then atime and mtime are put back, ctime still moves).
- `Gate` (in `src/backend/fault.rs`, with the `Effect::Gate` fault and a
  recorder of calls in flight): holds the calls it is attached to inside a
  backend until the test opens it. Tests wait for calls to arrive and to
  finish instead of sleeping, so "two calls are in flight at once" and every
  ordering of the two halves of an operation can be forced and observed.
- `tests/common/mod.rs`: `Harness` builds an engine over two temporary trees
  (both wrapped in a `FaultBackend`), with path-based helpers and mismatch
  assertions (`assert_no_mismatches`, `expect_mismatch(kind, field)`,
  `assert_trees_equal`). `HarnessBuilder::secondary_fault(fault)` installs a
  fault on the secondary before the engine starts, so that the mount-time
  probe sees it; a fault added with `Harness` afterwards comes too late for
  the probe.

## Adding a test

- A new semantic check: add a fault effect to `FaultBackend` if none fits,
  then a test in `engine_faults.rs` asserting the mismatch kind and field at
  the weakest check level that must catch it (and its absence below that).
- A new workload pattern: add it to `engine_healthy.rs` with
  `assert_no_mismatches()`, and to the stress mix if it can race.
- A new difference the engine adapts to: a test in `engine_probe.rs` that
  makes the secondary behave differently with `secondary_fault`, and runs the
  same steps with and without the probe (adapted vs reported), plus the
  neighboring behavior that must still be reported.
- A change to what may run concurrently: `engine_relaxed.rs` (a test that
  observes the concurrency with gates, one that forces the interleavings
  against a racing stat, one that injects a fault into a concurrent writer).
- Anything that depends on kernel behaviour (caching, mmap, locks across
  processes): `fuse_mount.rs`.

## Pitfalls

- In-process FUSE: never touch a FUSE-backed `mmap` from the test process
  itself (a page fault holds the address-space lock the server threads
  need); `fuse_mount.rs` maps in forked children.
- Different file systems legitimately differ (e.g. whether `fallocate`
  updates mtime, `ZERO_RANGE` support, directory link counts): cross-file
  system tests avoid those or allow them with rules, or rely on the probe
  (not with `--no-probe`).
