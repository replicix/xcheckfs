# Allow rules

Allow rules silence mismatches that are known and harmless (a secondary that
has no xattrs, a cache directory that legitimately differs). A matching
mismatch is dropped before anything else: it is not logged as a mismatch,
does not freeze or fail, is counted as `allowed`, and does not influence the
`mount` exit code.

## Table of Contents

- [Location](#location)
- [Format](#format)
- [Matching](#matching)
- [Adding rules at runtime](#adding-rules-at-runtime)
- [Examples](#examples)

## Location

`--rules FILE`, defaulting to `/etc/xcheckfs/rules.toml` when running as root
and `$XDG_CONFIG_HOME/xcheckfs/rules.toml` (`~/.config/xcheckfs/rules.toml`)
otherwise. A missing file means no rules; an unreadable or invalid file, an
unknown field, an unknown `op`, or an invalid glob aborts the mount. The file
is read once at startup: edits need a restart (rules added through the UI or
`ctl` are active immediately).

**The rules file must not be inside a mirrored tree** (primary, secondary or
mount point). xcheckfs writes it from the daemon, and a write through the
mirror would bypass or deadlock the lockstep engine. If it is, the mount
starts with a warning and new rules are kept for the session only.

## Format

TOML, a list of `[[allow]]` tables. Every given field must match; omitted
fields match anything. Unknown fields are rejected.

| Field | Matches | Values |
|---|---|---|
| `op` | the operation | an operation name in lower case (`lookup`, `getattr`, `setattr`, `read`, `write`, `setxattr`, `copy_file_range`, ...) or `"*"` |
| `kind` | the mismatch kind | `result attr data length readdir readlink xattr identity verify content lock` ([Checks](checks.md#mismatch-kinds)) |
| `field` | the attribute, xattr, or verification step | e.g. `nlink`, `mtime`, `list`; only mismatches that have a field can match |
| `path` | the path relative to the mount root | a glob, e.g. `/var/cache/**` |
| `primary` | the primary's value | exact string, case-insensitive |
| `secondary` | the secondary's value | exact string, case-insensitive |
| `note` | nothing (documentation) | free text |

Operation names are `lookup forget getattr setattr readlink mknod mkdir unlink
rmdir symlink rename link open read write flush release fsync opendir readdir
releasedir fsyncdir statfs setxattr getxattr listxattr removexattr access
create getlk setlk fallocate lseek copy_file_range ioctl poll bmap`.

## Matching

- `path` is a glob matched against the whole path of the mismatch (always
  starting with `/`). `*` also matches `/`, so `/var/cache/*` and
  `/var/cache/**` both cover everything below `/var/cache` (but not
  `/var/cache` itself). For a mismatch
  about a name (a failed `lookup`, `create`, `unlink`) the path is that of the
  child.
- `primary` and `secondary` are practical for `result` mismatches, whose
  values are errno names: `OK`, `ENOENT`, `EOPNOTSUPP`, ... (`ENOTSUP` is
  `EOPNOTSUPP`). For other kinds the values are descriptive text.
- Rules are checked before [deduplication](../explanation/DESIGN.md#deduplication);
  allowed mismatches never freeze, fail or detach.
- A rule with no fields matches everything: avoid it.

## Adding rules at runtime

When frozen, the decision `allow` adds a rule as narrow as the mismatch
itself (its operation, kind and field, and for `result` mismatches both
errno values) and `allow-path` also restricts it to that exact path. In the
TUI the modal asks for the scope; with `ctl` use `resolve ID allow` or
`resolve ID allow-path` ([Handle a frozen mismatch](../how-to-guides/handle-a-frozen-mismatch.md)).
The rule is active immediately and appended to the rules file with a
`note = "added interactively for mismatch #ID"`. Appending rewrites the file:
**comments in a hand-written file are not preserved**.

## Examples

Rules for measured differences between common file systems (ext4 xattr space,
tmpfs `fallocate`): [Known differences between file systems](fs-differences.md).

A secondary without extended attribute support: every xattr operation fails
with `EOPNOTSUPP` there while the primary succeeds.

```toml
# Any operation for which the secondary answers EOPNOTSUPP.
[[allow]]
kind = "result"
secondary = "EOPNOTSUPP"
note = "secondary has no xattrs"
```

Narrower, for `setxattr` only:

```toml
[[allow]]
op = "setxattr"
kind = "result"
primary = "OK"
secondary = "EOPNOTSUPP"
```

Directory link counts: prefer `--no-dir-nlink`, which compares file link
counts and skips directories. A rule cannot tell directories from files, so
use it only for a subtree where the difference is expected:

```toml
[[allow]]
kind = "attr"
field = "nlink"
path = "/archive/**"
note = "secondary counts subdirectories differently here"
```

A path glob: modification times below a cache directory may differ.

```toml
[[allow]]
kind = "attr"
field = "mtime"
path = "/var/cache/**"
```
