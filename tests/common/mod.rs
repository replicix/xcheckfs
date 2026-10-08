//! Shared test harness: an [`Engine`] over two temp directories (optionally with fault injection on either
//! side), path-based helpers that drive the engine the way the kernel would (lookup, open, read, ... by node id
//! and file handle), mismatch assertions, an independent tree comparer and a tiny deterministic PRNG.
//!
//! Everything here is used by several test crates, each of which only uses a part of it.
#![allow(dead_code, unused_imports)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tempfile::TempDir;
use xcheckfs::backend::fault::{Effect, Fault, FaultBackend, FaultId, FaultOp};
use xcheckfs::backend::posix::PosixBackend;
use xcheckfs::backend::{Backend, Lock, TimeSpec, XattrOut};
use xcheckfs::config::{CheckLevel, EngineConfig, MismatchMode};
use xcheckfs::engine::{Attr, Ctx, Engine, ROOT_ID, SetAttr};
use xcheckfs::events::EventSink;
use xcheckfs::policy::{Action, Mismatch, MismatchKind, Policy, Rule};
use xcheckfs::stats::{OpKind, Stats};
use xcheckfs::sys::FileKind;

pub use xcheckfs::backend::fault::{StatLie, Trigger};

/// Base directory for temp trees: tmpfs when available (fast, deterministic), else the system temp dir.
pub fn fast_base() -> PathBuf {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() && tempfile::tempdir_in(shm).is_ok() { shm.to_path_buf() } else { std::env::temp_dir() }
}

pub fn tmp_in(base: &Path) -> TempDir {
    tempfile::Builder::new().prefix("xcheckfs-t-").tempdir_in(base).expect("temp dir")
}

/// `st_dev` of a directory, to find out whether two bases are on the same file system.
pub fn dev_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.dev()).unwrap_or(0)
}

pub fn ctx() -> Ctx {
    // SAFETY: trivial syscalls.
    unsafe { Ctx { uid: libc::geteuid(), gid: libc::getegid(), pid: std::process::id() } }
}

/// Multiplier for the stress tests: `XCHECKFS_SOAK=20 cargo test` runs them 20 times as long to hunt rare races.
pub fn soak() -> usize {
    std::env::var("XCHECKFS_SOAK").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1)
}

pub fn is_root() -> bool {
    // SAFETY: trivial syscall.
    unsafe { libc::geteuid() == 0 }
}

// ---------------------------------------------------------------------------------------------------------------
// PRNG

/// xorshift64*: deterministic, good enough for workloads.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len() as u64) as usize]
    }
    /// Random data of random length in `0..max`.
    pub fn blob(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max) as usize;
        self.bytes(n)
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Deterministic pseudo-random data (cheap, compressible-ish but never all zero).
pub fn pattern(seed: u64, n: usize) -> Vec<u8> {
    Rng::new(seed).bytes(n)
}

// ---------------------------------------------------------------------------------------------------------------
// Harness

type Tweak = Box<dyn FnOnce(&mut EngineConfig)>;

pub struct HarnessBuilder {
    level: CheckLevel,
    mode: MismatchMode,
    rules: Vec<Rule>,
    pbase: PathBuf,
    sbase: PathBuf,
    tweak: Option<Tweak>,
    events: Option<usize>,
    pre_faults: Vec<Fault>,
}

