# SQLite stress through xcheckfs

Multi-process SQLite is the worst case for lock mirroring and shared `mmap`: every transaction takes `fcntl`
byte-range locks (rollback-journal modes around the 1 GiB offset; WAL additionally on the `-shm` file), the WAL index
(`-shm`) is mapped `MAP_SHARED` by every process, and the I/O is many tiny writes, fsyncs, journal
create/delete/truncate and checkpoints.

Needs Docker, a static `xcheckfs` binary, and an image with Python 3 (default `python:3-alpine`; only the stdlib
`sqlite3` module is used). Each run uses its own container and two volumes named `xc-sqlite-*` and removes them
afterwards.

```bash
# all scenarios and modes, three configurations (baseline, basic, thorough)
tests/apps/sqlite/run.sh -x /dir/with/xcheckfs -o /tmp/out -d 120

# quick look: one scenario, one mode, thorough only
tests/apps/sqlite/run.sh -x /dir/with/xcheckfs -c thorough -s normal -m wal -d 30
```

## What runs

`workload.py run` starts N processes (default 12) on one database (or 5 for `multidb`): writers doing "bank
transfers" in `BEGIN IMMEDIATE` transactions (10% rolled back on purpose), readers checking consistency in
snapshot transactions, and a maintenance process doing `wal_checkpoint(TRUNCATE)`, `VACUUM` and
`PRAGMA integrity_check`. Invariants: the balance sum is constant; `history.seq` is exactly `1..N` (no gap, no
duplicate, `N == meta.ops`); every balance equals the replay of the history; `integrity_check` is `ok`; each worker's
confirmed commits are present.

| Scenario | Adds |
|---|---|
| `normal` | modes `wal`, `delete`, `truncate`, `persist`, `wal-mmap` (`PRAGMA mmap_size`), `wal-full` (`synchronous=FULL`) |
| `multidb` | 5 independent databases, 2 writers + reader or maintenance each (many files, many locks) |
| `kill` | random `SIGKILL` of any process (also mid-transaction, with a pause inside the transaction), restart (hot journal / WAL recovery, locks of dead owners); plus *lockers* |
| `signal` | random `SIGTERM`/`SIGINT` (half the workers handle them gracefully) while 12 processes contend, one *holder* keeps the write lock for 0.3-0.6 s; plus lockers |

SQLite only uses non-blocking `F_SETLK`, so `kill` and `signal` also run **lockers**: blocking `F_SETLKW` locks on a
side file protecting counters (mutual exclusion check), shared/exclusive readers-writers on one range, and two lock
orders (`A,B` / `B,A`) that provoke `EDEADLK`. These cover the xcheckfs waiter queue, deadlock detection and the
`EINTR` watchdog, which SQLite itself never reaches.

`run.sh` per run: start container, (unless `baseline`) mount xcheckfs with `-m log --check basic|thorough`, run the
workload, sample `ctl status` every 5 s (ops, mismatches, `lock_waiters`), report hung workers with
`/proc/PID/wchan`, stack and `ctl status`, run the final checks through the mount, save `ctl status|stats|mismatches`,
unmount, run `xcheckfs verify PRIMARY SECONDARY`, then check both trees **directly** (no xcheckfs) with
`workload.py check` and compare the logical digests. One row per run goes to `OUT/summary.tsv`; details are in
`OUT/<config>-<scenario>-<mode>/` (`result.json`, `workload.log`, `xc.log`, `mismatches.json`, `verify.txt`,
`check-*.txt`).

Exit status is non-zero if any run had a violation, hang, mismatch or non-empty `verify`.

## CI mode

`run.sh ci` is a short run for a CI runner (4 vCPU, 16 GB, a container limited to 4 CPUs) that writes
`$OUT/results.json` for `tests/apps/report.py` (schema in [`../README.md`](../README.md)):

```bash
XCHECKFS=/path/static/xcheckfs OUT=/tmp/sqlite tests/apps/sqlite/run.sh ci
CONFIGS="basic basic-strict" SCENARIOS="normal/wal kill/wal" DURATION=10 XCHECKFS=... OUT=... tests/apps/sqlite/run.sh ci
```

| Variable | Default | |
|---|---|---|
| `XCHECKFS` | | static binary (a file; `-x` and `XCHECKFS_DIR` also accept a directory containing `xcheckfs`) |
| `OUT` | `$TMPDIR/xc-sqlite-out` | results and per-scenario logs |
| `CONFIGS` | `baseline basic thorough` | also `paranoid` and `<mode>-strict` (`--serialize strict`) |
| `SCENARIOS` | `normal/wal normal/delete normal/wal-mmap multidb/wal kill/wal signal/wal` | `scenario/mode` list |
| `DURATION` | `25` | seconds of load per scenario |
| `PROCS` | `12` | worker processes |

Each config is one run and each `scenario/mode` one workload entry: `throughput` is committed write transactions
per second summed over all workers (unit `tx/s`), `count` the commits, `errors` the invariant violations, hangs and
unexpected errors, `latency_ms` the transaction time from `BEGIN IMMEDIATE` to `COMMIT` of committed
transactions (workers keep a log-scale histogram, merged by the driver; percentiles are accurate to about 5%).
The `xcheckfs` object sums the final `ctl status` counters over the config's scenarios (every scenario has its own
container and mount). Checks: `invariants`, `no hangs`, `xcheckfs verify`, `secondary check`, `primary check`,
`digests equal`, `no mismatches` and `lock stats` (EDEADLK count and lock operations; fails only if the lockers
did no work). A failing or impossible scenario is recorded and the script carries on; `results.json` is always
written, and the exit status is 0 only if every run is ok. Per-scenario logs stay in `$OUT/<config>-<scenario>-<mode>/`
(`workload.log`, `result.json`, `xc.log`, `verify.txt`, `check-*.txt`, `status-final.json`, ...), ready to upload as
an artifact.
