//! Offline comparison of two directory trees (`xcheckfs verify`).
//!
//! Both trees are walked in lockstep. For every name present on both sides
//! the attributes (via [`compare::diff_stat`]), symlink target, xattrs, file
//! content and hard-link structure are compared. Names present on one side
//! only are reported and not descended into. Symlinks are never followed.
//!
//! Directories are processed in parallel on a dedicated rayon pool; a task
//! holds no descriptor while it waits for its children, and each worker reuses
//! one pair of thread-local read buffers, so memory stays bounded by the
//! directory listings of the current path plus `threads * 2 * CHUNK`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use rayon::prelude::*;

use crate::backend::DirEntry;
use crate::compare::{self, AttrRules};
use crate::sys::{self, FileKind, Stat, SysResult};

/// Bytes read from each side per comparison step.
const CHUNK: usize = 1 << 20;
/// Errors kept in the report; further ones are only summarised.
const MAX_ERRORS: usize = 1000;
/// Interval between progress callbacks.
const PROGRESS_TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct VerifyOptions {
    /// Compare file data (otherwise only metadata).
    pub content: bool,
    pub xattrs: bool,
    pub mtime: bool,
    pub time_tolerance: Duration,
    /// Also compare the link count of directories.
    pub dir_nlink: bool,
    /// Worker threads; `0` means one per CPU.
    pub threads: usize,
    /// Differences kept in the report; later ones are only counted.
    pub max_reports: usize,
    /// Do not descend into directories on another file system than the root.
    pub one_file_system: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions {
            content: true,
            xattrs: true,
            mtime: true,
            time_tolerance: Duration::ZERO,
            dir_nlink: false,
            threads: 0,
            max_reports: 1000,
            one_file_system: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Difference {
    /// Relative to the roots, with a leading `/` (`/` is the root itself).
    pub path: String,
    /// E.g. `only in primary`, `attr mode`, `content`, `xattr user.x`,
    /// `symlink target`, `hardlink structure`.
    pub what: String,
    pub primary: String,
    pub secondary: String,
}

#[derive(Clone, Debug, Default)]
pub struct VerifyReport {
    /// The first `max_reports` differences, sorted by path.
    pub differences: Vec<Difference>,
    /// All differences found, including those not kept in `differences`.
    pub total_differences: u64,
    /// Non-directory entries present on both sides.
    pub files: u64,
    /// Directories present on both sides (including the roots).
    pub dirs: u64,
    pub bytes_compared: u64,
    /// Entries that could not be read; they are not counted as differences.
    pub errors: Vec<String>,
}

/// Compares the tree below `primary` with the tree below `secondary`.
///
/// `progress`, when given, is called about five times a second from a helper
/// thread with a snapshot holding only the counters (`differences` and
/// `errors` are empty in snapshots). Returns `Err` only when a root cannot be
/// examined; everything else ends up in the report.
pub fn verify(
    primary: &Path,
    secondary: &Path,
    opts: &VerifyOptions,
    progress: Option<&(dyn Fn(&VerifyReport) + Sync)>,
) -> anyhow::Result<VerifyReport> {
    let ps = stat_path(primary, true).map_err(|e| anyhow::anyhow!("{}: {}", primary.display(), sys::fmt_errno(e)))?;
    let ss = stat_path(secondary, true)
        .map_err(|e| anyhow::anyhow!("{}: {}", secondary.display(), sys::fmt_errno(e)))?;
    if ps.kind() != FileKind::Dir {
        bail!("{}: not a directory", primary.display());
    }
    if ss.kind() != FileKind::Dir {
        bail!("{}: not a directory", secondary.display());
    }
    let threads = if opts.threads == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else {
        opts.threads
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        // Recursion depth follows directory depth.
        .stack_size(64 << 20)
        .thread_name(|i| format!("verify-{i}"))
        .build()
        .context("cannot create the thread pool")?;

    let ctx = Ctx {
        p_root: primary,
        s_root: secondary,
        opts,
        // (a directory link count of 1 means the file system does not count subdirectories, btrfs for one)
        rules: AttrRules {
            time_tolerance: opts.time_tolerance,
            dir_nlink: opts.dir_nlink && ps.nlink != 1 && ss.nlink != 1,
            mtime: opts.mtime,
        },
        p_dev: ps.dev,
        s_dev: ss.dev,
        files: AtomicU64::new(0),
        dirs: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        total: AtomicU64::new(0),
        diffs: Mutex::new(Vec::new()),
        errors: Mutex::new(Vec::new()),
        errors_dropped: AtomicU64::new(0),
        p_links: Mutex::new(HashMap::new()),
        s_links: Mutex::new(HashMap::new()),
    };

    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        if let Some(cb) = progress {
            let (ctx, done) = (&ctx, &done);
            scope.spawn(move || {
                let mut waited = Duration::ZERO;
                let step = Duration::from_millis(20);
                while !done.load(Ordering::Acquire) {
                    std::thread::sleep(step);
                    waited += step;
                    if waited >= PROGRESS_TICK {
                        waited = Duration::ZERO;
                        cb(&ctx.snapshot());
                    }
                }
            });
        }
        pool.install(|| ctx.compare_pair(b"/", Some((ps, ss))));
        done.store(true, Ordering::Release);
    });
    ctx.check_hardlinks();
    Ok(ctx.finish())
}

/// Relative paths (bytes) per (dev, ino), for entries with `nlink > 1`.
type LinkMap = Mutex<HashMap<(u64, u64), Vec<Vec<u8>>>>;

struct Ctx<'a> {
    p_root: &'a Path,
    s_root: &'a Path,
    opts: &'a VerifyOptions,
    rules: AttrRules,
    p_dev: u64,
    s_dev: u64,
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    total: AtomicU64,
    diffs: Mutex<Vec<Difference>>,
    errors: Mutex<Vec<String>>,
    errors_dropped: AtomicU64,
    /// Paths (relative, bytes) per (dev, ino) with `nlink > 1`, per side.
    p_links: LinkMap,
    s_links: LinkMap,
}

