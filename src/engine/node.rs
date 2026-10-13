//! The inode table: one [`Node`] per object the kernel knows about, holding
//! an O_PATH descriptor on each backend.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use super::fds::CountedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use std::os::fd::RawFd;

use parking_lot::{Mutex, RwLock};

use super::Side;
use super::locks::LockState;
use crate::sys::{FileKind, Ts};

pub struct Node {
    /// FUSE node id; also the st_ino reported to applications (the
    /// primary's inode number, with the root mapped to 1).
    pub id: u64,
    pub kind: FileKind,
    pub pfd: CountedFd,
    pub pident: (u64, u64),
    /// The secondary object. `None` when it does not exist on the secondary
    /// (it diverged): operations on the node then only use the primary.
    /// Replaced only by resync, under the node's exclusive stripe lock, or
    /// attached by a lookup when it was `None`.
    sec: RwLock<Option<SecRef>>,
    /// Incremented whenever `sec` is replaced: open handles opened against
    /// an older secondary object stop using it.
    sec_gen: AtomicU64,
    /// The last secondary object removed by `set_secondary(None)`.
    retired: Mutex<Option<Arc<CountedFd>>>,
    nlookup: AtomicU64,
    open: AtomicU64,
    /// Display path, maintained on lookup/create/rename. May be stale for
    /// descendants of a renamed directory; reports resolve the real path.
    pub hint: Mutex<Arc<str>>,
    /// Last observed (primary, secondary) ctimes, for change tracking.
    pub ctimes: Mutex<Option<(Ts, Ts)>>,
    /// Mirrored POSIX record locks.
    pub locks: Mutex<LockState>,
    /// Byte-range locks of data operations (relaxed serialization).
    pub ranges: super::ranges::RangeLocks,
    /// In-place data operations in flight (relaxed serialization).
    pub data: super::ranges::DataInFlight,
    /// Changes to the object that leave a FUSE side's kernel holding attributes it invalidated: every operation
    /// that holds the object exclusively, and every in-place write ([`Node::attrs_changed`]).
    attr_changes: AtomicU64,
    /// `attr_changes` when the last stat of the object past the kernels' attribute caches began.
    attr_fetched: AtomicU64,
}

impl Node {
    pub fn new(
        id: u64,
        kind: FileKind,
        pfd: OwnedFd,
        sfd: Option<OwnedFd>,
        pident: (u64, u64),
        sident: Option<(u64, u64)>,
        hint: Arc<str>,
    ) -> Node {
        Node {
            id,
            kind,
            pfd: CountedFd::new(pfd),
            pident,
            sec: RwLock::new(match (sfd, sident) {
                (Some(fd), Some(ident)) => Some(SecRef { fd: Arc::new(CountedFd::new(fd)), ident }),
                _ => None,
            }),
            sec_gen: AtomicU64::new(0),
            retired: Mutex::new(None),
            nlookup: AtomicU64::new(0),
            open: AtomicU64::new(0),
            hint: Mutex::new(hint),
            ctimes: Mutex::new(None),
            locks: Mutex::new(LockState::default()),
            ranges: Default::default(),
            data: Default::default(),
            attr_changes: AtomicU64::new(0),
            attr_fetched: AtomicU64::new(0),
        }
    }

    /// The object changed, or may have: the kernel of a FUSE side invalidated the attributes it caches for it.
    ///
    /// Until a stat has fetched them again, such a kernel can still answer a stat with the old ones: when the stat
    /// runs while another stat of the object installs the fresh attributes (mainline Linux 7.3, `fs/fuse`:
    /// `fuse_change_attributes_common` clears the invalid mask before it writes the attributes, and
    /// `fuse_update_get_attr` reads both without the inode's lock, so it can find the mask clear, the old cache
    /// time still valid, and the old attributes). Concurrent stats of one object are routine here (lookups and
    /// getattrs hold it shared), so the first stats after a change go past the caches ([`Node::attrs_unfetched`]).
    pub fn attrs_changed(&self) {
        self.attr_changes.fetch_add(1, SeqCst);
    }

