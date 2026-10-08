//! Mount-time probes: how each file system behaves where POSIX leaves the
//! choice to it, and which optional operations it supports.
//!
//! A few operations run in a scratch directory at the root of each tree,
//! through the same backend calls the engine uses for the real operations.
//! Where the two file systems legitimately differ, the engine adapts:
//! directory link counts are not compared, or the secondary's `mtime` is set
//! to the primary's after exactly the operation that stamps it on one file
//! system only. Differences in supported operations are not adapted to (an
//! experimental file system that lacks one should be noticed); they are
//! logged once, with the allow rule that accepts them.

use std::ffi::CString;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use serde::Serialize;

use crate::backend::{Backend, TimeSpec};
use crate::sys::{FileKind, SysResult, Ts};

const OLD: Ts = Ts { sec: 1_000_000, nsec: 0 };

/// What one file system does; `None` where the probe could not tell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Behavior {
    /// A directory's link count counts its subdirectories (2 + n).
    pub dir_nlink: Option<bool>,
    /// A rename that moves a directory to another parent stamps the
    /// directory's own `mtime` (its `..` entry changed).
    pub moved_dir_mtime: Option<bool>,
    /// `RENAME_EXCHANGE` of directories in different parents stamps their
    /// `mtime`.
    pub exchanged_dir_mtime: Option<bool>,
    /// A truncate to the current size stamps `mtime`: by path / by descriptor (`ftruncate`), of an empty / a
    /// non-empty file (tmpfs, for one, stamps the non-empty file only).
    pub truncate_same_size_mtime: Truncates,
    /// A hole punched into a range without data stamps `mtime`.
    pub punch_hole_in_hole_mtime: Option<bool>,
    /// Supported `fallocate` modes.
    pub fallocate: FallocModes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Truncates {
    pub path_empty: Option<bool>,
    pub path: Option<bool>,
    pub fd_empty: Option<bool>,
    pub fd: Option<bool>,
}

/// Which kinds of truncate to the current size the engine aligns after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TruncateAdapt {
    pub path_empty: bool,
    pub path: bool,
    pub fd_empty: bool,
    pub fd: bool,
}