thread_local! {
    /// Reused read buffers (primary, secondary) of the current worker.
    static BUFS: RefCell<(Vec<u8>, Vec<u8>)> = const { RefCell::new((Vec::new(), Vec::new())) };
}

impl Ctx<'_> {
    fn snapshot(&self) -> VerifyReport {
        VerifyReport {
            differences: Vec::new(),
            total_differences: self.total.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes_compared: self.bytes.load(Ordering::Relaxed),
            errors: Vec::new(),
        }
    }

    fn finish(self) -> VerifyReport {
        let mut r = self.snapshot();
        r.differences = self.diffs.into_inner().unwrap();
        r.differences.sort_by(|a, b| (&a.path, &a.what).cmp(&(&b.path, &b.what)));
        r.errors = self.errors.into_inner().unwrap();
        let dropped = self.errors_dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            r.errors.push(format!("... and {dropped} more errors"));
        }
        r
    }

    fn diff(&self, rel: &[u8], what: impl Into<String>, primary: impl Into<String>, secondary: impl Into<String>) {
        self.total.fetch_add(1, Ordering::Relaxed);
        let mut d = self.diffs.lock().unwrap();
        if d.len() < self.opts.max_reports {
            d.push(Difference {
                path: show(rel),
                what: what.into(),
                primary: primary.into(),
                secondary: secondary.into(),
            });
        }
    }

    fn error(&self, rel: &[u8], msg: impl std::fmt::Display) {
        let mut e = self.errors.lock().unwrap();
        if e.len() < MAX_ERRORS {
            e.push(format!("{}: {msg}", show(rel)));
        } else {
            self.errors_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Compares one name present on both sides (stats are passed for the
    /// roots, which are examined during setup).
    fn compare_pair(&self, rel: &[u8], known: Option<(Stat, Stat)>) {
        let (pp, sp) = (join_root(self.p_root, rel), join_root(self.s_root, rel));
        let (p, s) = match known {
            Some(x) => x,
            None => match (stat_path(&pp, false), stat_path(&sp, false)) {
                (Ok(p), Ok(s)) => (p, s),
                (Err(e), _) => return self.error(rel, format!("stat primary: {}", sys::fmt_errno(e))),
                (_, Err(e)) => return self.error(rel, format!("stat secondary: {}", sys::fmt_errno(e))),
            },
        };
        let kind = p.kind();
        if kind == FileKind::Dir {
            self.dirs.fetch_add(1, Ordering::Relaxed);
        } else {
            self.files.fetch_add(1, Ordering::Relaxed);
        }
        let attr = compare::diff_stat(&p, &s, &self.rules);
        let type_differs = attr.iter().any(|d| d.field == "type");
        for d in attr {
            self.diff(rel, format!("attr {}", d.field), d.primary, d.secondary);
        }
        if type_differs {
            return;
        }

        if kind != FileKind::Dir {
            if p.nlink > 1 {
                self.p_links.lock().unwrap().entry(p.ident()).or_default().push(rel.to_vec());
            }
            if s.nlink > 1 {
                self.s_links.lock().unwrap().entry(s.ident()).or_default().push(rel.to_vec());
            }
        }
        if self.opts.xattrs {
            self.compare_xattrs(rel, &pp, &sp);
        }
        match kind {
            FileKind::Symlink => self.compare_links(rel, &pp, &sp),
            FileKind::Regular if self.opts.content && p.size == s.size => {
                self.compare_content(rel, &pp, &sp)
            }
            FileKind::Dir => {
                let other_fs = self.opts.one_file_system && (p.dev != self.p_dev || s.dev != self.s_dev);
                if !other_fs {
                    self.compare_dir(rel, &pp, &sp);
                }
            }
            _ => {}
        }
    }

    fn compare_dir(&self, rel: &[u8], pp: &Path, sp: &Path) {
        let (pl, sl) = match (list_dir(pp), list_dir(sp)) {
            (Ok(p), Ok(s)) => (p, s),
            (Err(e), _) => return self.error(rel, format!("readdir primary: {}", sys::fmt_errno(e))),
            (_, Err(e)) => return self.error(rel, format!("readdir secondary: {}", sys::fmt_errno(e))),
        };
        let dd = compare::diff_dir(&pl, &sl);
        for (names, what, root, kind_side) in [
            (&dd.only_primary, "only in primary", pp, true),
            (&dd.only_secondary, "only in secondary", sp, false),
        ] {
            for n in names {
                let child = child_rel(rel, n);
                let path = root.join(OsStr::from_bytes(n));
                let kind = stat_path(&path, false).map_or("present", |s| s.kind().name());
                let (a, b) = if kind_side { (kind, "absent") } else { ("absent", kind) };
                self.diff(&child, what, a, b);
            }
        }
        // Both listings are sorted by name: one merge pass yields the names
        // present on both sides.
        let only: HashSet<&[u8]> =
            dd.only_primary.iter().chain(&dd.only_secondary).map(|n| n.as_slice()).collect();
        let common: Vec<&[u8]> = pl.iter().map(|e| e.name.as_slice()).filter(|n| !only.contains(n)).collect();
        common.par_iter().for_each(|n| self.compare_pair(&child_rel(rel, n), None));
    }

    fn compare_links(&self, rel: &[u8], pp: &Path, sp: &Path) {
        match (std::fs::read_link(pp), std::fs::read_link(sp)) {
            (Ok(a), Ok(b)) => {
                if a != b {
                    self.diff(rel, "symlink target", a.to_string_lossy(), b.to_string_lossy());
                }
            }
            (Err(e), _) => self.error(rel, format!("readlink primary: {e}")),
            (_, Err(e)) => self.error(rel, format!("readlink secondary: {e}")),
        }
    }

    fn compare_xattrs(&self, rel: &[u8], pp: &Path, sp: &Path) {
        // SELinux labels are assigned by the policy for each mount, not kept by the file system: two healthy
        // file systems mounted with different contexts label the same files differently.
        let labels = |mut v: Vec<(Vec<u8>, Vec<u8>)>| {
            v.retain(|(n, _)| n != b"security.selinux");
            v
        };
        let (pa, sa) = match (read_xattrs(pp).map(labels), read_xattrs(sp).map(labels)) {
            (Ok(p), Ok(s)) => (p, s),
            (Err(e), _) => return self.error(rel, format!("xattr primary: {}", sys::fmt_errno(e))),
            (_, Err(e)) => return self.error(rel, format!("xattr secondary: {}", sys::fmt_errno(e))),
        };
        // Both lists are sorted by name; merge them.
        let (mut i, mut j) = (0, 0);
        while i < pa.len() || j < sa.len() {
            let ord = match (pa.get(i), sa.get(j)) {
                (Some(a), Some(b)) => a.0.cmp(&b.0),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            let what = |n: &[u8]| format!("xattr {}", String::from_utf8_lossy(n));
            match ord {
                std::cmp::Ordering::Less => {
                    self.diff(rel, what(&pa[i].0), fmt_value(&pa[i].1), "absent");
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    self.diff(rel, what(&sa[j].0), "absent", fmt_value(&sa[j].1));
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    if pa[i].1 != sa[j].1 {
                        self.diff(rel, what(&pa[i].0), fmt_value(&pa[i].1), fmt_value(&sa[j].1));
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }

    fn compare_content(&self, rel: &[u8], pp: &Path, sp: &Path) {
        let (mut pf, mut sf) = match (open_ro(pp), open_ro(sp)) {
            (Ok(p), Ok(s)) => (p, s),
            (Err(e), _) => return self.error(rel, format!("open primary: {e}")),
            (_, Err(e)) => return self.error(rel, format!("open secondary: {e}")),
        };
        let res: Result<Option<String>, String> = BUFS.with_borrow_mut(|(pb, sb)| {
            pb.resize(CHUNK, 0);
            sb.resize(CHUNK, 0);
            let mut off = 0u64;
            loop {
                let np = read_full(&mut pf, pb).map_err(|e| format!("read primary: {e}"))?;
                let ns = read_full(&mut sf, sb).map_err(|e| format!("read secondary: {e}"))?;
                self.bytes.fetch_add(np as u64, Ordering::Relaxed);
                if np != ns || pb[..np] != sb[..ns] {
                    return Ok(compare::diff_data(off, &pb[..np], &sb[..ns]));
                }
                off += np as u64;
                if np < CHUNK {
                    return Ok(None);
                }
            }
        });
        match res {
            Ok(Some(d)) => self.diff(rel, "content", d, "differs from primary"),
            Ok(None) => {}
            Err(e) => self.error(rel, e),
        }
    }

    /// Checks that both sides group the same paths into hard-link sets.
    /// Only paths present on both sides take part, so links to files outside
    /// the compared trees are ignored (their `nlink` difference is reported
    /// as an attribute difference).
    fn check_hardlinks(&self) {
        let groups = |m: &LinkMap| -> Vec<Vec<Vec<u8>>> {
            let mut g: Vec<Vec<Vec<u8>>> = m
                .lock()
                .unwrap()
                .values()
                .filter(|v| v.len() >= 2)
                .map(|v| {
                    let mut v = v.clone();
                    v.sort();
                    v
                })
                .collect();
            g.sort();
            g
        };
        let (pg, sg) = (groups(&self.p_links), groups(&self.s_links));
        let index = |g: &[Vec<Vec<u8>>]| -> HashMap<Vec<u8>, usize> {
            g.iter().enumerate().flat_map(|(i, v)| v.iter().map(move |p| (p.clone(), i))).collect()
        };
        let (pi, si) = (index(&pg), index(&sg));
        let pset: HashSet<&Vec<Vec<u8>>> = pg.iter().collect();
        let sset: HashSet<&Vec<Vec<u8>>> = sg.iter().collect();
        let describe = |groups: &[Vec<Vec<u8>>], idx: &HashMap<Vec<u8>, usize>, path: &[u8]| match idx.get(path) {
            Some(&i) => {
                let names: Vec<String> = groups[i].iter().take(8).map(|p| show(p)).collect();
                let more = groups[i].len().saturating_sub(8);
                let tail = if more > 0 { format!(", ... +{more}") } else { String::new() };
                format!("linked with [{}{tail}]", names.join(", "))
            }
            None => "not linked".to_string(),
        };
        let mut reported: HashSet<&[u8]> = HashSet::new();
        for g in pg.iter().filter(|g| !sset.contains(g)) {
            reported.insert(&g[0]);
            self.diff(&g[0], "hardlink structure", describe(&pg, &pi, &g[0]), describe(&sg, &si, &g[0]));
        }
        for g in sg.iter().filter(|g| !pset.contains(g)) {
            if reported.contains(g[0].as_slice()) {
                continue;
            }
            self.diff(&g[0], "hardlink structure", describe(&pg, &pi, &g[0]), describe(&sg, &si, &g[0]));
        }
    }
}

/// Printable form of a relative path.
fn show(rel: &[u8]) -> String {
    String::from_utf8_lossy(rel).into_owned()
}

fn child_rel(rel: &[u8], name: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(rel.len() + 1 + name.len());
    v.extend_from_slice(rel);
    if rel != b"/" {
        v.push(b'/');
    }
    v.extend_from_slice(name);
    v
}

fn join_root(root: &Path, rel: &[u8]) -> PathBuf {
    if rel == b"/" {
        return root.to_path_buf();
    }
    let mut v = root.as_os_str().as_bytes().to_vec();
    v.extend_from_slice(rel);
    PathBuf::from(OsString::from_vec(v))
}

fn stat_path(path: &Path, follow: bool) -> SysResult<Stat> {
    let c = sys::cstr(path.as_os_str().as_bytes())?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid C string and out pointer.
    let r = unsafe {
        if follow { libc::stat(c.as_ptr(), st.as_mut_ptr()) } else { libc::lstat(c.as_ptr(), st.as_mut_ptr()) }
    };
    sys::cvt(r)?;
    // SAFETY: the call succeeded and initialised the buffer.
    Ok(Stat::from_libc(&unsafe { st.assume_init() }))
}

/// Directory listing without `.`/`..`. Types are left unknown: every common
/// entry is `lstat`ed anyway, which is authoritative.
fn list_dir(path: &Path) -> SysResult<Vec<DirEntry>> {
    let rd = std::fs::read_dir(path).map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    let mut v = Vec::new();
    for e in rd {
        let e = e.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        v.push(DirEntry { name: e.file_name().as_bytes().to_vec(), ino: 0, kind: None });
    }
    Ok(v)
}

/// Opens for reading without following a final symlink and, when allowed,
/// without touching the access time.
fn open_ro(path: &Path) -> std::io::Result<File> {
    let open = |flags: i32| {
        File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | flags).open(path)
    };
    match open(libc::O_NOATIME) {
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => open(0),
        r => r,
    }
}

/// Fills `buf` unless EOF comes first; returns the byte count.
fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Runs a size-probing xattr syscall until the buffer was big enough.
/// `call(ptr, len)` must be a `l*xattr` call; `ptr` is null for the probe.
fn xattr_call(mut call: impl FnMut(*mut u8, usize) -> libc::ssize_t) -> SysResult<Vec<u8>> {
    loop {
        let n = sys::cvt_size(call(std::ptr::null_mut(), 0))?;
        let mut buf = vec![0u8; n];
        if n == 0 {
            return Ok(buf);
        }
        match sys::cvt_size(call(buf.as_mut_ptr(), n)) {
            Ok(m) => {
                buf.truncate(m);
                return Ok(buf);
            }
            // The value grew between the two calls.
            Err(libc::ERANGE) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// All xattrs of a path (symlinks not followed) as sorted (name, value);
/// a file system without xattr support yields an empty list.
fn read_xattrs(path: &Path) -> SysResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let c = sys::cstr(path.as_os_str().as_bytes())?;
    // SAFETY (both closures): `c` is a valid C string and the buffer is
    // either null with size 0 or valid for `len` bytes.
    let list = match xattr_call(|p, len| unsafe { libc::llistxattr(c.as_ptr(), p.cast(), len) }) {
        Ok(l) => l,
        Err(libc::EOPNOTSUPP) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for name in compare::xattr_names(&list) {
        let cn = sys::cstr(name)?;
        let get = |cn: &CStr| xattr_call(|p, len| unsafe { libc::lgetxattr(c.as_ptr(), cn.as_ptr(), p.cast(), len) });
        match get(&cn) {
            Ok(v) => out.push((name.to_vec(), v)),
            // Removed while we were looking: treat as absent.
            Err(libc::ENODATA) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Short printable form of an xattr value.
fn fmt_value(v: &[u8]) -> String {
    const MAX: usize = 32;
    let shown = &v[..v.len().min(MAX)];
    let text = if shown.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("{:?}", String::from_utf8_lossy(shown))
    } else {
        format!("0x{}", shown.iter().map(|b| format!("{b:02x}")).collect::<String>())
    };
    if v.len() > MAX { format!("{text}... ({} bytes)", v.len()) } else { text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const MTIME: i64 = 1_700_000_000;

    /// Sets every mtime below (and including) `dir` to a fixed value,
    /// children first, so that building the same tree twice gives equal trees.
    fn normalize(dir: &Path) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                normalize(&p);
            } else {
                set_mtime(&p);
            }
        }
        set_mtime(dir);
    }

    fn set_mtime(p: &Path) {
        let c = sys::cstr(p.as_os_str().as_bytes()).unwrap();
        let ts = libc::timespec { tv_sec: MTIME, tv_nsec: 0 };
        let times = [ts, ts];
        // SAFETY: valid C string and a two-element timespec array.
        let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        assert_eq!(r, 0, "utimensat {}", p.display());
    }

    fn build(root: &Path) {
        std::fs::create_dir_all(root.join("d1/d2")).unwrap();
        std::fs::write(root.join("a"), b"hello world").unwrap();
        std::fs::write(root.join("d1/b"), vec![7u8; 3 << 20]).unwrap();
        std::fs::write(root.join("d1/d2/c"), b"deep").unwrap();
        std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink("a", root.join("ln")).unwrap();
        symlink("/dangling", root.join("dangling")).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
    }

    /// Two equal normalised trees; the tempdir guards are returned too.
    fn pair() -> (tempfile::TempDir, tempfile::TempDir) {
        let (p, s) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        build(p.path());
        build(s.path());
        normalize(p.path());
        normalize(s.path());
        (p, s)
    }

    fn run(p: &Path, s: &Path) -> VerifyReport {
        verify(p, s, &VerifyOptions { threads: 3, ..Default::default() }, None).unwrap()
    }

    fn has(r: &VerifyReport, path: &str, what: &str) -> bool {
        r.differences.iter().any(|d| d.path == path && d.what == what)
    }

    #[test]
    fn identical_trees() {
        let (p, s) = pair();
        let r = run(p.path(), s.path());
        assert_eq!(r.total_differences, 0, "{:?}", r.differences);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.dirs, 4); // root, d1, d1/d2, empty
        assert_eq!(r.files, 5); // a, d1/b, d1/d2/c, ln, dangling
        assert_eq!(r.bytes_compared, 11 + (3 << 20) + 4);
    }

    #[test]
    fn missing_and_extra_files() {
        let (p, s) = pair();
        std::fs::remove_file(s.path().join("d1/d2/c")).unwrap();
        std::fs::write(s.path().join("extra"), b"x").unwrap();
        std::fs::create_dir(p.path().join("pdir")).unwrap();
        normalize(p.path());
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert!(has(&r, "/d1/d2/c", "only in primary"), "{:?}", r.differences);
        assert!(has(&r, "/extra", "only in secondary"));
        let d = r.differences.iter().find(|d| d.path == "/pdir").unwrap();
        assert_eq!((d.primary.as_str(), d.secondary.as_str()), ("dir", "absent"));
        assert_eq!(r.total_differences, 3);
    }

    #[test]
    fn attribute_differences() {
        let (p, s) = pair();
        std::fs::set_permissions(s.path().join("a"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(s.path().join("d1/d2/c"), b"deeper").unwrap();
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert!(has(&r, "/a", "attr mode"), "{:?}", r.differences);
        assert!(has(&r, "/d1/d2/c", "attr size"));
        // Sizes differ, so content is not compared on top.
        assert!(!has(&r, "/d1/d2/c", "content"));
        // mtime off: the directory time change is ignored.
        let t = std::fs::File::options().write(true).open(s.path().join("a")).unwrap();
        t.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(5)).unwrap();
        let r = run(p.path(), s.path());
        assert!(has(&r, "/a", "attr mtime"));
        let o = VerifyOptions { mtime: false, ..Default::default() };
        let r = verify(p.path(), s.path(), &o, None).unwrap();
        assert!(!r.differences.iter().any(|d| d.what == "attr mtime"));
    }

    #[test]
    fn content_same_size() {
        let (p, s) = pair();
        let mut data = vec![7u8; 3 << 20];
        data[(2 << 20) + 17] = 8; // third chunk
        std::fs::write(s.path().join("d1/b"), &data).unwrap();
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert_eq!(r.total_differences, 1, "{:?}", r.differences);
        let d = &r.differences[0];
        assert_eq!((d.path.as_str(), d.what.as_str()), ("/d1/b", "content"));
        assert!(d.primary.contains(&format!("offset {}", (2 << 20) + 17)), "{}", d.primary);
        let o = VerifyOptions { content: false, ..Default::default() };
        assert_eq!(verify(p.path(), s.path(), &o, None).unwrap().total_differences, 0);
    }

    #[test]
    fn symlink_target_and_type() {
        let (p, s) = pair();
        std::fs::remove_file(s.path().join("ln")).unwrap();
        symlink("d1", s.path().join("ln")).unwrap();
        std::fs::remove_file(s.path().join("dangling")).unwrap();
        std::fs::write(s.path().join("dangling"), b"x").unwrap();
        normalize(s.path());
        let r = run(p.path(), s.path());
        let d = r.differences.iter().find(|d| d.what == "symlink target").unwrap();
        assert_eq!((d.path.as_str(), d.primary.as_str(), d.secondary.as_str()), ("/ln", "a", "d1"));
        let d = r.differences.iter().find(|d| d.path == "/dangling").unwrap();
        assert_eq!(d.what, "attr type");
        assert_eq!((d.primary.as_str(), d.secondary.as_str()), ("symlink", "file"));
        // The symlink size (target length) differs as well.
        assert_eq!(r.total_differences, 3, "{:?}", r.differences);
    }

    #[test]
    fn dir_vs_file() {
        let (p, s) = pair();
        std::fs::remove_dir(s.path().join("empty")).unwrap();
        std::fs::write(s.path().join("empty"), b"").unwrap();
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert!(has(&r, "/empty", "attr type"), "{:?}", r.differences);
        assert_eq!(r.total_differences, 1);
    }

    fn xattr_ok(dir: &Path) -> bool {
        let f = dir.join("a");
        let c = sys::cstr(f.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C strings and value buffer.
        unsafe { libc::setxattr(c.as_ptr(), c"user.t".as_ptr(), b"1".as_ptr().cast(), 1, 0) == 0 }
    }

    fn set_x(p: &Path, name: &CStr, v: &[u8]) {
        let c = sys::cstr(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C strings and value buffer.
        let r = unsafe { libc::setxattr(c.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0) };
        assert_eq!(r, 0, "setxattr");
    }

    #[test]
    fn xattr_differences() {
        let (p, s) = pair();
        if !xattr_ok(p.path()) || !xattr_ok(s.path()) {
            eprintln!("skipping: no user xattr support on the temp file system");
            return;
        }
        // The probe set user.t on both: equal so far.
        assert_eq!(run(p.path(), s.path()).total_differences, 0);
        set_x(&p.path().join("a"), c"user.x", b"one");
        set_x(&s.path().join("a"), c"user.x", b"two");
        set_x(&p.path().join("d1"), c"user.only", b"p");
        set_x(&s.path().join("d1/b"), c"user.only", b"s");
        normalize(p.path());
        normalize(s.path());
        let r = run(p.path(), s.path());
        let d = r.differences.iter().find(|d| d.what == "xattr user.x").unwrap();
        assert_eq!((d.path.as_str(), d.primary.as_str(), d.secondary.as_str()), ("/a", "\"one\"", "\"two\""));
        let d = r.differences.iter().find(|d| d.path == "/d1").unwrap();
        assert_eq!((d.what.as_str(), d.secondary.as_str()), ("xattr user.only", "absent"));
        let d = r.differences.iter().find(|d| d.path == "/d1/b").unwrap();
        assert_eq!(d.primary, "absent");
        assert_eq!(r.total_differences, 3, "{:?}", r.differences);
        let o = VerifyOptions { xattrs: false, ..Default::default() };
        assert_eq!(verify(p.path(), s.path(), &o, None).unwrap().total_differences, 0);
    }

    #[test]
    fn hardlink_vs_copy() {
        let (p, s) = pair();
        for r in [p.path(), s.path()] {
            std::fs::write(r.join("h1"), b"same").unwrap();
        }
        std::fs::hard_link(p.path().join("h1"), p.path().join("h2")).unwrap();
        std::fs::write(s.path().join("h2"), b"same").unwrap();
        normalize(p.path());
        normalize(s.path());
        let r = run(p.path(), s.path());
        let d = r.differences.iter().find(|d| d.what == "hardlink structure").unwrap();
        assert_eq!(d.path, "/h1");
        assert_eq!(d.primary, "linked with [/h1, /h2]");
        assert_eq!(d.secondary, "not linked");
        // nlink differs as an attribute as well.
        assert!(has(&r, "/h1", "attr nlink") && has(&r, "/h2", "attr nlink"));

        // The same structure on both sides is fine, in either direction.
        std::fs::remove_file(s.path().join("h2")).unwrap();
        std::fs::hard_link(s.path().join("h1"), s.path().join("h2")).unwrap();
        normalize(s.path());
        assert_eq!(run(p.path(), s.path()).total_differences, 0);
        let r = run(s.path(), p.path());
        assert_eq!(r.total_differences, 0, "{:?}", r.differences);
    }

    #[test]
    fn hardlink_different_grouping() {
        let (p, s) = pair();
        let link = |r: &Path, a: &str, b: &str| {
            std::fs::write(r.join(a), b"same").unwrap();
            std::fs::hard_link(r.join(a), r.join(b)).unwrap();
        };
        link(p.path(), "g1", "g2");
        link(p.path(), "g3", "g4");
        link(s.path(), "g1", "g3");
        link(s.path(), "g2", "g4");
        normalize(p.path());
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert!(r.differences.iter().any(|d| d.what == "hardlink structure"));
        // One report per mismatching group, de-duplicated by first path.
        assert_eq!(r.differences.iter().filter(|d| d.what == "hardlink structure").count(), 3);
    }

    #[test]
    fn report_cap_keeps_counting() {
        let (p, s) = pair();
        for i in 0..10 {
            std::fs::write(p.path().join(format!("only{i}")), b"x").unwrap();
        }
        normalize(p.path());
        let o = VerifyOptions { max_reports: 3, ..Default::default() };
        let r = verify(p.path(), s.path(), &o, None).unwrap();
        assert_eq!(r.differences.len(), 3);
        assert_eq!(r.total_differences, 10);
    }

    #[test]
    fn unreadable_file_is_an_error() {
        if sys::is_root() {
            eprintln!("skipping: root can read everything");
            return;
        }
        let (p, s) = pair();
        for r in [p.path(), s.path()] {
            std::fs::set_permissions(r.join("a"), std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        normalize(p.path());
        normalize(s.path());
        let r = run(p.path(), s.path());
        assert_eq!(r.total_differences, 0, "{:?}", r.differences);
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(r.errors[0].starts_with("/a: open primary"));
    }

    #[test]
    fn bad_roots_and_progress() {
        let (p, s) = pair();
        assert!(verify(&p.path().join("nope"), s.path(), &VerifyOptions::default(), None).is_err());
        assert!(verify(&p.path().join("a"), s.path(), &VerifyOptions::default(), None).is_err());
        let calls = AtomicU64::new(0);
        let cb = |_: &VerifyReport| {
            calls.fetch_add(1, Ordering::Relaxed);
        };
        let r = verify(p.path(), s.path(), &VerifyOptions::default(), Some(&cb)).unwrap();
        assert_eq!(r.total_differences, 0);
    }

    #[test]
    fn root_attributes_and_symlinked_root() {
        let (p, s) = pair();
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let r = run(p.path(), s.path());
        assert!(has(&r, "/", "attr mode"), "{:?}", r.differences);
        std::fs::set_permissions(s.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(p.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // A root given as a symlink to the tree is resolved.
        let holder = tempfile::tempdir().unwrap();
        symlink(s.path(), holder.path().join("link")).unwrap();
        let r = run(p.path(), &holder.path().join("link"));
        assert_eq!(r.total_differences, 0, "{:?}", r.differences);
    }
}
