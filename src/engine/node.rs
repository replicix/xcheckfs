//! The inode table: one [`Node`] per object the kernel knows about, holding
//! an O_PATH descriptor on each backend.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
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
    pub pfd: OwnedFd,
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
    retired: Mutex<Option<Arc<OwnedFd>>>,
    nlookup: AtomicU64,
    open: AtomicU64,
    /// Display path, maintained on lookup/create/rename. May be stale for
    /// descendants of a renamed directory; reports resolve the real path.
    pub hint: Mutex<Arc<str>>,
    /// Last observed (primary, secondary) ctimes, for change tracking.
    pub ctimes: Mutex<Option<(Ts, Ts)>>,
    /// Mirrored POSIX record locks.
    pub locks: Mutex<LockState>,
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
            pfd,
            pident,
            sec: RwLock::new(match (sfd, sident) {
                (Some(fd), Some(ident)) => Some(SecRef { fd: Arc::new(fd), ident }),
                _ => None,
            }),
            sec_gen: AtomicU64::new(0),
            retired: Mutex::new(None),
            nlookup: AtomicU64::new(0),
            open: AtomicU64::new(0),
            hint: Mutex::new(hint),
            ctimes: Mutex::new(None),
            locks: Mutex::new(LockState::default()),
        }
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

fn dev_null() -> Arc<OwnedFd> {
    static NULL: std::sync::OnceLock<Arc<OwnedFd>> = std::sync::OnceLock::new();
    NULL.get_or_init(|| {
        Arc::new(OwnedFd::from(
            std::fs::File::open("/dev/null").expect("/dev/null must exist"),
        ))
    })
    .clone()
}

struct SecRef {
    fd: Arc<OwnedFd>,
    ident: (u64, u64),
}

/// A descriptor of one side of a node: borrowed for the primary, shared
/// ownership for the (replaceable) secondary.
pub enum SideFd<'a> {
    Borrowed(BorrowedFd<'a>),
    Owned(Arc<OwnedFd>),
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
}

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
        if let Some(n) = g.by_id.remove(&id) {
            if let Some(si) = n.sident() {
                if g.by_sec.get(&si) == Some(&id) {
                    g.by_sec.remove(&si);
                }
            }
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
            *sec = Some(SecRef { fd: Arc::new(fd), ident });
        }
        n.sec_gen.fetch_add(1, SeqCst);
    }
}
