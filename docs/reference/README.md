# Reference

Technical descriptions of how specific parts of xcheckfs work.

## Interfaces

- [CLI](cli.md) — `mount`, `verify`, `ctl`: every flag with defaults, exit codes, signals, logging
- [TUI](tui.md) — panes, freeze modal, key bindings
- [Rules](rules.md) — allow-rules file: format, location, matching, examples
- [Control protocol](control-protocol.md) — socket path, commands, JSON responses

## Behavior

- [Checks](checks.md) — what is compared per operation at each check level, mismatch kinds, and what is never compared
- [Limitations](limitations.md) — everything xcheckfs cannot do, and what to do about it
- [Known differences between file systems](fs-differences.md) — measured ext4 / xfs / btrfs / f2fs / tmpfs differences, the mount-time probe, what is handled automatically, allow rules for the rest
