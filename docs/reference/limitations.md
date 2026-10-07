# Limitations

Everything xcheckfs cannot do or does differently from a plain file system,
in one place. Other pages link here instead of repeating it.

## Table of Contents

- [Risk](#risk)
- [Exclusive access](#exclusive-access)
- [Mount layout](#mount-layout)
- [Operations not mirrored](#operations-not-mirrored)
- [Locking](#locking)
- [Caching](#caching)
- [Not covered](#not-covered)
- [Repair](#repair)
- [Resources](#resources)
- [Freeze](#freeze)

## Risk

xcheckfs is in the data path of the primary: every read and write of the
primary goes through it. A bug in xcheckfs, a crash, or `fail` mode affects
the workload. **Take a snapshot or backup of the primary first.** In `fail`
mode `EIO` is returned for operations the primary already applied
([Design](../explanation/DESIGN.md#mismatch-handling)); use it for CI, not for
production.

## Exclusive access

Both trees must be accessed **only through the mount** while it runs. Any
other writer to the primary or the secondary makes them diverge, and xcheckfs
will report it (as `attr`, `data`, `readdir`, `identity` or `ctime` mismatches)
or, worse, mask it. When mounting over the primary, processes that already
had files open or a working directory inside it keep using the underlying
directory and bypass the mirror: stop them first.

A crash of xcheckfs between the two halves of an operation leaves the trees
diverged. After any crash or `kill -9`, run `xcheckfs verify` before mounting
again.

## Mount layout

- **No nested mounts under the primary.** Looking up an object on a different
  device than the primary's root returns `EIO` (and logs an error).
- The mount point must not be inside the secondary, nor inside the primary
  (unless it is the primary). Primary and secondary must not contain each
  other.
- The log file, pid file and control socket must not be inside a mirrored
  tree; a rules file inside one cannot be saved to ([Rules](rules.md#location)).

## Operations not mirrored

| Operation | Behavior |
|---|---|
| `ioctl` | `ENOTTY`: requests and replies are opaque. |
| `poll` | `ENOSYS` |
| `bmap` | `ENOSYS` |
| `O_TMPFILE` | Not available through the FUSE library used (fuser 0.18); the kernel falls back and `open(O_TMPFILE)` fails with `EOPNOTSUPP`. |
| `statx`, `syncfs` | Not available through fuser 0.18; `statx` is answered by the kernel's fallback to `getattr`, and `syncfs` is unavailable. |
| `O_DIRECT` | Stripped before reaching the backends ([ADR-4](../explanation/DECISIONS.md#adr-4-o_direct-is-stripped)). |

Unsupported operations are counted in the statistics.

## Locking

- `fcntl` record locks are mirrored with non-blocking OFD locks and an
  xcheckfs-side queue ([Design](../explanation/DESIGN.md#lock-mirroring)).
- **`FUSE_INTERRUPT` is not handled.** A process blocked in `F_SETLKW` cannot
  be interrupted by a non-fatal signal; its wait is cancelled (`EINTR`) when
  it closes the file or exits.
- OFD locks (`F_OFD_SETLK`) are released when the FUSE `RELEASE` of their
  file description arrives, which the kernel sends asynchronously right
  after `close(2)` returns: a lock attempt racing the close can briefly see
  the old lock.
- No deadlock detection: `EDEADLK` is never returned.
- `F_GETLK` reports pid 0 for the conflicting lock.
- **`flock(2)` is kernel-local** and not mirrored
  ([ADR-9](../explanation/DECISIONS.md#adr-9-flock2-stays-kernel-local-ioctl-is-not-mirrored)).
- With `--no-lock-mirroring`, `fcntl` locks are kernel-local too.

## Caching

By default the kernel caches attributes and directory entries for 1 s and
serves repeated reads from its page cache. Operations answered from those
caches never reach xcheckfs and are **not re-checked**.

For maximal coverage at a cost in speed, use
`--attr-timeout 0 --entry-timeout 0 --direct-io`. `--direct-io` breaks shared
writable `mmap` (a kernel restriction for FUSE direct I/O); do not use it for
workloads that need it.

## Not covered

- **Durability across power loss** is not tested: `fsync` results are
  compared, but whether data survives a crash is not observable.
- Properties that are file-system specific are not compared
  ([Checks](checks.md#never-compared)).
- `verify` and the live comparison cover regular files, directories,
  symlinks, hard links, xattrs and device nodes; they cannot see state that
  is not reachable through the POSIX interface of the two directories.

## Repair

How repair works: [Design](../explanation/DESIGN.md#repair-resync).

- **Repairs hide the evidence.** The secondary's version is gone after a
  repair. Use `--quarantine DIR` to keep a copy (up to `--quarantine-cap`
  bytes per object; more is truncated).
- **A repair goes through the secondary's own write path.** If that is the
  buggy part, the repair fails; verification catches it, the object stays
  diverged and is used on the primary only (`secondary_skipped`).
- **A large object blocks while it is repaired.** The repair runs inside the
  operation that hit the mismatch, which holds up that object (visible in the
  TUI's in-flight pane).
- **Not repairable:** `lock`, `lseek` and `statfs` mismatches.
- **Hard links** are restored by a bounded search (20 000 entries, from the
  repaired directory, or from the mount root for link-count repairs). A link
  whose other names are not found is copied as an independent file, and
  verification reports the repair as failed.
- **Resync gives up** on a path repaired `--resync-limit` times (default 5)
  within 10 minutes; it stays diverged and is used on the primary only.
- Open handles opened before the secondary's object was replaced, or opened
  while it was missing, continue on the primary only; the object itself is
  compared again by the next operation that looks at it (and repaired then).
- Repairs that walk beyond the locked directory (link-count repairs, type
  changes found on an object) do not lock what they walk: concurrent
  operations may report transient extra mismatches, which are repaired in
  turn.
- Because diverged objects are pushed back to the primary's state, a clean
  `verify` after a `resync` run says nothing about the secondary; the
  mismatch count does.

## Resources

- Each cached inode holds **two `O_PATH` descriptors** (one per side).
  `RLIMIT_NOFILE` is raised to the hard limit at start; xcheckfs warns when
  that is below 65 536. Raise the hard limit (`LimitNOFILE=` under systemd)
  for large trees.
- Reads are compared in memory, so a read of N bytes holds up to 2N bytes
  (pooled buffers).

## Freeze

In `freeze` mode applications block inside the kernel, waiting for FUSE
replies, until an operator decides ([Design](../explanation/DESIGN.md#freeze-gate)).
If `/proc/sys/fs/fuse/max_request_timeout` is non-zero, the kernel aborts the
FUSE connection when a request exceeds it, ending the mount. Keep it at `0`
(the default) when using `freeze`.
