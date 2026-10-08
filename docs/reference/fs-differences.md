# Known differences between file systems

Two correct file systems still differ in places. This page lists what was
measured by running one broad workload (about 22 000 operations: deep and wide
trees, odd names, hard and symbolic links, FIFOs, sparse files, every
`fallocate` mode, `copy_file_range`, unlinked-but-open files, setuid/setgid/
sticky modes, explicit timestamps, many and large xattrs, every kind of rename
including `RENAME_EXCHANGE`, record and BSD locks) through xcheckfs with
`--check thorough --attr-timeout 0 --entry-timeout 0` for every ordered pair of
ext4, xfs, btrfs, f2fs and tmpfs (25 pairs; Linux 7.3, SELinux enforcing), and
the test suite and fio / stress-ng over the same pairs
([Testing](../how-to-guides/development/TESTING.md)).

Pairs of the same file system (two directories on one ext4, xfs, btrfs, f2fs or
tmpfs) produced **no** mismatches. Everything below is a real difference of
the file systems, not of xcheckfs.

## Table of Contents

- [The mount-time probe](#the-mount-time-probe)
- [Behavior per file system](#behavior-per-file-system)
- [Handled automatically](#handled-automatically)
- [Reported: capability and capacity limits](#reported-capability-and-capacity-limits)
- [Reproducing](#reproducing)

## The mount-time probe

Where POSIX leaves a choice, file systems choose differently. Which choice a
file system made is not a function of its type that xcheckfs could look up:
it is learned. At mount (unless `--no-probe`) xcheckfs runs a few operations
in a scratch directory `.xcheckfs-probe-<pid>-<hex>` at the root of each tree,
through the backend calls the engine uses, removes it, and puts the root's
atime and mtime back (the root's ctime changes). The two results are compared:

- **A choice that differs** is *adapted to*: the engine does not report it, as
  described under [Handled automatically](#handled-automatically). Each
  adaptation is logged at info level at mount and listed in
  `xcheckfs ctl MNT status` under `info.adaptations`
  ([Control protocol](control-protocol.md#status)).
- **An optional operation only one side supports** (a `fallocate` mode) is
  *not* adapted to; see [Reported: capability and capacity limits](#reported-capability-and-capacity-limits).
  The mount logs it at warn level with the [allow rule](rules.md) that accepts
  the resulting mismatches, and lists it under `info.capability_gaps`.

If a root is not writable the probe learns nothing, and nothing is adapted
(the btrfs directory link count check below still applies). Why probes and
not a table: [Design](../explanation/DESIGN.md#mount-time-probe),
[ADR-13](../explanation/DECISIONS.md#adr-13-adapt-to-legitimate-file-system-differences-found-by-a-mount-time-probe).

## Behavior per file system

Measured with loop-mounted file systems on Linux 7.3. The probe finds these
(for any file system, not only the five).

| Behavior | ext4 | xfs | btrfs | f2fs | tmpfs |
|---|---|---|---|---|---|
| A directory's link count counts its subdirectories | yes | yes | no | yes | yes |
| Moving a directory to another parent stamps the directory's own mtime | no | no | no | yes | no |
| `RENAME_EXCHANGE` of directories in different parents stamps them | no | yes | no | yes | no |
| Truncate by path of a non-empty file to its current size stamps mtime | yes | no | no | yes | yes |
| Truncate by path of an empty file to size 0 stamps mtime | yes | no | no | yes | no |
| `ftruncate` to the current size stamps mtime (empty or not) | yes | yes | yes | yes | yes |
| Punching a hole into a range without data stamps mtime | yes | yes | no | yes | yes |

ZFS does not stamp mtime for the last case either.

## Handled automatically

When the probe finds that the two file systems differ in a row below, the
engine adapts. For the mtime rows, "aligns" means: right after exactly the
operation in exactly the situation that differs, xcheckfs sets the
secondary's mtime to the primary's (`utimens`; this moves the secondary's
ctime, and the object's [ctime baseline](checks.md#attributes) is reset). Each
alignment is counted in the status counter `aligned_mtimes`.

Alignment is limited to that situation. A secondary that does not stamp mtime
where every file system must (a truncate that changes the size, a hole punched
into data, a write) is still reported.

| Difference | What xcheckfs does |
|---|---|
| One side does not count subdirectories in a directory's link count (btrfs reports 1; others: 2 + subdirectories). | Directory link counts are not compared. Found by the probe for any file system; only when the probe could not run, a btrfs side is detected by the file system type instead (logged at info level). With `--no-probe` use `--no-dir-nlink` for other file systems with the same behavior. |
| Only one side stamps the mtime of a directory that was moved to another parent (f2fs). | After a `rename` of a directory into another parent, aligns the moved directory. A rename within one parent is not touched. |
| Only one side stamps the mtime of directories exchanged between parents (`RENAME_EXCHANGE`: xfs and f2fs). | After the exchange, aligns both directories. |
| Only one side stamps mtime for a truncate to the current size (ext4 and f2fs do; xfs and btrfs do not; tmpfs does for a non-empty file only). | After a `setattr` with a size equal to the primary's size before, and without an explicit mtime in the same `setattr`, aligns the file. Path truncate and `ftruncate`, of an empty and of a non-empty file, are probed separately and aligned only for the variant that differs (all five measured file systems stamp for `ftruncate`). |
| Only one side stamps mtime when a hole is punched into a range that holds no data (btrfs and ZFS do not; ext4, xfs, f2fs and tmpfs do). Seen with InnoDB page compression, tmpfs primary, ZFS secondary. | After a `fallocate` `PUNCH_HOLE` whose range held no data on the primary, aligns the file. That `fallocate` takes the file exclusively under `--serialize relaxed`, so that no concurrent write stamps mtime in between. Formerly an allow rule on `attr` `mtime` and `ctime` of `fallocate`. |
| `copy_file_range` may copy less than asked, and file systems differ in how much one call copies (xfs and btrfs vs ext4 and tmpfs). | The primary runs first; the secondary is then driven to exactly the primary's count (repeating its own short copies). Only a secondary that cannot get there is reported (`length`). |
| `SEEK_DATA`/`SEEK_HOLE` granularity, `st_blocks`, directory sizes, atime, statfs numbers. | Never compared ([Checks](checks.md#never-compared)). |

With `--no-probe` nothing in the first five rows is adapted to (except the
btrfs fallback): the differences are reported as `attr` `mtime` and `nlink`
mismatches.

## Reported: capability and capacity limits

These are reported as mismatches, which is right for an experimental file
system that is meant to match the primary. When the difference is accepted,
an [allow rule](rules.md) silences it. In `resync` mode the consequence (data
or xattrs that differ afterwards) is repaired; in `log` mode the consequence
is reported too.

The mount-time probe lists unsupported `fallocate` modes (`PUNCH_HOLE`,
`ZERO_RANGE`, `COLLAPSE_RANGE`, `INSERT_RANGE`) that only one side supports:
a warning in the log and an entry in `info.capability_gaps` of `ctl status`,
each with the rule that accepts the resulting `fallocate` `result`
mismatches, for example
`{ op = "fallocate", kind = "result", secondary = "EOPNOTSUPP" }`. Nothing is
allowed automatically: whether the gap is acceptable is for you to decide.

| Difference | Seen as | Allow rule |
|---|---|---|
| ext4 stores all xattrs of an inode in one block (about 4 KiB by default): more or larger values fail with `ENOSPC`, where xfs, btrfs and tmpfs succeed. | `setxattr` `result` (`ENOSPC` vs `OK`), then `listxattr` `xattr list` | `{ op = "setxattr", kind = "result", primary = "OK", secondary = "ENOSPC" }` (ext4 as secondary) |
| tmpfs does not support `FALLOC_FL_ZERO_RANGE` (listed by the mount as a capability gap). | `fallocate` `result` (`OK` vs `EOPNOTSUPP`), then `read` `data` of that range | `{ op = "fallocate", kind = "result", secondary = "EOPNOTSUPP" }` (tmpfs as secondary) |

Swap `primary` and `secondary` in the rule when the limited file system is
the primary.

## Reproducing

The measurement is easy to repeat for other pairs (as root): create the file
systems (for example on loop devices), point `PRIMARY` and `SECONDARY` at
empty directories on them, mount with
`xcheckfs mount -b -m log --check thorough --attr-timeout 0 --entry-timeout 0 MNT PRIMARY SECONDARY`,
run a workload in `MNT`, then group `xcheckfs ctl MNT mismatches 1000` by
`op`, `kind`, `field`, `primary` and `secondary`.
