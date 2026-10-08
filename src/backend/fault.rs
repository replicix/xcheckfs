//! Fault-injecting backend wrapper for the test suite.
//!
//! [`FaultBackend`] forwards every [`Backend`] method to an inner backend and
//! consults a runtime-changeable list of [`Fault`]s on the way. A fault says
//! *which* trait method ([`FaultOp`]), optionally *which name / which file*,
//! *when* ([`Trigger`]) and *what* to do ([`Effect`]). Typical use:
//!
//! ```ignore
//! let fb = FaultBackend::new(Arc::new(PosixBackend::open("secondary", dir)?));
//! fb.add(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 3 }).path("/f").once());
//! // ... run the workload through the engine, then:
//! fb.clear();
//! ```
//!
//! Two more tools for concurrency tests: an in-flight recorder ([`FaultBackend::record`]: per method and object,
//! how many calls were running at once) and [`Effect::Gate`] (a call blocks until the test opens a [`Gate`]),
//! which together prove that, and force how, the halves of engine operations interleave.
//!
//! Effects only make sense for some methods (a `ShortRead` on `unlink` is
//! meaningless); an effect that does not apply to the method it fires on is
//! ignored. Faults are evaluated in the order they were added and all firing
//! faults apply (so `Delay` can be combined with another effect).

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};

use super::{Backend, DirEntry, Lock, StatFs, TimeSpec, XattrOut};
use crate::sys::{self, FileKind, Stat, SysResult, Ts};

macro_rules! fault_ops {
    ($($v:ident),* $(,)?) => {
        /// The [`Backend`] method a fault applies to.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum FaultOp { $($v),* }
        impl FaultOp {
            pub const ALL: &'static [FaultOp] = &[$(FaultOp::$v),*];
        }
    };
}

fault_ops! {
    Root, Lookup, StatAt, Stat, Chmod, Chown, Truncate, Utimens, Readlink, Mknod, Mkdir, Unlink, Rmdir,
    Symlink, Rename, Link, Open, Create, Pread, Pwrite, Flush, Fsync, Opendir, Readdir, Statfs, Setxattr,
    Getxattr, Listxattr, Removexattr, Access, Fallocate, Lseek, CopyFileRange, Getlk, Setlk,
}

/// When a matching call triggers the fault. Calls are counted per fault and
/// only calls that match the fault's op / name / path are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Every matching call.
    Always,
    /// Only the first matching call.
    Once,
    /// Only the n-th matching call (1-based).
    Nth(u64),
    /// Every matching call after the first `n` ones.
    After(u64),
    /// Every n-th matching call.
    Every(u64),
}

impl Trigger {
    fn fires(&self, n: u64) -> bool {
        match *self {
            Trigger::Always => true,
            Trigger::Once => n == 1,
            Trigger::Nth(k) => n == k,
            Trigger::After(k) => n > k,
            Trigger::Every(k) => k > 0 && n.is_multiple_of(k),
        }
    }
}

/// How `stat`/`lookup`/`stat_at` results are falsified.
#[derive(Clone)]
pub enum StatLie {
    /// Report this size.
    Size(u64),
    /// Replace the permission bits (file type is kept).
    Mode(u32),
    /// Report this link count.
    Nlink(u64),
    Uid(u32),
    Gid(u32),
    /// Shift mtime by this many seconds.
    MtimeShift(i64),
    /// Shift ctime by this many seconds.
    CtimeShift(i64),
    /// Change the reported object type, e.g. `S_IFLNK` (permission bits are kept).
    Type(u32),
    /// Arbitrary modification.
    Custom(Arc<dyn Fn(&mut Stat) + Send + Sync>),
}

impl StatLie {
    pub fn apply(&self, st: &mut Stat) {
        let shift = |t: &mut Ts, secs: i64| t.sec += secs;
        match self {
            StatLie::Size(s) => st.size = *s,
            StatLie::Mode(m) => st.mode = (st.mode & libc::S_IFMT) | (m & 0o7777),
            StatLie::Nlink(n) => st.nlink = *n,
            StatLie::Uid(u) => st.uid = *u,
            StatLie::Gid(g) => st.gid = *g,
            StatLie::MtimeShift(s) => shift(&mut st.mtime, *s),
            StatLie::CtimeShift(s) => shift(&mut st.ctime, *s),
            StatLie::Type(t) => st.mode = (t & libc::S_IFMT) | (st.mode & 0o7777),
            StatLie::Custom(f) => f(st),
        }
    }
}

impl std::fmt::Debug for StatLie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatLie::Size(v) => write!(f, "Size({v})"),
            StatLie::Mode(v) => write!(f, "Mode({v:o})"),
            StatLie::Nlink(v) => write!(f, "Nlink({v})"),
            StatLie::Uid(v) => write!(f, "Uid({v})"),
            StatLie::Gid(v) => write!(f, "Gid({v})"),
            StatLie::MtimeShift(v) => write!(f, "MtimeShift({v})"),
            StatLie::CtimeShift(v) => write!(f, "CtimeShift({v})"),
            StatLie::Type(v) => write!(f, "Type({v:o})"),
            StatLie::Custom(_) => write!(f, "Custom"),
        }
    }
}


