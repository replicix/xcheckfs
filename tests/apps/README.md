# Applications through xcheckfs

Real applications run on an xcheckfs mount in Docker, to check that two
healthy file systems never mismatch under their I/O patterns and to measure
the overhead:

| Directory | Application | Exercises |
|---|---|---|
| [`postgres/`](postgres/) | PostgreSQL under pgbench | buffered page overwrites, WAL recycling by rename, fsync storms, `VACUUM FULL`, many relation files |
| [`mysql/`](mysql/) | MySQL / InnoDB under sysbench | `O_DIRECT` + native AIO, concurrent in-place writes to one file, doublewrite buffer, `fallocate`, hole punching |
| [`sqlite/`](sqlite/) | multi-process SQLite | `fcntl` byte-range locks, shared `mmap` of the WAL index, tiny writes, journal churn, killed lock owners |

Each harness has its own README with the long-running scenarios (crash
recovery, paranoid checks, compression). They need Docker with `/dev/fuse`
and a static xcheckfs binary.

## CI mode

`run.sh ci` (every harness) is a short run sized for a CI runner (4 CPUs,
16 GB): each configuration in `CONFIGS` gets fresh volumes, the workload, a
clean shutdown, `xcheckfs verify` of the two trees, and the application's own
consistency checks on each side. It writes `$OUT/results.json` and exits
non-zero if any configuration failed.

```bash
XCHECKFS=/path/to/static/xcheckfs OUT=/tmp/pg tests/apps/postgres/run.sh ci
CONFIGS="basic basic-strict" OUT=/tmp/pg tests/apps/postgres/run.sh ci
tests/apps/report.py /tmp/pg/results.json /tmp/my/results.json
```

`CONFIGS` (default `baseline basic thorough`) takes `baseline` (no xcheckfs),
`basic`, `thorough`, `paranoid`, and any of these with `-strict`
(`--serialize strict`, to compare against the default relaxed serialization).
The xcheckfs configurations mount with `-m log`, so every mismatch is
counted.

## results.json

```json
{
  "schema": 1,
  "app": "postgres",
  "title": "PostgreSQL 18, pgbench",
  "params": {"scale": 10, "clients": 8, "duration_s": 60},
  "runs": [
    {
      "config": "thorough",
      "xcheckfs_args": "--check thorough -m log",
      "ok": true,
      "wall_s": 312.4,
      "workloads": [
        {"name": "tpcb", "unit": "tps", "throughput": 412.7, "count": 24762, "errors": 0,
         "latency_ms": {"avg": 19.3, "p50": 15.1, "p95": 41.0, "p99": 77.2, "max": 310.0}}
      ],
      "xcheckfs": {"ops": 1, "mismatches": 0, "concurrent_data_ops": 0, "...": "final `ctl status`"},
      "checks": [
        {"name": "xcheckfs verify", "ok": true, "detail": "0 differences"},
        {"name": "pg_amcheck (secondary)", "ok": true, "detail": ""}
      ]
    }
  ]
}
```

- `ok` is false if the workload failed or hung, there was any mismatch, or any
  check failed.
- `xcheckfs` is the final `xcheckfs ctl status` object (`null` for `baseline`).
- Latency fields that the benchmark tool does not report are omitted.

`report.py` turns one or more `results.json` files into Markdown tables
(throughput relative to `baseline`, latency percentiles, xcheckfs counters,
checks). With `--base DIR` it also shows the change against an earlier set of
results, which CI uses to compare a pull request against the latest `main`.
