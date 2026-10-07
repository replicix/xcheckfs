# How-to guides

Goal-oriented directions for a specific task or problem.

## Operations

- [Test an experimental file system](test-an-experimental-fs.md) — end to end: back up, seed, verify, mount over the primary or at a separate mount point, monitor, handle mismatches per mode, unmount, verify again
- [Handle a frozen mismatch](handle-a-frozen-mismatch.md) — TUI decisions and the equivalent `xcheckfs ctl ... resolve` commands
- [Run in CI](run-in-ci.md) — `fail` mode, `thorough`/`paranoid`, exit code 3, collecting mismatches

## Development

- [Testing](development/TESTING.md) — the test suite
- [Releasing](development/RELEASING.md) — tag a version; CI tests, builds the static binaries and packages, and publishes the GitHub release
