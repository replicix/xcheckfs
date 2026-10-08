# Run in CI

Use xcheckfs as a gate: run a workload on a mount of two scratch trees and
fail the job if the experimental file system (the secondary) ever disagrees
with the primary.

## Ready-made suites

`tests/external/` has opt-in scripts that run the libfuse `test_syscalls`
suite (no root needed) and pjdfstest (root) through a mount and fail on any
mismatch; see [tests/external/README.md](../../tests/external/README.md).

## Application tests of xcheckfs

The `Applications` workflow (`.github/workflows/apps.yml`) runs PostgreSQL,
MySQL and multi-process SQLite on an xcheckfs mount in Docker
([tests/apps/README.md](../../tests/apps/README.md)) on pushes to `main`,
pull requests (except documentation-only ones) and by hand, and the release
workflow waits for it. Each application must end with no mismatch, identical
trees (`xcheckfs verify`) and a consistent database on each side. It also
reports throughput and latency against a plain volume:

- the job summary of the run;
- one comment per pull request, updated in place, that compares with the
  latest `main` run that has results (for pull requests from forks, whose
  token is read-only, the job summary has the same report);
- artifacts: `apps-<app>` (results and logs of every run) and, from `main`,
  `apps-results` (the `results.json` files later pull requests compare with).

Use the same pattern for your own workload: `run.sh ci` of a harness is a
template, and `tests/apps/report.py` renders any `results.json`.

## Recipe

`fail` mode returns `EIO` at the first mismatch, so the workload itself
fails; `thorough` or `paranoid` reads back every mutation; the exit code of
`mount` is 3 if any mismatch was recorded. `fail` is meant for CI: the
primary has already applied the operation, so `EIO` would mislead a real
application ([Design](../explanation/DESIGN.md#mismatch-handling)). Always
pass `--on-mismatch fail`: the default, `resync`, repairs the secondary and
lets the workload carry on ([Design](../explanation/DESIGN.md#repair-resync)),
and the final `verify` below would then not see the difference.

```bash
set -u
P=$(mktemp -d)                  # primary: a trusted scratch directory
S=/mnt/experimental/scratch     # secondary: on the file system under test
M=$(mktemp -d)                  # mount point

xcheckfs verify "$P" "$S" || exit 1          # both start empty and identical

xcheckfs mount --check paranoid --on-mismatch fail \
    --rules ci/rules.toml --log-file mirror.log "$M" "$P" "$S" &
XC=$!
until mountpoint -q "$M"; do kill -0 $XC || exit 1; sleep 0.2; done  # give up if mount died

run_the_workload "$M"; WL=$?                  # pjdfstest, fsx, fsstress, your tests...

xcheckfs ctl "$M" mismatches 1000 > mismatches.json   # artifact for the job
fusermount3 -u "$M"
wait $XC; MIRROR=$?                           # 0 clean, 3 mismatches, 1 error

xcheckfs verify "$P" "$S"; VERIFY=$?          # 0 identical, 3 differences, 2 unreadable

[ $WL -eq 0 ] && [ $MIRROR -eq 0 ] && [ $VERIFY -eq 0 ]
```

## Choices

- **Serialization**: the default, `--serialize relaxed`, lets in-place
  writes to disjoint ranges of one file reach the secondary concurrently,
  which is what exposes its concurrency bugs; keep it for CI. A stat that
  races such a write does not compare `mtime`/`ctime`
  ([Checks](../reference/checks.md#racy-stats)). `--serialize strict` is the
  fallback if you need to rule concurrency out.
- **Check level**: `thorough` or `paranoid` ([Checks](../reference/checks.md)).
- **Coverage vs. caching**: by default the kernel caches attributes, entries
  and pages, and cached answers are not re-checked. Add
  `--attr-timeout 0 --entry-timeout 0 --direct-io all` for maximal coverage,
  unless the workload uses shared writable `mmap`
  ([Limitations](../reference/limitations.md#caching)).
- **Expected differences**: keep an [allow-rules file](../reference/rules.md)
  in the repository and pass it with `--rules`. It must be outside the
  mirrored trees. Use `--no-dir-nlink` and `--time-tolerance` for properties
  the secondary cannot match.
- **Foreground job or `--background`**: a foreground job lets you `wait` for
  the exit code (above). With `--background --pid-file FILE` the command
  returns once the mount is up, but the daemon's exit code is not observable:
  read the count instead and then stop the daemon.

  ```bash
  n=$(xcheckfs ctl "$M" status | jq .mismatches)
  kill "$(cat FILE)"                        # or: fusermount3 -u "$M"
  [ "$n" -eq 0 ]
  ```
- **Several instances**: each mount has its own control socket, derived from
  its mount point; pass `--control-socket` if the default location is not
  writable.
- **Privileges**: the runner needs `/dev/fuse` and `fusermount3` (or root).
  As a non-root user, only that user can access the mount unless
  `--allow-other` is set and `user_allow_other` is enabled in `/etc/fuse.conf`.

## Reading the result

- The `MISMATCH #N` lines in the log file, or `mismatches.json`, name the
  operation, kind, path, field and both values
  ([Control protocol](../reference/control-protocol.md#mismatch)).
- A workload failing with `EIO` and no mismatch in the log is a failure of
  the workload or the primary, not of the comparison.
- `verify` after the run finds differences the live comparison could not see
  ([Limitations](../reference/limitations.md#caching)).
