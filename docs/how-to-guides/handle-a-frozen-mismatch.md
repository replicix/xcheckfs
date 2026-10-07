# Handle a frozen mismatch

In `freeze` mode a mismatch holds the operation that found it, and every new
operation, until you decide ([Design](../explanation/DESIGN.md#freeze-gate)).
Applications wait in the kernel meanwhile, so decide promptly. Enable it with
`--on-mismatch freeze`, `m` in the TUI (it cycles `resync`, `log`, `fail`,
`freeze`), or `xcheckfs ctl MOUNTPOINT mode freeze`.

## Decide in the TUI

When a mismatch freezes, a modal shows its operation, kind, path, field,
both values and detail. With several pending mismatches, `Left`/`Right` pick
one. Press `?` for the complete key list ([TUI reference](../reference/tui.md)).

| Key | Decision | Effect |
|---|---|---|
| `c` | continue | Return the primary's result. Identical mismatches on this object are only counted from now on. |
| `a` | allow | Choose the scope (`1` everywhere for this operation/kind/field, `2` this path only), add an [allow rule](../reference/rules.md), continue. |
| `r` | retry | Re-run the read-only operation on both sides (only for read-only operations; useful after repairing the secondary by hand). |
| `s` | resync | [Repair](../explanation/DESIGN.md#repair-resync) the secondary from the primary (the object, the name or the directory, depending on the mismatch), then reply with the primary's result. Not for lock, `lseek` and `statfs` mismatches, or an object missing on the secondary. With `--quarantine`, the secondary's version is saved first. |
| `e` | fail | Return `EIO` for this operation. |
| `d` | detach | Stop mirroring for the rest of the session (asks for confirmation). |

`D` detaches outside the modal, and `q` quits (confirmation; releases all
pending mismatches with `continue`).

## Decide with `ctl`

The same decisions, for scripts or a second terminal:

```bash
xcheckfs ctl /mnt status                 # "state": "frozen", "pending": 1
xcheckfs ctl /mnt pending                # id, op, kind, path, values, retryable, resyncable
xcheckfs ctl /mnt resolve 3 continue
xcheckfs ctl /mnt resolve 3 allow        # rule for this operation/kind/field everywhere
xcheckfs ctl /mnt resolve 3 allow-path   # rule restricted to this exact path
xcheckfs ctl /mnt resolve 3 retry
xcheckfs ctl /mnt resolve 3 resync
xcheckfs ctl /mnt resolve 3 fail
xcheckfs ctl /mnt resolve 3 detach
```

`retry` and `resync` on a mismatch where they are not offered (`retryable` or
`resyncable` is false in `pending`) act like `continue`.

To release everything at once: `xcheckfs ctl /mnt mode log` (resolves all
pending with `continue`), or `xcheckfs ctl /mnt detach`.

## Which decision?

| Situation | Decision |
|---|---|
| Real defect in the secondary, but you want to keep going | `resync` or `continue` |
| The secondary's state is wrong and you repaired it by hand | `retry` |
| Expected difference (e.g. no xattr support) | `allow` (or `allow-path`) |
| Lock mismatch: not resyncable | `continue`, or `allow` if expected |
| The secondary is unusable | `detach` |

Allowed rules are appended to the rules file; see
[Rules](../reference/rules.md#adding-rules-at-runtime) for where and what
that rewrites.

## If the mount is stuck

- Applications blocked: `ctl MOUNTPOINT status` shows `frozen`. Decide, or set
  `mode log`.
- Unmount says "busy": same; frozen operations keep the mount busy.
- Do not leave `freeze` running unattended: if
  `/proc/sys/fs/fuse/max_request_timeout` is non-zero the kernel aborts the
  connection ([Limitations](../reference/limitations.md#freeze)).
