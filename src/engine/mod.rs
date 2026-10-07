//! The lockstep engine.
//!
//! Every operation:
//!
//! 1. passes the freeze gate,
//! 2. takes the stripe locks of *every object whose observable state it
//!    reads or changes*, all at once and in stripe order (so no deadlocks),
//! 3. runs the operation on the primary and the secondary (concurrently by
//!    default),
//! 4. compares the results while still holding the locks, optionally reads
//!    the effect back (thorough mode), and reports mismatches to the policy,
//! 5. returns the primary's result.
//!
//! Because conflicting operations hold the same stripe locks while both file
//! systems execute them, both file systems see conflicting operations in the
//! same order, and no other operation can observe one file system "between"
//! the two halves of an operation. That is what makes comparisons free of
//! false positives under concurrency.
//!
//! The engine is independent of FUSE so the test suite can drive it directly.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::backend::{Backend, DirEntry};
use crate::compare::{self, AttrRules};
use crate::config::{CheckLevel, EngineConfig};
use crate::events::{EventSink, OpEvent, UiEvent};
use crate::policy::{Mismatch, MismatchKind, Policy, Verdict};
use crate::stats::{OpKind, Stats};
use crate::sys::{self, Creds, CredsGuard, FileKind, Stat, SysResult};

pub mod locks;
pub mod node;
mod ops;
mod resync;

pub use node::Node;
use node::NodeTable;

pub const ROOT_ID: u64 = 1;

/// Caller identity of a request.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

impl Ctx {
    pub fn root() -> Ctx {
        Ctx::default()
    }
}

/// Attributes as returned to the kernel: our node id plus the primary's stat.
#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub id: u64,
    pub st: Stat,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<crate::backend::TimeSpec>,
    pub mtime: Option<crate::backend::TimeSpec>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Primary,
    Secondary,
}

/// Internal control flow of an operation body.
pub(crate) enum Fail {
    Errno(i32),
    /// Re-run the (read-only) operation.
    Retry,
}

impl From<i32> for Fail {
    fn from(e: i32) -> Fail {
        Fail::Errno(e)
    }
}

pub(crate) type OpResult<T> = Result<T, Fail>;

/// Per-operation bookkeeping.
pub(crate) struct Cx {
    op: OpKind,
    pns: u64,
    sns: u64,
    /// Widest execution window of a dual call in this operation: from the
    /// common start to the later of the two halves' completion. Each file
    /// system stamps times somewhere inside it.
    window_ns: u64,
    /// Repairs requested by the policy, run after the operation's locks
    /// are released and before the reply.
    pub(crate) resync: Vec<(resync::ResyncReq, String)>,
    /// Additional repair to queue with the next resyncable mismatch (the
    /// target name of a rename).
    pub(crate) resync_extra: Option<resync::ResyncReq>,
    sec_errno: Option<i32>,
    mismatch: bool,
    bytes: u64,
}

pub struct OpenFile {
    pub node: Arc<Node>,
    /// The node's secondary generation when the handle was opened.
    pub sec_gen: u64,
    pub pfd: std::os::fd::OwnedFd,
    pub sfd: Option<std::os::fd::OwnedFd>,
    pub flags: i32,
    pub written: std::sync::atomic::AtomicBool,
}

/// (name, ino, kind) of one directory snapshot entry.
pub type DirSnapEntry = (Vec<u8>, u64, FileKind);

pub struct OpenDir {
    pub node: Arc<Node>,
    pub sec_gen: u64,
    pub pfd: std::os::fd::OwnedFd,
    pub sfd: Option<std::os::fd::OwnedFd>,
    /// Snapshot taken at offset 0: (name, ino, kind), "." and ".." first.
    pub entries: Mutex<Option<Arc<Vec<DirSnapEntry>>>>,
}

impl OpenFile {
    /// The handle has a secondary descriptor, and it still belongs to the
    /// node's current secondary object (resync may have replaced it).
    pub fn has_sec(&self) -> bool {
        self.sfd.is_some() && self.sec_gen == self.node.sec_gen()
    }

    pub fn fd(&self, side: Side) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        match side {
            Side::Primary => self.pfd.as_fd(),
            Side::Secondary => self.sfd.as_ref().expect("secondary file fd").as_fd(),
        }
    }
}