impl HarnessBuilder {
    /// Connects an event channel of this capacity (engine `op_events` is switched on); see `Harness::events`.
    pub fn events(mut self, capacity: usize) -> Self {
        self.events = Some(capacity);
        self
    }
    pub fn level(mut self, l: CheckLevel) -> Self {
        self.level = l;
        self
    }
    pub fn mode(mut self, m: MismatchMode) -> Self {
        self.mode = m;
        self
    }
    pub fn rule(mut self, r: Rule) -> Self {
        self.rules.push(r);
        self
    }
    /// Base directories for the primary and secondary trees (different file systems for cross-FS tests).
    pub fn bases(mut self, p: &Path, s: &Path) -> Self {
        self.pbase = p.to_path_buf();
        self.sbase = s.to_path_buf();
        self
    }
    /// A fault on the secondary from the start: the engine's mount-time probe sees it too (to make the secondary
    /// behave like another kind of file system).
    pub fn secondary_fault(mut self, f: Fault) -> Self {
        self.pre_faults.push(f);
        self
    }
    pub fn config(mut self, f: impl FnOnce(&mut EngineConfig) + 'static) -> Self {
        self.tweak = Some(Box::new(f));
        self
    }
    pub fn build(self) -> Harness {
        self.build_with(|_, _| {})
    }
    /// Builds the harness after `seed(primary_dir, secondary_dir)` pre-populated the trees (e.g. to make the
    /// secondary start out different).
    pub fn build_with(self, seed: impl FnOnce(&Path, &Path)) -> Harness {
        // Like the binary: two descriptors per cached inode exceed small
        // default soft limits (1024) in the bigger workloads.
        static NOFILE: std::sync::Once = std::sync::Once::new();
        NOFILE.call_once(|| {
            xcheckfs::sys::raise_nofile_limit();
        });
        let ptmp = tmp_in(&self.pbase);
        let stmp = tmp_in(&self.sbase);
        seed(ptmp.path(), stmp.path());
        let pbe: Arc<dyn Backend> = Arc::new(PosixBackend::open("primary", ptmp.path()).unwrap());
        let sbe: Arc<dyn Backend> = Arc::new(PosixBackend::open("secondary", stmp.path()).unwrap());
        let pfault = FaultBackend::new(pbe);
        let fault = FaultBackend::new(sbe);
        let mut cfg = EngineConfig { check: self.level, ..EngineConfig::default() };
        // `XCHECKFS_SERIALIZE=strict cargo test` runs every test (that does not choose itself) in strict mode.
        if std::env::var("XCHECKFS_SERIALIZE").is_ok_and(|v| v == "strict") {
            cfg.serialize = xcheckfs::config::Serialization::Strict;
        }
        if let Some(t) = self.tweak {
            t(&mut cfg);
        }
        let stats = Arc::new(Stats::default());
        let (sink, events) = match self.events {
            Some(cap) => {
                cfg.op_events = true;
                let (s, r) = EventSink::channel(cap);
                (s, Some(r))
            }
            None => (EventSink::disabled(), None),
        };
        for f in self.pre_faults {
            fault.add(f);
        }
        let policy = Arc::new(Policy::new(self.mode, self.rules, None, false, sink.clone(), stats.clone()).unwrap());
        let engine = Arc::new(
            Engine::new(cfg, pfault.clone(), fault.clone(), policy.clone(), stats.clone(), sink).unwrap(),
        );
        Engine::spawn_lock_watchdog(&engine);
        Harness { engine, policy, stats, pfault, fault, ptmp, stmp, ctx: ctx(), mark: AtomicUsize::new(0), events }
    }
}

/// An [`Engine`] over two temp trees. Both backends are wrapped in a [`FaultBackend`] (no faults by default):
/// `fault` is the secondary's, `pfault` the primary's.
pub struct Harness {
    pub engine: Arc<Engine>,
    pub policy: Arc<Policy>,
    pub stats: Arc<Stats>,
    pub fault: Arc<FaultBackend>,
    pub pfault: Arc<FaultBackend>,
    pub ptmp: TempDir,
    pub stmp: TempDir,
    pub ctx: Ctx,
    mark: AtomicUsize,
    /// Receiver of the engine/policy event stream, when built with `.events(cap)`.
    pub events: Option<crossbeam_channel::Receiver<xcheckfs::events::UiEvent>>,
}

/// An open file: node id + file handle.
#[derive(Clone, Copy, Debug)]
pub struct Fh {
    pub ino: u64,
    pub fh: u64,
}

/// Where the two trees of a test live: `XCHECKFS_TEST_PRIMARY` and `XCHECKFS_TEST_SECONDARY` (directories, for
/// example on two different file systems), each defaulting to [`fast_base`]. Tests that choose their own bases
/// keep them.
pub fn test_bases() -> (PathBuf, PathBuf) {
    let base = |var: &str| std::env::var_os(var).map(PathBuf::from).unwrap_or_else(fast_base);
    (base("XCHECKFS_TEST_PRIMARY"), base("XCHECKFS_TEST_SECONDARY"))
}

impl Harness {
    pub fn builder() -> HarnessBuilder {
        let (pbase, sbase) = test_bases();
        HarnessBuilder {
            level: CheckLevel::Basic,
            mode: MismatchMode::Log,
            rules: vec![],
            pbase,
            sbase,
            tweak: None,
            events: None,
            pre_faults: Vec::new(),
        }
    }

    pub fn new(level: CheckLevel, mode: MismatchMode) -> Harness {
        Harness::builder().level(level).mode(mode).build()
    }

