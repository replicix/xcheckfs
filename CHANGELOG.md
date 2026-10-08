# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/replicix/xcheckfs/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/replicix/xcheckfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/replicix/xcheckfs/releases/tag/v0.1.0