impl OpenDir {
    pub fn has_sec(&self) -> bool {
        self.sfd.is_some() && self.sec_gen == self.node.sec_gen()
    }

    pub fn fd(&self, side: Side) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        match side {
            Side::Primary => self.pfd.as_fd(),
            Side::Secondary => self.sfd.as_ref().expect("secondary dir fd").as_fd(),
        }
    }
}

struct Handles<T> {
    shards: Vec<RwLock<HashMap<u64, Arc<T>>>>,
}

impl<T> Handles<T> {
    fn new() -> Self {
        Handles { shards: (0..16).map(|_| RwLock::new(HashMap::new())).collect() }
    }
    fn insert(&self, fh: u64, v: Arc<T>) {
        self.shards[(fh % 16) as usize].write().insert(fh, v);
    }
    fn get(&self, fh: u64) -> Option<Arc<T>> {
        self.shards[(fh % 16) as usize].read().get(&fh).cloned()
    }
    fn remove(&self, fh: u64) -> Option<Arc<T>> {
        self.shards[(fh % 16) as usize].write().remove(&fh)
    }
}

enum Guard<'a> {
    #[allow(dead_code)]
    R(RwLockReadGuard<'a, ()>),
    #[allow(dead_code)]
    W(RwLockWriteGuard<'a, ()>),
}

pub(crate) struct LockSet<'a> {
    _g: Vec<Guard<'a>>,
}

/// Simple pool of I/O buffers so reads don't allocate.
struct BufPool {
    bufs: Mutex<Vec<Vec<u8>>>,
}

impl BufPool {
    fn get(&self, len: usize) -> Vec<u8> {
        let mut b = self.bufs.lock().pop().unwrap_or_default();
        b.resize(len, 0);
        b
    }
    fn put(&self, mut b: Vec<u8>) {
        if b.capacity() <= 4 << 20 {
            b.clear();
            let mut g = self.bufs.lock();
            if g.len() < 256 {
                g.push(b);
            }
        }
    }
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub(crate) b: [Arc<dyn Backend>; 2],
    pub policy: Arc<Policy>,
    pub stats: Arc<Stats>,
    events: EventSink,
    nodes: NodeTable,
    files: Handles<OpenFile>,
    dirs: Handles<OpenDir>,
    next_fh: AtomicU64,
    stripes: Box<[RwLock<()>]>,
    stripe_mask: usize,
    /// Primary root inode number (swapped with 1 in the id space).
    root_ino: u64,
    root_dev: u64,
    primary_root: PathBuf,
    pool: Option<rayon::ThreadPool>,
    use_creds: bool,
    groups: Mutex<HashMap<u32, (Instant, Vec<u32>)>>,
    bufs: BufPool,
    attr_rules: AttrRules,
    slack: Slack,
    repairs: resync::Repairs,
    quarantine_seq: AtomicU64,
    /// Shadow of granted record locks and blocked requests (deadlock detection).
    pub(crate) lock_graph: Mutex<locks::WaitGraph>,
}