    pub fn basic() -> Harness {
        Harness::new(CheckLevel::Basic, MismatchMode::Log)
    }
    pub fn thorough() -> Harness {
        Harness::new(CheckLevel::Thorough, MismatchMode::Log)
    }
    pub fn paranoid() -> Harness {
        Harness::new(CheckLevel::Paranoid, MismatchMode::Log)
    }

    pub fn p_root(&self) -> &Path {
        self.ptmp.path()
    }
    pub fn s_root(&self) -> &Path {
        self.stmp.path()
    }
    /// Path of `/a/b` inside the primary / secondary tree (for direct access behind the engine's back).
    pub fn p_path(&self, rel: &str) -> PathBuf {
        self.p_root().join(rel.trim_start_matches('/'))
    }
    pub fn s_path(&self, rel: &str) -> PathBuf {
        self.s_root().join(rel.trim_start_matches('/'))
    }

    // ---- fault shortcuts (secondary)

    pub fn inject(&self, f: Fault) -> FaultId {
        self.fault.add(f)
    }
    pub fn inject_always(&self, op: FaultOp, e: Effect) -> FaultId {
        self.fault.inject(op, e)
    }
    pub fn clear_faults(&self) {
        self.fault.clear();
        self.pfault.clear();
    }

    // ---- mismatches

    pub fn mismatches(&self) -> Vec<Arc<Mismatch>> {
        self.policy.history()
    }

    /// Mismatches reported since the last call to `mark()` (or since the beginning).
    pub fn new_mismatches(&self) -> Vec<Arc<Mismatch>> {
        self.policy.history().into_iter().skip(self.mark.load(Ordering::SeqCst)).collect()
    }

    /// Forgets everything reported so far (assertions only look at later mismatches).
    pub fn mark(&self) {
        self.mark.store(self.policy.history().len(), Ordering::SeqCst);
    }

    pub fn describe_mismatches(&self) -> String {
        let v = self.new_mismatches();
        if v.is_empty() {
            return "no mismatches".into();
        }
        v.iter().map(|m| m.summary()).collect::<Vec<_>>().join("\n  ")
    }

    pub fn assert_no_mismatches(&self) {
        let v = self.new_mismatches();
        assert!(v.is_empty(), "unexpected mismatches ({}):\n  {}", v.len(), self.describe_mismatches());
        assert_eq!(self.stats.mismatches.load(Ordering::SeqCst), self.policy.history().len() as u64, "stats/history");
    }

    /// Asserts that a mismatch of `kind` (and, when given, `field`) was reported; returns it.
    #[track_caller]
    pub fn expect_mismatch(&self, kind: MismatchKind, field: Option<&str>) -> Arc<Mismatch> {
        match self.find_mismatch(kind, field) {
            Some(m) => m,
            None => panic!(
                "expected a {} mismatch (field {:?}), got:\n  {}",
                kind.name(),
                field,
                self.describe_mismatches()
            ),
        }
    }

    pub fn find_mismatch(&self, kind: MismatchKind, field: Option<&str>) -> Option<Arc<Mismatch>> {
        self.new_mismatches().into_iter().find(|m| m.kind == kind && field.is_none_or(|f| m.field.as_deref() == Some(f)))
    }

    /// Like `expect_mismatch`, additionally requiring the op and a path suffix.
    #[track_caller]
    pub fn expect_mismatch_on(&self, kind: MismatchKind, field: Option<&str>, op: OpKind, path_suffix: &str) -> Arc<Mismatch> {
        let found = self.new_mismatches().into_iter().find(|m| {
            m.kind == kind
                && m.op == op
                && field.is_none_or(|f| m.field.as_deref() == Some(f))
                && m.path.ends_with(path_suffix)
        });
        found.unwrap_or_else(|| {
            panic!(
                "expected {} {:?} mismatch in {} on ..{path_suffix}, got:\n  {}",
                kind.name(),
                field,
                op.name(),
                self.describe_mismatches()
            )
        })
    }

    /// Asserts that no mismatch of `kind` was reported (others are allowed).
    #[track_caller]
    pub fn assert_no_mismatch_kind(&self, kind: MismatchKind) {
        let v: Vec<_> = self.new_mismatches().into_iter().filter(|m| m.kind == kind).collect();
        assert!(v.is_empty(), "unexpected {} mismatches:\n  {}", kind.name(), self.describe_mismatches());
    }

    // ---- path resolution

    pub fn split(path: &str) -> (&str, &str) {
        let p = path.trim_end_matches('/');
        match p.rfind('/') {
            Some(i) => (if i == 0 { "/" } else { &p[..i] }, &p[i + 1..]),
            None => ("/", p),
        }
    }

