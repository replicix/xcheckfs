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

**Decision**: 4096 reader/writer locks selected by a hash of the node id; each
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
bypasses the *kernel page cache in front of xcheckfs* so every read and write
reaches the engine ([Limitations](../reference/limitations.md)).

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
**Consequence accepted**: no `EDEADLK` detection, the pid of a conflicting
lock is reported as 0, and `FUSE_INTERRUPT` is not handled
([Limitations](../reference/limitations.md)).

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
