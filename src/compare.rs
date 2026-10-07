//! Pure comparison functions: what counts as "the same" between the two
//! file systems. Kept free of engine state so `verify` and the tests can use
//! them directly.

use std::time::Duration;

use crate::backend::DirEntry;
use crate::sys::{FileKind, Stat, Ts};

/// One attribute difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrDiff {
    pub field: &'static str,
    pub primary: String,
    pub secondary: String,
}

#[derive(Clone, Copy, Debug)]
pub struct AttrRules {
    pub time_tolerance: Duration,
    pub dir_nlink: bool,
    /// Compare mtime absolutely (disabled for trees whose mtimes are known
    /// to differ, e.g. by `verify --no-mtime`).
    pub mtime: bool,
}

/// Compares two stats of the same object.
///
/// Never compared: `dev`, `ino` (file-system specific), `blocks`,
/// `blksize` (allocation specific), `atime` (mount-option specific), the
/// size of directories (format specific) and `ctime` (absolute values differ
/// after any copy; ctime *changes* are tracked by [`ctime_change`]).
pub fn diff_stat(p: &Stat, s: &Stat, r: &AttrRules) -> Vec<AttrDiff> {
    let mut d = Vec::new();
    let mut push = |field, a: String, b: String| d.push(AttrDiff { field, primary: a, secondary: b });
    let (kp, ks) = (p.kind(), s.kind());
    if kp != ks {
        push("type", kp.name().into(), ks.name().into());
        return d;
    }
    if p.perm() != s.perm() {
        push("mode", format!("{:04o}", p.perm()), format!("{:04o}", s.perm()));
    }
    if p.uid != s.uid {
        push("uid", p.uid.to_string(), s.uid.to_string());
    }
    if p.gid != s.gid {
        push("gid", p.gid.to_string(), s.gid.to_string());
    }
    if kp != FileKind::Dir && p.size != s.size {
        push("size", p.size.to_string(), s.size.to_string());
    }
    if (kp != FileKind::Dir || r.dir_nlink) && p.nlink != s.nlink {
        push("nlink", p.nlink.to_string(), s.nlink.to_string());
    }
    if matches!(kp, FileKind::CharDev | FileKind::BlockDev) && p.rdev != s.rdev {
        push("rdev", format!("{:#x}", p.rdev), format!("{:#x}", s.rdev));
    }
    if r.mtime && !times_close(&p.mtime, &s.mtime, r.time_tolerance) {
        push("mtime", p.mtime.to_string(), s.mtime.to_string());
    }
    d
}

pub fn times_close(a: &Ts, b: &Ts, tol: Duration) -> bool {
    a.abs_diff_ns(b) <= tol.as_nanos()
}

/// ctime change tracking: given the previous and current ctimes seen on
/// both sides, reports a difference when one side's ctime moved by more than
/// the tolerance while the other did not move at all.
pub fn ctime_change(prev: (Ts, Ts), cur: (Ts, Ts), tol: Duration) -> Option<AttrDiff> {
    let pc = cur.0 != prev.0;
    let sc = cur.1 != prev.1;
    if pc == sc {
        return None;
    }
    let moved = if pc { cur.0.abs_diff_ns(&prev.0) } else { cur.1.abs_diff_ns(&prev.1) };
    if moved <= tol.as_nanos() {
        return None;
    }
    Some(AttrDiff {
        field: "ctime",
        primary: format!("{} -> {}", prev.0, cur.0),
        secondary: format!("{} -> {}", prev.1, cur.1),
    })
}

/// Result of comparing two directory listings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirDiff {
    pub only_primary: Vec<Vec<u8>>,
    pub only_secondary: Vec<Vec<u8>>,
    pub type_differs: Vec<Vec<u8>>,
}

impl DirDiff {
    pub fn is_empty(&self) -> bool {
        self.only_primary.is_empty() && self.only_secondary.is_empty() && self.type_differs.is_empty()
    }
    pub fn describe(&self, limit: usize) -> String {
        let f = |v: &[Vec<u8>]| {
            let mut s: Vec<String> =
                v.iter().take(limit).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
            if v.len() > limit {
                s.push(format!("… +{}", v.len() - limit));
            }
            s.join(", ")
        };
        let mut out = Vec::new();
        if !self.only_primary.is_empty() {
            out.push(format!("only in primary: [{}]", f(&self.only_primary)));
        }
        if !self.only_secondary.is_empty() {
            out.push(format!("only in secondary: [{}]", f(&self.only_secondary)));
        }
        if !self.type_differs.is_empty() {
            out.push(format!("type differs: [{}]", f(&self.type_differs)));
        }
        out.join("; ")
    }
}