impl TruncateAdapt {
    /// Whether a truncate to the current `size`, by descriptor or by path, needs the alignment.
    pub fn needed(&self, by_fd: bool, size: u64) -> bool {
        match (by_fd, size == 0) {
            (false, true) => self.path_empty,
            (false, false) => self.path,
            (true, true) => self.fd_empty,
            (true, false) => self.fd,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FallocModes {
    pub punch_hole: Option<bool>,
    pub zero_range: Option<bool>,
    pub collapse_range: Option<bool>,
    pub insert_range: Option<bool>,
}

/// What the engine does about the differences of two file systems.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adapt {
    /// Directory link counts are not compared.
    pub no_dir_nlink: bool,
    /// After a directory moves to another parent, its secondary `mtime` is set to the primary's.
    pub moved_dir_mtime: bool,
    /// The same after `RENAME_EXCHANGE` across parents.
    pub exchanged_dir_mtime: bool,
    /// The same after a truncate to the current size (of these kinds).
    pub truncate_same_size_mtime: TruncateAdapt,
    /// The same after punching a hole into a range that held no data on the primary.
    pub punch_hole_in_hole_mtime: bool,
}

impl Adapt {
    /// The adaptations for `p` (primary) and `s` (secondary), and a description of each.
    pub fn between(p: &Behavior, s: &Behavior) -> (Adapt, Vec<String>) {
        let differ = |a: Option<bool>, b: Option<bool>| matches!((a, b), (Some(x), Some(y)) if x != y);
        let mut notes = Vec::new();
        let who = |a: Option<bool>| if a == Some(true) { "primary" } else { "secondary" };
        let no_dir_nlink = p.dir_nlink == Some(false) || s.dir_nlink == Some(false);
        if no_dir_nlink {
            notes.push(format!(
                "the {} does not count subdirectories in a directory's link count: directory link counts are not compared",
                if p.dir_nlink == Some(false) { "primary" } else { "secondary" }
            ));
        }
        let mut one = |a: Option<bool>, b: Option<bool>, what: &str| {
            let d = differ(a, b);
            if d {
                notes.push(format!("only the {} stamps the mtime {what}: the secondary's is set to the primary's", who(a)));
            }
            d
        };
        let adapt = Adapt {
            no_dir_nlink,
            moved_dir_mtime: one(p.moved_dir_mtime, s.moved_dir_mtime, "of a directory moved to another parent"),
            exchanged_dir_mtime: one(
                p.exchanged_dir_mtime,
                s.exchanged_dir_mtime,
                "of directories exchanged (RENAME_EXCHANGE) between parents",
            ),
            truncate_same_size_mtime: TruncateAdapt {
                path_empty: one(
                    p.truncate_same_size_mtime.path_empty,
                    s.truncate_same_size_mtime.path_empty,
                    "of an empty file truncated by path to size 0",
                ),
                path: one(
                    p.truncate_same_size_mtime.path,
                    s.truncate_same_size_mtime.path,
                    "of a file truncated by path to its current size",
                ),
                fd_empty: one(
                    p.truncate_same_size_mtime.fd_empty,
                    s.truncate_same_size_mtime.fd_empty,
                    "of an empty file ftruncate()d to size 0",
                ),
                fd: one(
                    p.truncate_same_size_mtime.fd,
                    s.truncate_same_size_mtime.fd,
                    "of a file ftruncate()d to its current size",
                ),
            },
            punch_hole_in_hole_mtime: one(
                p.punch_hole_in_hole_mtime,
                s.punch_hole_in_hole_mtime,
                "of a file when a hole is punched where it holds no data",
            ),
        };
        (adapt, notes)
    }
}

/// Operations one file system supports and the other does not: one line each, with the allow rule that accepts
/// the resulting `result` mismatches.
pub fn capability_gaps(p: &Behavior, s: &Behavior) -> Vec<String> {
    let modes = [
        ("FALLOC_FL_PUNCH_HOLE", p.fallocate.punch_hole, s.fallocate.punch_hole),
        ("FALLOC_FL_ZERO_RANGE", p.fallocate.zero_range, s.fallocate.zero_range),
        ("FALLOC_FL_COLLAPSE_RANGE", p.fallocate.collapse_range, s.fallocate.collapse_range),
        ("FALLOC_FL_INSERT_RANGE", p.fallocate.insert_range, s.fallocate.insert_range),
    ];
    modes
        .iter()
        .filter_map(|&(mode, a, b)| match (a, b) {
            (Some(true), Some(false)) => Some(format!(
                "the secondary does not support fallocate {mode} (the primary does): such calls are reported as \
                 `fallocate` `result` mismatches; to accept that: {{ op = \"fallocate\", kind = \"result\", secondary = \"EOPNOTSUPP\" }}"
            )),
            (Some(false), Some(true)) => Some(format!(
                "the primary does not support fallocate {mode} (the secondary does): such calls are reported as \
                 `fallocate` `result` mismatches; to accept that: {{ op = \"fallocate\", kind = \"result\", primary = \"EOPNOTSUPP\" }}"
            )),
            _ => None,
        })
        .collect()
}

/// Probes the file system of `be` in a scratch directory at its root, which
/// is removed again; the root's times are restored. Nothing is learned (all
/// `None`) when the root is not writable.
pub fn probe(be: &dyn Backend) -> Behavior {
    let Ok(root) = be.root() else { return Behavior::default() };
    let Ok(rst) = be.stat(root.as_fd()) else { return Behavior::default() };
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let name = cs(&format!(".xcheckfs-probe-{}-{nanos:08x}", std::process::id()));
    if be.mkdir(root.as_fd(), &name, 0o700).is_err() {
        tracing::debug!("{}: cannot probe the file system (the root is not writable)", be.label());
        return Behavior::default();
    }
    let b = match be.lookup(root.as_fd(), &name) {
        Ok((d, _)) => run(be, d.as_fd()),
        Err(_) => Behavior::default(),
    };
    remove_tree(be, root.as_fd(), &name);
    let _ = be.utimens(root.as_fd(), FileKind::Dir, None, TimeSpec::Set(rst.atime), TimeSpec::Set(rst.mtime));
    b
}

fn cs(s: &str) -> CString {
    CString::new(s).expect("no NUL")
}

fn run(be: &dyn Backend, d: BorrowedFd<'_>) -> Behavior {
    let mut b = Behavior { dir_nlink: dir_nlink(be, d).ok(), ..Default::default() };
    b.moved_dir_mtime = moved_dir(be, d).ok();
    b.exchanged_dir_mtime = exchanged_dirs(be, d).ok();
    b.truncate_same_size_mtime = Truncates {
        path_empty: truncate_same_size(be, d, "t0", 0, false).ok(),
        path: truncate_same_size(be, d, "t1", 8192, false).ok(),
        fd_empty: truncate_same_size(be, d, "t2", 0, true).ok(),
        fd: truncate_same_size(be, d, "t3", 8192, true).ok(),
    };
    b.punch_hole_in_hole_mtime = punch_in_hole(be, d).ok().flatten();
    b.fallocate = FallocModes {
        punch_hole: falloc_supported(be, d, "fp", libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE),
        zero_range: falloc_supported(be, d, "fz", libc::FALLOC_FL_ZERO_RANGE),
        collapse_range: falloc_supported(be, d, "fc", libc::FALLOC_FL_COLLAPSE_RANGE),
        insert_range: falloc_supported(be, d, "fi", libc::FALLOC_FL_INSERT_RANGE),
    };
    b
}

fn sub(be: &dyn Backend, d: BorrowedFd<'_>, name: &str) -> SysResult<OwnedFd> {
    let c = cs(name);
    be.mkdir(d, &c, 0o700)?;
    Ok(be.lookup(d, &c)?.0)
}

fn age(be: &dyn Backend, fd: BorrowedFd<'_>, kind: FileKind) -> SysResult<()> {
    be.utimens(fd, kind, None, TimeSpec::Set(OLD), TimeSpec::Set(OLD))
}

fn stamped(be: &dyn Backend, fd: BorrowedFd<'_>) -> SysResult<bool> {
    Ok(be.stat(fd)?.mtime != OLD)
}

fn dir_nlink(be: &dyn Backend, d: BorrowedFd<'_>) -> SysResult<bool> {
    let a = sub(be, d, "n")?;
    be.mkdir(a.as_fd(), &cs("c"), 0o700)?;
    Ok(be.stat(a.as_fd())?.nlink >= 3)
}

fn moved_dir(be: &dyn Backend, d: BorrowedFd<'_>) -> SysResult<bool> {
    let (p, q) = (sub(be, d, "mp")?, sub(be, d, "mq")?);
    let m = sub(be, p.as_fd(), "m")?;
    age(be, m.as_fd(), FileKind::Dir)?;
    be.rename(p.as_fd(), &cs("m"), q.as_fd(), &cs("m"), 0)?;
    stamped(be, m.as_fd())
}

fn exchanged_dirs(be: &dyn Backend, d: BorrowedFd<'_>) -> SysResult<bool> {
    let (p, q) = (sub(be, d, "xp")?, sub(be, d, "xq")?);
    let (x, y) = (sub(be, p.as_fd(), "x")?, sub(be, q.as_fd(), "y")?);
    age(be, x.as_fd(), FileKind::Dir)?;
    age(be, y.as_fd(), FileKind::Dir)?;
    be.rename(p.as_fd(), &cs("x"), q.as_fd(), &cs("y"), libc::RENAME_EXCHANGE)?;
    Ok(stamped(be, x.as_fd())? || stamped(be, y.as_fd())?)
}

/// A regular file `name` in `d` with `data` bytes: (path descriptor, open read-write descriptor).
fn file(be: &dyn Backend, d: BorrowedFd<'_>, name: &str, data: usize) -> SysResult<(OwnedFd, OwnedFd)> {
    let c = cs(name);
    let f = be.create(d, &c, libc::O_RDWR | libc::O_EXCL, 0o600)?;
    if data > 0 {
        be.pwrite(f.as_fd(), &vec![0x5a; data], 0)?;
    }
    Ok((be.lookup(d, &c)?.0, f))
}

fn truncate_same_size(be: &dyn Backend, d: BorrowedFd<'_>, name: &str, size: usize, by_fd: bool) -> SysResult<bool> {
    let (node, f) = file(be, d, name, size)?;
    age(be, node.as_fd(), FileKind::Regular)?;
    be.truncate(node.as_fd(), by_fd.then(|| f.as_fd()), size as u64)?;
    stamped(be, node.as_fd())
}

/// `None` when punching holes is not supported at all.
fn punch_in_hole(be: &dyn Backend, d: BorrowedFd<'_>) -> SysResult<Option<bool>> {
    let (node, f) = file(be, d, "h", 0)?;
    be.truncate(node.as_fd(), Some(f.as_fd()), 1 << 20)?;
    age(be, node.as_fd(), FileKind::Regular)?;
    match be.fallocate(f.as_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 1 << 16, 1 << 16) {
        Ok(()) => Ok(Some(stamped(be, node.as_fd())?)),
        Err(libc::EOPNOTSUPP) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `fallocate(mode)` works on a 1 MiB file (block-aligned ranges in its middle); `None` for errors other
/// than "not supported".
fn falloc_supported(be: &dyn Backend, d: BorrowedFd<'_>, name: &str, mode: i32) -> Option<bool> {
    let (_, f) = file(be, d, name, 1 << 20).ok()?;
    match be.fallocate(f.as_fd(), mode, 1 << 16, 1 << 16) {
        Ok(()) => Some(true),
        Err(libc::EOPNOTSUPP) => Some(false),
        Err(_) => None,
    }
}

/// Removes `dir/name` and everything below it (the probe's scratch tree: a few levels).
fn remove_tree(be: &dyn Backend, dir: BorrowedFd<'_>, name: &CString) {
    if let Ok((fd, st)) = be.lookup(dir, name) {
        if st.kind() == FileKind::Dir {
            let list = be.opendir(fd.as_fd()).and_then(|d| be.readdir(d.as_fd())).unwrap_or_default();
            for e in list {
                if let Ok(c) = CString::new(e.name) {
                    remove_tree(be, fd.as_fd(), &c);
                }
            }
            let _ = be.rmdir(dir, name);
        } else {
            let _ = be.unlink(dir, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapts_only_to_known_differences() {
        let p = Behavior { moved_dir_mtime: Some(false), exchanged_dir_mtime: Some(true), dir_nlink: Some(true), ..Default::default() };
        let s = Behavior { moved_dir_mtime: Some(true), exchanged_dir_mtime: None, dir_nlink: Some(false), ..Default::default() };
        let (a, notes) = Adapt::between(&p, &s);
        assert!(a.moved_dir_mtime && a.no_dir_nlink);
        assert!(!a.exchanged_dir_mtime, "unknown on one side: no adaptation");
        assert_eq!(notes.len(), 2);
        assert_eq!(Adapt::between(&p, &p).0, Adapt::default());
    }

    #[test]
    fn capability_gaps_name_the_allow_rule() {
        let p = Behavior { fallocate: FallocModes { zero_range: Some(true), ..Default::default() }, ..Default::default() };
        let s = Behavior { fallocate: FallocModes { zero_range: Some(false), ..Default::default() }, ..Default::default() };
        let g = capability_gaps(&p, &s);
        assert_eq!(g.len(), 1);
        assert!(g[0].contains("ZERO_RANGE") && g[0].contains("secondary = \"EOPNOTSUPP\""));
        assert!(capability_gaps(&p, &p).is_empty());
    }
}
