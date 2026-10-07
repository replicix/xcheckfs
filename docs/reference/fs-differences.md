# Known differences between file systems

Two correct file systems still differ in places. This page lists what was
measured by running one broad workload (about 22 000 operations: deep and wide
trees, odd names, hard and symbolic links, FIFOs, sparse files, every
`fallocate` mode, `copy_file_range`, unlinked-but-open files, setuid/setgid/
sticky modes, explicit timestamps, many and large xattrs, every kind of rename
including `RENAME_EXCHANGE`, record and BSD locks) through xcheckfs with
`--check thorough --attr-timeout 0 --entry-timeout 0` for every ordered pair of
ext4, xfs, btrfs and tmpfs (Linux 7.3, SELinux enforcing).

Pairs of the same file system (two directories on one ext4, xfs, btrfs or
tmpfs) produced **no** mismatches, and neither did xfs ↔ btrfs. Everything
below is a real difference of the file systems, not of xcheckfs.

## Handled automatically

| Difference | What xcheckfs does |
|---|---|
| btrfs reports a link count of 1 for every directory (others: 2 + subdirectories). | When either side is btrfs, directory link counts are not compared (logged at info level). For other file systems with the same behavior use `--no-dir-nlink`. |
| `copy_file_range` may copy less than asked, and file systems differ in how much one call copies (xfs and btrfs vs ext4 and tmpfs). | The primary runs first; the secondary is then driven to exactly the primary's count (repeating its own short copies). Only a secondary that cannot get there is reported (`length`). |
| `SEEK_DATA`/`SEEK_HOLE` granularity, `st_blocks`, directory sizes, atime, statfs numbers. | Never compared ([Checks](checks.md#never-compared)). |

## Reported: capability and capacity limits

These are reported as mismatches, which is right for an experimental file
system that is meant to match the primary. When the difference is accepted,
an [allow rule](rules.md) silences it. In `resync` mode the consequence (data
or xattrs that differ afterwards) is repaired; in `log` mode the consequence
is reported too.

| Difference | Seen as | Allow rule |
|---|---|---|
| ext4 stores all xattrs of an inode in one block (about 4 KiB by default): more or larger values fail with `ENOSPC`, where xfs, btrfs and tmpfs succeed. | `setxattr` `result` (`ENOSPC` vs `OK`), then `listxattr` `xattr list` | `{ op = "setxattr", kind = "result", primary = "OK", secondary = "ENOSPC" }` (ext4 as secondary) |
| tmpfs does not support `FALLOC_FL_ZERO_RANGE`. | `fallocate` `result` (`OK` vs `EOPNOTSUPP`), then `read` `data` of that range | `{ op = "fallocate", kind = "result", secondary = "EOPNOTSUPP" }` (tmpfs as secondary) |

Swap `primary` and `secondary` in the rule when the limited file system is
the primary.

## Reproducing

The measurement is easy to repeat for other pairs (as root): create the file
systems (for example on loop devices), point `PRIMARY` and `SECONDARY` at
empty directories on them, mount with
`xcheckfs mount -b -m log --check thorough --attr-timeout 0 --entry-timeout 0 MNT PRIMARY SECONDARY`,
run a workload in `MNT`, then group `xcheckfs ctl MNT mismatches 1000` by
`op`, `kind`, `field`, `primary` and `secondary`.
