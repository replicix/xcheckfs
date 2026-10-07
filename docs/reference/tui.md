# TUI

`xcheckfs mount --ui tui ...` runs an interactive dashboard in the terminal
(not combinable with `--background`). Press `?` at any time for the key
bindings of your version; the in-app help is authoritative. Log lines are
shown in the Log pane instead of on stderr (unless `--log-file` is given).

The TUI exits when you quit (`q`, then `y`) or when the file system is
unmounted from outside (`fusermount3 -u MOUNTPOINT`); the one-line summary is
printed to stderr afterwards.

## Panes

| Pane | Content |
|---|---|
| Header | Mount point, primary → secondary, state badge (running, frozen, detached), check level, mismatch mode, uptime, operation count and rate, error (mismatch) / allowed / repeat counters, and a warning when an operation has been in flight for over a second. |
| Operations | Per operation: count, rate, errors (mismatches; an errno both file systems agree on is a normal result, not an error), and p50/p99 latency of the primary, the secondary and the ratio secondary/primary (yellow above 2x, red above 10x). `w` switches between the last 5 s and since start; an operation with nothing in the last 5 s shows its since-start values dimmed. |
| Throughput, Counters | Read/write/operation rates; cached inodes, open files and directories, lock waiters, verifications, skipped secondary halves, dropped UI events, rules loaded, and the [repair](../explanation/DESIGN.md#repair-resync) counters: `repaired`, `unrepaired` (failed or given up) and `quarantined`. In the compact layout (one-line summary) the first two are `fixed` and `unfixed`. |
| Operations log | One line per operation with both results and latencies, scrollable (`--history` lines kept); can follow the tail, show only errors (mismatches and differing results), or filter by text. |
| In flight | Operations currently executing, including one that is running a repair: a large object blocks while it is resynced ([Limitations](limitations.md#repair)). |
| Mismatches | All recorded mismatches; `Enter` shows the full details of the selected one. |
| Log | xcheckfs's own log lines. |

## Freeze modal

When a mismatch freezes the file system ([Design](../explanation/DESIGN.md#freeze-gate)),
a modal lists its operation, kind, path, values and detail, with the available
decisions. Keys and effects: [Handle a frozen mismatch](../how-to-guides/handle-a-frozen-mismatch.md#decide-in-the-tui).

## Keys

| Key | Action |
|---|---|
| `q`, `Ctrl-C` | Quit (asks for confirmation; releases frozen operations) |
| `?`, `F1` | Help |
| `Tab`, `Shift-Tab` | Cycle focus between the panes |
| `Up` `Down` `PgUp` `PgDn` `Home` `End` (also `j` `k` `g` `G`) | Scroll the focused pane |
| `m` | Cycle the mismatch mode: resync, log, fail, freeze, resync (from `detach` too) |
| `D` | Detach the secondary (asks for confirmation) |
| `w` | Latency window: last 5 s / since start |
| `f` | Follow the tail of the operations log |
| `e` | Only errors (mismatches) in the operations log |
| `/` | Filter the operations log; `Esc` clears |
| `Enter` | Details of the selected mismatch |
