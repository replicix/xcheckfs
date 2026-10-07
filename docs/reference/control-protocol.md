# Control protocol

A running mount serves a Unix socket so scripts, CI and `xcheckfs ctl` can
inspect it and resolve frozen mismatches without the TUI (also in
`--background` mode). The socket does not live inside the mount, so it keeps
working while the file system is frozen.

## Table of Contents

- [Socket path](#socket-path)
- [Protocol](#protocol)
- [Commands](#commands)
- [Objects](#objects)

## Socket path

`--control-socket PATH` if given. Otherwise derived *lexically* from the
mount point (made absolute, `.` and `..` resolved, symlinks not resolved), so
use the same spelling for `mount` and `ctl`, or pass `--socket`:

| Running as | Directory |
|---|---|
| root | `/run/xcheckfs/` |
| other, `$XDG_RUNTIME_DIR` set | `$XDG_RUNTIME_DIR/xcheckfs/` |
| other, otherwise | `/tmp/xcheckfs-UID/` |

File name: the last path component (ASCII letters, digits, `.`, `_`, `-`;
at most 24 characters), a dash, and 16 hex digits of a hash of the full path,
plus `.sock` — for `/mnt/data`, `data-<hash>.sock`. The directory is created
with mode `0700` and the socket is `0600`. A stale socket file is replaced; a
socket another xcheckfs is listening on is an error. The socket is removed on
unmount. The path must not be inside a mirrored tree.

## Protocol

One command per line (UTF-8, whitespace-separated words); one JSON object per
line as the response. A connection may send several commands. Every response
has `"ok"`; failures are `{"ok": false, "error": "..."}`. `xcheckfs ctl`
sends one command and prints the response pretty-printed.

```bash
echo status | socat - UNIX-CONNECT:/run/xcheckfs/data-0123456789abcdef.sock
```

## Commands

| Command | Response |
|---|---|
| `status` (also an empty line) | The [status object](#status). |
| `stats` | `{"ok", "ops": [...], "status": {...}}`: one entry per operation with a non-zero count (`op`, `count`, `errno_results` (operations the primary answered with an errno — normal results, not errors), `mismatches`, `bytes`, `p50_ns`, `p99_ns`, `max_ns` and `primary_p50_ns`, `primary_p99_ns`, `secondary_p50_ns`, `secondary_p99_ns`). |
| `mismatches [N]` | `{"ok", "mismatches": [...]}`: the last `N` (default 50) [mismatches](#mismatch), newest first. At most the last 1000 are kept. |
| `pending` | `{"ok", "pending": [...]}`: frozen mismatches awaiting a decision. |
| `resolve ID ACTION` | `{"ok": true}`, or an error if `ID` is not pending. `ACTION` is `continue`, `retry`, `resync`, `fail`, `detach`, `allow` or `allow-path`. |
| `mode MODE` | `{"ok": true, "mode": "..."}`. `MODE` is `resync`, `log`, `fail`, `freeze` or `detach`. Leaving `freeze` releases everything pending with `continue`. |
| `detach` | `{"ok": true}`. Detaches the secondary and releases everything pending. |
| `rules` | `{"ok", "path", "persistent", "rules": [...]}`: the active [allow rules](rules.md) and where new ones are saved (`persistent` is false when they cannot be). |

`resolve` actions are explained in
[Handle a frozen mismatch](../how-to-guides/handle-a-frozen-mismatch.md).
`retry` and `resync` on a mismatch that cannot be retried or resynced
([Checks](checks.md#mismatch-kinds)) behave like `continue`. The single
letters `c`, `r`, `s`, `e`, `d` are accepted for `continue`, `retry`,
`resync`, `fail`, `detach`.

## Objects

### Status

| Field | Meaning |
|---|---|
| `info` | `mountpoint`, `primary`, `secondary`, `check`, `pid` |
| `state` | `running`, `frozen` or `detached` |
| `mode` | current mismatch mode |
| `uptime_secs` | |
| `ops` | operations served |
| `mismatches` | mismatches recorded (excluding allowed and repeats) |
| `allowed` | mismatches dropped by allow rules |
| `repeats` | identical mismatches suppressed by [deduplication](../explanation/DESIGN.md#deduplication) |
| `pending` | frozen mismatches awaiting a decision |
| `secondary_skipped` | operations whose secondary half was skipped because the object does not exist on the secondary |
| `verifications` | read-backs and content/listing comparisons done at `thorough`/`paranoid` |
| `resyncs` | [repairs](../explanation/DESIGN.md#repair-resync) that verified |
| `resync_failures` | repairs whose verification failed |
| `resync_giveups` | repairs refused because the path reached `--resync-limit` |
| `quarantined` | secondary objects saved to the [quarantine](../explanation/DESIGN.md#quarantine) directory |
| `bytes_read`, `bytes_written` | |
| `nodes`, `open_files`, `open_dirs` | cached inodes and open handles |
| `lock_waiters` | blocking lock requests queued |

### Mismatch

| Field | Meaning |
|---|---|
| `id` | sequence number, as in `resolve ID` and the `MISMATCH #ID` log line |
| `time` | Unix time, seconds (float) |
| `op` | the operation, lower case (`lookup`, `read`, `copy_file_range`, ...), the same names rules use. |
| `kind` | [mismatch kind](checks.md#mismatch-kinds), lower case |
| `ino` | node id |
| `path` | path relative to the mount root, best effort |
| `field` | attribute, xattr name, verification step, or `null` |
| `primary`, `secondary` | the two values |
| `detail` | human-readable explanation |
| `retryable`, `resyncable` | which decisions are available |