/// Order-independent listing comparison. `DT_UNKNOWN` matches any type.
pub fn diff_dir(p: &[DirEntry], s: &[DirEntry]) -> DirDiff {
    let mut a: Vec<&DirEntry> = p.iter().collect();
    let mut b: Vec<&DirEntry> = s.iter().collect();
    a.sort_unstable_by(|x, y| x.name.cmp(&y.name));
    b.sort_unstable_by(|x, y| x.name.cmp(&y.name));
    let mut d = DirDiff::default();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x.name == y.name => {
                if let (Some(kx), Some(ky)) = (x.kind, y.kind)
                    && kx != ky {
                        d.type_differs.push(x.name.clone());
                    }
                i += 1;
                j += 1;
            }
            (Some(x), Some(y)) if x.name < y.name => {
                d.only_primary.push(x.name.clone());
                i += 1;
            }
            (Some(_), Some(y)) => {
                d.only_secondary.push(y.name.clone());
                j += 1;
            }
            (Some(x), None) => {
                d.only_primary.push(x.name.clone());
                i += 1;
            }
            (None, Some(y)) => {
                d.only_secondary.push(y.name.clone());
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    d
}

/// Where two buffers first differ, with a short hex context and hashes, for
/// reports. `None` if equal.
pub fn diff_data(base_off: u64, p: &[u8], s: &[u8]) -> Option<String> {
    if p == s {
        return None;
    }
    let first = p.iter().zip(s.iter()).position(|(a, b)| a != b).unwrap_or(p.len().min(s.len()));
    let ctx = |b: &[u8]| {
        let end = (first + 16).min(b.len());
        b[first.min(b.len())..end].iter().map(|x| format!("{x:02x}")).collect::<String>()
    };
    let ndiff = p.iter().zip(s.iter()).filter(|(a, b)| a != b).count();
    Some(format!(
        "first difference at offset {} ({} differing bytes in {} compared); primary[..16]={} secondary[..16]={}; xxh3 {:016x} vs {:016x}; lengths {} vs {}",
        base_off + first as u64,
        ndiff + p.len().abs_diff(s.len()),
        p.len().max(s.len()),
        ctx(p),
        ctx(s),
        xxhash_rust::xxh3::xxh3_64(p),
        xxhash_rust::xxh3::xxh3_64(s),
        p.len(),
        s.len()
    ))
}

/// Splits a listxattr buffer into sorted names.
pub fn xattr_names(buf: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = buf.split(|&b| b == 0).filter(|n| !n.is_empty()).collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(mode: u32, size: u64, mtime: i64) -> Stat {
        Stat { mode, size, nlink: 1, mtime: Ts { sec: mtime, nsec: 0 }, ..Default::default() }
    }
    const R: AttrRules = AttrRules { time_tolerance: Duration::from_secs(1), dir_nlink: true, mtime: true };

    #[test]
    fn stat_diffs() {
        let a = st(libc::S_IFREG | 0o644, 10, 100);
        assert!(diff_stat(&a, &a, &R).is_empty());
        let b = st(libc::S_IFREG | 0o600, 11, 100);
        let d = diff_stat(&a, &b, &R);
        assert_eq!(d.iter().map(|x| x.field).collect::<Vec<_>>(), vec!["mode", "size"]);
        let c = st(libc::S_IFREG | 0o644, 10, 105);
        assert_eq!(diff_stat(&a, &c, &R)[0].field, "mtime");
        let dir_a = st(libc::S_IFDIR | 0o755, 4096, 100);
        let dir_b = st(libc::S_IFDIR | 0o755, 60, 100);
        assert!(diff_stat(&dir_a, &dir_b, &R).is_empty(), "dir size is not compared");
        let l = st(libc::S_IFLNK | 0o777, 3, 100);
        assert_eq!(diff_stat(&a, &l, &R)[0].field, "type");
    }

    #[test]
    fn ctime_tracking() {
        let t = |s| Ts { sec: s, nsec: 0 };
        let tol = Duration::from_secs(1);
        assert!(ctime_change((t(10), t(50)), (t(10), t(50)), tol).is_none());
        assert!(ctime_change((t(10), t(50)), (t(20), t(60)), tol).is_none());
        assert!(ctime_change((t(10), t(50)), (t(20), t(50)), tol).is_some());
        // coarse granularity on one side: small moves are tolerated
        assert!(ctime_change((t(10), t(50)), (Ts { sec: 10, nsec: 300 }, t(50)), tol).is_none());
    }

    #[test]
    fn dir_diffs() {
        let e = |n: &str, k| DirEntry { name: n.as_bytes().to_vec(), ino: 0, kind: k };
        let p = vec![e("a", Some(FileKind::Regular)), e("b", Some(FileKind::Dir)), e("c", None)];
        let s = vec![e("c", Some(FileKind::Dir)), e("b", Some(FileKind::Regular)), e("d", None)];
        let d = diff_dir(&p, &s);
        assert_eq!(d.only_primary, vec![b"a".to_vec()]);
        assert_eq!(d.only_secondary, vec![b"d".to_vec()]);
        assert_eq!(d.type_differs, vec![b"b".to_vec()]);
        assert!(diff_dir(&p, &p).is_empty());
    }

    #[test]
    fn data_diffs() {
        assert!(diff_data(0, b"abc", b"abc").is_none());
        let d = diff_data(100, b"abcdef", b"abXdef").unwrap();
        assert!(d.contains("offset 102"), "{d}");
        assert!(diff_data(0, b"abc", b"ab").unwrap().contains("lengths 3 vs 2"));
    }
}
