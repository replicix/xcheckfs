# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Relaxed serialization (`--serialize relaxed`, the default): reads, in-place
  writes, `fallocate` and `copy_file_range` that change neither a file's size
  nor its metadata hold a byte-range lock instead of the whole object, so
  disjoint ranges of one file reach both file systems concurrently. Overlapping
  ranges are still ordered identically on both sides; anything that extends
  the file or changes metadata stays exclusive. `--serialize strict` keeps the
  previous behavior. See [Design](docs/explanation/DESIGN.md#concurrent-data-operations).
- A stat that overlaps an in-place write no longer compares `mtime` and `ctime`
  (the two sides may legitimately have stamped different subsets of the
  concurrent writes); `thorough` adds a per-write check that each side's
  `mtime` is not older than the write (`verify` mismatch, field `write mtime`).
- `--lock-stripes N` (default 65536, from 4096; clamped to 16 to 4194304).
- `--direct-io auto` (the default): files the application opens with `O_DIRECT`
  use direct I/O with parallel direct writes, so concurrent writes to one file
  reach xcheckfs concurrently. The mount also requests `FUSE_ASYNC_DIO`,
  `FUSE_DIRECT_IO_ALLOW_MMAP` and a deeper background queue.
- Status counters `concurrent_data_ops`, `range_waits` and `attr_time_skipped`
  ([Control protocol](docs/reference/control-protocol.md#status)).
- Application tests (`tests/apps/`): PostgreSQL (pgbench), MySQL/InnoDB
  (sysbench) and multi-process SQLite on a mount in Docker, with a throughput
  and latency report. The new `Applications` workflow runs them on pushes to
  `main` and pull requests, posts the report as a job summary and a pull
  request comment compared with the latest `main`, and keeps the results as
  artifacts. The release workflow waits for it.
- Concurrency tests for relaxed serialization (`tests/engine_relaxed.rs`); the
  stress tests run under both serializations.
- Mount-time probe: a few operations in a scratch directory at the root of each
  tree (removed again; the roots' times are restored, their ctime changes)
  find where the two file systems legitimately differ. Directory link counts
  are then not compared when a side does not count subdirectories; and right
  after exactly the operation that differs, the secondary's `mtime` is set to
  the primary's for a directory moved to another parent or exchanged
  (`RENAME_EXCHANGE`) across parents, a truncate to the current size (by path
  or by descriptor), and a hole punched into a range without data (this covers
  the ZFS difference). A secondary that never stamps `mtime` is still
  reported. Optional `fallocate` modes only one side supports are logged at
  warn level at mount with the allow rule that accepts them.
  `--no-probe` switches the probe off.
- Status counter `aligned_mtimes`; `info.adaptations` and
  `info.capability_gaps` in `ctl status`
  ([Control protocol](docs/reference/control-protocol.md#status)).
- [File system differences](docs/reference/fs-differences.md) measured for
  ext4, xfs, btrfs, f2fs and tmpfs (all 25 ordered pairs, Linux 7.3), with a
  per-file-system table. Tests: `tests/engine_probe.rs`; fault effect
  `StampMtime`, `RestoreTimes` for truncate and `fallocate`, and
  `HarnessBuilder::secondary_fault`.

### Changed

- Directory link counts of a btrfs side are now found by the probe, for any
  file system that does not count subdirectories; the file system type check
  remains only as a fallback when the probe could not run.
- `--threads` defaults to twice the number of CPUs, between 16 and 64 (was the
  number of CPUs, at most 16). The pool running the secondary halves is at
  least as large.
- `--direct-io` takes a value (`off`, `auto`, `all`) and defaults to `auto`;
  without a value it means `all`, as before. Shared writable `mmap` of files
  using direct I/O needs kernel 6.7 or newer.
- Less log noise: the FUSE library's failed replies to `FUSE_INTERRUPT` and its
  warnings about an already-unmounted mount are no longer logged.
- The control socket's directory is made `0700` only when xcheckfs creates it;
  an existing directory such as `/run` keeps its mode.
- A `copy_file_range` mismatch found at `thorough` also says whether the
  source ranges of the two file systems were equal and whether each side's
  copy matches its source.

### Fixed

- `xcheckfs verify` no longer reports directory link counts when either tree
  is on a file system that does not count subdirectories (btrfs), nor SELinux
  labels, which the policy assigns per mount.

- A repair of an object's hard links, or of an entry replaced by path, changed
  names in a directory on the secondary without restoring the directory's
  times: a later `getattr` of the directory could report an `mtime` mismatch
  when the directory's last real change was more than `--time-tolerance` ago.

## [0.1.2]

### Changed

- Dependencies: `toml` 1.1 (from 0.8), `signal-hook` 0.4 (from 0.3),
  `globset` 0.4.20.
- Release and CI workflows: `actions/checkout` v7, `actions/upload-artifact`
  v6 and `actions/download-artifact` v7 (Node.js 24 runtime).

## [0.1.1]

### Fixed

- When xcheckfs runs out of file descriptors, both file systems still see
  the same operations: a half that alone ran out is retried after freeing a
  small descriptor reserve, instead of leaving the trees diverged and
  reporting a false mismatch. Closing a file no longer fails with `EMFILE`.
  A warning points at `RLIMIT_NOFILE`.
- The libfuse `test_syscalls` script clones libfuse when no checkout is
  given, and the external test scripts no longer require an offline crate
  cache.

## [0.1.0]

Initial release.

### Added

- `xcheckfs mount MOUNTPOINT PRIMARY SECONDARY`: a FUSE file system that
  serves the trusted primary and mirrors every operation to the experimental
  secondary in lockstep, with concurrent dual execution. Applications always
  get the primary's result. The mount point may be the primary itself.
- Comparison of return codes, attributes, file data, directory listings, link
  targets, extended attributes, hard-link identity and mirrored `fcntl`
  record locks.
- Check levels `basic`, `thorough` (reads back the effect of every mutation)
  and `paranoid` (whole-file and whole-listing comparisons).
- Mismatch modes `resync` (default: report, then repair the secondary from the
  primary), `log`, `fail`, `freeze` (hold until an operator decides) and
  `detach`; `--quarantine` keeps the secondary's overwritten version of
  repaired objects; per-object repair budget.
- Mirrored `fcntl` locks behave like native ones for blocked requests:
  `EDEADLK` for lock cycles (also across files) and `EINTR` when the waiting
  thread is signalled.
- Known differences between file systems are handled where they are not
  bugs: directory link counts are not compared against btrfs, short
  `copy_file_range` copies are completed on the secondary; the rest is
  documented with allow rules (`docs/reference/fs-differences.md`).
- Sparse files cost what their data costs in paranoid comparisons and in
  repairs (`SEEK_DATA`/`SEEK_HOLE`).
- Allow rules (TOML) to silence known, harmless differences.
- `xcheckfs verify PRIMARY SECONDARY`: offline comparison of two trees.
- `xcheckfs ctl`: control socket for status, recent mismatches, pending frozen
  mismatches, resolving them and changing the mismatch mode at runtime.
- Interfaces: colored log output and an interactive terminal dashboard
  (`--ui tui`); background mode with syslog or log-file output.
- Fault-injecting backend and a test suite covering healthy trees under
  concurrency, every injected fault, mismatch policies, record locks, repairs,
  property-based random workloads (proptest), end-to-end tests of the binary
  and real FUSE mounts; a fio + stress-ng lane in CI; opt-in libfuse
  `test_syscalls` and pjdfstest scripts.
- Release pipeline: static musl binaries for x86_64 and aarch64, `.deb` and
  `.rpm` packages, and an installer script.
- Documentation in the Diátaxis layout under `docs/`.

[Unreleased]: https://github.com/replicix/xcheckfs/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/replicix/xcheckfs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/replicix/xcheckfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/replicix/xcheckfs/releases/tag/v0.1.0