/// A latch that holds the calls it is attached to ([`Effect::Gate`]) until the test opens it. Event-driven: the
/// test waits for calls to *arrive* ([`Gate::wait_arrived`]) and for released calls to *finish*
/// ([`Gate::wait_done`]), so interleavings of the two halves of engine operations can be forced without sleeping.
/// A call never waits longer than `timeout` (30 s by default): it then proceeds and [`Gate::timed_out`] says so, so
/// that a broken test fails instead of hanging.
pub struct Gate {
    st: Mutex<GateState>,
    cv: Condvar,
    timeout: Duration,
}

#[derive(Default)]
struct GateState {
    open: bool,
    /// Calls that reached the gate (blocked or not).
    arrived: u64,
    /// Calls that were let through and have finished the call to the inner backend.
    done: u64,
    timed_out: bool,
}

impl Gate {
    /// A closed gate.
    pub fn new() -> Arc<Gate> {
        Gate::with_timeout(Duration::from_secs(30))
    }

    pub fn with_timeout(timeout: Duration) -> Arc<Gate> {
        Arc::new(Gate { st: Mutex::new(GateState::default()), cv: Condvar::new(), timeout })
    }

    /// Lets every waiting and every future call through.
    pub fn open(&self) {
        self.st.lock().open = true;
        self.cv.notify_all();
    }

    pub fn close(&self) {
        self.st.lock().open = false;
    }

    pub fn is_open(&self) -> bool {
        self.st.lock().open
    }

    pub fn arrived(&self) -> u64 {
        self.st.lock().arrived
    }

    pub fn done(&self) -> u64 {
        self.st.lock().done
    }

    /// Whether any call gave up waiting for the gate.
    pub fn timed_out(&self) -> bool {
        self.st.lock().timed_out
    }

    /// Waits until `n` calls have arrived; false on timeout.
    pub fn wait_arrived(&self, n: u64, timeout: Duration) -> bool {
        self.wait_for(timeout, |s| s.arrived >= n)
    }

    /// Waits until `n` calls have been let through and finished; false on timeout.
    pub fn wait_done(&self, n: u64, timeout: Duration) -> bool {
        self.wait_for(timeout, |s| s.done >= n)
    }

    fn wait_for(&self, timeout: Duration, cond: impl Fn(&GateState) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.st.lock();
        while !cond(&st) {
            if self.cv.wait_until(&mut st, deadline).timed_out() {
                return cond(&st);
            }
        }
        true
    }

    fn pass(self: &Arc<Gate>) -> GateTicket {
        let mut st = self.st.lock();
        st.arrived += 1;
        self.cv.notify_all();
        let deadline = Instant::now() + self.timeout;
        while !st.open {
            if self.cv.wait_until(&mut st, deadline).timed_out() {
                st.timed_out = true;
                break;
            }
        }
        GateTicket(self.clone())
    }
}

/// Counts the call as finished when dropped.
struct GateTicket(Arc<Gate>);

impl Drop for GateTicket {
    fn drop(&mut self) {
        self.0.st.lock().done += 1;
        self.0.cv.notify_all();
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.st.lock();
        write!(f, "Gate(open={}, arrived={}, done={})", st.open, st.arrived, st.done)
    }
}

/// Identity of a file system object: `(st_dev, st_ino)`.
pub type ObjId = (u64, u64);

/// `(st_dev, st_ino)` of a path (following symlinks), to name the object to [`FaultBackend::max_inflight`].
pub fn obj_of(path: &std::path::Path) -> ObjId {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).unwrap_or_else(|e| panic!("obj_of {}: {e}", path.display()));
    (md.dev(), md.ino())
}

#[derive(Default, Clone, Copy)]
struct Level {
    cur: u32,
    max: u32,
    total: u64,
}

impl Level {
    fn enter(&mut self) {
        self.cur += 1;
        self.total += 1;
        self.max = self.max.max(self.cur);
    }
}

/// Calls in flight, per method and object (and per method over all objects).
#[derive(Default)]
struct Recorder {
    on: AtomicBool,
    st: Mutex<RecState>,
    cv: Condvar,
}

#[derive(Default)]
struct RecState {
    by_obj: HashMap<(FaultOp, ObjId), Level>,
    by_op: HashMap<FaultOp, Level>,
}

/// Leaves the in-flight record when dropped.
struct RecGuard<'a> {
    rec: &'a Recorder,
    op: FaultOp,
    objs: Vec<ObjId>,
}

impl Drop for RecGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.rec.st.lock();
        for o in &self.objs {
            if let Some(l) = st.by_obj.get_mut(&(self.op, *o)) {
                l.cur -= 1;
            }
        }
        if let Some(l) = st.by_op.get_mut(&self.op) {
            l.cur -= 1;
        }
        self.rec.cv.notify_all();
    }
}

