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

- **Stripe locks, not one global lock.** Locks are 4096 reader/writer locks
  ("stripes") selected by a hash of the node id. Read-only operations share,
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
  the number of CPUs, at most 16).
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
