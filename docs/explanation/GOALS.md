# xcheckfs Goals

xcheckfs validates an experimental file system against a trusted one by
running both on the same, real workload and comparing every result. One
binary, no services, no changes to the file systems under test.

## High Level Goals

- **Test on live data without risk to the workload.** The primary is
  authoritative: applications always get its results and its data. The
  secondary can be wrong, slow, or broken without anyone noticing, except
  through xcheckfs's reports.
- **Any POSIX-compatible file system, on either side.** Native, FUSE, or
  network file systems; xcheckfs only needs a directory on each. It knows
  nothing about the file system under test.
- **No false positives under concurrency.** Conflicting operations are
  executed on both file systems in the same order, and nothing can observe
  one file system between the two halves of an operation
  ([Design](DESIGN.md#lockstep-execution)). A report means a real
  disagreement or a documented limitation.
- **Compare what applications can observe.** Return codes, attributes,
  data, directory listings, link targets, xattrs, hard-link structure, lock
  results ([Checks](../reference/checks.md)). Properties that are
  file-system specific by nature are deliberately left out.
- **Choose how strict to be.** Three check levels (`basic`, `thorough`,
  `paranoid`) trade cost for coverage; five mismatch modes (`resync`, `log`,
  `fail`, `freeze`, `detach`) decide what a disagreement does to the workload.
- **Known differences are data, not noise.** Allow rules silence
  expected differences, and identical repeated mismatches are reported once.
- **Operable.** A TUI for watching and deciding, a control socket for
  scripts and CI, a background mode, an offline `verify` to prepare and
  audit the two trees.

## Non-Goals

- **Testing durability.** Power-loss and crash consistency are not tested
  ([Limitations](../reference/limitations.md)).
- **A replication or backup tool.** The secondary receives a copy of every
  operation, but xcheckfs is not meant to keep it in sync. Only `resync`
  repairs a single object on operator request, and `verify` only reports.
- **Performance benchmarking.** Both sides run concurrently and every
  operation takes locks and compares; latencies are reported per side but
  are not benchmark results.
- **Coverage of everything a kernel offers.** Arbitrary `ioctl`, `poll`,
  `bmap`, `O_TMPFILE`, `statx` and `syncfs` are not mirrored
  ([Limitations](../reference/limitations.md)).
- **Sharing the trees.** Both trees must be accessed only through the mount
  while it runs.
- **Multiple secondaries.** The comparison is primary versus one secondary.

## Prior art

Differential testing and N-version comparison run the same input through
independent implementations and treat disagreement as a signal. xcheckfs
applies this to file systems with real applications as the input generator
and one implementation designated as the oracle.

Established file system test tools check one file system against fixed
expectations, not against a reference:

- **pjdfstest** encodes POSIX behavior as individual assertions.
- **fsx** and **fsstress** generate random operation sequences and check
  internal consistency (fsx compares reads with an in-memory model; fsstress
  mostly looks for crashes and errors).
- **xfstests** runs a curated catalog of regression tests with expected
  output.

These are complementary: they find defects against the specification,
xcheckfs finds divergences from a particular trusted behavior, including
the undocumented behavior real software depends on. Running those tools
*on top of* the mount (see [Run in CI](../how-to-guides/run-in-ci.md)) uses
their workloads and xcheckfs's oracle together.
