# CLI

```
xcheckfs mount  [OPTIONS] MOUNTPOINT PRIMARY SECONDARY
xcheckfs verify [OPTIONS] PRIMARY SECONDARY
xcheckfs ctl    [--socket PATH] [MOUNTPOINT] [COMMAND...]
```

## Table of Contents

- [mount](#mount)
- [verify](#verify)
- [ctl](#ctl)
- [Exit codes](#exit-codes)
- [Signals](#signals)
- [Logging](#logging)

## mount

Mounts `PRIMARY` at `MOUNTPOINT` and mirrors every operation to
`SECONDARY`. `MOUNTPOINT` may be `PRIMARY` itself, which routes all access
through xcheckfs. All three must be existing directories.

Refused at startup: primary and secondary containing each other; a mount
point inside the secondary, or inside the primary (unless it *is* the
primary); a log file, pid file, control socket or quarantine directory inside
any of the three trees; `--ui tui` together with `--background`. A rules file
inside a tree is accepted with a warning, and new rules are not saved to it
([Rules](rules.md#location)).

### Interface and logging

| Flag | Default | Description |
|---|---|---|
| `-u`, `--ui log\|tui` | `log` | Foreground interface: log lines on stderr, or the interactive dashboard. |
| `-l`, `--log-level error\|warn\|info\|debug\|trace` | `warn` | Log level. Mismatches are logged at error level. |
| `-v` | | Raise verbosity: `-v` info, `-vv` debug, `-vvv` trace. Conflicts with `-l`. |
| `--log-file FILE` | | Append logs to `FILE` instead of stderr/syslog/TUI log pane. |
| `--no-color` | | No colors on stderr (colors are used only when stderr is a terminal). |
| `--history N` | `10000` | Operations kept in the TUI's scrollable log. |

### Checking and mismatches

| Flag | Default | Description |
|---|---|---|
| `-c`, `--check basic\|thorough\|paranoid` | `basic` | How much is checked per operation ([Checks](checks.md)). |
| `-m`, `--on-mismatch resync\|log\|fail\|freeze\|detach` | `resync` | What a mismatch does ([Design](../explanation/DESIGN.md#mismatch-handling)). `resync` repairs the secondary from the primary ([Design](../explanation/DESIGN.md#repair-resync)); use `fail` in CI ([Run in CI](../how-to-guides/run-in-ci.md)). |
| `--quarantine DIR` | | Before a repair overwrites or removes the secondary's version of an object, copy it into a new directory under `DIR` ([layout](../explanation/DESIGN.md#quarantine)). Created if missing; must be outside the mirrored trees. Without it, repairs are only reported. |
| `--quarantine-cap BYTES` | `67108864` (64 MiB) | Most bytes saved per quarantined object; more is truncated and noted in `mismatch.txt`. |
| `--resync-limit N` | `5` | Repairs of one path within 10 minutes before resync gives up on it ([Design](../explanation/DESIGN.md#repair-limits-and-failures)). At least 1. |
| `--rules FILE` | `/etc/xcheckfs/rules.toml` as root, else `$XDG_CONFIG_HOME/xcheckfs/rules.toml` (`~/.config/...`) | Allow-rules file ([Rules](rules.md)). A missing file means no rules. |
| `--time-tolerance DUR` | `1s` | Allowed mtime difference, and minimum ctime movement that must be matched by the other side. Accepts `s`, `ms`, `us`, `ns` suffixes (e.g. `500ms`). |
| `--no-dir-nlink` | off | Do not compare link counts of directories (some file systems always report 1). |

### Execution

| Flag | Default | Description |
|---|---|---|
| `--threads N` | CPUs, at most 16 | FUSE worker threads. |
| `--sequential` | off | Run the secondary half after the primary instead of concurrently. |
| `--no-creds` | off | Do not switch to the caller's credentials for mutations (only effective as root; [Design](../explanation/DESIGN.md#credentials-and-permissions)). |
| `--no-lock-mirroring` | off | Let the kernel handle `fcntl` locks locally ([Design](../explanation/DESIGN.md#lock-mirroring)). |

### Kernel interaction

| Flag | Default | Description |
|---|---|---|
| `--attr-timeout SECS` | `1.0` | Kernel attribute cache timeout. `0` means every `stat` reaches xcheckfs. |
| `--entry-timeout SECS` | `1.0` | Kernel dentry cache timeout. `0` means every lookup reaches xcheckfs. |
| `--direct-io` | off | Bypass the kernel page cache so every read and write reaches xcheckfs. Breaks shared writable `mmap` ([Limitations](limitations.md#caching)). |
| `--allow-other` | on as root, else off | Let other users access the mount. As non-root, `fusermount3` additionally requires `user_allow_other` in `/etc/fuse.conf`. |
| `-o OPTS` | | Extra comma-separated mount options, e.g. `-o suid,dev`. Known: `dev nodev suid nosuid ro rw exec noexec atime noatime sync async dirsync auto_unmount`; anything else is passed through. `default_permissions`, `fsname=xcheckfs:PRIMARY` and `subtype=xcheckfs` are always set. |

### Process

| Flag | Default | Description |
|---|---|---|
| `-b`, `--background` | off | Run as a daemon. Logs go to syslog (facility `daemon`, identity `xcheckfs`) unless `--log-file` is given. The command returns once the mount is up (exit 0) or failed (exit 1, with the reason on stderr). |
| `--pid-file FILE` | | Write the pid here; removed on clean unmount. |
| `--control-socket PATH` | derived from `MOUNTPOINT` | Control socket ([Control protocol](control-protocol.md#socket-path)). |

## verify

Compares two trees offline, in parallel, without mounting. Run it before
the first mount and after the last unmount. Compares attributes (as in
[Checks](checks.md#attributes)), symlink targets, xattrs, file content, and
hard-link structure; names present on one side only are reported and not
descended into. Symlinks are never followed; directories on another file
system than the root are not entered.

| Flag | Default | Description |
|---|---|---|
| `--no-content` | off | Do not compare file contents. |
| `--no-xattrs` | off | Do not compare extended attributes. |
| `--no-mtime` | off | Do not compare modification times. |
| `--no-dir-nlink` | off | Do not compare link counts of directories. |
| `--time-tolerance DUR` | `1s` | Allowed mtime difference. |
| `--max-reports N` | `200` | Stop listing differences after `N`; counting continues. |
| `--threads N` | CPUs | Worker threads. |

Output: one line per difference on stdout (`PATH: WHAT: primary=... secondary=...`),
unreadable entries as `error: ...` on stderr, a final summary line, and live
progress on stderr when it is a terminal. `WHAT` is `only in primary`,
`only in secondary`, `attr FIELD`, `content`, `symlink target`,
`xattr NAME`, or `hardlink structure`.

## ctl

Sends one command to a running mount's [control socket](control-protocol.md)
and prints the JSON response. Without a command it sends `status`. Give the
mount point (the same spelling as to `mount`) or `--socket PATH`.

```
xcheckfs ctl MOUNTPOINT [status|stats|mismatches [N]|pending|resolve ID ACTION|mode MODE|detach|rules]
xcheckfs ctl --socket PATH COMMAND...
```

## Exit codes

| Command | Code | Meaning |
|---|---|---|
| `mount` | 0 | Unmounted; no mismatch was recorded (allowed and repeated ones do not count). |
| `mount` | 3 | Unmounted; at least one mismatch was recorded. |
| `mount` | 1 | Startup failure or other error (message on stderr). With `--background` the exit status is that of the parent: 0 once mounted, 1 if the mount failed; the daemon's own status is not observable. |
| `verify` | 0 | No differences, no errors. |
| `verify` | 3 | At least one difference. |
| `verify` | 2 | No differences, but some entries could not be read. |
| `verify` | 1 | A root could not be examined, or another fatal error. |
| `ctl` | 0 | The response had `"ok": true`. |
| `ctl` | 1 | `"ok": false`, or the socket could not be reached. |

Command-line usage errors exit with 2 (from the argument parser) for every
subcommand.

## Signals

| Signal | Effect |
|---|---|
| `SIGINT`, `SIGTERM`, `SIGHUP` | Release frozen operations, unmount, print the summary, exit. |
| `SIGUSR1` | Log a one-line summary (operations, mismatches, repairs, bytes, verifications). |

Unmount from outside with `fusermount3 -u MOUNTPOINT` (or `umount` as root).
If the mount is busy because operations are frozen, release them first:
`xcheckfs ctl MOUNTPOINT mode log`.

## Logging

| Situation | Destination |
|---|---|
| `--log-file FILE` | `FILE`, appended, no colors (takes precedence over everything below) |
| `--ui tui` | The TUI's log pane |
| `--background` | syslog |
| otherwise | stderr, colored when it is a terminal and `--no-color` is not set |

`RUST_LOG`, when set, replaces the built-in filter (which is
`warn,xcheckfs=LEVEL`). A mismatch is one log line at error level starting
with `MISMATCH #ID`, naming the operation, kind, field, path, inode and both
values.
