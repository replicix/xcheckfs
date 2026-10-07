# Contributing to xcheckfs

Thanks for helping. This page is the short version; the details live in
[docs/](docs/README.md).

## Build and test

```bash
cargo build                                   # needs Rust 1.88+; see the pin below
cargo test --locked                           # everything except the opt-in external suites, ~15 s
cargo clippy --locked --all-targets -- -D warnings
```

CI runs exactly the last two commands, so a change must pass both with zero
warnings. The FUSE mount tests need `/dev/fuse` and `fusermount3` (package
`fuse3`) and skip themselves otherwise.

The test lanes (unit, healthy tree pairs, fault injection, policy, locks,
real mounts, libfuse and pjdfstest) and how to add a test are described in
[TESTING.md](docs/how-to-guides/development/TESTING.md). A change that alters
what xcheckfs detects needs a fault-injection test in `tests/engine_faults.rs`;
a change to the engine needs the healthy-pair suites to still report zero
mismatches.

## Toolchain

`rust-toolchain.toml` pins the exact compiler used by CI and releases
(rustup picks it up automatically). The minimum supported Rust version is
1.88 (`rust-version` in `Cargo.toml`); do not use newer language or library
features than that without raising it deliberately in the same change.

## Commits and pull requests

- Subject line in the imperative mood, about 70 characters ("Fix rename
  identity check", not "Fixed" or "Fixes").
- The body explains *why*: the problem, the alternatives rejected, the
  trade-off. The diff already shows *what*.
- One logical change per commit; keep unrelated cleanups out.
- No AI-tool or co-author trailers.
- Add a line under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for
  user-visible changes.

## Reporting what xcheckfs finds

- **A mismatch xcheckfs reported, and you believe the experimental file
  system is wrong:** use the *Mismatch report* issue form. Include the JSON
  from `xcheckfs ctl MNT mismatches` and whether `xcheckfs verify PRIMARY
  SECONDARY` confirms the difference. Many of these belong in the experimental
  file system's own tracker; we will say so and help narrow them down.
- **xcheckfs itself is wrong** (crash, hang, wrong result delivered to an
  application, or a false positive, that is a mismatch where both file
  systems behaved correctly or legitimately differ): use the *Bug report*
  form. For a false positive, say which comparison fired
  (`kind` and `field` in the mismatch JSON) and why the two results are both
  acceptable; if it is a known harmless difference, an
  [allow rule](docs/reference/rules.md) may be the answer instead.
- Security problems: see [SECURITY.md](SECURITY.md); do not open a public issue.

## Documentation

Docs follow [Diátaxis](https://diataxis.fr/): tutorials, how-to guides,
reference, explanation. The
[documentation guide](docs/documentation-guide/README.md) says where a page
goes and has a template for features. Document each feature or limitation once,
in the page that owns it, and change the page in the same commit as the
behavior.

## Releases

Maintainers cut releases by tagging; see
[RELEASING.md](docs/how-to-guides/development/RELEASING.md).

## Conduct

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).
By contributing you agree that your contribution is licensed under
[MPL-2.0](LICENSE).
