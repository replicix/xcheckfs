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
| `cargo test --test engine_faults` | Every injected fault is detected at the levels where it must be, and not where it can't be | nothing |
| `cargo test --test engine_policy` | log / fail / freeze (every operator action) / detach, allow rules, de-duplication | nothing |
| `cargo test --test engine_locks` | Mirrored record locks: conflicts, FIFO waiters, release on flush/close, lock faults, races with I/O | nothing |
| `cargo test --test fuse_mount` | Real in-process mounts driven through the kernel: all operation types, forked processes for POSIX locks, mmap, stress, faults through the mount | `/dev/fuse` and `fusermount3`; skips otherwise |
| `tests/external/run-libfuse-syscalls.sh` | libfuse `test_syscalls` through the `xcheckfs` binary | a libfuse checkout, opt-in |
| `tests/external/run-pjdfstest.sh` | pjdfstest through the `xcheckfs` binary | root, network, opt-in |

`cargo test` runs everything except the external scripts in about 15 s.
`XCHECKFS_SOAK=N` makes the stress tests run N times longer. FUSE tests have
a watchdog that aborts a hung connection after 120 s
(`XCHECKFS_WATCHDOG_SECS`).

## Building blocks

- `src/backend/fault.rs` (feature `fault-injection`): `FaultBackend` wraps
  any backend and injects faults selected by operation, name/path and trigger
  (`once`, `nth`, `after`, `every`). Effects: errno, applied-then-errno,
  silently skipped operation, delay, corrupted/short/dropped reads and
  writes, lying stats, dropped/added/retyped directory entries, wrong link
  targets, xattr lies, lock lies, wrong offsets.
- `tests/common/mod.rs`: `Harness` builds an engine over two temporary trees
  (both wrapped in a `FaultBackend`), with path-based helpers and mismatch
  assertions (`assert_no_mismatches`, `expect_mismatch(kind, field)`,
  `assert_trees_equal`).

## Adding a test

- A new semantic check: add a fault effect to `FaultBackend` if none fits,
  then a test in `engine_faults.rs` asserting the mismatch kind and field at
  the weakest check level that must catch it (and its absence below that).
- A new workload pattern: add it to `engine_healthy.rs` with
  `assert_no_mismatches()`, and to the stress mix if it can race.
- Anything that depends on kernel behaviour (caching, mmap, locks across
  processes): `fuse_mount.rs`.

## Pitfalls

- In-process FUSE: never touch a FUSE-backed `mmap` from the test process
  itself (a page fault holds the address-space lock the server threads
  need); `fuse_mount.rs` maps in forked children.
- Different file systems legitimately differ (e.g. whether `fallocate`
  updates mtime, `ZERO_RANGE` support, directory link counts): cross-file
  system tests avoid those or allow them with rules.