impl Recorder {
    /// Enters `op` on the objects behind `fds`: counted once for the method and once for every distinct object.
    fn enter(&self, inner: &dyn Backend, op: FaultOp, fds: &[BorrowedFd<'_>]) -> Option<RecGuard<'_>> {
        if !self.on.load(Relaxed) {
            return None;
        }
        let mut objs: Vec<ObjId> = Vec::new();
        for fd in fds {
            if let Ok(st) = inner.stat(*fd) {
                let o = (st.dev, st.ino);
                if !objs.contains(&o) {
                    objs.push(o);
                }
            }
        }
        let mut st = self.st.lock();
        for o in &objs {
            st.by_obj.entry((op, *o)).or_default().enter();
        }
        st.by_op.entry(op).or_default().enter();
        self.cv.notify_all();
        Some(RecGuard { rec: self, op, objs })
    }
}

/// What a firing fault does.
#[derive(Clone, Debug)]
pub enum Effect {
    // ---- generic (any method)
    /// Return this errno without calling the inner backend.
    Errno(i32),
    /// Call the inner backend (the operation IS applied), then return this errno instead of its result.
    AppliedThenErrno(i32),
    /// Report success without doing anything ("silently not applied"): unlink, rmdir, rename, chmod, chown,
    /// truncate, utimens, mknod, mkdir, symlink, link, setxattr, removexattr, fallocate, flush, fsync,
    /// pwrite (full length reported), copy_file_range (full length reported).
    Skip,
    /// Sleep this long before executing the operation.
    Delay(Duration),
    /// Hold the call (before it does anything, but after it was counted as in flight) until the test opens the
    /// [`Gate`]; combine with a [`Trigger`] to hold only some calls.
    Gate(Arc<Gate>),

    // ---- data
    /// pread: flip all bits of the returned byte at `offset` (if within the returned data).
    CorruptRead { offset: usize },
    /// pread: return at most this many bytes (a real short read).
    ShortRead(usize),
    /// pwrite / copy_file_range: perform and report at most this many bytes (a real short write).
    ShortWrite(usize),
    /// pwrite: report success, write nothing.
    DropWrite,
    /// pwrite: flip all bits of the byte at `offset` of the data before writing it, report success.
    CorruptWrite { offset: usize },
    /// pwrite: write the data `k` bytes further on (or earlier, when negative) than asked, report success for the
    /// asked offset (a file system that mishandles an offset).
    ShiftOffset(i64),
    /// pwrite, truncate, fallocate: the call happens, but atime/mtime are put back to what they were before it (a
    /// file system that does not update mtime there). The kernel stamps ctime when the times are restored, so ctime
    /// still moves. The save-write-restore sequences of concurrent writes are serialized (the effect would
    /// otherwise not be well-defined); the calls still overlap as far as the recorder is concerned.
    RestoreTimes,
    /// rename: a directory that moved to another parent (both, for `RENAME_EXCHANGE`) gets its mtime set to now
    /// (a file system that stamps it, which POSIX allows).
    StampMtime,

    // ---- attributes
    /// stat / stat_at / lookup: falsify the returned attributes.
    Stat(StatLie),

    // ---- directories and links
    /// readdir: hide the entry with this name.
    DropEntry(Vec<u8>),
    /// readdir: add a bogus entry (regular file) with this name.
    AddEntry(Vec<u8>),
    /// readdir: report this entry name with a different file type.
    ChangeEntryKind(Vec<u8>, FileKind),
    /// readlink: return this target.
    ReadlinkTarget(Vec<u8>),

    // ---- xattrs
    /// getxattr: return this value (size probes return its length).
    XattrValue(Vec<u8>),
    /// listxattr: append this name to the list.
    XattrListAdd(Vec<u8>),
    /// listxattr: remove this name from the list.
    XattrListDrop(Vec<u8>),

    // ---- locks and offsets
    /// setlk: every lock/unlock request succeeds without being taken (lock tables stay empty).
    LockAlwaysOk,
    /// setlk: every lock request (not unlock) fails with EAGAIN without calling through.
    LockAlwaysConflict,
    /// getlk: always report "no conflicting lock".
    GetlkFree,
    /// getlk: report this lock instead of the real answer.
    GetlkResult(Lock),
    /// lseek: return this offset.
    Offset(i64),
}

/// A fault specification: op + optional filters + trigger + effect.
#[derive(Clone, Debug)]
pub struct Fault {
    pub op: FaultOp,
    /// For name-based methods (lookup, unlink, rename (either name), create, getxattr, ...).
    pub name: Option<Vec<u8>>,
    /// Suffix of the path of the object the call works on: the file/directory descriptor, joined with the name for
    /// name-based calls. `"/d/f"` matches `unlink(dir=/d, "f")` and `pread(fd of /d/f)`.
    pub path: Option<Vec<u8>>,
    pub trigger: Trigger,
    pub effect: Effect,
}

impl Fault {
    pub fn new(op: FaultOp, effect: Effect) -> Fault {
        Fault { op, name: None, path: None, trigger: Trigger::Always, effect }
    }
    pub fn name(mut self, n: impl AsRef<[u8]>) -> Fault {
        self.name = Some(n.as_ref().to_vec());
        self
    }
    pub fn path(mut self, p: impl AsRef<[u8]>) -> Fault {
        self.path = Some(p.as_ref().to_vec());
        self
    }
    pub fn trigger(mut self, t: Trigger) -> Fault {
        self.trigger = t;
        self
    }
    pub fn once(self) -> Fault {
        self.trigger(Trigger::Once)
    }
    pub fn nth(self, n: u64) -> Fault {
        self.trigger(Trigger::Nth(n))
    }
    pub fn after(self, n: u64) -> Fault {
        self.trigger(Trigger::After(n))
    }
    pub fn every(self, n: u64) -> Fault {
        self.trigger(Trigger::Every(n))
    }
}

/// Handle of an added fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultId(pub u64);

struct Entry {
    id: u64,
    fault: Fault,
    matched: AtomicU64,
    hits: AtomicU64,
}

/// The fault-injecting wrapper. Share it as `Arc<FaultBackend>`; hand clones coerced to `Arc<dyn Backend>` to
/// the engine and keep the typed `Arc` to change faults while the engine runs.
pub struct FaultBackend {
    inner: Arc<dyn Backend>,
    label: String,
    faults: RwLock<Vec<Arc<Entry>>>,
    next_id: AtomicU64,
    calls: Vec<AtomicU64>,
    rec: Recorder,
    times_lock: Mutex<()>,
}

/// The effects that fired for one call.
struct Fx(Vec<Effect>);

impl Fx {
    fn none(&self) -> bool {
        self.0.is_empty()
    }
    fn delay(&self) {
        for e in &self.0 {
            if let Effect::Delay(d) = e {
                std::thread::sleep(*d);
            }
        }
    }
    fn gates(&self) -> impl Iterator<Item = &Arc<Gate>> {
        self.0.iter().filter_map(|e| if let Effect::Gate(g) = e { Some(g) } else { None })
    }
    fn errno_before(&self) -> Option<i32> {
        self.0.iter().find_map(|e| if let Effect::Errno(n) = e { Some(*n) } else { None })
    }
    fn errno_after(&self) -> Option<i32> {
        self.0.iter().find_map(|e| if let Effect::AppliedThenErrno(n) = e { Some(*n) } else { None })
    }
    fn skip(&self) -> bool {
        self.0.iter().any(|e| matches!(e, Effect::Skip | Effect::DropWrite))
    }
    fn has(&self, f: impl Fn(&Effect) -> bool) -> bool {
        self.0.iter().any(f)
    }
    fn stat_lies(&self) -> impl Iterator<Item = &StatLie> {
        self.0.iter().filter_map(|e| if let Effect::Stat(l) = e { Some(l) } else { None })
    }
}

impl FaultBackend {
    pub fn new(inner: Arc<dyn Backend>) -> Arc<FaultBackend> {
        let label = format!("fault({})", inner.label());
        Arc::new(FaultBackend {
            inner,
            label,
            faults: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(1),
            calls: FaultOp::ALL.iter().map(|_| AtomicU64::new(0)).collect(),
            rec: Recorder::default(),
            times_lock: Mutex::new(()),
        })
    }

    pub fn inner(&self) -> &Arc<dyn Backend> {
        &self.inner
    }

    /// Adds a fault; it is active immediately, also for calls already running concurrently.
    pub fn add(&self, f: Fault) -> FaultId {
        let id = self.next_id.fetch_add(1, Relaxed);
        self.faults.write().push(Arc::new(Entry { id, fault: f, matched: AtomicU64::new(0), hits: AtomicU64::new(0) }));
        FaultId(id)
    }

    /// Shorthand: `add(Fault::new(op, effect))` (fires on every call).
    pub fn inject(&self, op: FaultOp, effect: Effect) -> FaultId {
        self.add(Fault::new(op, effect))
    }

    /// Removes one fault; returns how often it fired.
    pub fn remove(&self, id: FaultId) -> u64 {
        let mut g = self.faults.write();
        match g.iter().position(|e| e.id == id.0) {
            Some(i) => g.remove(i).hits.load(Relaxed),
            None => 0,
        }
    }

    /// Removes all faults.
    pub fn clear(&self) {
        self.faults.write().clear();
    }

    /// How often a fault fired (0 if it was removed).
    pub fn hits(&self, id: FaultId) -> u64 {
        self.faults.read().iter().find(|e| e.id == id.0).map(|e| e.hits.load(Relaxed)).unwrap_or(0)
    }

    /// Number of calls to a method (whether or not a fault fired), since creation or `reset_calls`.
    pub fn calls(&self, op: FaultOp) -> u64 {
        self.calls[op as usize].load(Relaxed)
    }

    pub fn total_calls(&self) -> u64 {
        self.calls.iter().map(|c| c.load(Relaxed)).sum()
    }

    pub fn reset_calls(&self) {
        for c in &self.calls {
            c.store(0, Relaxed);
        }
    }

    /// Switches the in-flight recorder on or off. While on, every call is counted as in flight from its entry
    /// (before any gate or delay) to its return, per method and per object (`fstat` of the descriptor), see
    /// [`FaultBackend::max_inflight`]. Costs one `fstat` per call.
    pub fn record(&self, on: bool) {
        self.rec.on.store(on, Relaxed);
    }

    /// Calls of `op` on `obj` right now.
    pub fn inflight(&self, op: FaultOp, obj: ObjId) -> u32 {
        self.rec.st.lock().by_obj.get(&(op, obj)).map_or(0, |l| l.cur)
    }

    /// The largest number of calls of `op` on `obj` that were in flight at once since recording started (or
    /// `reset_inflight`).
    pub fn max_inflight(&self, op: FaultOp, obj: ObjId) -> u32 {
        self.rec.st.lock().by_obj.get(&(op, obj)).map_or(0, |l| l.max)
    }

    /// Like [`FaultBackend::max_inflight`], over all objects.
    pub fn max_inflight_any(&self, op: FaultOp) -> u32 {
        self.rec.st.lock().by_op.get(&op).map_or(0, |l| l.max)
    }

    /// Recorded calls of `op` on `obj`.
    pub fn recorded_calls(&self, op: FaultOp, obj: ObjId) -> u64 {
        self.rec.st.lock().by_obj.get(&(op, obj)).map_or(0, |l| l.total)
    }

    /// Forgets the maxima and counts (calls in flight stay counted).
    pub fn reset_inflight(&self) {
        let mut guard = self.rec.st.lock();
        let st = &mut *guard;
        for l in st.by_obj.values_mut().chain(st.by_op.values_mut()) {
            l.max = l.cur;
            l.total = 0;
        }
    }

    /// Waits until at least `n` calls of `op` on `obj` are in flight at the same moment; false on timeout.
    pub fn wait_inflight(&self, op: FaultOp, obj: ObjId, n: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.rec.st.lock();
        loop {
            if st.by_obj.get(&(op, obj)).is_some_and(|l| l.cur >= n) {
                return true;
            }
            if self.rec.cv.wait_until(&mut st, deadline).timed_out() {
                return st.by_obj.get(&(op, obj)).is_some_and(|l| l.cur >= n);
            }
        }
    }

    fn fire(&self, op: FaultOp, names: &[&[u8]], fd: Option<BorrowedFd<'_>>) -> Fx {
        self.calls[op as usize].fetch_add(1, Relaxed);
        let g = self.faults.read();
        if g.is_empty() {
            return Fx(Vec::new());
        }
        let mut out = Vec::new();
        let mut base: Option<Vec<u8>> = None;
        for e in g.iter() {
            let f = &e.fault;
            if f.op != op {
                continue;
            }
            if let Some(n) = &f.name
                && !names.iter().any(|x| x == &n.as_slice()) {
                    continue;
                }
            if let Some(suffix) = &f.path {
                let base = base.get_or_insert_with(|| {
                    fd.and_then(sys::fd_path).map(|p| p.as_os_str().as_encoded_bytes().to_vec()).unwrap_or_default()
                });
                let ok = if names.is_empty() {
                    base.ends_with(suffix)
                } else {
                    names.iter().any(|n| {
                        let mut full = base.clone();
                        full.push(b'/');
                        full.extend_from_slice(n);
                        full.ends_with(suffix)
                    })
                };
                if !ok {
                    continue;
                }
            }
            let n = e.matched.fetch_add(1, Relaxed) + 1;
            if f.trigger.fires(n) {
                e.hits.fetch_add(1, Relaxed);
                out.push(f.effect.clone());
            }
        }
        Fx(out)
    }

    /// Common path: handles Delay, Errno, Skip (with `skip_val`) and AppliedThenErrno; `call` does the work
    /// and applies method-specific effects.
    fn exec<T>(
        &self,
        op: FaultOp,
        names: &[&[u8]],
        fd: Option<BorrowedFd<'_>>,
        skip_val: Option<T>,
        call: impl FnOnce(&Fx) -> SysResult<T>,
    ) -> SysResult<T> {
        self.exec2(op, names, fd, None, skip_val, call)
    }

    /// `exec` for a call that works on two descriptors (`extra` is recorded as well).
    fn exec2<T>(
        &self,
        op: FaultOp,
        names: &[&[u8]],
        fd: Option<BorrowedFd<'_>>,
        extra: Option<BorrowedFd<'_>>,
        skip_val: Option<T>,
        call: impl FnOnce(&Fx) -> SysResult<T>,
    ) -> SysResult<T> {
        let _rec = if self.rec.on.load(Relaxed) {
            let fds: Vec<BorrowedFd<'_>> = fd.into_iter().chain(extra).collect();
            self.rec.enter(&*self.inner, op, &fds)
        } else {
            None
        };
        let fx = self.fire(op, names, fd);
        if fx.none() {
            return call(&fx);
        }
        // (the tickets mark the call as finished when `exec2` returns, whichever way)
        let _tickets: Vec<GateTicket> = fx.gates().map(|g| g.pass()).collect();
        fx.delay();
        if let Some(e) = fx.errno_before() {
            return Err(e);
        }
        if fx.skip()
            && let Some(v) = skip_val {
                return Ok(v);
            }
        let r = call(&fx);
        if let Some(e) = fx.errno_after() {
            return Err(e);
        }
        r
    }

    fn exec_unit(
        &self,
        op: FaultOp,
        names: &[&[u8]],
        fd: Option<BorrowedFd<'_>>,
        call: impl FnOnce(&Fx) -> SysResult<()>,
    ) -> SysResult<()> {
        self.exec(op, names, fd, Some(()), call)
    }
}

impl FaultBackend {
    /// Runs `call`; with [`Effect::RestoreTimes`], puts the node's atime/mtime back afterwards.
    fn keeping_times(&self, fx: &Fx, node: BorrowedFd<'_>, file: Option<BorrowedFd<'_>>, call: impl FnOnce() -> SysResult<()>) -> SysResult<()> {
        if !fx.0.iter().any(|e| matches!(e, Effect::RestoreTimes)) {
            return call();
        }
        let _serial = self.times_lock.lock();
        let saved = self.inner.stat(node).ok();
        call()?;
        if let Some(st) = saved {
            let _ = self.inner.utimens(node, st.kind(), file, TimeSpec::Set(st.atime), TimeSpec::Set(st.mtime));
        }
        Ok(())
    }
}

fn lie_stat(fx: &Fx, mut st: Stat) -> Stat {
    for l in fx.stat_lies() {
        l.apply(&mut st);
    }
    st
}

impl Backend for FaultBackend {
    fn label(&self) -> &str {
        &self.label
    }

    fn root(&self) -> SysResult<OwnedFd> {
        self.exec(FaultOp::Root, &[], None, None, |_| self.inner.root())
    }

    fn lookup(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<(OwnedFd, Stat)> {
        self.exec(FaultOp::Lookup, &[name.to_bytes()], Some(dir), None, |fx| {
            self.inner.lookup(dir, name).map(|(fd, st)| (fd, lie_stat(fx, st)))
        })
    }

    fn stat_at(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<Stat> {
        self.exec(FaultOp::StatAt, &[name.to_bytes()], Some(dir), None, |fx| {
            self.inner.stat_at(dir, name).map(|st| lie_stat(fx, st))
        })
    }

    fn stat(&self, fd: BorrowedFd<'_>) -> SysResult<Stat> {
        self.exec(FaultOp::Stat, &[], Some(fd), None, |fx| self.inner.stat(fd).map(|st| lie_stat(fx, st)))
    }

    fn chmod(&self, node: BorrowedFd<'_>, kind: FileKind, mode: u32) -> SysResult<()> {
        self.exec_unit(FaultOp::Chmod, &[], Some(node), |_| self.inner.chmod(node, kind, mode))
    }

    fn chown(&self, node: BorrowedFd<'_>, uid: Option<u32>, gid: Option<u32>) -> SysResult<()> {
        self.exec_unit(FaultOp::Chown, &[], Some(node), |_| self.inner.chown(node, uid, gid))
    }

    fn truncate(&self, node: BorrowedFd<'_>, file: Option<BorrowedFd<'_>>, size: u64) -> SysResult<()> {
        self.exec_unit(FaultOp::Truncate, &[], Some(node), |fx| {
            self.keeping_times(fx, node, file, || self.inner.truncate(node, file, size))
        })
    }

    fn utimens(
        &self,
        node: BorrowedFd<'_>,
        kind: FileKind,
        file: Option<BorrowedFd<'_>>,
        atime: TimeSpec,
        mtime: TimeSpec,
    ) -> SysResult<()> {
        self.exec_unit(FaultOp::Utimens, &[], Some(node), |_| self.inner.utimens(node, kind, file, atime, mtime))
    }

    fn readlink(&self, node: BorrowedFd<'_>) -> SysResult<Vec<u8>> {
        self.exec(FaultOp::Readlink, &[], Some(node), None, |fx| {
            let r = self.inner.readlink(node)?;
            Ok(fx.0.iter().find_map(|e| if let Effect::ReadlinkTarget(t) = e { Some(t.clone()) } else { None }).unwrap_or(r))
        })
    }

    fn mknod(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32, rdev: u64) -> SysResult<()> {
        self.exec_unit(FaultOp::Mknod, &[name.to_bytes()], Some(dir), |_| self.inner.mknod(dir, name, mode, rdev))
    }

    fn mkdir(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> SysResult<()> {
        self.exec_unit(FaultOp::Mkdir, &[name.to_bytes()], Some(dir), |_| self.inner.mkdir(dir, name, mode))
    }

    fn unlink(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        self.exec_unit(FaultOp::Unlink, &[name.to_bytes()], Some(dir), |_| self.inner.unlink(dir, name))
    }

    fn rmdir(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        self.exec_unit(FaultOp::Rmdir, &[name.to_bytes()], Some(dir), |_| self.inner.rmdir(dir, name))
    }

    fn symlink(&self, target: &CStr, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        self.exec_unit(FaultOp::Symlink, &[name.to_bytes()], Some(dir), |_| self.inner.symlink(target, dir, name))
    }

    fn rename(
        &self,
        dir: BorrowedFd<'_>,
        name: &CStr,
        newdir: BorrowedFd<'_>,
        newname: &CStr,
        flags: u32,
    ) -> SysResult<()> {
        self.exec_unit(FaultOp::Rename, &[name.to_bytes(), newname.to_bytes()], Some(dir), |fx| {
            self.inner.rename(dir, name, newdir, newname, flags)?;
            let moved = || self.inner.stat(dir).ok().map(|s| s.ident()) != self.inner.stat(newdir).ok().map(|s| s.ident());
            if fx.0.iter().any(|e| matches!(e, Effect::StampMtime)) && moved() {
                let mut names = vec![(newdir, newname)];
                if flags & libc::RENAME_EXCHANGE != 0 {
                    names.push((dir, name));
                }
                for (d, n) in names {
                    if let Ok((fd, st)) = self.inner.lookup(d, n)
                        && st.kind() == FileKind::Dir {
                            let _ = self.inner.utimens(std::os::fd::AsFd::as_fd(&fd), FileKind::Dir, None, TimeSpec::Omit, TimeSpec::Now);
                        }
                }
            }
            Ok(())
        })
    }

    fn link(&self, node: BorrowedFd<'_>, newdir: BorrowedFd<'_>, newname: &CStr) -> SysResult<()> {
        self.exec_unit(FaultOp::Link, &[newname.to_bytes()], Some(node), |_| self.inner.link(node, newdir, newname))
    }

    fn open(&self, node: BorrowedFd<'_>, flags: i32) -> SysResult<OwnedFd> {
        self.exec(FaultOp::Open, &[], Some(node), None, |_| self.inner.open(node, flags))
    }

    fn create(&self, dir: BorrowedFd<'_>, name: &CStr, flags: i32, mode: u32) -> SysResult<OwnedFd> {
        self.exec(FaultOp::Create, &[name.to_bytes()], Some(dir), None, |_| self.inner.create(dir, name, flags, mode))
    }

    fn pread(&self, file: BorrowedFd<'_>, buf: &mut [u8], off: u64) -> SysResult<usize> {
        self.exec(FaultOp::Pread, &[], Some(file), None, |fx| {
            let mut n = self.inner.pread(file, buf, off)?;
            for e in &fx.0 {
                match e {
                    Effect::ShortRead(k) => n = n.min(*k),
                    Effect::CorruptRead { offset } if *offset < n => buf[*offset] ^= 0xff,
                    _ => {}
                }
            }
            Ok(n)
        })
    }

    fn pwrite(&self, file: BorrowedFd<'_>, data: &[u8], off: u64) -> SysResult<usize> {
        self.exec(FaultOp::Pwrite, &[], Some(file), Some(data.len()), |fx| {
            let mut data = std::borrow::Cow::Borrowed(data);
            let mut report = None;
            let mut at = off;
            let mut restore = false;
            for e in &fx.0 {
                match e {
                    Effect::ShortWrite(k) => {
                        let k = (*k).min(data.len());
                        data.to_mut().truncate(k);
                        report = Some(k);
                    }
                    Effect::CorruptWrite { offset } if *offset < data.len() => data.to_mut()[*offset] ^= 0xff,
                    Effect::ShiftOffset(k) => at = at.saturating_add_signed(*k),
                    Effect::RestoreTimes => restore = true,
                    _ => {}
                }
            }
            let _serial = restore.then(|| self.times_lock.lock());
            let saved = if restore { self.inner.stat(file).ok() } else { None };
            let n = self.inner.pwrite(file, &data, at)?;
            if let Some(st) = saved {
                let _ = self.inner.utimens(file, FileKind::Regular, Some(file), TimeSpec::Set(st.atime), TimeSpec::Set(st.mtime));
            }
            Ok(report.map(|r| r.min(n)).unwrap_or(n))
        })
    }

    fn flush(&self, file: BorrowedFd<'_>) -> SysResult<()> {
        self.exec_unit(FaultOp::Flush, &[], Some(file), |_| self.inner.flush(file))
    }

    fn fsync(&self, file: BorrowedFd<'_>, datasync: bool) -> SysResult<()> {
        self.exec_unit(FaultOp::Fsync, &[], Some(file), |_| self.inner.fsync(file, datasync))
    }

    fn opendir(&self, node: BorrowedFd<'_>) -> SysResult<OwnedFd> {
        self.exec(FaultOp::Opendir, &[], Some(node), None, |_| self.inner.opendir(node))
    }

    fn readdir(&self, dir: BorrowedFd<'_>) -> SysResult<Vec<DirEntry>> {
        self.exec(FaultOp::Readdir, &[], Some(dir), None, |fx| {
            let mut v = self.inner.readdir(dir)?;
            for e in &fx.0 {
                match e {
                    Effect::DropEntry(n) => v.retain(|d| &d.name != n),
                    Effect::AddEntry(n) => v.push(DirEntry { name: n.clone(), ino: 0, kind: Some(FileKind::Regular) }),
                    Effect::ChangeEntryKind(n, k) => {
                        for d in v.iter_mut().filter(|d| &d.name == n) {
                            d.kind = Some(*k);
                        }
                    }
                    _ => {}
                }
            }
            Ok(v)
        })
    }

    fn statfs(&self, node: BorrowedFd<'_>) -> SysResult<StatFs> {
        self.exec(FaultOp::Statfs, &[], Some(node), None, |_| self.inner.statfs(node))
    }

    fn setxattr(&self, node: BorrowedFd<'_>, name: &CStr, value: &[u8], flags: i32) -> SysResult<()> {
        self.exec_unit(FaultOp::Setxattr, &[name.to_bytes()], Some(node), |_| self.inner.setxattr(node, name, value, flags))
    }

    fn getxattr(&self, node: BorrowedFd<'_>, name: &CStr, size: usize) -> SysResult<XattrOut> {
        self.exec(FaultOp::Getxattr, &[name.to_bytes()], Some(node), None, |fx| {
            let r = self.inner.getxattr(node, name, size)?;
            Ok(match fx.0.iter().find_map(|e| if let Effect::XattrValue(v) = e { Some(v) } else { None }) {
                Some(v) if size == 0 => XattrOut::Size(v.len()),
                Some(v) => XattrOut::Data(v.clone()),
                None => r,
            })
        })
    }

    fn listxattr(&self, node: BorrowedFd<'_>, size: usize) -> SysResult<XattrOut> {
        self.exec(FaultOp::Listxattr, &[], Some(node), None, |fx| {
            if !fx.has(|e| matches!(e, Effect::XattrListAdd(_) | Effect::XattrListDrop(_))) {
                return self.inner.listxattr(node, size);
            }
            // Work on the complete list, then re-apply the caller's size semantics.
            let XattrOut::Data(full) = self.inner.listxattr(node, 65536)? else { unreachable!() };
            let mut names: Vec<Vec<u8>> = full.split(|&b| b == 0).filter(|n| !n.is_empty()).map(|n| n.to_vec()).collect();
            for e in &fx.0 {
                match e {
                    Effect::XattrListAdd(n) => names.push(n.clone()),
                    Effect::XattrListDrop(n) => names.retain(|x| x != n),
                    _ => {}
                }
            }
            let mut out = Vec::new();
            for n in names {
                out.extend_from_slice(&n);
                out.push(0);
            }
            if size == 0 {
                Ok(XattrOut::Size(out.len()))
            } else if out.len() > size {
                Err(libc::ERANGE)
            } else {
                Ok(XattrOut::Data(out))
            }
        })
    }

    fn removexattr(&self, node: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        self.exec_unit(FaultOp::Removexattr, &[name.to_bytes()], Some(node), |_| self.inner.removexattr(node, name))
    }

    fn access(&self, node: BorrowedFd<'_>, mask: i32) -> SysResult<()> {
        self.exec(FaultOp::Access, &[], Some(node), None, |_| self.inner.access(node, mask))
    }

    fn fallocate(&self, file: BorrowedFd<'_>, mode: i32, off: u64, len: u64) -> SysResult<()> {
        self.exec_unit(FaultOp::Fallocate, &[], Some(file), |fx| {
            self.keeping_times(fx, file, Some(file), || self.inner.fallocate(file, mode, off, len))
        })
    }

    fn lseek(&self, file: BorrowedFd<'_>, off: i64, whence: i32) -> SysResult<i64> {
        self.exec(FaultOp::Lseek, &[], Some(file), None, |fx| {
            let r = self.inner.lseek(file, off, whence)?;
            Ok(fx.0.iter().find_map(|e| if let Effect::Offset(o) = e { Some(*o) } else { None }).unwrap_or(r))
        })
    }

    fn copy_file_range(
        &self,
        fin: BorrowedFd<'_>,
        off_in: u64,
        fout: BorrowedFd<'_>,
        off_out: u64,
        len: usize,
        flags: u32,
    ) -> SysResult<usize> {
        self.exec2(FaultOp::CopyFileRange, &[], Some(fout), Some(fin), Some(len), |fx| {
            let mut len = len;
            let mut short = None;
            for e in &fx.0 {
                if let Effect::ShortWrite(k) = e {
                    len = len.min(*k);
                    short = Some(*k);
                }
            }
            let n = self.inner.copy_file_range(fin, off_in, fout, off_out, len, flags)?;
            Ok(short.map(|k| k.min(n)).unwrap_or(n))
        })
    }

    fn getlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<Lock> {
        self.exec(FaultOp::Getlk, &[], Some(file), None, |fx| {
            if let Some(l) = fx.0.iter().find_map(|e| if let Effect::GetlkResult(l) = e { Some(*l) } else { None }) {
                return Ok(l);
            }
            if fx.has(|e| matches!(e, Effect::GetlkFree)) {
                return Ok(Lock { typ: libc::F_UNLCK, ..*lock });
            }
            self.inner.getlk(file, lock)
        })
    }

    fn setlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<()> {
        self.exec(FaultOp::Setlk, &[], Some(file), None, |fx| {
            if lock.typ != libc::F_UNLCK {
                if fx.has(|e| matches!(e, Effect::LockAlwaysOk)) {
                    return Ok(());
                }
                if fx.has(|e| matches!(e, Effect::LockAlwaysConflict)) {
                    return Err(libc::EAGAIN);
                }
            }
            self.inner.setlk(file, lock)
        })
    }
}