impl Engine {
    pub fn new(
        cfg: EngineConfig,
        primary: Arc<dyn Backend>,
        secondary: Arc<dyn Backend>,
        policy: Arc<Policy>,
        stats: Arc<Stats>,
        events: EventSink,
    ) -> anyhow::Result<Engine> {
        let pfd = primary.root().map_err(|e| anyhow::anyhow!("primary root: {}", sys::fmt_errno(e)))?;
        let sfd = secondary.root().map_err(|e| anyhow::anyhow!("secondary root: {}", sys::fmt_errno(e)))?;
        let pst = primary.stat(std::os::fd::AsFd::as_fd(&pfd)).map_err(|e| anyhow::anyhow!("{e}"))?;
        let sst = secondary.stat(std::os::fd::AsFd::as_fd(&sfd)).map_err(|e| anyhow::anyhow!("{e}"))?;
        if pst.kind() != FileKind::Dir || sst.kind() != FileKind::Dir {
            anyhow::bail!("both roots must be directories");
        }
        let primary_root = sys::fd_path(std::os::fd::AsFd::as_fd(&pfd)).unwrap_or_default();
        let n = cfg.lock_stripes.max(16).next_power_of_two();
        let pool = if cfg.parallel {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .thread_name(|i| format!("xcheckfs-sec-{i}"))
                    .num_threads(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 32))
                    .build()?,
            )
        } else {
            None
        };
        let use_creds = cfg.creds && sys::is_root();
        // btrfs always reports a link count of 1 for directories: comparing
        // directory link counts against it only produces noise.
        let btrfs = [&pfd, &sfd].iter().any(|fd| sys::fs_magic(std::os::fd::AsFd::as_fd(*fd)) == Some(sys::BTRFS_MAGIC));
        if btrfs && cfg.dir_nlink {
            tracing::info!("a btrfs side reports directory link counts as 1: not comparing them");
        }
        let attr_rules =
            AttrRules { time_tolerance: cfg.time_tolerance, dir_nlink: cfg.dir_nlink && !btrfs, mtime: true };
        let root = Node::new(
            ROOT_ID,
            FileKind::Dir,
            pfd,
            Some(sfd),
            pst.ident(),
            Some(sst.ident()),
            Arc::from(""),
        );
        let nodes = NodeTable::default();
        let root = match nodes.insert_or_get(Arc::new(root)) {
            node::Inserted::New(n) | node::Inserted::Existing(n) => n,
        };
        *root.ctimes.lock() = Some((pst.ctime, sst.ctime));
        let e = Engine {
            cfg,
            b: [primary, secondary],
            policy,
            stats,
            events,
            nodes,
            files: Handles::new(),
            dirs: Handles::new(),
            next_fh: AtomicU64::new(1),
            stripes: (0..n).map(|_| RwLock::new(())).collect(),
            stripe_mask: n - 1,
            root_ino: pst.ino,
            root_dev: pst.dev,
            primary_root,
            pool,
            use_creds,
            groups: Mutex::new(HashMap::new()),
            bufs: BufPool { bufs: Mutex::new(Vec::new()) },
            attr_rules,
            slack: Slack::default(),
            repairs: resync::Repairs::default(),
            quarantine_seq: AtomicU64::new(0),
            lock_graph: Mutex::new(locks::WaitGraph::default()),
        };
        e.stats.nodes.store(1, Relaxed);
        // Compare the roots once; differences are reported like any other.
        let diffs = compare::diff_stat(&pst, &sst, &e.attr_rules);
        let mut cx = e.cx(OpKind::Getattr);
        for d in diffs {
            let _ = e.report(&mut cx, &root, MismatchKind::Attr, Some(d.field.into()), d.primary, d.secondary, "mount root".into(), false);
        }
        // (the repairs the policy asked for, like at the end of an operation)
        for (r, why) in std::mem::take(&mut cx.resync) {
            e.resync(r, &why);
        }
        Ok(e)
    }

    pub fn primary(&self) -> &dyn Backend {
        &*self.b[0]
    }

    pub fn secondary(&self) -> &dyn Backend {
        &*self.b[1]
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Maps a primary inode number to a node id (root <-> 1 swap).
    pub fn map_ino(&self, ino: u64) -> u64 {
        if ino == self.root_ino {
            ROOT_ID
        } else if ino == ROOT_ID {
            self.root_ino
        } else {
            ino
        }
    }

    pub(crate) fn node(&self, id: u64) -> OpResult<Arc<Node>> {
        self.nodes.get(id).ok_or(Fail::Errno(libc::ESTALE))
    }

    pub(crate) fn file(&self, fh: u64) -> OpResult<Arc<OpenFile>> {
        self.files.get(fh).ok_or(Fail::Errno(libc::EBADF))
    }

    pub(crate) fn dir(&self, fh: u64) -> OpResult<Arc<OpenDir>> {
        self.dirs.get(fh).ok_or(Fail::Errno(libc::EBADF))
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Relaxed)
    }

    fn detached(&self) -> bool {
        self.stats.detached.load(Relaxed)
    }

    /// Whether the secondary half should run for an operation touching these
    /// nodes. Counts skipped halves.
    pub(crate) fn sec_for(&self, nodes: &[&Node]) -> bool {
        if self.detached() {
            return false;
        }
        if nodes.iter().all(|n| n.has_sec()) {
            true
        } else {
            self.stats.secondary_skipped.fetch_add(1, Relaxed);
            false
        }
    }

    pub(crate) fn slack_of(&self, id: u64) -> u64 {
        self.slack.get(id)
    }

    /// Marks an object created by the current operation as changed by it.
    pub(crate) fn touched_new(id: u64) {
        TOUCHED.with(|t| t.borrow_mut().push(id));
    }

    pub(crate) fn thorough(&self) -> bool {
        self.cfg.check >= CheckLevel::Thorough && !self.detached()
    }

    pub(crate) fn paranoid(&self) -> bool {
        self.cfg.check >= CheckLevel::Paranoid && !self.detached()
    }

    // ---------------------------------------------------------------- locks

    fn stripe(&self, id: u64) -> usize {
        ((id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 29) as usize) & self.stripe_mask
    }

    /// Acquires the stripes of all `(id, exclusive)` pairs at once, in
    /// stripe order. Duplicate stripes are merged (exclusive wins).
    pub(crate) fn lock(&self, ids: &[(u64, bool)]) -> LockSet<'_> {
        TOUCHED.with(|t| t.borrow_mut().extend(ids.iter().filter(|(_, x)| *x).map(|(id, _)| *id)));
        let mut v: Vec<(usize, bool)> = ids.iter().map(|&(id, x)| (self.stripe(id), x)).collect();
        v.sort_unstable();
        let mut merged: Vec<(usize, bool)> = Vec::with_capacity(v.len());
        for (s, x) in v {
            match merged.last_mut() {
                Some(last) if last.0 == s => last.1 |= x,
                _ => merged.push((s, x)),
            }
        }
        LockSet {
            _g: merged
                .into_iter()
                .map(|(s, x)| {
                    if x { Guard::W(self.stripes[s].write()) } else { Guard::R(self.stripes[s].read()) }
                })
                .collect(),
        }
    }

    // ---------------------------------------------------------------- creds

    pub(crate) fn creds(&self, ctx: &Ctx) -> Option<Creds> {
        if !self.use_creds || (ctx.uid == 0 && ctx.gid == 0) {
            return None;
        }
        let groups = {
            let mut g = self.groups.lock();
            let now = Instant::now();
            match g.get(&ctx.pid) {
                Some((t, v)) if now.duration_since(*t).as_secs() < 2 => v.clone(),
                _ => {
                    if g.len() > 4096 {
                        g.clear();
                    }
                    let v = sys::proc_groups(ctx.pid).unwrap_or_default();
                    g.insert(ctx.pid, (now, v.clone()));
                    v
                }
            }
        };
        Some(Creds { uid: ctx.uid, gid: ctx.gid, groups })
    }

    // ------------------------------------------------------- dual execution

    /// Runs `f` on the primary and (if `sec`) the secondary, concurrently
    /// when configured. Records per-side latencies.
    pub(crate) fn both<U: Send>(
        &self,
        cx: &mut Cx,
        sec: bool,
        creds: Option<&Creds>,
        f: impl Fn(Side, &dyn Backend) -> SysResult<U> + Sync,
    ) -> (SysResult<U>, Option<SysResult<U>>) {
        let start = Instant::now();
        let run = |side: Side| -> (SysResult<U>, u64, u64) {
            let t0 = Instant::now();
            let _g = match creds {
                Some(c) => match CredsGuard::switch(c) {
                    Ok(g) => g,
                    Err(e) => return (Err(e), 0, 0),
                },
                None => CredsGuard::none(),
            };
            let be = match side {
                Side::Primary => &*self.b[0],
                Side::Secondary => &*self.b[1],
            };
            let r = f(side, be);
            (r, t0.elapsed().as_nanos() as u64, start.elapsed().as_nanos() as u64)
        };
        let (p, s) = if !sec {
            (run(Side::Primary), None)
        } else if let Some(pool) = &self.pool {
            let mut s = None;
            let mut p = None;
            pool.in_place_scope(|sc| {
                sc.spawn(|_| s = Some(run(Side::Secondary)));
                p = Some(run(Side::Primary));
            });
            (p.unwrap(), s)
        } else {
            let p = run(Side::Primary);
            (p, Some(run(Side::Secondary)))
        };
        cx.pns += p.1;
        cx.window_ns = cx.window_ns.max(p.2);
        let st = self.stats.op(cx.op);
        st.primary.record(p.1);
        let s = s.map(|(r, ns, end)| {
            cx.window_ns = cx.window_ns.max(end);
            cx.sns += ns;
            st.secondary.record(ns);
            cx.sec_errno = Some(r.as_ref().err().copied().unwrap_or(0));
            r
        });
        (p.0, s)
    }

    /// Runs the secondary half alone, after the primary's (for operations
    /// whose secondary half depends on the primary's result).
    pub(crate) fn secondary_only<U>(&self, cx: &mut Cx, f: impl FnOnce(&dyn Backend) -> SysResult<U>) -> SysResult<U> {
        let t0 = Instant::now();
        let r = f(&*self.b[1]);
        let ns = t0.elapsed().as_nanos() as u64;
        cx.sns += ns;
        cx.window_ns = cx.window_ns.max(ns + cx.pns);
        self.stats.op(cx.op).secondary.record(ns);
        cx.sec_errno = Some(r.as_ref().err().copied().unwrap_or(0));
        r
    }

    /// The ranges of a file pair that hold data on either side (the union
    /// of both sides' `SEEK_DATA`/`SEEK_HOLE` extents, up to `size`).
    /// Holes on both sides read as zeros on both, so only these ranges need
    /// comparing or copying: a terabyte-sized sparse file costs what its data
    /// costs. Without `SEEK_DATA` support the whole range is returned.
    pub(crate) fn data_ranges(&self, pf: std::os::fd::BorrowedFd<'_>, sf: std::os::fd::BorrowedFd<'_>, size: u64) -> Vec<(u64, u64)> {
        let extents = |be: &dyn Backend, fd: std::os::fd::BorrowedFd<'_>| -> Vec<(u64, u64)> {
            let mut v = Vec::new();
            let mut off = 0u64;
            while off < size {
                let d = match be.lseek(fd, off as i64, libc::SEEK_DATA) {
                    Ok(d) => d as u64,
                    Err(libc::ENXIO) => break,
                    Err(_) => return vec![(0, size)],
                };
                if d >= size {
                    break;
                }
                let h = be.lseek(fd, d as i64, libc::SEEK_HOLE).map(|h| h as u64).unwrap_or(size).min(size);
                if h <= d || v.len() >= 1 << 16 {
                    return vec![(0, size)];
                }
                v.push((d, h));
                off = h;
            }
            v
        };
        let mut all = extents(&*self.b[0], pf);
        all.extend(extents(&*self.b[1], sf));
        all.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(all.len());
        for (a, b) in all {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        merged
    }

    // ------------------------------------------------------------ reporting

    pub(crate) fn cx(&self, op: OpKind) -> Cx {
        Cx { op, pns: 0, sns: 0, window_ns: 0, resync: Vec::new(), resync_extra: None, sec_errno: None, mismatch: false, bytes: 0 }
    }

    /// Current path of a node relative to the mount root.
    pub fn path_of(&self, n: &Node) -> String {
        if n.id == ROOT_ID {
            return "/".into();
        }
        match sys::fd_path(n.p()) {
            Some(p) => match p.strip_prefix(&self.primary_root) {
                Ok(rel) => format!("/{}", rel.display()),
                Err(_) => p.display().to_string(),
            },
            None => format!("{}", n.hint()),
        }
    }

    pub(crate) fn child_path(&self, parent: &Node, name: &OsStr) -> String {
        let p = self.path_of(parent);
        if p == "/" {
            format!("/{}", name.to_string_lossy())
        } else {
            format!("{p}/{}", name.to_string_lossy())
        }
    }

    pub(crate) fn child_hint(parent: &Node, name: &OsStr) -> Arc<str> {
        Arc::from(format!("{}/{}", parent.hint(), name.to_string_lossy()))
    }

    /// Reports a mismatch on `node`. `excl` says whether the caller holds
    /// the node's stripe exclusively (resync can then run in place).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn report(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        kind: MismatchKind,
        field: Option<String>,
        primary: String,
        secondary: String,
        detail: String,
        excl: bool,
    ) -> OpResult<()> {
        self.report_at(cx, node, None, kind, field, primary, secondary, detail, excl)
    }

    /// Like [`Engine::report`], for a mismatch about `name` inside `node`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn report_at(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        name: Option<&OsStr>,
        kind: MismatchKind,
        field: Option<String>,
        primary: String,
        secondary: String,
        detail: String,
        _excl: bool,
    ) -> OpResult<()> {
        cx.mismatch = true;
        let read_only = is_read_only(cx.op);
        let req = if self.detached() || !node.has_sec() {
            None
        } else {
            match (name, kind) {
                // Lock tables, seek offsets and capacities are not object state.
                (_, MismatchKind::Lock) => None,
                (None, _) if matches!(cx.op, OpKind::Lseek | OpKind::Statfs) => None,
                (Some(n), _) => Some(resync::ResyncReq::entry(node, n)),
                (None, MismatchKind::Readdir) => Some(resync::ResyncReq::Dir(node.clone())),
                (None, _) => Some(resync::ResyncReq::Object(node.clone())),
            }
        };
        let resyncable = req.is_some();
        if !resyncable {
            tracing::debug!(
                "mismatch on node {} is not repairable (detached {}, has_sec {}, op {:?}, kind {:?})",
                node.id,
                self.detached(),
                node.has_sec(),
                cx.op,
                kind
            );
        }
        let path = match name {
            Some(n) => self.child_path(node, n),
            None => self.path_of(node),
        };
        let m = Mismatch {
            id: 0,
            time: SystemTime::now(),
            op: cx.op,
            kind,
            ino: node.id,
            path,
            field,
            primary,
            secondary,
            detail,
            retryable: read_only,
            resyncable,
        };
        let reason = m.describe();
        match self.policy.report(m) {
            Verdict::Continue => Ok(()),
            Verdict::Fail => Err(Fail::Errno(libc::EIO)),
            Verdict::Retry => Err(Fail::Retry),
            Verdict::Resync => {
                for r in req.into_iter().chain(cx.resync_extra.take()) {
                    if !cx.resync.iter_mut().any(|(x, _)| x.absorb(&r)) {
                        cx.resync.push((r, reason.clone()));
                    }
                }
                Ok(())
            }
        }
    }

    /// Compares two results by errno (0 = success).
    pub(crate) fn cmp_result<T>(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        name: Option<&OsStr>,
        p: &SysResult<T>,
        s: &Option<SysResult<T>>,
        excl: bool,
    ) -> OpResult<()> {
        let Some(s) = s else { return Ok(()) };
        let pe = p.as_ref().err().copied().unwrap_or(0);
        let se = s.as_ref().err().copied().unwrap_or(0);
        if pe != se {
            self.report_at(
                cx,
                node,
                name,
                MismatchKind::Result,
                None,
                sys::errno_name(pe).into(),
                sys::errno_name(se).into(),
                String::new(),
                excl,
            )?;
        }
        Ok(())
    }

    /// Compares two stats of `node` and tracks ctime changes.
    pub(crate) fn cmp_stat(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        p: &Stat,
        s: &Stat,
        excl: bool,
        what: &str,
    ) -> OpResult<()> {
        let mut rules = self.attr_rules;
        rules.time_tolerance += Duration::from_nanos(cx.window_ns.max(self.slack_of(node.id)));
        for d in compare::diff_stat(p, s, &rules) {
            self.report(cx, node, MismatchKind::Attr, Some(d.field.into()), d.primary, d.secondary, what.into(), excl)?;
        }
        let prev = node.ctimes.lock().replace((p.ctime, s.ctime));
        // POSIX does not require a ctime update when the last link is removed (tmpfs does it, ZFS does not):
        // the ctime of a file without links is not comparable. The baseline above is still refreshed.
        let unlinked = p.nlink == 0 && s.nlink == 0;
        if let (Some(prev), false) = (prev, unlinked)
            && let Some(d) = compare::ctime_change(prev, (p.ctime, s.ctime), rules.time_tolerance) {
                self.report(
                    cx,
                    node,
                    MismatchKind::Attr,
                    Some("ctime".into()),
                    d.primary,
                    d.secondary,
                    "ctime changed on one side only".into(),
                    excl,
                )?;
            }
        Ok(())
    }

    pub(crate) fn cmp_dir(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        p: &[DirEntry],
        s: &[DirEntry],
        what: &str,
    ) -> OpResult<()> {
        let d = compare::diff_dir(p, s);
        if !d.is_empty() {
            self.report(
                cx,
                node,
                MismatchKind::Readdir,
                None,
                format!("{} entries", p.len()),
                format!("{} entries", s.len()),
                format!("{what}: {}", d.describe(8)),
                false,
            )?;
        }
        Ok(())
    }

    /// Wraps an operation: gate, retry/resync loop, statistics and events.
    pub(crate) fn run<T>(
        &self,
        op: OpKind,
        ino: u64,
        detail: impl FnOnce() -> String,
        mut body: impl FnMut(&mut Cx) -> OpResult<T>,
    ) -> Result<T, i32> {
        self.policy.gate();
        let t0 = Instant::now();
        let want_events = self.cfg.op_events && self.events.enabled();
        let detail = if want_events { detail() } else { String::new() };
        let inflight = want_events.then(|| self.stats.inflight.begin(op, ino, detail.clone()));
        let mut cx = self.cx(op);
        TOUCHED.with(|t| t.borrow_mut().clear());
        let res = loop {
            match body(&mut cx) {
                Err(Fail::Retry) => continue,
                Err(Fail::Errno(e)) => break Err(e),
                Ok(v) => break Ok(v),
            }
        };
        // Repairs run with their own locks, after the operation released its
        // locks; the primary's result is returned unchanged.
        for (r, why) in std::mem::take(&mut cx.resync) {
            self.resync(r, &why);
        }
        let total = t0.elapsed().as_nanos() as u64;
        if !is_read_only(op) && cx.window_ns > self.cfg.time_tolerance.as_nanos() as u64 / 10 {
            TOUCHED.with(|t| self.slack.record(&t.borrow(), cx.window_ns));
        }
        let st = self.stats.op(op);
        st.count.fetch_add(1, Relaxed);
        st.total.record(total);
        st.bytes.fetch_add(cx.bytes, Relaxed);
        let errno = res.as_ref().err().copied().unwrap_or(0);
        if errno != 0 {
            st.errors.fetch_add(1, Relaxed);
        }
        if let Some(id) = inflight {
            self.stats.inflight.end(id);
        }
        if want_events {
            self.events.send(
                UiEvent::Op(OpEvent {
                    time: SystemTime::now(),
                    op,
                    ino,
                    detail,
                    errno,
                    sec_errno: cx.sec_errno,
                    total_ns: total,
                    primary_ns: cx.pns,
                    secondary_ns: cx.sns,
                    bytes: cx.bytes,
                    mismatch: cx.mismatch,
                }),
                &self.stats,
            );
        }
        tracing::trace!(
            op = op.name(),
            ino,
            errno,
            sec = ?cx.sec_errno,
            ns = total,
            mismatch = cx.mismatch,
            "op"
        );
        res
    }
}