    /// `Some(changes)` while a stat of the object must go past the kernels' attribute caches
    /// ([`Backend::stat_fresh`](crate::backend::Backend::stat_fresh)): it changed since the last such stat began.
    /// Pass `changes` to [`Node::attrs_fetched`] once that stat succeeded on both sides.
    pub fn attrs_unfetched(&self) -> Option<u64> {
        let changes = self.attr_changes.load(SeqCst);
        (self.attr_fetched.load(SeqCst) != changes).then_some(changes)
    }

    /// A stat past the caches that began at `changes` ([`Node::attrs_unfetched`]) succeeded: the kernels hold
    /// what it fetched (or what a later stat fetched), so later stats may be answered from their caches again,
    /// unless the object changed meanwhile.
    pub fn attrs_fetched(&self, changes: u64) {
        self.attr_fetched.fetch_max(changes, SeqCst);
    }

    pub fn p(&self) -> BorrowedFd<'_> {
        self.pfd.as_fd()
    }

    /// The secondary descriptor. Only call when [`Node::has_sec`] is true.
    ///
    /// Never panics (a panic aborts the daemon and takes the mount down):
    /// code that checked [`Node::has_sec`] without holding the node's lock
    /// can race with a repair detaching the secondary. It then gets the
    /// detached object (or, if there never was one, /dev/null), whose
    /// results are harmless: the operation already decided to compare.
    pub fn s(&self) -> SideFd<'_> {
        if let Some(s) = self.sec.read().as_ref() {
            return SideFd::Owned(s.fd.clone());
        }
        if let Some(fd) = self.retired.lock().clone() {
            return SideFd::Owned(fd);
        }
        tracing::debug!("node {}: secondary used after detach", self.id);
        SideFd::Owned(dev_null())
    }

    /// The secondary descriptor, if there is one. Use this where the node's stripe lock is not held: the secondary
    /// is only removed under the node's exclusive lock.
    pub fn try_s(&self) -> Option<SideFd<'_>> {
        self.sec.read().as_ref().map(|s| SideFd::Owned(s.fd.clone()))
    }

    pub fn fd(&self, side: Side) -> SideFd<'_> {
        match side {
            Side::Primary => SideFd::Borrowed(self.p()),
            Side::Secondary => self.s(),
        }
    }

    pub fn has_sec(&self) -> bool {
        self.sec.read().is_some()
    }

    pub fn sident(&self) -> Option<(u64, u64)> {
        self.sec.read().as_ref().map(|s| s.ident)
    }

    pub fn sec_gen(&self) -> u64 {
        self.sec_gen.load(SeqCst)
    }

    pub fn hint(&self) -> Arc<str> {
        self.hint.lock().clone()
    }

    pub fn set_hint(&self, h: Arc<str>) {
        *self.hint.lock() = h;
    }
}

fn dev_null() -> Arc<CountedFd> {
    static NULL: std::sync::OnceLock<Arc<CountedFd>> = std::sync::OnceLock::new();
    NULL.get_or_init(|| {
        Arc::new(CountedFd::new(OwnedFd::from(
            std::fs::File::open("/dev/null").expect("/dev/null must exist"),
        )))
    })
    .clone()
}

struct SecRef {
    fd: Arc<CountedFd>,
    ident: (u64, u64),
}

/// A descriptor of one side of a node: borrowed for the primary, shared
/// ownership for the (replaceable) secondary.
pub enum SideFd<'a> {
    Borrowed(BorrowedFd<'a>),
    Owned(Arc<CountedFd>),
}

impl AsFd for SideFd<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            SideFd::Borrowed(b) => *b,
            SideFd::Owned(o) => o.as_fd(),
        }
    }
}

impl SideFd<'_> {
    pub fn raw(&self) -> RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.as_fd())
    }
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<u64, Arc<Node>>,
    /// Secondary identity -> node id, for hard-link structure checks.
    by_sec: HashMap<(u64, u64), u64>,
    /// Ids of nodes removed while their attributes were unfetched ([`Node::attrs_unfetched`]): the object's next
    /// node starts so. Objects that are gone (link count 0: removed, or replaced by a rename) are not recorded:
    /// every one of them was held exclusively by its removal, and a workload that deletes much would otherwise
    /// crowd out the live ones. At most [`UNFETCHED_CAP`] ids; beyond, the oldest mark is dropped (a stat of that
    /// object may then be answered from a kernel's stale cache, as before these marks existed).
    unfetched: Unfetched,
}


