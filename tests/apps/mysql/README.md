# MySQL / InnoDB through xcheckfs

InnoDB is a hard case for the data path: 16 KiB in-place random overwrites with `O_DIRECT`, native AIO
(`io_submit`, which takes FUSE's direct-I/O path), the doublewrite buffer, `fsync`/`fdatasync` storms from the redo
log and the binary log (`sync_binlog=1`, `flush_log_at_trx_commit=1`), `fallocate`, hole punching (page
compression) and many threads writing the same tablespace file.

`run.sh` runs `mysql:8.4` (buffer pool 256M, so there is real I/O) under sysbench `oltp_read_write`,
`oltp_update_index`, `oltp_delete` and `oltp_insert` (10 tables x 200 000 rows, 24 threads) in Docker:

| Stage | What it does |
|---|---|
| `baseline`, `baselinebuf` | data directory on a plain docker volume, no xcheckfs (reference TPS); `baselinebuf` uses buffered I/O (`innodb_flush_method=fsync`), which is what the backends effectively see through xcheckfs |
| `basic`, `thorough` | the same, with xcheckfs mounted over `/var/lib/mysql` (`--check basic` / `thorough`, `-m log`, PRIMARY and SECONDARY are two volumes). At the end: clean `mysqladmin shutdown`, unmount, `xcheckfs verify` (expect 0 differences) |
| `paranoid` | `--check paranoid --direct-io --attr-timeout 0 --entry-timeout 0` (maximal coverage; bare `--direct-io` means `--direct-io all`: every file, not just the ones opened with `O_DIRECT`), otherwise like `thorough` |
| `validate NAME` | a fresh `mysql:8.4` directly on the SECONDARY volume (no xcheckfs) and on the PRIMARY volume: `mysqlcheck --all-databases --check --extended`, `CHECK TABLE`, row counts and `CHECKSUM TABLE`, compared with the numbers taken from the live server |
| `compress` | InnoDB page compression (`ALTER TABLE ... COMPRESSION='zlib'` + `OPTIMIZE TABLE`, which punches holes), with `INNODB_TABLESPACES` (`FS_BLOCK_SIZE`, `FILE_SIZE`, `ALLOCATED_SIZE`) and `stat` to see that the holes exist. Needs a file system with a block size below 16 KiB, so it uses tmpfs volumes: baseline, tmpfs/tmpfs, tmpfs/(default volume file system) |
| `crash` | `docker kill -s KILL` of the container (and so of xcheckfs) mid-run, `xcheckfs verify` of the diverged trees, then mysqld through a fresh mount in `resync` mode (InnoDB crash recovery through xcheckfs), plain recovery on copies of both sides |
| `crashloop` | the same kill/verify/recover cycle `CRASH_ROUNDS` times on the same volumes, to hit the divergence window more than once |
| `ci` | the short run for CI, see [CI mode](#ci-mode) |
| `all`, `summary`, `clean` | everything / print the TPS table / remove all `xc-mysql-*` Docker objects |

```bash
XCHECKFS=/path/to/static/xcheckfs OUT=/tmp/xc-mysql-results ./tests/apps/mysql/run.sh all
```

Run it as a user that may use Docker. The container gets `--device /dev/fuse --cap-add SYS_ADMIN --security-opt
apparmor:unconfined`; mysqld runs as the unprivileged `mysql` user inside, xcheckfs as root, which also exercises the
per-caller credential switching. `XCHECKFS` must be a static binary (the mysql image is Oracle Linux 9). Defaults and
tunables (`TABLES`, `TABLE_SIZE`, `THREADS`, `T_RW`, ...) are documented at the top of `run.sh`.

## CI mode

`run.sh ci` is the short variant for a GitHub runner (4 vCPU, 16 GB); the harness-wide description and the
`results.json` schema are in [`../README.md`](../README.md).

```bash
XCHECKFS=/path/to/static/xcheckfs OUT=/tmp/apps/mysql tests/apps/mysql/run.sh ci
CONFIGS="basic basic-strict" OUT=/tmp/apps/mysql tests/apps/mysql/run.sh ci
```

For each config in `CONFIGS` (default `baseline basic thorough`; also `paranoid` and any `<mode>-strict`, which adds
`--serialize strict`) it uses fresh volumes and runs prepare, `oltp_read_write`, `oltp_update_index`, `oltp_delete`
and `oltp_insert` (each one entry in `workloads`), a clean `mysqladmin shutdown`, the final `ctl status`,
`xcheckfs verify`, and a plain `mysql:8.4` on each volume (`mysqlcheck`, `CHECK TABLE ... EXTENDED`, row counts and
`CHECKSUM TABLE` equal to the live server's), then drops the volumes. xcheckfs runs with its defaults (`--direct-io
auto`, `--serialize relaxed`, so InnoDB's concurrent `O_DIRECT` writes reach it concurrently) and `-m log`. A
failure in one config is recorded as a failed run and the others still run; `results.json` is always written and
the exit status is 0 only if every run is ok.

Defaults (all can be overridden with the variables at the top of `run.sh`): 6 tables x 50 000 rows, 16 threads, 45 s
`oltp_read_write` and 20 / 15 / 20 s of the other three, `mysqld` limited to 4 CPUs and sysbench to 2. Logs
(`xc.log`, `mysqld.log`, `verify.txt`, sysbench output, the `mysqlcheck` output of both volumes) are in
`$OUT/<config>/`; `$OUT/run.log` is the driver's log. Needed images: `mysql:8.4`, `alpine:3.20` and the sysbench
image (pulled or built when missing).

## Files

- `run.sh`: the driver. Results (sysbench output, summary table `summary.tsv`, `ctl status` polls, final `ctl
  mismatches` JSON, xcheckfs and mysqld logs, `verify` output, check output) go to `$OUT/<scenario>/`.
- `entry.sh`: the container entry point. It mounts xcheckfs over `/var/lib/mysql` (PRIMARY `/xc/p`, SECONDARY
  `/xc/s`), runs the image's `docker-entrypoint.sh mysqld ...` as a child, and after mysqld exits collects `ctl`
  output into `/run` (read with `docker cp`) and stops xcheckfs with SIGTERM (clean unmount).
- `Dockerfile.sysbench`: `ubuntu:24.04` with sysbench and the MySQL client tools (the server image has no `mysqlcheck`).

## Things to know

- Use `--control-socket` with a *dedicated directory* (`/run/xc/ctl.sock`): xcheckfs `chmod`s the parent directory
  of the socket to 0700 only if it created that directory itself, so an existing `/run` stays untouched; the
  dedicated directory is kept anyway, so that the socket never depends on that.
- Docker's local-driver tmpfs volumes disappear when no container uses them, so `compress` keeps a holder container
  alive for the duration of a scenario.
- Throughput numbers depend heavily on the file system under the docker volumes and on other load on the machine;
  compare the three runs of one invocation, not across machines. `xcheckfs` strips `O_DIRECT` before the backends
  (ADR-4), so a data directory on a file system with real direct I/O can be *faster* through the mount than
  without it. On the kernel side, files the application opens with `O_DIRECT` use FUSE direct I/O (`--direct-io
  auto`, the default) with parallel direct writes, so concurrent InnoDB writes reach xcheckfs concurrently and
  AIO takes the direct path; the backends still see buffered I/O.
