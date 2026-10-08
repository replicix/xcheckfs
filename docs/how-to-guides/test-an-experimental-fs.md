# Test an experimental file system

End-to-end procedure for validating an experimental file system (the
*secondary*) against a trusted one (the *primary*) with a real workload.
For a no-risk first try with scratch directories, do the
[First session](../tutorials/first-session.md) tutorial.

## 1. Protect the primary

xcheckfs sits in the data path of the primary ([Limitations](../reference/limitations.md#risk)).
Take a snapshot or backup of the primary first.

## 2. Seed the secondary

Both trees must be identical before the first mount. As root, so ownership
and xattrs survive:

```bash
rsync -aHAX --numeric-ids /home/ /experimental/
```

`-H` keeps hard links, `-A` ACLs, `-X` xattrs. If the secondary has no xattr
support, drop `-X` and compare with `--no-xattrs` in the next step; the same
difference will later show up in the live comparison and can be silenced with
an [allow rule](../reference/rules.md#examples).

## 3. Verify the trees

```bash
xcheckfs verify /home /experimental
```

Exit code 0 means identical ([CLI](../reference/cli.md#verify)). Fix
differences (re-run `rsync`, or adjust flags such as `--no-dir-nlink`,
`--time-tolerance 2s`, `--no-mtime` for properties the secondary cannot
reproduce) until it is clean, and use the **same** tolerance flags for
`mount`. A difference that is expected and permanent belongs in the
rules file.

## 4. Choose the check level and mode

For a first run: `--check basic --on-mismatch resync` (the defaults). Move to
`thorough` or `paranoid` when basic is quiet ([Checks](../reference/checks.md)).
Use `--on-mismatch freeze` when you are watching and want to decide on each
difference; see [Handle a frozen mismatch](handle-a-frozen-mismatch.md). Use
`--on-mismatch log` when the secondary must not be touched: it stays diverged
after the first mismatch.

In `resync` mode every mismatch is followed by a repair of the secondary from
the primary, which destroys the evidence. Add `--quarantine DIR` (outside both
trees; created if missing) to save the secondary's version first, as
`DIR/<unix-time>-<seq>-<path>/object` with a `mismatch.txt`
([Design](../explanation/DESIGN.md#quarantine)).

## 5. Mount

**Over the primary, as root**: the recommended way, because no application can
reach the primary without going through the mirror.

```bash
sudo xcheckfs mount --ui tui --quarantine /var/tmp/xcheckfs-quarantine /home /home /experimental
```

Stop everything that uses `/home` first: processes that already have files
open or a working directory there keep using the underlying directory and
bypass the mirror ([Limitations](../reference/limitations.md#exclusive-access)).

**At a separate mount point**: the primary stays untouched, but then you must
make sure that applications use only the mount point, and nothing else writes
to the primary or the secondary.

```bash
xcheckfs mount --ui tui --quarantine /var/tmp/xcheckfs-quarantine /mnt /home /experimental
```

At mount, xcheckfs probes both trees (a few operations in a scratch directory
at each root, removed again; the roots' ctime changes) and tells you what it
found: choices POSIX leaves to the file system, on which the two differ, are
adapted to instead of reported (logged at info level, listed under
`info.adaptations`); `fallocate` modes only one of them supports are logged at
warn level with the allow rule that accepts them, and listed under
`info.capability_gaps`. Check both with `xcheckfs ctl MNT status` after the
mount; add the rule only if the gap is acceptable. `--no-probe` switches this
off ([Known differences](../reference/fs-differences.md#the-mount-time-probe)).

Unattended: add `--background --log-file /var/log/xcheckfs.log` (the log file
must be outside the trees) and watch with `ctl`.

## 6. Monitor

- **TUI**: header, operations log, statistics and latencies of both sides,
  in-flight operations and mismatches. Press `?` for the key bindings.
- **Log output**: mismatches are lines `MISMATCH #N ...` at error level; in
  `resync` mode each is followed by a line saying the object was repaired (or
  that the repair failed) and, with `--quarantine`, where the secondary's
  version was saved.
- **Anywhere**: `xcheckfs ctl /home status` (counters, state, and the repair
  counters `resyncs`, `resync_failures`, `resync_giveups`, `quarantined`),
  `xcheckfs ctl /home mismatches 20`, `xcheckfs ctl /home stats` (latencies
  per operation and side). See the [control protocol](../reference/control-protocol.md).

## 7. Handle mismatches

| Mode | What happens | What you do |
|---|---|---|
| `resync` | The application is unaffected (its reply waits for the repair); the mismatch is reported once per (object, operation, kind, field), and the secondary is repaired from the primary. | Read `mismatches` and the `--quarantine` directory; fix the secondary (or add an allow rule if the difference is expected). Watch `resync_failures` and `resync_giveups`: those objects stay diverged and are used on the primary only; re-seed them and run `verify` before trusting later results. |
| `log` | The application is unaffected; the mismatch is reported once per (object, operation, kind, field). | Read `mismatches`; fix the secondary (or add an allow rule if the difference is expected). The secondary has now diverged from the primary: re-seed the object (or use `resync` or `freeze` mode) and run `verify` before trusting later results. |
| `freeze` | The operation, and every new one, waits. | Decide: [Handle a frozen mismatch](handle-a-frozen-mismatch.md). |
| `fail` | The operation returns `EIO` although the primary applied it. | Meant for CI: [Run in CI](run-in-ci.md). |
| `detach` | Mirroring stops for the rest of the session; the primary keeps working. | Use when the secondary is broken beyond usefulness. Remount to resume after repairing. |

You can switch the mode while mounted: `m` in the TUI cycles `resync`, `log`,
`fail`, `freeze`; or `xcheckfs ctl /home mode freeze`.

## 8. Unmount

Quit the TUI (`q`, then confirm), send `SIGTERM`/`SIGINT` to the process, or
run `fusermount3 -u /home` (`umount /home` as root). Frozen operations are
released with `continue`. If the unmount reports the mount busy because
operations are frozen: `xcheckfs ctl /home mode log` and retry.

The process exits with code 3 if any mismatch was recorded, else 0
([exit codes](../reference/cli.md#exit-codes)); it prints a one-line summary.

## 9. Verify again

```bash
xcheckfs verify /home /experimental
```

In `log` mode a clean result after a workload is the end-to-end proof that the
trees stayed identical. In `resync` mode the trees converge by repair, so
judge the run by the mismatch count and `resync_failures`; `verify` still
finds differences the live comparison could not see (for example reads served
from the kernel cache, see [Limitations](../reference/limitations.md#caching)).
After a crash, run `verify` before mounting again.