/// The marks of [`Inner::unfetched`], evicted oldest first.
#[derive(Default)]
struct Unfetched {
    /// id -> the sequence number it was marked with
    ids: HashMap<u64, u64>,
    /// (sequence number, id) in marking order; entries of ids unmarked or marked again since are skipped
    order: VecDeque<(u64, u64)>,
    seq: u64,
}

impl Unfetched {
    fn insert(&mut self, id: u64, cap: usize) {
        self.seq += 1;
        self.ids.insert(id, self.seq);
        self.order.push_back((self.seq, id));
        while self.ids.len() > cap || self.order.len() > 2 * cap {
            let Some((seq, old)) = self.order.pop_front() else { break };
            if self.ids.get(&old) == Some(&seq) {
                self.ids.remove(&old);
            }
        }
    }

    fn remove(&mut self, id: u64) -> bool {
        self.ids.remove(&id).is_some()
    }

    fn contains(&self, id: u64) -> bool {
        !self.ids.is_empty() && self.ids.contains_key(&id)
    }
}

const UNFETCHED_CAP: usize = 1 << 16;

#[derive(Default)]
pub struct NodeTable {
    inner: RwLock<Inner>,
}

pub enum Inserted {
    /// The node was new and has been inserted.
    New(Arc<Node>),
    /// A node with this id already existed (its lookup count was bumped).
    Existing(Arc<Node>),
}

