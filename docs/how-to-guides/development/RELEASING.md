# Releasing

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) when a version tag is pushed.

## Cut a release

1. Set the version in `Cargo.toml` (`version = "X.Y.Z"`), run `cargo build`
   so `Cargo.lock` follows, and commit.
2. Tag the commit and push both:

   ```bash
   git tag -a vX.Y.Z -m "xcheckfs X.Y.Z"
   git push origin main vX.Y.Z
   ```

   A tag with a suffix (`v0.2.0-rc.1`) is published as a pre-release.

The workflow then:

1. runs the CI checks (`clippy -D warnings`, `cargo test`) — a failure stops
   the release;
2. refuses to continue if the tag is not `v` + the `Cargo.toml` version;
3. builds static musl binaries for `x86_64-unknown-linux-musl` and
   `aarch64-unknown-linux-musl`, each natively on a runner of its
   architecture (`ubuntu-24.04`, `ubuntu-24.04-arm`), with the `dist` profile
   (stripped, LTO; the toolchain is pinned in `rust-toolchain.toml`), and
   smoke-tests them;
4. packages each as `xcheckfs-<target>.tar.xz` (binary, README, LICENSE),
   `.deb` (`cargo-deb`) and `.rpm` (`cargo-generate-rpm`), both depending on
   `fuse3`;
5. creates the GitHub release with generated notes and uploads all files plus
   `install.sh` and `SHA256SUMS`.

If a run fails after the tag was pushed, fix the problem, then move the tag
and push it again (`git tag -fa vX.Y.Z` and
`git push -f origin vX.Y.Z`), after deleting a partially created release with
`gh release delete vX.Y.Z`.

## Release files

| File | What |
|---|---|
| `xcheckfs-<target>.tar.xz` | Static binary (any Linux distribution), README, LICENSE. The name has no version, so `releases/latest/download/<name>` always works. |
| `xcheckfs_<version>-1_<amd64\|arm64>.deb` | Debian/Ubuntu package (`/usr/bin/xcheckfs`). |
| `xcheckfs-<version>-1.<x86_64\|aarch64>.rpm` | Fedora/RHEL/SUSE package. |
| `install.sh` | Installs the right tarball for the machine, verifying `SHA256SUMS`. |
| `SHA256SUMS` | Checksums of all files above. |

## Build the release files locally

For the machine's own architecture, the same commands as CI (needs
`musl-tools` for `musl-gcc`, and `cargo install cargo-deb cargo-generate-rpm`):

```bash
CC_x86_64_unknown_linux_musl=musl-gcc \
    cargo build --locked --profile dist --target x86_64-unknown-linux-musl
cargo deb --no-build --no-strip --target x86_64-unknown-linux-musl --profile dist
cargo generate-rpm --target x86_64-unknown-linux-musl --profile dist
```

For the other architecture, `cross build --locked --profile dist --target
aarch64-unknown-linux-musl` (Docker) produces the same binary; give it its
own `CARGO_TARGET_DIR`, since build scripts compiled on the host may not run
in cross's older container.
