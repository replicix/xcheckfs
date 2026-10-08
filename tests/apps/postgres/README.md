# PostgreSQL under pgbench through xcheckfs

Runs `postgres:18` with PGDATA on an xcheckfs mount (PRIMARY and SECONDARY are
two Docker volumes) under a heavy pgbench workload, then proves that both
trees are identical and that the SECONDARY is a valid, consistent database
when used on its own.

What it exercises: buffered 8 KiB page overwrites written back by the kernel,
WAL segment recycling by rename, fsync/fdatasync storms at checkpoints
(`checkpoint_timeout=30s`, `max_wal_size=256MB`), `sync_file_range`/`posix_fadvise`
hints, `ftruncate` from VACUUM, `VACUUM FULL` (new file + rename + unlink),
`CREATE INDEX CONCURRENTLY`/`DROP INDEX`, and about 1300 relation files (200
hash partitions with TOAST tables and two indexes each).

## Requirements

Docker with `/dev/fuse`, the `postgres:18` and `alpine:3.20` images, `jq`, and
a static `xcheckfs` binary (`XCHECKFS=/path/to/xcheckfs`; default
`target/release/xcheckfs`, then `$PATH`). The containers run with
`--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor:unconfined`.

## Run

```bash
XCHECKFS=/path/to/xcheckfs tests/apps/postgres/run.sh all      # baseline, basic, thorough
tests/apps/postgres/run.sh bench thorough                      # one configuration
tests/apps/postgres/run.sh crash                               # SIGKILL + resync recovery
tests/apps/postgres/run.sh ci                                  # short run for CI, writes results.json
tests/apps/postgres/run.sh clean                               # remove xc-pg-* leftovers
```

Knobs (environment): `SCALE=50`, `CLIENTS=16`, `JOBS=4`, `DURATION=180`,
`CRASH_SCALE=20`, `CRASH_AFTER=40`, `CRASH_ROUNDS=1` (more rounds = repeated
recover/run/SIGKILL cycles on the same volumes, each killed after a random 5-35 s), `KEEP=1` (keep volumes), `XC_EXTRA` (extra
`xcheckfs mount` flags, e.g. `--attr-timeout 0`), `OUT` (results,
default `tests/apps/postgres/out/`, git-ignored). A quick smoke test:
`SCALE=2 DURATION=12 run.sh bench thorough`.

## CI mode

`run.sh ci` is the short variant used by the GitHub workflow (sized for a
4 vCPU / 16 GB runner; the three default configurations take about 12 minutes
together on such a runner). `CONFIGS` (default `baseline basic thorough`; also `paranoid` and
`<mode>-strict`, i.e. that mode with `--serialize strict`) are run one after
the other, each on fresh volumes: start (initdb through xcheckfs), `pgbench -i`,
`work/setup.sql`, then the three phases `tpcb`, `N` and `churn` (+ maintenance),
a clean stop, `xcheckfs verify`, and `pg_checksums`, `pg_amcheck` and row
counts on each volume. Defaults differ from the long scenarios: `SCALE=10`,
`CLIENTS=8`, `JOBS=4`, `DURATION=40` (seconds per phase); all can be overridden
from the environment, as can `XC_EXTRA`.

```bash
XCHECKFS=/path/static/xcheckfs OUT=$RUNNER_TEMP/apps/postgres tests/apps/postgres/run.sh ci
```

`$OUT/results.json` follows the schema in [`../README.md`](../README.md) (one
`workloads` entry per pgbench phase: throughput = `tps` excluding connection
time, `count` = processed transactions, `errors` = failed transactions plus
aborted clients, latency average from pgbench and percentiles from the `-l`
logs). A run is `ok` only if every check passed and every phase completed. A
configuration that fails fatally is recorded as a failed run (check `run
completed`) and the next one still runs; the exit status is non-zero if any
run failed. The per-config logs and validation output are in `$OUT/<config>/`.

## Layout

- `entry.sh`: container entrypoint. With `XC_MODE=basic|thorough|paranoid` it
  mounts xcheckfs (`-m log`, so findings stay visible) over `/var/lib/postgresql`
  (PRIMARY `/xc/p`, SECONDARY `/xc/s`) *before* the official entrypoint runs,
  so `initdb` itself goes through xcheckfs. It then runs `docker-entrypoint.sh
  postgres ...` as a child (not `exec`) so that `docker stop` does a fast
  shutdown, saves the final `ctl status/stats/mismatches` JSON and unmounts.
  `XC_MODE=baseline` uses a plain volume at `/var/lib/postgresql`.
- `run.sh`: host driver. Per configuration: start, `pgbench -i -s 50`,
  `work/setup.sql`, then `pgbench -c 16 -j 4 -T 180` for TPC-B-like, `-N`,
  and `work/churn.pgbench` (UPDATE/DELETE/INSERT churn) with
  `work/maint.sh` in parallel (VACUUM FULL, CHECKPOINT, CIC + DROP INDEX,
  bulk create/delete/VACUUM-truncate/drop). `ctl status` snapshots are taken
  after each phase. Then `docker stop`, `xcheckfs verify` (expect exit 0),
  and validation of both volumes on their own with a plain `postgres:18`
  container: `pg_checksums --check` (server stopped), `pg_amcheck --all
  --heapallindexed`, and row counts / sums compared between the two sides.
- `crash`: thorough + `-m log`, `docker kill -s KILL` mid-run, `verify`, then a
  fresh xcheckfs mount in `-m resync --quarantine` on the same volumes so WAL
  recovery runs through it; the repairs are reported, then both sides are
  verified and validated as above.

Server settings: `shared_buffers=128MB`, `checkpoint_timeout=30s`,
`max_wal_size=256MB`, `full_page_writes=on`, `fsync=on`,
`synchronous_commit=on`, `wal_recycle=on`, `autovacuum_naptime=5s`, data
checksums on.

## Results

Everything lands in `$OUT/<config>/`: `summary.txt` (also at `$OUT/`),
`pgbench-*.txt`, `pctl-*.txt` (latency percentiles from `pgbench -l`),
`xc-status-*.json`, `xc-mismatches-*.json`, `xc.log`, `verify.txt`, `val-*`.

## Notes

- xcheckfs `chmod`s the parent directory of `--control-socket` to 0700 only
  when it creates that directory itself. The socket still lives in its own
  directory (`/run/xc/`) rather than directly in `/run`; this is harmless and
  keeps older binaries (which chmod an existing directory too, and would lock
  the `postgres` user out of `/var/run/postgresql`) working.
- Postgres backends are signalled constantly (SIGURG/SIGUSR1 latch wakeups). A
  handled signal that hits a process blocked in a FUSE request makes the
  kernel send `FUSE_INTERRUPT`, which the FUSE library answers with `ENOSYS`;
  the kernel then answers that reply with `ENOENT`. Current xcheckfs ignores
  this harmless failed reply, so `xc.log` is not flooded with
  `Failed to send FUSE reply: No such file or directory` any more (older
  binaries logged hundreds to thousands of such lines per run; they were never
  mismatches and no data was affected).
- SIGKILL of the container can leave the trees diverged (documented): the
  `crash` scenario reports what `verify` finds and what the resync mount
  repairs. Divergence is rare (the window between the primary's and the
  secondary's half of one write), so use `CRASH_ROUNDS=8` or more.
- Container-local paths are used for the socket; logs go to the bind-mounted
  results directory so they survive `docker kill`.