    pub fn try_lookup(&self, path: &str) -> Result<Attr, i32> {
        let mut cur = Attr { id: ROOT_ID, st: Default::default() };
        let mut first = true;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            cur = self.engine.lookup(&self.ctx, cur.id, OsStr::new(comp))?;
            first = false;
        }
        if first {
            return self.engine.getattr(&self.ctx, ROOT_ID);
        }
        Ok(cur)
    }

    #[track_caller]
    pub fn lookup(&self, path: &str) -> Attr {
        self.try_lookup(path).unwrap_or_else(|e| panic!("lookup {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn exists(&self, path: &str) -> bool {
        self.try_lookup(path).is_ok()
    }

    fn parent_of(&self, path: &str) -> Result<(u64, OsString), i32> {
        let (dir, name) = Harness::split(path);
        Ok((self.try_lookup(dir)?.id, OsString::from(name)))
    }

    pub fn getattr(&self, path: &str) -> Attr {
        let a = self.lookup(path);
        self.engine.getattr(&self.ctx, a.id).unwrap_or_else(|e| panic!("getattr {path}: {e}"))
    }

    // ---- namespace operations

    pub fn try_mkdir(&self, path: &str, mode: u32) -> Result<Attr, i32> {
        let (p, n) = self.parent_of(path)?;
        self.engine.mkdir(&self.ctx, p, &n, mode)
    }
    #[track_caller]
    pub fn mkdir(&self, path: &str) -> Attr {
        self.try_mkdir(path, 0o755).unwrap_or_else(|e| panic!("mkdir {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }
    /// `mkdir -p`
    pub fn mkdir_p(&self, path: &str) {
        let mut cur = String::new();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur.push('/');
            cur.push_str(c);
            if !self.exists(&cur) {
                self.mkdir(&cur);
            }
        }
    }

    pub fn try_rmdir(&self, path: &str) -> Result<(), i32> {
        let (p, n) = self.parent_of(path)?;
        self.engine.rmdir(&self.ctx, p, &n)
    }
    pub fn try_unlink(&self, path: &str) -> Result<(), i32> {
        let (p, n) = self.parent_of(path)?;
        self.engine.unlink(&self.ctx, p, &n)
    }
    #[track_caller]
    pub fn unlink(&self, path: &str) {
        self.try_unlink(path).unwrap_or_else(|e| panic!("unlink {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }
    #[track_caller]
    pub fn rmdir(&self, path: &str) {
        self.try_rmdir(path).unwrap_or_else(|e| panic!("rmdir {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn try_rename_flags(&self, from: &str, to: &str, flags: u32) -> Result<(), i32> {
        let (p, n) = self.parent_of(from)?;
        let (np, nn) = self.parent_of(to)?;
        self.engine.rename(&self.ctx, p, &n, np, &nn, flags)
    }
    pub fn try_rename(&self, from: &str, to: &str) -> Result<(), i32> {
        self.try_rename_flags(from, to, 0)
    }
    #[track_caller]
    pub fn rename(&self, from: &str, to: &str) {
        self.try_rename(from, to).unwrap_or_else(|e| panic!("rename {from} {to}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn try_link(&self, from: &str, to: &str) -> Result<Attr, i32> {
        let src = self.try_lookup(from)?;
        let (np, nn) = self.parent_of(to)?;
        self.engine.link(&self.ctx, src.id, np, &nn)
    }
    #[track_caller]
    pub fn link(&self, from: &str, to: &str) -> Attr {
        self.try_link(from, to).unwrap_or_else(|e| panic!("link {from} {to}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn try_symlink(&self, target: &str, path: &str) -> Result<Attr, i32> {
        let (p, n) = self.parent_of(path)?;
        self.engine.symlink(&self.ctx, p, &n, OsStr::new(target))
    }
    #[track_caller]
    pub fn symlink(&self, target: &str, path: &str) -> Attr {
        self.try_symlink(target, path).unwrap_or_else(|e| panic!("symlink {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }
    pub fn readlink(&self, path: &str) -> Result<Vec<u8>, i32> {
        let a = self.try_lookup(path)?;
        self.engine.readlink(&self.ctx, a.id)
    }

    pub fn mknod_fifo(&self, path: &str) -> Result<Attr, i32> {
        let (p, n) = self.parent_of(path)?;
        self.engine.mknod(&self.ctx, p, &n, libc::S_IFIFO | 0o644, 0)
    }

    pub fn setattr(&self, path: &str, a: SetAttr) -> Result<Attr, i32> {
        let n = self.try_lookup(path)?;
        self.engine.setattr(&self.ctx, n.id, a, None)
    }
    #[track_caller]
    pub fn chmod(&self, path: &str, mode: u32) -> Attr {
        self.setattr(path, SetAttr { mode: Some(mode), ..Default::default() }).expect("chmod")
    }
    pub fn truncate(&self, path: &str, size: u64) -> Result<Attr, i32> {
        self.setattr(path, SetAttr { size: Some(size), ..Default::default() })
    }
    pub fn utimes(&self, path: &str, atime: i64, mtime: i64) -> Result<Attr, i32> {
        use xcheckfs::sys::Ts;
        self.setattr(
            path,
            SetAttr {
                atime: Some(TimeSpec::Set(Ts { sec: atime, nsec: 0 })),
                mtime: Some(TimeSpec::Set(Ts { sec: mtime, nsec: 0 })),
                ..Default::default()
            },
        )
    }

    /// Sets only the mtime (atime untouched). Stress workloads use this: an explicit atime is immediately
    /// disturbed by any outside reader of the file (indexers, backup agents), which makes "atime == what I set"
    /// checks inherently racy against processes we do not control.
    pub fn set_mtime(&self, path: &str, mtime: i64) -> Result<Attr, i32> {
        use xcheckfs::sys::Ts;
        self.setattr(path, SetAttr { mtime: Some(TimeSpec::Set(Ts { sec: mtime, nsec: 0 })), ..Default::default() })
    }

    pub fn readdir(&self, path: &str) -> Vec<String> {
        let a = self.lookup(path);
        let fh = self.engine.opendir(&self.ctx, a.id).unwrap_or_else(|e| panic!("opendir {path}: {e}"));
        let mut names = Vec::new();
        let mut off = 0;
        loop {
            let mut n = 0;
            self.engine
                .readdir(&self.ctx, a.id, fh, off, &mut |_, next, _, name| {
                    names.push(String::from_utf8_lossy(name).into_owned());
                    off = next;
                    n += 1;
                    false
                })
                .unwrap_or_else(|e| panic!("readdir {path}: {e}"));
            if n == 0 {
                break;
            }
        }
        self.engine.releasedir(&self.ctx, a.id, fh).unwrap();
        names.retain(|n| n != "." && n != "..");
        names.sort();
        names
    }

    /// `rm -rf` through the engine.
    #[track_caller]
    pub fn rmdir_tree(&self, p: &str) {
        for n in self.readdir(p) {
            let c = format!("{p}/{n}");
            if self.getattr(&c).st.mode & libc::S_IFMT == libc::S_IFDIR {
                self.rmdir_tree(&c);
            } else {
                self.unlink(&c);
            }
        }
        self.rmdir(p);
    }

    // ---- files

    pub fn try_open(&self, path: &str, flags: i32) -> Result<Fh, i32> {
        let a = self.try_lookup(path)?;
        let fh = self.engine.open(&self.ctx, a.id, flags)?;
        Ok(Fh { ino: a.id, fh })
    }
    #[track_caller]
    pub fn open(&self, path: &str, flags: i32) -> Fh {
        self.try_open(path, flags).unwrap_or_else(|e| panic!("open {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn try_create(&self, path: &str, mode: u32, flags: i32) -> Result<Fh, i32> {
        let (p, n) = self.parent_of(path)?;
        let (a, fh) = self.engine.create(&self.ctx, p, &n, mode, flags | libc::O_RDWR)?;
        Ok(Fh { ino: a.id, fh })
    }
    #[track_caller]
    pub fn create(&self, path: &str) -> Fh {
        self.try_create(path, 0o644, libc::O_EXCL).unwrap_or_else(|e| panic!("create {path}: {}", xcheckfs::sys::fmt_errno(e)))
    }

    pub fn close(&self, f: Fh) {
        self.engine.flush(&self.ctx, f.ino, f.fh, f.fh).expect("flush");
        self.engine.release(&self.ctx, f.ino, f.fh).expect("release");
    }

    pub fn try_pwrite(&self, f: Fh, off: u64, data: &[u8]) -> Result<usize, i32> {
        let mut done = 0;
        while done < data.len() {
            let n = self.engine.write(&self.ctx, f.ino, f.fh, off + done as u64, &data[done..])? as usize;
            if n == 0 {
                break;
            }
            done += n;
        }
        Ok(done)
    }
    #[track_caller]
    pub fn pwrite(&self, f: Fh, off: u64, data: &[u8]) {
        let n = self.try_pwrite(f, off, data).unwrap_or_else(|e| panic!("write: {}", xcheckfs::sys::fmt_errno(e)));
        assert_eq!(n, data.len(), "short write");
    }

    pub fn try_pread(&self, f: Fh, off: u64, len: usize) -> Result<Vec<u8>, i32> {
        let mut out = Vec::new();
        self.engine.read(&self.ctx, f.ino, f.fh, off, len as u32, &mut |d| out.extend_from_slice(d))?;
        Ok(out)
    }
    #[track_caller]
    pub fn pread(&self, f: Fh, off: u64, len: usize) -> Vec<u8> {
        self.try_pread(f, off, len).unwrap_or_else(|e| panic!("read: {}", xcheckfs::sys::fmt_errno(e)))
    }

    /// Creates (or truncates) `path` and writes `data` in 64 KiB pieces.
    #[track_caller]
    pub fn write_file(&self, path: &str, data: &[u8]) -> Attr {
        // Like the kernel: open(O_TRUNC) when the name exists, create otherwise.
        let f = match self.try_open(path, libc::O_WRONLY | libc::O_TRUNC) {
            Ok(f) => f,
            Err(libc::ENOENT) => match self.try_create(path, 0o644, 0) {
                Ok(f) => f,
                Err(e) => panic!("write_file {path}: create: {}", xcheckfs::sys::fmt_errno(e)),
            },
            Err(e) => panic!("write_file {path}: open: {}", xcheckfs::sys::fmt_errno(e)),
        };
        for (i, chunk) in data.chunks(64 << 10).enumerate() {
            self.pwrite(f, (i * (64 << 10)) as u64, chunk);
        }
        self.close(f);
        self.lookup(path)
    }

    #[track_caller]
    pub fn append(&self, path: &str, data: &[u8]) {
        let f = self.open(path, libc::O_WRONLY | libc::O_APPEND);
        // offset is ignored for O_APPEND
        self.pwrite(f, 0, data);
        self.close(f);
    }

    #[track_caller]
    pub fn read_file(&self, path: &str) -> Vec<u8> {
        let f = self.open(path, libc::O_RDONLY);
        let mut out = Vec::new();
        loop {
            let d = self.pread(f, out.len() as u64, 128 << 10);
            if d.is_empty() {
                break;
            }
            out.extend_from_slice(&d);
        }
        self.close(f);
        out
    }

    // ---- xattrs

    pub fn setxattr(&self, path: &str, name: &str, value: &[u8]) -> Result<(), i32> {
        let a = self.try_lookup(path)?;
        self.engine.setxattr(&self.ctx, a.id, OsStr::new(name), value, 0)
    }
    pub fn getxattr(&self, path: &str, name: &str) -> Result<Vec<u8>, i32> {
        let a = self.try_lookup(path)?;
        match self.engine.getxattr(&self.ctx, a.id, OsStr::new(name), 65536)? {
            XattrOut::Data(d) => Ok(d),
            XattrOut::Size(n) => Ok(vec![0; n]),
        }
    }
    /// The `user.*` names only: hosts add their own (e.g. `security.selinux`).
    pub fn user_xattrs(&self, path: &str) -> Result<Vec<String>, i32> {
        Ok(self.listxattr(path)?.into_iter().filter(|n| n.starts_with("user.")).collect())
    }

    pub fn listxattr(&self, path: &str) -> Result<Vec<String>, i32> {
        let a = self.try_lookup(path)?;
        match self.engine.listxattr(&self.ctx, a.id, 65536)? {
            XattrOut::Data(d) => {
                let mut v: Vec<String> =
                    d.split(|&b| b == 0).filter(|n| !n.is_empty()).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
                v.sort();
                Ok(v)
            }
            XattrOut::Size(_) => Ok(vec![]),
        }
    }
    pub fn removexattr(&self, path: &str, name: &str) -> Result<(), i32> {
        let a = self.try_lookup(path)?;
        self.engine.removexattr(&self.ctx, a.id, OsStr::new(name))
    }

    // ---- locks

    pub fn getlk(&self, ino: u64, owner: u64, typ: i32, start: u64, len: u64) -> Result<Lock, i32> {
        self.engine.getlk(&self.ctx, ino, owner, Lock { typ, start, len, pid: 0 })
    }

    /// Non-blocking lock request; the reply is returned synchronously.
    pub fn setlk(&self, ino: u64, owner: u64, typ: i32, start: u64, len: u64) -> Result<(), i32> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.engine.setlk(
            &self.ctx,
            ino,
            0, // no file handle: owners live until flushed
            owner,
            Lock { typ, start, len, pid: 0 },
            false,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.recv_timeout(Duration::from_secs(10)).expect("setlk reply")
    }

    /// Blocking lock request; the receiver yields the reply when the lock is granted (or the request fails).
    pub fn setlkw(&self, ino: u64, owner: u64, typ: i32, start: u64, len: u64) -> std::sync::mpsc::Receiver<Result<(), i32>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.engine.setlk(
            &self.ctx,
            ino,
            0, // no file handle: owners live until flushed
            owner,
            Lock { typ, start, len, pid: 0 },
            true,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx
    }

    // ---- policy helpers

    /// Waits until a mismatch is pending (freeze mode) and returns its id.
    pub fn wait_pending(&self, timeout: Duration) -> u64 {
        let t0 = std::time::Instant::now();
        loop {
            if let Some(p) = self.policy.pending().first() {
                return p.mismatch.id;
            }
            assert!(t0.elapsed() < timeout, "no mismatch became pending");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn resolve_when_pending(self: &Arc<Self>, action: Action) -> std::thread::JoinHandle<()> {
        let h = self.clone();
        std::thread::spawn(move || {
            let id = h.wait_pending(Duration::from_secs(10));
            assert!(h.policy.resolve(id, action));
        })
    }

    /// Compares the two trees behind the engine's back (independent of the engine's own checks).
    pub fn tree_diff(&self) -> Vec<String> {
        tree_diff(self.p_root(), self.s_root(), &TreeOpts::default())
    }

    #[track_caller]
    pub fn assert_trees_equal(&self) {
        let d = self.tree_diff();
        assert!(d.is_empty(), "trees differ:\n  {}", d.join("\n  "));
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Raw xattr helpers on paths (no symlink following)

pub fn raw_setxattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let p = CString::new(path.as_os_str().as_bytes()).unwrap();
    let n = CString::new(name).unwrap();
    // SAFETY: valid C strings and buffer.
    let r = unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), value.as_ptr() as *const _, value.len(), 0) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

pub fn raw_getxattr(path: &Path, name: &str) -> std::io::Result<Vec<u8>> {
    let p = CString::new(path.as_os_str().as_bytes()).unwrap();
    let n = CString::new(name).unwrap();
    let mut buf = vec![0u8; 65536];
    // SAFETY: valid C strings and buffer.
    let r = unsafe { libc::lgetxattr(p.as_ptr(), n.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    buf.truncate(r as usize);
    Ok(buf)
}

pub fn raw_listxattr(path: &Path) -> std::io::Result<Vec<String>> {
    let p = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut buf = vec![0u8; 65536];
    // SAFETY: valid C string and buffer.
    let r = unsafe { libc::llistxattr(p.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len()) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    buf.truncate(r as usize);
    let mut v: Vec<String> =
        buf.split(|&b| b == 0).filter(|n| !n.is_empty()).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    v.sort();
    Ok(v)
}

/// The `user.*` names of [`raw_listxattr`]: hosts add their own (e.g.
/// `security.selinux` under SELinux).
pub fn raw_user_xattrs(path: &Path) -> std::io::Result<Vec<String>> {
    Ok(raw_listxattr(path)?.into_iter().filter(|n| n.starts_with("user.")).collect())
}

/// Whether `user.*` xattrs work in this directory.
pub fn xattrs_supported(dir: &Path) -> bool {
    let probe = dir.join(".xattr-probe");
    if std::fs::write(&probe, b"x").is_err() {
        return false;
    }
    let ok = raw_setxattr(&probe, "user.probe", b"1").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

// ---------------------------------------------------------------------------------------------------------------
// Independent tree comparison

#[derive(Clone, Debug)]
pub struct TreeOpts {
    pub content: bool,
    pub xattrs: bool,
    /// Compare mtimes with this tolerance (None: ignore).
    pub mtime: Option<Duration>,
    pub dir_nlink: bool,
    pub owner: bool,
}

impl Default for TreeOpts {
    fn default() -> Self {
        TreeOpts { content: true, xattrs: true, mtime: Some(Duration::from_secs(2)), dir_nlink: true, owner: true }
    }
}

struct Entry {
    md: std::fs::Metadata,
}

fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
    let dir = root.join(rel);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    for e in rd.flatten() {
        let r = rel.join(e.file_name());
        let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
        let is_dir = md.is_dir();
        out.insert(r.clone(), Entry { md });
        if is_dir {
            walk(root, &r, out);
        }
    }
}

fn hardlink_groups(t: &BTreeMap<PathBuf, Entry>) -> BTreeSet<Vec<PathBuf>> {
    let mut m: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
    for (p, e) in t {
        if !e.md.is_dir() && e.md.nlink() > 1 {
            m.entry((e.md.dev(), e.md.ino())).or_default().push(p.clone());
        }
    }
    m.into_values().map(|mut v| {
        v.sort();
        v
    })
    .collect()
}

/// Compares two directory trees; returns human-readable differences (empty = identical).
pub fn tree_diff(a: &Path, b: &Path, o: &TreeOpts) -> Vec<String> {
    let (mut ta, mut tb) = (BTreeMap::new(), BTreeMap::new());
    walk(a, Path::new(""), &mut ta);
    walk(b, Path::new(""), &mut tb);
    let mut d = Vec::new();
    for (p, ea) in &ta {
        let Some(eb) = tb.get(p) else {
            d.push(format!("/{}: only in primary", p.display()));
            continue;
        };
        let (ma, mb) = (&ea.md, &eb.md);
        let name = format!("/{}", p.display());
        if ma.file_type() != mb.file_type() {
            d.push(format!("{name}: type differs ({:?} vs {:?})", ma.file_type(), mb.file_type()));
            continue;
        }
        let ft = ma.file_type();
        if ft.is_symlink() {
            let (la, lb) = (std::fs::read_link(a.join(p)), std::fs::read_link(b.join(p)));
            if la.ok() != lb.ok() {
                d.push(format!("{name}: symlink target differs"));
            }
            continue;
        }
        if ma.mode() & 0o7777 != mb.mode() & 0o7777 {
            d.push(format!("{name}: mode {:o} vs {:o}", ma.mode() & 0o7777, mb.mode() & 0o7777));
        }
        if o.owner && (ma.uid() != mb.uid() || ma.gid() != mb.gid()) {
            d.push(format!("{name}: owner {}:{} vs {}:{}", ma.uid(), ma.gid(), mb.uid(), mb.gid()));
        }
        if ft.is_file() && ma.size() != mb.size() {
            d.push(format!("{name}: size {} vs {}", ma.size(), mb.size()));
        }
        if (!ft.is_dir() || o.dir_nlink) && ma.nlink() != mb.nlink() {
            d.push(format!("{name}: nlink {} vs {}", ma.nlink(), mb.nlink()));
        }
        if let Some(tol) = o.mtime
            && (ma.mtime() - mb.mtime()).unsigned_abs() > tol.as_secs() {
                d.push(format!("{name}: mtime {} vs {}", ma.mtime(), mb.mtime()));
            }
        if (ft.is_char_device() || ft.is_block_device()) && ma.rdev() != mb.rdev() {
            d.push(format!("{name}: rdev differs"));
        }
        if ft.is_file() && o.content && ma.size() == mb.size() {
            match (std::fs::read(a.join(p)), std::fs::read(b.join(p))) {
                (Ok(x), Ok(y)) if x == y => {}
                (Ok(x), Ok(y)) => {
                    let i = x.iter().zip(&y).position(|(p, q)| p != q).unwrap_or(0);
                    d.push(format!("{name}: content differs (first at {i})"));
                }
                (x, y) => d.push(format!("{name}: unreadable ({} / {})", x.is_ok(), y.is_ok())),
            }
        }
        if o.xattrs && !ft.is_symlink() {
            let (xa, xb) = (raw_listxattr(&a.join(p)).unwrap_or_default(), raw_listxattr(&b.join(p)).unwrap_or_default());
            let user = |v: &Vec<String>| v.iter().filter(|n| n.starts_with("user.")).cloned().collect::<Vec<_>>();
            if user(&xa) != user(&xb) {
                d.push(format!("{name}: xattr names {:?} vs {:?}", user(&xa), user(&xb)));
            } else {
                for n in user(&xa) {
                    if raw_getxattr(&a.join(p), &n).ok() != raw_getxattr(&b.join(p), &n).ok() {
                        d.push(format!("{name}: xattr {n} differs"));
                    }
                }
            }
        }
    }
    for p in tb.keys() {
        if !ta.contains_key(p) {
            d.push(format!("/{}: only in secondary", p.display()));
        }
    }
    let (ga, gb) = (hardlink_groups(&ta), hardlink_groups(&tb));
    if ga != gb {
        d.push(format!("hard-link structure differs: {:?} vs {:?}", ga, gb));
    }
    d
}

pub fn os(s: &str) -> OsString {
    OsString::from_vec(s.as_bytes().to_vec())
}