thread_local! {
    /// Objects the current operation locked exclusively (i.e. may change).
    pub(crate) static TOUCHED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Per-object extra timestamp tolerance.
///
/// Both file systems stamp mtime/ctime somewhere inside the execution window
/// of the operation that changed the object; when one half is delayed (a
/// slow experimental file system, a saturated pool), the stamps can be
/// further apart than `--time-tolerance`. Windows wider than a tenth of the
/// tolerance are remembered per object id, also after the kernel forgets
/// the node. The map is bounded: on overflow its maximum becomes a global
/// floor, which only ever loosens checks, never causes false positives.
#[derive(Default)]
struct Slack {
    any: std::sync::atomic::AtomicBool,
    floor: AtomicU64,
    map: RwLock<HashMap<u64, u64>>,
}

const SLACK_CAP: usize = 1 << 20;

impl Slack {
    fn record(&self, ids: &[u64], ns: u64) {
        if ids.is_empty() {
            return;
        }
        let mut m = self.map.write();
        if m.len() + ids.len() > SLACK_CAP {
            let max = m.values().copied().max().unwrap_or(0).max(ns);
            self.floor.fetch_max(max, Relaxed);
            m.clear();
        }
        for id in ids {
            let e = m.entry(*id).or_insert(0);
            *e = (*e).max(ns);
        }
        self.any.store(true, Relaxed);
    }

    fn get(&self, id: u64) -> u64 {
        let floor = self.floor.load(Relaxed);
        if !self.any.load(Relaxed) {
            return floor;
        }
        self.map.read().get(&id).copied().unwrap_or(0).max(floor)
    }
}

pub(crate) fn is_read_only(op: OpKind) -> bool {
    matches!(
        op,
        OpKind::Lookup
            | OpKind::Getattr
            | OpKind::Readlink
            | OpKind::Read
            | OpKind::Readdir
            | OpKind::Getxattr
            | OpKind::Listxattr
            | OpKind::Access
            | OpKind::Statfs
            | OpKind::Lseek
            | OpKind::Getlk
    )
}

pub(crate) fn cname(name: &OsStr) -> OpResult<CString> {
    Ok(sys::cstr(name.as_bytes())?)
}
