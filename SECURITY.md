# Security policy

## Supported versions

Only the latest [release](https://github.com/replicix/xcheckfs/releases) is
supported with security fixes.

## Reporting a vulnerability

Please report privately, not in a public issue:

- GitHub private vulnerability reporting: the **Security** tab of
  [replicix/xcheckfs](https://github.com/replicix/xcheckfs/security) >
  *Report a vulnerability*; or
- email the maintainer, Attila Nagy, at <nagy.attila@gmail.com>.

Include the version, kernel, how xcheckfs was mounted, and a reproducer if you
have one. You can expect an acknowledgement within a week. We will coordinate a
fix and disclosure date with you and credit you in the release notes unless you
prefer otherwise.

## Threat surface

xcheckfs is a FUSE file system that sits in the data path of the primary file
system, so the following matters when judging impact:

- It is normally mounted as root over the primary itself, with `allow_other`,
  so that every user's access goes through it. A bug in xcheckfs can therefore
  affect every user of that tree: wrong results, data loss, or privilege
  issues in how operations are replayed on the secondary.
- Operations are executed on both trees with the caller's credentials as
  supplied by the kernel; a flaw in how identity, permissions or paths are
  handled is in scope.
- The control socket (`xcheckfs ctl`) is a Unix socket in a `0700` directory
  with mode `0600`: only the user who started the mount can use it. It can
  change the mismatch mode and resolve frozen operations, so anything that
  weakens that boundary is in scope.
- Secondary-side repairs and `--quarantine` copy file contents; the secondary
  is untrusted for correctness, but xcheckfs must never modify the primary on
  its behalf.

Reports that a deliberately misbehaving experimental file system produces
mismatches are the tool working as intended, not vulnerabilities.
