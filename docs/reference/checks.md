# Checks

What xcheckfs compares for each operation, per check level
(`--check basic|thorough|paranoid`). Levels are cumulative: `thorough`
includes `basic`, `paranoid` includes `thorough`. How the engine runs and
compares: [Design](../explanation/DESIGN.md#what-an-operation-does).

## Table of Contents

- [Everywhere](#everywhere)
- [Per operation](#per-operation)
- [Attributes](#attributes)
- [Aligned mtime](#aligned-mtime)
- [Racy stats](#racy-stats)
- [Never compared](#never-compared)
- [Mismatch kinds](#mismatch-kinds)

## Everywhere

Every operation first compares the **result code** of both sides: success or
the same errno. A different outcome is a `result` mismatch (`primary=ENOENT
secondary=OK`). Errno names are those of `errno(3)`; `ENOTSUP` is reported as
`EOPNOTSUPP`. If both sides fail with the same errno nothing further is
compared. Where a *verification* (a `thorough` read-back) fails on both sides,
only a warning is logged: both agree, and the primary is trusted.

## Per operation

| Operation | basic | thorough adds | paranoid adds |
|---|---|---|---|
| `lookup` | result; attributes of the entry; hard-link identity; ctime change | | |
| `getattr` | result; attributes; ctime change | | |
| `setattr` (chmod, chown, truncate, utimens) | result of each step; attributes afterwards | requested mode, uid, gid, size, atime and mtime are really applied on each side (setgid may be dropped) | |
| `readlink` | result; link target | | |
| `mknod`, `mkdir`, `create` | result; attributes of the new entry; identity | | parent listing |
| `symlink` | as above | link target equals the requested one | parent listing |
| `link` | result; attributes of the new name (incl. `nlink`); identity | | listing of the target directory |
| `unlink`, `rmdir` | result | the name is gone on both sides; attributes of the removed object if it lives on (`nlink`, ctime change) | parent listing |
| `rename` | result | identity of source and destination names before and after is as the flags require (move, replace, exchange); attributes of the moved object | listings of source and target directory |
| `open` | result | with `O_TRUNC`: size is 0 | |
| `read` | result; length; data (byte for byte) | | |
| `write` | result; bytes written | the written range read back equals the written data (honors `O_APPEND`); each side's `mtime` is not older than the start of the write minus `--time-tolerance` (at least 50 ms; `write mtime`) | |
| `release` | | | if the file was written: complete content comparison |
| `fallocate` | result | attributes afterwards; for punch-hole and zero-range: the range reads as zeros (at most 16 MiB checked) | |
| `copy_file_range` | result; bytes copied (the secondary is driven to the primary's count: short copies are legal) | the copied range read back is equal (at most 16 MiB checked) | |
| `lseek` | result; offset (not for `SEEK_DATA`/`SEEK_HOLE`) | | |
| `opendir` | result | | |
| `readdir` | result; the listing at offset 0, as a set of (name, type) | | |
| `setxattr` | result | the value reads back equal | |
| `getxattr` | result; value (byte for byte) | | |
| `listxattr` | result; set of names | | |
| `removexattr` | result | the attribute is gone (`ENODATA`) | |
| `access` | result (with the caller's credentials) | | |
| `getlk`, `setlk` | grant or conflict; for `getlk` the type and range of the conflicting lock | | |
| `flush`, `fsync`, `fsyncdir`, `statfs` | result only | | |

`readdir` listings are compared as sets: order is file-system specific, and a
type reported as unknown (`DT_UNKNOWN`) matches any type. At `paranoid`, the
parent listing is compared after `create`, `mkdir`, `mknod`, `symlink`,
`link`, `unlink`, `rmdir` and `rename`; "written" means any `write`,
`fallocate` or `copy_file_range` on the open file.

The whole-file comparison at close (paranoid) and the content repairs and
their verification (resync) only visit the ranges that hold data on either
side (`SEEK_DATA`/`SEEK_HOLE`), so a huge sparse file costs what its data
costs; holes on both sides read as zeros on both. The close-time comparison
stops after 4 GiB of data per file.

## Attributes

Compared (same type required; on a type difference nothing else is compared):

| Field | Note |
|---|---|
| `type` | regular, directory, symlink, fifo, socket, char/block device |
| `mode` | permission bits including setuid, setgid, sticky (`07777`) |
| `uid`, `gid` | |
| `size` | not for directories |
| `nlink` | also for directories, unless `--no-dir-nlink` or the [mount-time probe](fs-differences.md#the-mount-time-probe) found that either side does not count subdirectories (btrfs) |
| `rdev` | device nodes only |
| `mtime` | within `--time-tolerance` (default 1 s) plus the object's slack, see below; directories too. Where POSIX leaves it to the file system whether an operation stamps `mtime` and the [mount-time probe](fs-differences.md#the-mount-time-probe) found the two differ, the secondary's is set to the primary's right after exactly that operation ([aligned](#aligned-mtime)) |
| `ctime` | as a *change*, see below |

**ctime** is not compared as a value (absolute values differ after any copy).
xcheckfs remembers the last ctime pair of each object and reports `attr ctime`
("ctime changed on one side only") when one side's ctime moved by more than
`--time-tolerance` while the other did not move at all. The same happens when
a secondary is modified behind xcheckfs's back
([Limitations](limitations.md#exclusive-access)).

**Slack.** Each file system stamps a changed object's times somewhere inside
the execution window of the operation (from the common start to the later of
the two halves). When that window is wider than a tenth of
`--time-tolerance` (a slow secondary, a saturated machine), it is remembered
for every object the operation changed, also after the kernel forgets the
object, and added to the tolerance of its mtime and ctime checks. A slow
experimental file system therefore never causes timestamp false positives;
it only loosens timestamp checks for the objects it was slow on.

## Aligned mtime

Some operations stamp `mtime` on one file system and not on another, and
POSIX allows both. If the mount-time probe found that the two file systems
differ in one of these, the secondary's `mtime` is set to the primary's right
after exactly that operation, in exactly that situation, so that the
difference is not reported:

- a directory moved to another parent (or two directories exchanged between
  parents with `RENAME_EXCHANGE`): the moved directories' own `mtime`;
- a truncate to the size the file already had (by path or through a file
  handle, as probed), when the same `setattr` sets no `mtime`;
- a hole punched into a range that held no data on the primary.

A secondary that does not stamp `mtime` where every file system must (a
truncate that changes the size, a hole punched into data, a write, a rename
within a parent) is still reported. Alignments are counted in
`aligned_mtimes` ([Control protocol](control-protocol.md#status)); which
differences were found is in `info.adaptations`. Which file systems stamp
what, and why the probe: [Known differences](fs-differences.md),
[Design](../explanation/DESIGN.md#mount-time-probe).

## Racy stats

With `--serialize relaxed` (the default), in-place writes to one file run
concurrently with each other and with stats of that file
([Design](../explanation/DESIGN.md#concurrent-data-operations)). A stat that
overlapped one (taken while it was in flight, or one that started or finished
meanwhile) may see one side's `mtime` and `ctime` already stamped by a write
and the other's not yet, or the two sides reflecting different subsets of
several concurrent writes. For such a stat:

- `mtime` and `ctime` are not compared, and the stored ctime pair (the
  baseline of the ctime change check) is left as it was, so the next quiet
  stat still catches a side whose ctime never moved;
- `type`, `mode`, `uid`, `gid`, `size`, `nlink` and `rdev` are compared as
  always: they cannot change while in-place writes are in flight;
- the skip is counted in `attr_time_skipped`
  ([Control protocol](control-protocol.md#status)).

The rule applies where attributes are compared under a shared lock: `lookup`
(the entry's attributes), `getattr`, and the stat after `fallocate` at
`thorough`. Attributes compared under an exclusive lock cannot overlap an
in-place write.

At `thorough` the skipped comparison has a replacement for writes: after each
`write`, the `write mtime` check requires both sides' `mtime` to be no older
than the start of that write (minus `--time-tolerance`, at least 50 ms; a later `mtime` is
fine, a concurrent write may have stamped it). A file system that does not
update `mtime` on write fails it even if every stat raced. A failure is a
`verify` mismatch with field `write mtime`. Under `--serialize strict` no stat
is racy and the ordinary comparison always applies.

## Never compared

| What | Why |
|---|---|
| `dev`, `ino` | file-system specific |
| `blocks`, `blksize` | allocation specific |
| `atime` | depends on mount options (`noatime`, `relatime`) |
| size of directories | format specific |
| absolute `ctime` | see above |
| `statfs` values | capacities differ by nature; only the result is compared |
| `security.selinux` (by `verify`) | the label is assigned by the security policy for each mount, not kept by the file system |
| order of directory entries and of xattr names | file-system specific |
| `SEEK_DATA` / `SEEK_HOLE` offsets | hole granularity is file-system specific |
| pid of a conflicting lock | always reported as 0 |
| `fsync` durability | not observable ([Limitations](limitations.md#not-covered)) |

## Mismatch kinds

The `kind` of a mismatch, as used in logs, the [control protocol](control-protocol.md)
and [allow rules](rules.md).

| Kind | Meaning |
|---|---|
| `result` | Success or errno differs. |
| `attr` | A returned attribute differs; `field` names it (`mode`, `uid`, `gid`, `size`, `nlink`, `rdev`, `mtime`, `ctime`, `type`). |
| `data` | Returned data differs. |
| `length` | A returned length, count or offset differs (`read`, `write`, `copy_file_range`, `lseek`). |
| `readdir` | Directory listings differ. |
| `readlink` | Symlink targets differ. |
| `xattr` | Extended attribute value or name list differs; `field` is the attribute name or `list`. |
| `identity` | Hard-link structure differs: the primary says two names are one inode, the secondary disagrees (or the reverse). |
| `verify` | A `thorough` read-back shows the mutation was not applied as requested; `field` names the step (`write read-back`, `write mtime`, `setattr applied`, `removed`, `renamed`, `truncated`, `symlink target`, `zeroed range`, `copied range`, `xattr set`, `xattr removed`). |
| `content` | Whole-file content differs on close (`paranoid`). |
| `lock` | Lock grant/deny or the conflicting lock differs. |

A mismatch is *retryable* when its operation is read-only (`lookup`,
`getattr`, `readlink`, `read`, `readdir`, `getxattr`, `listxattr`, `access`,
`statfs`, `lseek`, `getlk`) and *resyncable* unless it is of kind `lock`,
comes from `lseek` or `statfs`, or concerns an object that does not exist on
the secondary ([Design](../explanation/DESIGN.md#repair-resync)).