impl NodeTable {
    pub fn get(&self, id: u64) -> Option<Arc<Node>> {
        self.inner.read().by_id.get(&id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The node id currently registered for a secondary identity.
    pub fn by_secondary(&self, ident: (u64, u64)) -> Option<u64> {
        self.inner.read().by_sec.get(&ident).copied()
    }

    /// Inserts `node` unless one with the same id exists; either way the
    /// returned node's lookup count is incremented by one.
    pub fn insert_or_get(&self, n: Arc<Node>) -> Inserted {
        let mut g = self.inner.write();
        if let Some(x) = g.by_id.get(&n.id) {
            x.nlookup.fetch_add(1, SeqCst);
            return Inserted::Existing(x.clone());
        }
        n.nlookup.store(1, SeqCst);
        if g.unfetched.remove(n.id) {
            n.attrs_changed();
        }
        if let Some(si) = n.sident() {
            g.by_sec.insert(si, n.id);
        }
        g.by_id.insert(n.id, n.clone());
        Inserted::New(n)
    }

    /// Finds a node and adds a lookup reference to it, atomically with respect to [`NodeTable::forget`] (which
    /// takes the write lock): a concurrent FORGET for an older reference cannot remove the node in between.
    pub fn get_ref(&self, id: u64) -> Option<Arc<Node>> {
        let g = self.inner.read();
        let n = g.by_id.get(&id)?.clone();
        n.nlookup.fetch_add(1, SeqCst);
        Some(n)
    }

    pub fn forget(&self, id: u64, count: u64) {
        let mut g = self.inner.write();
        let Some(n) = g.by_id.get(&id) else { return };
        let prev = n.nlookup.fetch_sub(count.min(n.nlookup.load(SeqCst)), SeqCst);
        if prev <= count && n.open.load(SeqCst) == 0 && id != 1 {
            Self::remove(&mut g, id);
        }
    }

    pub fn opened(&self, n: &Node) {
        n.open.fetch_add(1, SeqCst);
    }

    pub fn closed(&self, n: &Node) {
        let mut g = self.inner.write();
        let prev = n.open.fetch_sub(1, SeqCst);
        if prev == 1 && n.nlookup.load(SeqCst) == 0 && n.id != 1 {
            // Only remove if the table still holds this very node.
            if g.by_id.get(&n.id).is_some_and(|x| std::ptr::eq(&**x, n)) {
                Self::remove(&mut g, n.id);
            }
        }
    }

    fn remove(g: &mut Inner, id: u64) {
        let Some(n) = g.by_id.remove(&id) else { return };
        if let Some(si) = n.sident()
            && g.by_sec.get(&si) == Some(&id) {
                g.by_sec.remove(&si);
            }
        // (an O_PATH descriptor of a removed object reports link count 0)
        if n.attrs_unfetched().is_some() && crate::sys::fstat(n.p()).is_ok_and(|st| st.nlink > 0) {
            g.unfetched.insert(id, UNFETCHED_CAP);
        }
    }

    /// Whether the object with this id, which has no node, changed since its attributes were last fetched past the
    /// kernels' caches (its node was removed before that).
    pub fn unfetched(&self, id: u64) -> bool {
        let g = self.inner.read();
        g.unfetched.contains(id)
    }

    /// A stat past the caches of the object with this id, which has no node, succeeded.
    pub fn fetched(&self, id: u64) {
        if self.unfetched(id) {
            self.inner.write().unfetched.remove(id);
        }
    }

    /// Replaces (or removes, or attaches) the secondary object of a node.
    pub fn set_secondary(&self, n: &Node, new: Option<(OwnedFd, (u64, u64))>) {
        let mut g = self.inner.write();
        let mut sec = n.sec.write();
        if let Some(old) = sec.take() {
            if g.by_sec.get(&old.ident) == Some(&n.id) {
                g.by_sec.remove(&old.ident);
            }
            *n.retired.lock() = Some(old.fd);
        }
        if let Some((fd, ident)) = new {
            if g.by_id.get(&n.id).is_some_and(|x| std::ptr::eq(&**x, n)) {
                g.by_sec.insert(ident, n.id);
            }
            *sec = Some(SecRef { fd: Arc::new(CountedFd::new(fd)), ident });
        }
        n.sec_gen.fetch_add(1, SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opath(p: &std::path::Path) -> OwnedFd {
        let c = crate::sys::cstr(p.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid C string.
        crate::sys::owned(unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) }).unwrap()
    }

    /// A node of `fd`'s object under `id`, changed since its attributes were fetched, inserted and forgotten.
    fn forget_changed(t: &NodeTable, id: u64, fd: &OwnedFd) {
        let n = Arc::new(Node::new(id, FileKind::Dir, fd.try_clone().unwrap(), None, (0, id), None, "x".into()));
        n.attrs_changed();
        assert!(matches!(t.insert_or_get(n), Inserted::New(_)));
        t.forget(id, 1);
        assert!(t.get(id).is_none());
    }

    /// Every removed object was held exclusively by its removal: were they recorded, a workload that deletes more
    /// than the cap would crowd out the marks of live directories (their stale link counts would be compared again).
    #[test]
    fn unfetched_marks_of_live_objects_survive_mass_deletion() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("live")).unwrap();
        std::fs::write(d.path().join("gone"), b"").unwrap();
        let live = opath(&d.path().join("live"));
        let gone = opath(&d.path().join("gone"));
        std::fs::remove_file(d.path().join("gone")).unwrap();
        let t = NodeTable::default();
        forget_changed(&t, 7, &live);
        for id in 1000..1000 + UNFETCHED_CAP as u64 + 10 {
            forget_changed(&t, id, &gone);
            assert!(!t.unfetched(id), "a removed object is not recorded");
        }
        assert!(t.unfetched(7));
        // and the object's next node starts unfetched
        let n = Arc::new(Node::new(7, FileKind::Dir, live.try_clone().unwrap(), None, (0, 7), None, "x".into()));
        assert!(matches!(t.insert_or_get(n.clone()), Inserted::New(_)));
        assert!(n.attrs_unfetched().is_some());
        assert!(!t.unfetched(7));
    }

    /// Beyond the cap the oldest marks go, one by one; a mark set again counts from then.
    #[test]
    fn unfetched_marks_are_evicted_oldest_first() {
        let mut u = Unfetched::default();
        for id in 0..4 {
            u.insert(id, 4);
        }
        u.insert(0, 4); // 0 is now the newest
        u.insert(4, 4);
        assert!(!u.contains(1));
        assert!([0, 2, 3, 4].iter().all(|&id| u.contains(id)));
        assert!(u.remove(2));
        for id in 5..7 {
            u.insert(id, 4);
        }
        assert!(!u.contains(3) && u.contains(0) && u.contains(4) && u.contains(5) && u.contains(6));
        for id in 100..10_000 {
            u.insert(id, 4);
            u.remove(id);
        }
        assert!(u.order.len() <= 8, "the order queue stays bounded");
    }
}
