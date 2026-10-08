# xcheckfs Design

Companion documents: [GOALS.md](GOALS.md), [DECISIONS.md](DECISIONS.md),
[TESTING.md](../how-to-guides/development/TESTING.md). This document
explains how the system works and why; the reference pages hold the exact
flags, formats and lists, and the ADRs record the alternatives rejected.

## Table of Contents

- [Overview](#overview)
- [Backends](#backends)
- [Node ids](#node-ids)
- [Lockstep execution](#lockstep-execution)
- [What an operation does](#what-an-operation-does)
- [Concurrent data operations](#concurrent-data-operations)
- [Mount-time probe](#mount-time-probe)
- [Credentials and permissions](#credentials-and-permissions)
- [Directory listings](#directory-listings)
- [Lock mirroring](#lock-mirroring)
- [Mismatch handling](#mismatch-handling)
- [Freeze gate](#freeze-gate)
- [Deduplication](#deduplication)
- [Repair (resync)](#repair-resync)
- [Detach](#detach)
- [Processes and threads](#processes-and-threads)

## Overview

```
   application
        |
   kernel VFS ──► FUSE ──► xcheckfs engine
                              |  1. freeze gate
                              |  2. stripe locks (all at once)
                    ┌─────────┴─────────┐
                    ▼                   ▼  3. run concurrently
               primary backend     secondary backend
                    └─────────┬─────────┘
                              |  4. compare, report to policy
                              ▼
                     primary's result ──► application
```

The engine implements the FUSE operations by running each one on two
*backends*, comparing the outcomes, and returning the primary's. It is
independent of FUSE so the test suite drives it directly
([TESTING.md](../how-to-guides/development/TESTING.md)).

## Backends

A backend is one directory on one file system. It works purely on file
descriptors it handed out itself: `O_PATH` descriptors for inodes and
regular descriptors for open files and directories. The engine never touches
paths, so a rename performed through the mount cannot make a backend operate
on the wrong object. Paths exist only for reports and are resolved from the
descriptor (`/proc/self/fd`) when needed.

The roots are opened when xcheckfs starts, before the mount, which is why the
mount point may be the primary itself: the backend keeps reaching the
underlying directory while applications see the mount.

## Node ids

Every object the kernel has looked up is a *node* holding one `O_PATH`
descriptor per backend. The node id (the FUSE inode number, and so the
`st_ino` applications see) is the **primary's inode number**, with the
root's inode number swapped with `1` (FUSE requires the root to be 1; the
swap keeps ids unique). Consequences:

- Applications see the primary's inode numbers, so inode-based logic keeps
  working.
- Hard links are identical by construction on the primary. On lookup xcheckfs
  also checks that the secondary agrees: two names are the same inode on one
  side if and only if they are on the other (`identity` mismatch).
- An object on a different device than the primary root (a nested mount) is
  refused with `EIO`; identities would no longer be unique.
- A node whose object does not exist on the secondary has no secondary
  descriptor: it has *diverged*, and operations on it run on the primary only
  (counted as `secondary_skipped`). It is connected to the secondary again
  when the object exists there again (after a [repair](#repair-resync)).

## Lockstep execution

For every operation, the engine takes the locks of *every object whose
observable state the operation reads or changes*, runs both halves, and
compares the results while still holding the locks. Conflicting operations
therefore hold the same lock while both file systems execute them: both see
conflicting operations in the same order, and no other operation can observe
one file system "between" the two halves. Without this, concurrent workloads
would produce false positives (the two file systems legitimately end up in
different orders), and a checker that cries wolf is useless.

- **Stripe locks, not one global lock.** Locks are reader/writer locks
  ("stripes", 65536 by default, `--lock-stripes`) selected by a hash of the
  node id. Read-only operations share,
  mutations are exclusive. Independent objects proceed in parallel, so the
  mirror does not serialize the workload ([ADR-3](DECISIONS.md#adr-3-stripe-locks-not-a-global-lock)).
- **All at once, in stripe order.** An operation sorts the stripes it needs
  (merging duplicates, exclusive wins) and acquires them in that order, so
  operations cannot deadlock on each other.
- **Re-validating names.** Operations that name children (`create`, `unlink`,
  `rename`, `link`, `lookup`) must lock the child's inode, but the name ->
  inode mapping can change before the lock is held. The engine looks the
  names up on the primary, locks, looks up again, and retries (bounded)
  until the mapping is the one it locked.
- **Concurrent dual execution.** The two halves run at the same time (the
  secondary on a worker pool, the primary on the calling thread), so an
  operation costs the slower side, not the sum. `--sequential` runs the
  secondary after the primary instead.

## What an operation does

1. **Freeze gate**: blocks while a mismatch awaits a decision.
2. **Lock** the stripes of everything the operation observes or changes.
3. **Execute** on both backends; record both latencies.
4. **Compare** return codes first, then what the operation returns, then
   (higher check levels) read the effect back; report to the policy. The
   exact comparisons per level are in the [checks reference](../reference/checks.md).
5. **Repair**, in `resync` mode, after the locks are released
   ([below](#repair-resync)).
6. **Return the primary's result**, unless the policy says otherwise
   ([modes](#mismatch-handling)).

**Compare before insert.** When a lookup or create produces a new node, the
engine checks identity and attributes against the secondary *before* adding
the node to the table. A node that failed its checks is therefore never
cached half-validated, and a retry after a decision starts clean.

**Buffers are compared byte for byte.** Reads and read-backs are compared
with `memcmp`-equivalent slice equality; hashes (xxh3) appear only in reports
([ADR-2](DECISIONS.md#adr-2-compare-buffers-byte-for-byte-do-not-hash)).

**ctime is tracked as a change, not as a value.** After any copy the
absolute ctimes differ between the two trees, so they are never compared.
Instead the engine remembers the last ctime pair of each node and reports a
difference when one side's ctime moved while the other did not. This catches
metadata changes made behind xcheckfs's back as well as operations that
should have updated ctime but did not ([ADR-5](DECISIONS.md#adr-5-ctime-is-tracked-as-a-change-not-as-a-value)).

## Concurrent data operations

With `--serialize strict` every write holds its object's stripe exclusively,
so one file never sees two writes at once on either file system. That is
safe but hides exactly the bugs a busy application provokes: a database
writing many pages of one file in parallel would reach the experimental file
system one write at a time, and its concurrency defects would never show.
`--serialize relaxed` (the default) keeps the comparison sound and lets
non-overlapping data operations on one file run concurrently on both file
systems
([ADR-12](DECISIONS.md#adr-12-relaxed-serialization-byte-range-locks-for-in-place-data-operations)).

**What runs concurrently.** Operations that change neither the file's size
nor its metadata, *in place*:

- `read`;
- `write` that is not `O_APPEND` and ends within the primary's current size;
- `fallocate` with `FALLOC_FL_KEEP_SIZE` (which `PUNCH_HOLE` implies), or
  whose range ends within the size, except `COLLAPSE_RANGE` and
  `INSERT_RANGE`;
- `copy_file_range`, when the destination range ends within the size and, for
  a copy within one file, source and destination do not overlap;
- `lseek` with `SEEK_DATA` or `SEEK_HOLE` and the close-time content
  comparison of `paranoid`, which hold the whole file as a shared range.

Such an operation takes the object's stripe **shared**, then a **byte-range
lock**: exclusive for a writer, shared for a reader. The size cannot change
under a shared stripe (only exclusive holders change it), which is what makes
"within the size" a stable test. The range table of an object is
first-come-first-served: a request waits for every earlier conflicting one,
held or queued, so a stream of readers cannot starve a writer. An operation
that needs several ranges (`copy_file_range`) asks for them as one request.
All stripes are taken before any range, and ranges in node id order, so
waits cannot form a cycle.

**What stays exclusive.** Everything else: `O_APPEND` writes, writes that
extend the file, `truncate` and the other `setattr` steps, `fallocate` that
extends the file or uses `COLLAPSE_RANGE` or `INSERT_RANGE`, a
`copy_file_range` that extends its destination or copies within one file with
overlapping ranges, and any write, size-bound `fallocate` or `copy_file_range`
destination on a file with set-uid or set-gid bits (an unprivileged write
clears them, a mode change a concurrent `getattr` could see on one file
system only). The operation is first looked at under the
shared stripe; if it does not qualify, the shared stripe is released and the
exclusive one taken.

**Why the comparison stays sound.** Both halves of an operation run while
the operation holds its range, so two operations with overlapping ranges
(one of them a writer) are ordered by the range lock and execute in that
order on both file systems. Writes to disjoint ranges commute on every POSIX
file system, so it does not matter that the two sides interleave them
differently. Without the range lock, overlapping writes could be applied in
different orders on the two sides and leave different bytes behind: a false
mismatch, and in `resync` mode a repair of an innocent secondary.

**Racy stats.** A stat taken while in-place writes are in flight can see one
file system's `mtime` and `ctime` already updated by a write and the other's
not yet, or the two sides reflecting different subsets of several concurrent
writes. Each object counts its writing data operations in flight, with a
sequence number that changes on every start. A `getattr`, a `lookup`, and the
stat after a `fallocate` note the object's state before the stat; if an in-place write was in
flight at that point, started, or finished meanwhile, the stat *overlapped*
one. For such a stat `mtime` and `ctime` are not compared, and the ctime
baseline ([ctime tracking](#what-an-operation-does)) is left unchanged, so the
next quiet stat still catches a file system that never updates ctime. Size,
type, mode, owner and link count cannot change under a shared stripe and are
compared as always. Skipped comparisons are counted (`attr_time_skipped`). At
`thorough`, each in-place write additionally checks that both sides' `mtime`
is not older than the moment the write started (minus `--time-tolerance`, at least 50 ms):
this "write mtime" check still catches a file system that does not stamp
`mtime` at all ([Checks](../reference/checks.md#racy-stats)).

**What the kernel serializes by itself.** The mirror can only see what
reaches it. Buffered writes to one file are serialized by the kernel's inode
lock before they reach FUSE, so with the page cache a file sees one write at
a time; directory mutations are serialized by the VFS per directory. To
let more reach xcheckfs, the mount requests:

- `FUSE_PARALLEL_DIROPS`: lookups and other operations in one directory in
  parallel;
- `FUSE_ASYNC_DIO`: the requests of one asynchronous direct I/O submission
  reach the file systems together instead of one after another;
- `FUSE_DIRECT_IO_ALLOW_MMAP`: shared `mmap` of files opened with direct I/O
  (kernel 6.7 or newer);
- a larger background queue (`max_background` 64, congestion threshold 48),
  so readahead and asynchronous direct I/O keep many requests in flight;
- per file, with `--direct-io auto` (the default) for files opened with
  `O_DIRECT`, and with `all` for every file, direct I/O with
  `FOPEN_PARALLEL_DIRECT_WRITES`: the kernel then sends writes that do not
  extend the file to xcheckfs concurrently instead of holding the inode lock.

`FUSE_HANDLE_KILLPRIV_V2` is deliberately not requested. With it the file
system, not the kernel, would have to clear set-uid and set-gid bits on
write, and the mirror would have to apply and compare that on both sides;
that is deferred. Writable mmap pages reach xcheckfs through the kernel's
writeback, at times the application does not control
([Limitations](../reference/limitations.md#caching)).

The workers keep up with this: the FUSE pool is larger than the CPU count
(see [Processes and threads](#processes-and-threads)), and the pool that runs
the secondary halves is at least as large, so a secondary half never queues
behind other operations' halves.

## Mount-time probe

POSIX leaves a few choices to the file system, and two correct file systems
choose differently: whether a directory's link count counts its
subdirectories, whether moving a directory to another parent stamps the
directory's own `mtime`, whether `RENAME_EXCHANGE` of directories in
different parents does, whether a truncate to the size a file already has
stamps `mtime` (by path and through a descriptor), and whether punching a
hole into a range without data does. Reporting these would bury the
mismatches that matter; the measured differences are in
[Known differences](../reference/fs-differences.md).

**Probe, not a table.** At mount (unless `--no-probe`) the engine runs a few
operations in a scratch directory `.xcheckfs-probe-<pid>-<hex>` at the root
of each tree, through the same backend calls it uses for real operations,
and records what each file system did. It then removes the directory and
puts the root's atime and mtime back. The experimental file system has no
known type to look up, and a table keyed by file system type goes stale; a
probe covers whatever is mounted
([ADR-13](DECISIONS.md#adr-13-adapt-to-legitimate-file-system-differences-found-by-a-mount-time-probe)).
If a root is not writable the probe learns nothing and nothing is adapted.

**What is adapted to, and what is not.** Only choices POSIX allows:

- directory link counts are not compared if either side does not count
  subdirectories;
- for the mtime choices, the engine acts only when the primary and the
  secondary differ. Right after the operation, and only in the situation
  that differs (a directory moved or exchanged across parents; a truncate
  whose size equals the primary's size before and that sets no `mtime`
  itself, by path or by descriptor as probed; a hole punched into a range
  that held no data on the primary), it sets the secondary's `mtime` to the
  primary's with `utimens`. That moves the secondary's ctime, so the node's
  ctime baseline is reset ([ctime](#what-an-operation-does)). A punch hole
  that may be aligned takes the file exclusively under relaxed serialization,
  so a concurrent write cannot stamp `mtime` between the punch and the
  alignment. Each alignment counts in `aligned_mtimes`.

Optional operations that only one side supports (`fallocate` modes) are not
adapted to: an experimental file system that lacks one should be noticed. The
mount logs each at warn level with the allow rule that accepts the resulting
`result` mismatches, and lists them under `info.capability_gaps`; adaptations
are logged at info level and listed under `info.adaptations`
([Control protocol](../reference/control-protocol.md#status)).

**Why the alignment is so narrow.** It applies only to the exact operation
and situation where file systems are known to differ, so a secondary that
never stamps `mtime` is still caught: a size-changing truncate, a hole
punched into data, a write, and a rename within one parent all stay
compared. Aligning broadly (say, skipping the `mtime` comparison for a node
until its next change) would hide those defects.

**Cost.** The probe creates and removes files in each root (the roots' ctime
changes), and the secondary's `mtime` is modified by xcheckfs where aligned.

When the probe could not run, a btrfs side (by file system type) still
switches off directory link-count comparison, as before the probe existed.

## Credentials and permissions

The mount uses `default_permissions`: the kernel checks access against the
attributes xcheckfs returns (the primary's), with the caller's full
credentials. Both backends are then driven with the same decisions.

When xcheckfs runs as root, it switches the executing thread to the caller's
`fsuid`, `fsgid` and supplementary groups (read from `/proc/<pid>/status`,
cached for two seconds) around every mutating operation, `open` and `access`.
Created files get the caller's ownership on both file systems, and the
secondary's own permission checks run for the caller, so a secondary that
mishandles permissions is detected as a `result` mismatch. `--no-creds`
disables the switch. Run as a non-root user, everything runs as that user on
both sides.

## Directory listings

`readdir` at offset 0 takes a *snapshot* from both backends. The snapshots
are compared **as sets** (names and, where known, types; directory order is
file-system specific), and the application is served the primary's snapshot
for all further offsets. Offsets are therefore stable even while the
directory changes under an open listing.

## Lock mirroring

POSIX record locks (`fcntl` `F_SETLK`, `F_SETLKW`, `F_GETLK`) are mirrored so
both file systems see the same lock tables ([ADR-8](DECISIONS.md#adr-8-lock-mirroring-with-non-blocking-ofd-locks-and-a-waiter-queue)).

- Each lock owner (the kernel's `lock_owner`, i.e. a process) gets its own
  open file description on both backends, and locks are taken with
  **non-blocking open-file-description locks** (`F_OFD_SETLK`). The results
  of both sides are compared (granted or conflicting; `EAGAIN` and `EACCES`
  count as the same conflict).
- A blocking request that conflicts on both sides is **queued by xcheckfs**
  per inode. After every successful lock change or owner release, queued
  requests are retried in FIFO order.
- Closing the file releases the owner's locks on both sides, retries the
  queue, and cancels the owner's own waiters with `EINTR`.
- Because nothing blocks in the kernel's lock code, xcheckfs does the
  kernel's two other jobs for blocked requests itself: a request that would
  wait for itself through a chain of owners (also across files) is answered
  `EDEADLK`, using a shadow of the granted ranges and of the queued requests;
  and a request whose thread has a pending signal is ended with `EINTR` (a
  watchdog reads `/proc/<tid>/status`, since the FUSE library drops
  `FUSE_INTERRUPT`).

**Why not block inside the backends?** Two reasons. First, when a lock is
released, the two file systems may wake their waiters in different,
unspecified orders; the owners that hold the lock would then differ and the
lock tables diverge, producing false `lock` mismatches. A single FIFO queue
on the xcheckfs side wakes waiters in one order for both. Second, a blocked
backend call parks a worker thread while it holds the inode's lockstep lock,
so the operation that would release the lock cannot proceed.

`flock(2)` is not mirrored: the kernel handles it locally
([ADR-9](DECISIONS.md#adr-9-flock2-stays-kernel-local-ioctl-is-not-mirrored)).
`--no-lock-mirroring` hands `fcntl` locks to the kernel as well.

## Mismatch handling

Every disagreement becomes a *mismatch* with a kind (`result`, `attr`,
`data`, `length`, `readdir`, `readlink`, `xattr`, `identity`, `verify`,
`content`, `lock`; see [checks](../reference/checks.md#mismatch-kinds)), an
operation, a path, and the two values. The policy then:

1. drops it if an [allow rule](../reference/rules.md) matches (counted as
   `allowed`);
2. drops it if an identical one was already reported ([dedup](#deduplication));
   in `resync` mode a repeat is still repaired;
3. records it, logs it at error level, notifies the UI, and acts according to
   the mode.

| Mode | Effect on the operation |
|---|---|
| `resync` | The secondary is [repaired](#repair-resync); the primary's result is returned, after the repair. |
| `log` | The primary's result is returned; the secondary stays diverged. |
| `fail` | `EIO` is returned. |
| `freeze` | The operation waits for an operator decision ([freeze gate](#freeze-gate)). |
| `detach` | The secondary is [detached](#detach); the primary's result is returned. |

**Fail-mode caveat.** The primary has already applied the operation when the
mismatch is found. `EIO` therefore tells the application that the operation
failed although it took effect, which misleads applications that retry or
roll back. `fail` is meant for CI, where the goal is to make a test fail
loudly, not to keep an application correct ([ADR-6](DECISIONS.md#adr-6-default-mode-is-log-fail-is-for-ci)).
The default is `resync` ([ADR-11](DECISIONS.md#adr-11-resync-is-the-default-mismatch-mode)).

The mode can be changed at runtime (TUI or `ctl mode`). Leaving `freeze`
releases everything pending with `continue`.

## Freeze gate

In `freeze` mode a mismatch blocks the reporting thread inside the policy
until an operator decides. The thread holds its stripe locks, so conflicting
operations wait behind it, and every *new* operation blocks at the gate at
the start of `run`. Operations already past the gate may finish and may
report further mismatches, each pending separately. When the last pending
decision is made, the gate opens.

Decisions: `continue`, `allow` / `allow-path` (adds an allow rule, persisted
when possible, then continues), `retry` (re-executes a read-only operation on
both sides), `resync`, `fail`, `detach`. How to use them:
[Handle a frozen mismatch](../how-to-guides/handle-a-frozen-mismatch.md).

**Freeze blocks applications in the kernel.** They sit in uninterruptible
FUSE requests until released ([Limitations](../reference/limitations.md)).
Unmounting or quitting the TUI releases all pending mismatches with `continue`.

## Deduplication

A workload that reads a corrupt file in a loop would report the same
mismatch forever. The policy keys mismatches by *(node, operation, kind,
field)*: the first is reported, later identical ones only increment the
`repeats` counter. In `fail` mode repeats still return `EIO`; in `resync`
mode they are repaired again (the bug struck again; see
[limits](#repair-limits-and-failures)); in the other modes they continue. Choosing `retry` forgets the key, so a retry that still
fails can freeze again.

Allowed mismatches never enter the dedup table and are counted separately.

## Repair (resync)

Repair makes the secondary equal to the primary again, so that a run on live
data keeps testing every object instead of comparing objects that already
diverged ([ADR-11](DECISIONS.md#adr-11-resync-is-the-default-mismatch-mode)).
**The primary is never modified.** In `resync` mode (the default) every
repairable mismatch requests a repair; in `freeze` mode the operator's
`resync` decision does the same.

The request is queued on the operation. It runs after the operation released
its locks and before the reply, under locks of its own, so the application
gets the primary's result as usual, only later. The operation is not
re-executed (`retry` does that). What is repaired depends on the mismatch:

| Mismatch | Repair |
|---|---|
| About a name in a directory (`lookup`, `create`, `unlink`, `rename`, ...) | **Entry**: that name. A `rename` repairs both names. |
| `readdir` | **Directory**: every name in which the two listings differ. |
| Anything else | **Object**: the one object. |
| `lock`, or from `lseek` or `statfs` | Not repairable: lock tables, seek offsets and capacities are not object state. |

**Object repair** rewrites the content of a regular file (only the 1 MiB
chunks that differ, then the size) and copies extended attributes, owner,
mode and times. A directory object is repaired as a directory: its entries
and its attributes. An object whose type or symlink target differs cannot be
fixed in place; its entry is replaced.

**Entry repair** compares the name on both sides and:

- copies an entry that is missing on the secondary, with whole subtrees;
- removes an entry that is only on the secondary;
- replaces an entry of the wrong type, with the wrong symlink target, or with
  the wrong hard-link identity;
- repairs the object of an entry that matches in type: for a directory its
  attributes only, its contents are repaired by mismatches about them.

Copies preserve hard links. A copied file with several names is linked to a
copy made by the same repair, to the secondary object of a known node, or to
another name found by a bounded search (20 000 entries) under the repaired
directory. Otherwise it is copied as an independent file, and verification
then reports the repair as failed.

### Verification

Every repair is verified by comparing again: attributes, content, listing,
link target, xattr names, and hard-link identity of the repaired entries.
Objects of known nodes that were replaced are reconnected to their new
secondary object, and an object that had no secondary object reconnects on
its next lookup once it exists again. Open handles opened before the
secondary object was replaced continue on the primary only.

### Repair limits and failures

A repair that fails verification counts as `resync_failures` and leaves the
object diverged. A non-directory is then used on the primary only (counted in
`secondary_skipped`). Resync also gives up on a path after `--resync-limit`
repairs within 10 minutes (counted in `resync_giveups`), with the same
consequence: a secondary that diverges again after every repair would
otherwise be rewritten forever. Both are logged at error level.

### Quarantine

With `--quarantine DIR`, the secondary's version of an object is copied,
recursively and up to `--quarantine-cap` bytes, **before** a repair
overwrites or removes it:

```
DIR/<unix-time>-<seq>-<path with / as %>/object        the copy
DIR/<unix-time>-<seq>-<path with / as %>/mismatch.txt  path, reason, the secondary's stat, notes
```

`notes` records what is missing, for example a truncation at the cap. Without
`--quarantine` a repair is only reported (log line and the counters), and the
evidence on the secondary is gone. `DIR` is created if missing and must not be
inside a mirrored tree. What cannot be repaired: [Limitations](../reference/limitations.md#repair).

## Detach

`detach` (a mode, a freeze decision, `D` in the TUI, or `ctl detach`) stops
all mirroring for the rest of the session: operations go to the primary only,
nothing is compared, and nothing is reported. It exists so a broken secondary
can be taken out without unmounting the primary's tree. It cannot be undone;
unmount and mount again to resume.

## Processes and threads

- FUSE requests are served by a pool of worker threads (`--threads`, default
  twice the number of CPUs, between 16 and 64: workers mostly wait in two
  file systems' system calls). The secondary halves of operations run on a
  second pool, at least as large as the FUSE pool and the CPU count (at
  most 128).
- `RLIMIT_NOFILE` is raised to the hard limit: each cached inode holds two
  `O_PATH` descriptors.
- `SIGINT`, `SIGTERM` and `SIGHUP` release frozen operations and unmount
  cleanly; `SIGUSR1` logs a one-line summary.
- `--background` forks before any thread exists, detaches, and keeps the
  parent waiting until the mount is up, so the parent's exit status reports
  whether the mount succeeded.
- The control socket, log file, pid file and rules file must not live inside
  a mirrored tree: writing them through the mount would be mirrored too, and
  while frozen it would deadlock.
