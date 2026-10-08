# xcheckfs Decisions (ADRs)

Each record: decision, alternatives rejected, and why. Context in
[DESIGN.md](DESIGN.md). New decisions (or changed minds) are appended here.

## ADR-1: The primary is authoritative

**Decision**: applications always get the primary's results, data, attributes
and inode numbers. The secondary is only compared against it. A mismatch never
changes what the application sees, except in `fail` mode (`EIO`) and in
`freeze` and `resync` modes (a delay). Repairs write to the secondary only
([ADR-11](#adr-11-resync-is-the-default-mismatch-mode)).
**Rejected**: majority voting or "return the secondary's result when the
primary fails" — there are only two file systems, so there is no majority, and
the point is risk-free testing on live data: a broken secondary must not be
able to corrupt what applications read.
**Consequence accepted**: bugs that are only visible as a difference are
found, but bugs in the primary are not (see [GOALS.md](GOALS.md)).

## ADR-2: Compare buffers byte for byte, do not hash

**Decision**: read results and read-backs are compared with plain slice
equality. xxh3 hashes appear only in reports, to make a mismatch easy to
recognise.
**Rejected**: comparing hashes (fewer bytes to hold, cheaper to store) —
both buffers are in memory anyway after the two reads, a byte comparison is
as cheap as hashing them, and it cannot produce a false negative through a
hash collision. Whole-file comparison on close (`paranoid`) streams in 1 MiB
chunks for the same reason.

## ADR-3: Stripe locks, not a global lock

**Decision**: reader/writer locks (65536 by default, `--lock-stripes`) selected by a hash of the node id; each
operation takes the stripes of every object it observes or changes, all at
once, in stripe order, with name -> inode re-validation.
**Rejected**: one global lock — correct but serializes the whole workload and
makes the mirror a bottleneck that distorts what is being tested. Per-inode
locks acquired one after another — deadlocks (`rename` across directories)
unless the set is known up front and acquired in a global order, which is
what the stripe order provides.
**Consequence accepted**: two unrelated inodes can share a stripe and wait for
each other; harmless for correctness.

## ADR-4: `O_DIRECT` is stripped

**Decision**: `O_DIRECT` (and `O_NOCTTY`) are removed from the flags passed to
the backends when opening.
**Rejected**: passing it through — it needs aligned buffers, sizes and
offsets, which the FUSE layer cannot guarantee for the backend's `pread` and
`pwrite`, and it changes caching, not semantics. The kernel's FUSE layer
still handles the caller-visible part. `--direct-io` is a different thing: it
bypasses the *kernel page cache in front of xcheckfs* (for files opened with
`O_DIRECT` by default, for every file with `all`) so the reads and writes
reach the engine ([Limitations](../reference/limitations.md)).

## ADR-5: ctime is tracked as a change, not as a value

**Decision**: absolute ctimes are never compared; the engine remembers each
node's last ctime pair and reports when one side moved by more than
`--time-tolerance` while the other did not move.
**Rejected**: comparing absolute ctimes — they differ after any copy and
cannot be set. Ignoring ctime — misses operations that should update it
(`link`, `unlink`, `rename`, `chmod`, ...) and metadata changed behind
xcheckfs's back. The tolerance admits secondaries with coarse timestamp
granularity.

## ADR-6: Default mode is log; fail is for CI

**Status**: superseded by [ADR-11](#adr-11-resync-is-the-default-mismatch-mode)
for the default; `fail` as the CI mode and the rejections below stand.
**Decision**: `--on-mismatch log` is the default. `fail` returns `EIO` and is
documented as a CI mode.
**Rejected**: `freeze` as default — on an unattended machine it would stop
every application at the first difference. `fail` as default — the primary has
already applied the operation, so `EIO` misleads applications that retry or
roll back ([DESIGN.md](DESIGN.md#mismatch-handling)). In CI that is exactly
what is wanted: the test suite fails loudly.

## ADR-7: Node ids are the primary's inode numbers

**Decision**: the FUSE node id, and so `st_ino`, is the primary's inode
number with the root's number swapped with `1`.
**Rejected**: allocating own ids — applications would see inode numbers that
differ from the primary's, and hard links would need a separate table to be
recognised. **Consequence accepted**: objects on another device than the
primary root cannot be represented; nested mounts return `EIO`.

## ADR-8: Lock mirroring with non-blocking OFD locks and a waiter queue

**Decision**: each lock owner gets its own descriptor on both backends;
locks use `F_OFD_SETLK` (never the blocking variant); blocking requests wait
in a per-inode FIFO queue inside xcheckfs, retried after every lock change.
**Rejected**: blocking in the backends — the two file systems may wake waiters
in different orders, and a blocked call holds a worker thread and the inode's
lockstep lock ([DESIGN.md](DESIGN.md#lock-mirroring)). Forwarding classic
`fcntl` locks — they are per process, and xcheckfs is one process serving all
applications, so owners would merge. Letting the kernel handle locks locally
(`--no-lock-mirroring`) stays available for secondaries without lock
support.
**Consequence**: since xcheckfs never blocks inside a backend, it keeps
the wait-for graph itself (a shadow of granted ranges per owner and of
blocked requests) to answer `EDEADLK` like the kernel, and it ends a blocked
request whose thread has a pending signal with `EINTR` (the FUSE library
drops `FUSE_INTERRUPT`, so `/proc/<tid>/status` is checked). The pid of a
conflicting lock is reported as 0 ([Limitations](../reference/limitations.md)).

## ADR-9: flock(2) stays kernel-local; ioctl is not mirrored

**Decision**: `flock` locks are left to the kernel (the FUSE flock capability
is not requested); `ioctl` returns `ENOTTY`, `poll` and `bmap` return
`ENOSYS`.
**Rejected**: mirroring ioctl — the request and reply are opaque bytes whose
meaning only the file system knows, so a mirror cannot decide what to execute
twice or how to compare replies. Mirroring `flock` — it would need a second
lock-table design on top of the `fcntl` one.

## ADR-10: Backends work on descriptors, not paths

**Decision**: backends hand out `O_PATH` descriptors and regular descriptors
and take only descriptors (plus a single name component) as input.
**Rejected**: path-based backends — a rename through the mount would let a
cached path point to a different object on one side only, producing
exactly the divergence xcheckfs is meant to find, but caused by itself.
**Consequence accepted**: two descriptors per cached inode, and `RLIMIT_NOFILE`
is raised to the hard limit.

## ADR-11: resync is the default mismatch mode

**Status**: supersedes [ADR-6](#adr-6-default-mode-is-log-fail-is-for-ci) for
the default.
**Decision**: `--on-mismatch resync` is the default. A mismatch is reported,
the secondary's object is repaired from the primary and verified, and the
application gets the primary's result ([Design](DESIGN.md#repair-resync)).
**Why**: the purpose of a long run on live data is to test every object. In
`log` mode an object that diverged stays different: every later operation on
it compares against the old difference, repeats are only counted, and a new
defect on it cannot be told from the first. Objects that no longer exist on
the secondary stop being compared at all. After a repair the object is
identical again, so the next mismatch is a new finding.
**Rejected**: keeping `log` as the default, for the reasons above; it stays
available for runs that must not touch the secondary. `freeze` as default:
see [ADR-6](#adr-6-default-mode-is-log-fail-is-for-ci). Unlimited repairs: a
secondary that diverges again after every repair would be rewritten forever,
so resync gives up on a path after `--resync-limit` repairs in 10 minutes.
Quarantine on by default: it needs a directory outside both trees, which
xcheckfs cannot choose, and can copy up to `--quarantine-cap` per object.
**Consequence accepted**: repairs hide the evidence on the secondary unless
`--quarantine` is given. A repair writes through the secondary's own write
path; if that is the buggy part, verification catches it. Resyncing a large
object runs inside the operation that hit the mismatch and blocks the object
meanwhile. The secondary is pushed back to the primary's state, so a clean
end state proves nothing: the mismatch counter is the result
([Limitations](../reference/limitations.md#repair)).

## ADR-12: Relaxed serialization: byte-range locks for in-place data operations

**Decision**: with `--serialize relaxed` (the default), `read`, `write`,
`fallocate` and the `copy_file_range` destination that change neither the
file's size nor its metadata take the object's stripe shared plus a
first-come-first-served byte-range lock (exclusive for writers, shared for
readers), held across both halves. Disjoint ranges of one file therefore run
concurrently on both file systems; overlapping ranges are ordered by the lock
and execute in that order on both sides. Everything that changes the size or
is not purely in place (`O_APPEND`, writes past the end, `truncate`,
`setattr`, `fallocate` that extends or collapses/inserts, files with
set-uid/set-gid bits) stays exclusive. A stat that overlapped an in-place
write skips the `mtime`/`ctime` comparison (the racy-stat rule), keeps the
ctime baseline, and `thorough` adds a per-write check that each side's
`mtime` is not older than the write's start ([Design](DESIGN.md#concurrent-data-operations)).
**Why**: the operations that matter most for testing a file system, parallel
I/O to the same file by databases and similar applications, are the ones
where concurrency bugs live. Serializing them in the mirror hides those bugs
from the experimental file system, so the change is about coverage first and
speed second.
**Rejected**: strict per-object exclusive writes (the previous behavior, still
available as `--serialize strict`): it never presents the secondary with two
writes to one file at once, which hides concurrency defects, and it makes
the mirror slower than the workload needs to be. No ordering at all: two
overlapping writes could then be applied in different orders on the two file
systems, leaving different bytes behind, which would be reported as a
mismatch (and repaired in `resync` mode) although neither file system is
wrong. Comparing timestamps of racing stats anyway: one side would have
stamped a concurrent write that the other has not yet, a false `attr` mismatch
with a window as wide as the slower file system.
**Consequence accepted**: time comparison is lost for a stat that overlapped
an in-place write: `mtime` and `ctime` of that stat are not compared, and the
`thorough` write check and the next quiet stat are what catch a file system
that does not update them. Other attributes are compared as always.
`FUSE_HANDLE_KILLPRIV_V2` is deliberately not enabled: the file system would
have to clear set-uid and set-gid bits on write itself, and that behavior
would have to be mirrored and compared on both sides; deferred.

## ADR-13: Adapt to legitimate file-system differences found by a mount-time probe

**Decision**: at mount (unless `--no-probe`) a few operations run in a
scratch directory at the root of each tree and show, per file system, where
POSIX leaves a choice: whether a directory's link count counts its
subdirectories, whether moving or exchanging directories across parents
stamps their `mtime`, whether a truncate to the current size does (by path and
by descriptor), whether punching a hole where there is no data does. Where the
two file systems differ, the engine adapts: directory link counts are not
compared; otherwise, right after exactly that operation in exactly that
situation, the secondary's `mtime` is set to the primary's (`utimens`) and the
node's ctime baseline is reset. Optional operations that only one side
supports (`fallocate` modes) are not adapted to: they are logged at warn
level with the allow rule that accepts them. Adaptations and gaps are listed
in `ctl status` ([Design](DESIGN.md#mount-time-probe)).
**Why**: these differences are properties of correct file systems, so
reporting them is noise that hides real mismatches; and the experimental file
system under test has no known type.
**Rejected**:
- A table keyed by file system magic: it does not cover an experimental file
  system, and goes stale as file systems change between kernel versions.
- Skipping the `mtime` comparison per node until its next change: it needs
  state per node, and hides more, including a secondary that never stamps
  `mtime` where it must.
- Leaving it to the user with allow rules: manual, and the same noise until
  the rules exist; a rule cannot tell the legitimate case from a defect of the
  same field.
**Consequence accepted**: the probe writes a scratch directory into each
root (the roots' ctime changes; atime and mtime are restored), and the
secondary's `mtime` is modified by xcheckfs (`utimens`, which also moves its
ctime). A root that is not writable is not probed and nothing is adapted. The
adaptation is limited to the exact situation, so a secondary that does not
stamp `mtime` elsewhere is still reported.
