## What and why

<!-- What does this change, and why? Link the issue if there is one. -->

## Checklist

- [ ] Tests added or updated (a new detection needs a fault-injection test; see [TESTING.md](../docs/how-to-guides/development/TESTING.md))
- [ ] `cargo test --locked` passes
- [ ] `cargo clippy --locked --all-targets -- -D warnings` is clean
- [ ] Docs updated in the page that owns the behavior ([documentation guide](../docs/documentation-guide/README.md))
- [ ] `CHANGELOG.md` entry under `[Unreleased]` (user-visible changes)
