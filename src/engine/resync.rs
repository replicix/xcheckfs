//! Repairing the secondary from the primary ("resync").
//!
//! The policy asks for a repair (`--on-mismatch resync`, or the operator's
//! choice in freeze mode); the request is queued on the operation and run
//! after the operation released its locks and before its reply, with locks
//! of its own. The primary is never modified.
//!
//! Three kinds of repair:
//!
//! - **object**: one object's content, xattrs, owner, mode and times;
//! - **entry**: one name in a directory: an entry missing on the secondary
//!   is copied from the primary (whole subtrees, hard links preserved), an
//!   extra one is removed, one of the wrong type, with the wrong link target
//!   or the wrong hard-link identity is replaced;
//! - **directory**: every name in which the two listings differ.
//!
//! Every repair is verified by comparing again. A repair that does not
//! verify, or an object repaired more than `resync_limit` times in ten
//! minutes, is given up: the object stays diverged and is handled on the
//! primary only. With `--quarantine DIR`, the secondary's version is copied
//! there before it is overwritten or removed.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::io::Write as _;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

use super::{Engine, Node};
use crate::backend::{Backend, TimeSpec, XattrOut};
use crate::compare;
use crate::sys::{FileKind, Stat, SysResult, fmt_errno};

const CHUNK: usize = 1 << 20;
const XATTR_MAX: usize = 65536;
const BUDGET_WINDOW: Duration = Duration::from_secs(600);

pub(crate) enum ResyncReq {
    Object(Arc<Node>),
    /// Names of one directory. Requests for several names of the same directory are merged into one repair, which
    /// is verified after all of its names are repaired (a rename's two names, for example).
    Entry(Arc<Node>, Vec<OsString>),
    Dir(Arc<Node>),
}

impl ResyncReq {
    pub(crate) fn entry(d: &Arc<Node>, name: &OsStr) -> ResyncReq {
        ResyncReq::Entry(d.clone(), vec![name.to_os_string()])
    }

    /// Whether `o` is already covered by this request; names of the same directory are added to it.
    pub(crate) fn absorb(&mut self, o: &ResyncReq) -> bool {
        match (self, o) {
            (ResyncReq::Object(a), ResyncReq::Object(b)) | (ResyncReq::Dir(a), ResyncReq::Dir(b)) => a.id == b.id,
            (ResyncReq::Entry(a, xs), ResyncReq::Entry(b, ys)) if a.id == b.id => {
                for y in ys {
                    if !xs.contains(y) {
                        xs.push(y.clone());
                    }
                }
                true
            }
            _ => false,
        }
    }
}

/// (primary, secondary) descriptors of directories.
type DirPairs = Vec<(OwnedFd, OwnedFd)>;

/// State of one repair's copies: hard links made so far, and the
/// (primary, secondary) directory searched for other names of hard links.
struct CopyCtx<'a> {
    links: HashMap<(u64, u64), OwnedFd>,
    scope: (BorrowedFd<'a>, BorrowedFd<'a>),
}

/// Repair budget per object path.
#[derive(Default)]
pub(crate) struct Repairs {
    map: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Repairs {
    /// The budget of `key` is used up (the object was given up on).
    fn exhausted(&self, key: &str, limit: u32) -> bool {
        let now = Instant::now();
        self.map.lock().get(key).is_some_and(|v| v.iter().filter(|t| now.duration_since(**t) < BUDGET_WINDOW).count() >= limit as usize)
    }

    fn allow(&self, key: &str, limit: u32) -> bool {
        let now = Instant::now();
        let mut m = self.map.lock();
        if m.len() > 10_000 {
            m.retain(|_, v| v.back().is_some_and(|t| now.duration_since(*t) < BUDGET_WINDOW));
        }
        let v = m.entry(key.to_string()).or_default();
        while v.front().is_some_and(|t| now.duration_since(*t) >= BUDGET_WINDOW) {
            v.pop_front();
        }
        if v.len() >= limit as usize {
            return false;
        }
        v.push_back(now);
        true
    }
}

fn e(what: &str, err: i32) -> String {
    format!("{what}: {}", fmt_errno(err))
}

fn cname(n: &OsStr) -> Result<CString, String> {
    CString::new(n.as_bytes()).map_err(|_| "name contains NUL".to_string())
}

impl Engine {
    fn p(&self) -> &dyn Backend {
        &*self.b[0]
    }

    fn s(&self) -> &dyn Backend {
        &*self.b[1]
    }

    /// Runs one queued repair. `why` is the mismatch that caused it.
    pub(crate) fn resync(&self, req: ResyncReq, why: &str) {
        if self.detached() {
            return;
        }
        let key = match &req {
            ResyncReq::Object(n) => self.path_of(n),
            ResyncReq::Entry(d, names) => self.child_path(d, &names[0]),
            ResyncReq::Dir(d) => self.path_of(d),
        };
        if !self.repairs.allow(&key, self.cfg.resync_limit) {
            self.stats.resync_giveups.fetch_add(1, Relaxed);
            tracing::error!(
                "{key}: repaired {} times within 10 minutes and diverged again; giving up, it stays diverged",
                self.cfg.resync_limit
            );
            // (a directory keeps its secondary: everything below it would otherwise be unchecked)
            if let ResyncReq::Object(n) = &req && n.kind != FileKind::Dir {
                let _l = self.lock(&[(n.id, true)]);
                self.nodes.set_secondary(n, None);
            }
            return;
        }
        let res = match &req {
            // A directory's own state includes its entries (link count, size).
            // (unless the secondary object is not a directory at all: then the entry is replaced)
            ResyncReq::Object(n) if n.kind == FileKind::Dir && self.secondary_is_dir(n) => self.repair_entries(n, None, why),
            ResyncReq::Object(n) => {
                let mut dirs = Vec::new();
                let r = self.repair_object(n, &key, why, &mut dirs);
                self.restore_dir_times(dirs);
                r
            }
            ResyncReq::Entry(d, names) => self.repair_entries(d, Some(names.clone()), why),
            ResyncReq::Dir(d) => self.repair_entries(d, None, why),
        };
        match res {
            Ok(()) => {
                self.stats.resyncs.fetch_add(1, Relaxed);
                tracing::warn!("{key}: secondary repaired from the primary");
            }
            Err(err) => {
                self.stats.resync_failures.fetch_add(1, Relaxed);
                let what = match &req {
                    ResyncReq::Object(_) => "object",
                    ResyncReq::Entry(..) => "entry",
                    ResyncReq::Dir(_) => "directory",
                };
                tracing::error!("{key}: {what} repair FAILED ({err}); it stays diverged");
            }
        }
    }

    // ------------------------------------------------------------- object

    /// The object was repaired `resync_limit` times recently and diverged again: it stays primary-only until the
    /// budget window has passed (a lookup must not reconnect it meanwhile).
    pub(crate) fn repairs_exhausted(&self, n: &Node) -> bool {
        self.repairs.exhausted(&self.path_of(n), self.cfg.resync_limit)
    }

    fn secondary_is_dir(&self, n: &Node) -> bool {
        n.try_s().is_some_and(|fd| self.s().stat(fd.as_fd()).is_ok_and(|st| st.kind() == FileKind::Dir))
    }

    /// `dirs` collects the directories whose names the repair changed on the secondary (their times then differ
    /// from the primary's: [`Engine::restore_dir_times`] makes them equal again, after the object's lock is
    /// released).
    fn repair_object(&self, n: &Arc<Node>, path: &str, why: &str, dirs: &mut DirPairs) -> Result<(), String> {
        let _l = self.lock(&[(n.id, true)]);
        if !n.has_sec() {
            return Ok(());
        }
        let Some(mut sfd) = n.try_s() else { return Ok(()) };
        let pst = self.p().stat(n.p()).map_err(|x| e("stat primary", x))?;
        let mut sst = self.s().stat(sfd.as_fd()).map_err(|x| e("stat secondary", x))?;
        let wrong_target = pst.kind() == FileKind::Symlink
            && sst.kind() == FileKind::Symlink
            && self.p().readlink(n.p()).ok() != self.s().readlink(sfd.as_fd()).ok();
        if pst.kind() != sst.kind() || wrong_target {
            // Cannot be fixed in place: replace the directory entry.
            drop(sfd);
            return self.repair_by_path(n, path, why, dirs);
        }
        if pst.kind() != FileKind::Dir && pst.nlink != sst.nlink {
            // The link count is the symptom of a different hard-link structure: it cannot be fixed on the object,
            // only by making the names right.
            if let Err(x) = self.repair_links(n, &pst, path, why, dirs) {
                tracing::warn!("{path}: cannot repair the hard links: {x}");
            }
            sfd = n.try_s().ok_or("the secondary object is gone")?;
            sst = self.s().stat(sfd.as_fd()).map_err(|x| e("stat secondary", x))?;
        }
        self.quarantine_fd(sfd.as_fd(), &sst, path, why);
        let res = self
            .copy_state(n.p(), sfd.as_fd(), &pst)
            .and_then(|()| self.verify_objects(n.p(), sfd.as_fd(), n.kind));
        *n.ctimes.lock() = None;
        if res.is_err() {
            self.nodes.set_secondary(n, None);
        }
        res
    }

    /// Makes the names of the non-directory `n` (whose link count differs between the file systems) the same on
    /// both: every name the primary has for the inode (found by a bounded breadth-first walk of both trees from the
    /// root, in lockstep) must lead to one secondary inode, and no other secondary name may lead to it.
    fn repair_links(&self, n: &Arc<Node>, pst: &Stat, path: &str, why: &str, changed: &mut DirPairs) -> Result<(), String> {
        const MAX_ENTRIES: usize = 20_000;
        let (p, s) = (self.p(), self.s());
        let root = self.nodes.get(super::ROOT_ID).ok_or("no root")?;
        let mut dirs: Vec<(OwnedFd, OwnedFd)> = Vec::new();
        let mut names: Vec<(usize, CString)> = Vec::new();
        let mut queue = VecDeque::new();
        let root_s = root.try_s().ok_or("the root has no secondary")?;
        queue.push_back((crate::sys::dup(root.p()).map_err(|x| e("dup", x))?, crate::sys::dup(root_s.as_fd()).map_err(|x| e("dup", x))?));
        let mut seen = 0;
        while let Some((pd, sd)) = queue.pop_front() {
            let idx = dirs.len();
            for ent in self.list(p, pd.as_fd())? {
                seen += 1;
                if seen > MAX_ENTRIES {
                    return Err(format!("more than {MAX_ENTRIES} entries to search"));
                }
                let Ok(c) = CString::new(ent.name) else { continue };
                if ent.ino == pst.ino {
                    names.push((idx, c));
                } else if ent.kind.is_none_or(|k| k == FileKind::Dir)
                    && let (Ok((cp, st)), Ok((cs, sst))) = (p.lookup(pd.as_fd(), &c), s.lookup(sd.as_fd(), &c))
                        && st.kind() == FileKind::Dir && sst.kind() == FileKind::Dir {
                            queue.push_back((cp, cs));
                        }
            }
            dirs.push((pd, sd));
        }
        // the secondary inode all the names must lead to: the node's own, unless it is gone
        let mut target = None;
        if let Some(nfd) = n.try_s()
            && let Ok(fd) = crate::sys::dup(nfd.as_fd()) {
                target = s.stat(fd.as_fd()).ok().filter(|st| st.nlink > 0 && st.kind() == pst.kind()).map(|st| (fd, st));
            }
        if target.is_none() {
            for (i, c) in &names {
                if let Ok((fd, st)) = s.lookup(dirs[*i].1.as_fd(), c)
                    && st.kind() == pst.kind() {
                        target = Some((fd, st));
                        break;
                    }
            }
        }
        let (tfd, tst) = target.ok_or("no secondary object to link the names to")?;
        let mut errors = Vec::new();
        let mut touched = vec![false; dirs.len()];
        for (i, c) in &names {
            let (pd, sd) = &dirs[*i];
            let name = String::from_utf8_lossy(c.to_bytes()).into_owned();
            match s.stat_at(sd.as_fd(), c) {
                Ok(st) if st.ident() == tst.ident() => continue,
                Ok(st) if st.kind() == FileKind::Dir => {
                    errors.push(format!("{name}: a directory on the secondary"));
                    continue;
                }
                Ok(_) => {
                    // another object under this name: replace it
                    self.quarantine_at(sd.as_fd(), c, &format!("{}/{name}", self.path_of_dir(pd.as_fd())), why);
                    touched[*i] = true;
                    if let Err(x) = s.unlink(sd.as_fd(), c) {
                        errors.push(e(&format!("unlink {name}"), x));
                        continue;
                    }
                }
                Err(_) => {}
            }
            touched[*i] = true;
            if let Err(x) = s.link(tfd.as_fd(), sd.as_fd(), c) {
                errors.push(e(&format!("link {name}"), x));
            }
        }
        // names of the secondary inode that the primary does not have
        for (i, (pd, sd)) in dirs.iter().enumerate() {
            let Ok(list) = self.list(s, sd.as_fd()) else { continue };
            for ent in list.into_iter().filter(|x| x.ino == tst.ino && x.kind != Some(FileKind::Dir)) {
                let Ok(c) = CString::new(ent.name) else { continue };
                if s.stat_at(sd.as_fd(), &c).is_ok_and(|st| st.ident() != tst.ident()) {
                    continue;
                }
                if p.stat_at(pd.as_fd(), &c).is_ok_and(|st| st.ino == pst.ino) {
                    continue;
                }
                let name = String::from_utf8_lossy(c.to_bytes()).into_owned();
                self.quarantine_at(sd.as_fd(), &c, &format!("{}/{name}", self.path_of_dir(pd.as_fd())), why);
                touched[i] = true;
                if let Err(x) = s.unlink(sd.as_fd(), &c) {
                    errors.push(e(&format!("unlink {name}"), x));
                }
            }
        }
        changed.extend(dirs.into_iter().zip(touched).filter_map(|(d, t)| t.then_some(d)));
        if n.sident() != Some(tst.ident()) {
            self.nodes.set_secondary(n, Some((tfd, tst.ident())));
        }
        tracing::debug!("{path}: hard links of {} repaired ({} names)", n.id, names.len());
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    /// Path (relative to the mount root, "/" for the root itself) of a primary directory descriptor.
    fn path_of_dir(&self, fd: BorrowedFd<'_>) -> String {
        match crate::sys::fd_path(fd) {
            Some(p) => match p.strip_prefix(&self.primary_root) {
                Ok(rel) if rel.as_os_str().is_empty() => String::new(),
                Ok(rel) => format!("/{}", rel.display()),
                Err(_) => p.display().to_string(),
            },
            None => String::new(),
        }
    }

    /// Replaces the secondary entry of `n` found by its current path (for
    /// type and link-target differences, which cannot be repaired in place).
    fn repair_by_path(&self, n: &Arc<Node>, path: &str, why: &str, dirs: &mut DirPairs) -> Result<(), String> {
        if path.ends_with(" (deleted)") {
            self.nodes.set_secondary(n, None);
            return Err("the object has no name on the primary any more".into());
        }
        let rel = Path::new(path.trim_start_matches('/'));
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
            return Err("cannot replace the root".into());
        };
        let walk = |be: &dyn Backend| -> SysResult<OwnedFd> {
            let mut fd = be.root()?;
            for c in parent.components() {
                let c = CString::new(c.as_os_str().as_bytes()).map_err(|_| libc::EINVAL)?;
                fd = be.lookup(fd.as_fd(), &c)?.0;
            }
            Ok(fd)
        };
        let pdir = walk(self.p()).map_err(|x| e("primary parent", x))?;
        let sdir = walk(self.s()).map_err(|x| e("secondary parent", x))?;
        let c = cname(name)?;
        let mut cc = CopyCtx { links: HashMap::new(), scope: (pdir.as_fd(), sdir.as_fd()) };
        let res = self.repair_entry(pdir.as_fd(), sdir.as_fd(), &c, path, why, &mut cc);
        drop(cc);
        match self.s().lookup(sdir.as_fd(), &c) {
            Ok((fd, st)) if st.kind() == n.kind => self.nodes.set_secondary(n, Some((fd, st.ident()))),
            _ => self.nodes.set_secondary(n, None),
        }
        *n.ctimes.lock() = None;
        let res = res.and_then(|()| self.verify_entry(pdir.as_fd(), sdir.as_fd(), &c));
        dirs.push((pdir, sdir));
        res
    }

    /// Sets the times of each secondary directory in `dirs` (whose names a repair changed) to the primary's, under
    /// the directory's lock when it is a known node: the change of names stamped the secondary's times only.
    fn restore_dir_times(&self, mut dirs: DirPairs) {
        let (p, s) = (self.p(), self.s());
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|(pd, _)| p.stat(pd.as_fd()).is_ok_and(|st| seen.insert(st.ino)));
        for (pd, sd) in dirs {
            let Ok(pst) = p.stat(pd.as_fd()) else { continue };
            let node = self.nodes.get(self.map_ino(pst.ino));
            let _l = node.as_ref().map(|n| self.lock(&[(n.id, true)]));
            // (again under the lock: an operation may have changed the directory meanwhile, on both sides)
            let Ok(pst) = p.stat(pd.as_fd()) else { continue };
            let _ = s.utimens(sd.as_fd(), FileKind::Dir, None, TimeSpec::Set(pst.atime), TimeSpec::Set(pst.mtime));
            if let Some(n) = &node {
                *n.ctimes.lock() = None;
            }
        }
    }

    // ------------------------------------------------------------ entries

    fn repair_entries(&self, dir: &Arc<Node>, names: Option<Vec<OsString>>, why: &str) -> Result<(), String> {
        let partial = names.is_some();
        if !dir.has_sec() {
            return Err("the directory itself is missing on the secondary".into());
        }
        let names: Vec<CString> = match names {
            Some(v) => v.iter().map(|n| cname(n)).collect::<Result<_, _>>()?,
            None => {
                let sd = dir.try_s().ok_or("the directory itself is missing on the secondary")?;
                let (pl, sl) = (self.list(self.p(), dir.p())?, self.list(self.s(), sd.as_fd())?);
                let d = compare::diff_dir(&pl, &sl);
                d.only_primary
                    .into_iter()
                    .chain(d.only_secondary)
                    .chain(d.type_differs)
                    .map(|n| CString::new(n).map_err(|_| "name contains NUL".to_string()))
                    .collect::<Result<_, _>>()?
            }
        };
        let children: Vec<(&Node, &CStr)> = names.iter().map(|c| (&**dir, c.as_c_str())).collect();
        let (_l, _) = self.lock_children(&[(dir.id, true)], &children, true);
        // (it may have lost its secondary since the check above: that happens under the lock just taken)
        let sdir = dir.try_s().ok_or("the directory itself is missing on the secondary")?;
        let mut cc = CopyCtx { links: HashMap::new(), scope: (dir.p(), sdir.as_fd()) };
        let mut errors = Vec::new();
        for c in &names {
            let path = self.child_path(dir, OsStr::from_bytes(c.as_bytes()));
            if let Err(x) = self.repair_entry(dir.p(), sdir.as_fd(), c, &path, why, &mut cc) {
                errors.push(format!("{path}: {x}"));
            }
        }
        // The directory's own attributes (the repairs also changed its times
        // on the secondary only).
        if let Ok(pst) = self.p().stat(dir.p()) {
            self.copy_attrs(dir.p(), sdir.as_fd(), &pst);
        }
        *dir.ctimes.lock() = None;
        // Reconnect known nodes to their (new) secondary objects.
        for c in &names {
            self.reconnect(dir.p(), sdir.as_fd(), c, 0);
        }
        for c in &names {
            if let Err(x) = self.verify_entry(dir.p(), sdir.as_fd(), c) {
                errors.push(format!("{}: {x}", String::from_utf8_lossy(c.as_bytes())));
            }
        }
        // (only the named entries were repaired: other names of the directory may legitimately still differ, which
        // shows in its listing and link count; those have their own mismatches and repairs)
        let dir_check = if partial { self.verify_dir_attrs(dir.p(), sdir.as_fd()) } else { self.verify_objects(dir.p(), sdir.as_fd(), FileKind::Dir) };
        if let Err(x) = dir_check {
            errors.push(format!("the directory: {x}"));
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    /// Points the node known for the primary object `pdir/name` at the secondary object now found at `sdir/name`
    /// (or detaches it when there is none of the same type). When a directory got a new secondary object, its
    /// content was copied too: the nodes below it were attached to the removed copies and follow.
    fn reconnect(&self, pdir: BorrowedFd<'_>, sdir: BorrowedFd<'_>, name: &CStr, depth: usize) {
        let Ok((pfd, pst)) = self.p().lookup(pdir, name) else { return };
        let Some(n) = self.nodes.get(self.map_ino(pst.ino)) else { return };
        let sl = self.s().lookup(sdir, name).ok();
        let mut changed = false;
        match &sl {
            Some((fd, sst)) if sst.kind() == n.kind => {
                if n.sident() != Some(sst.ident())
                    && let Ok(fd) = crate::sys::dup(fd.as_fd()) {
                        self.nodes.set_secondary(&n, Some((fd, sst.ident())));
                        changed = true;
                    }
            }
            // (only the entries named by the repair are locked: below them, a secondary is replaced but never taken
            // away, because operations that hold the node's lock rely on it)
            _ if depth == 0 && n.has_sec() => self.nodes.set_secondary(&n, None),
            _ => {}
        }
        *n.ctimes.lock() = None;
        if let (true, FileKind::Dir, Some((sfd, _)), true) = (changed, n.kind, &sl, depth < 64) {
            for ent in self.list(self.p(), pfd.as_fd()).unwrap_or_default() {
                if let Ok(c) = CString::new(ent.name) {
                    self.reconnect(pfd.as_fd(), sfd.as_fd(), &c, depth + 1);
                }
            }
        }
    }

    /// Makes `sdir/name` equal to `pdir/name`.
    fn repair_entry(
        &self,
        pdir: BorrowedFd<'_>,
        sdir: BorrowedFd<'_>,
        name: &CStr,
        path: &str,
        why: &str,
        cc: &mut CopyCtx<'_>,
    ) -> Result<(), String> {
        let pst = self.p().stat_at(pdir, name).ok();
        let sst = self.s().stat_at(sdir, name).ok();
        match (pst, sst) {
            (None, None) => Ok(()),
            (None, Some(_)) => {
                self.quarantine_at(sdir, name, path, why);
                self.remove_tree(sdir, name)
            }
            (Some(_), None) => self.copy_tree(pdir, sdir, name, cc),
            (Some(ps), Some(ss)) => {
                let wrong_identity = ps.nlink > 1
                    && self.nodes.get(self.map_ino(ps.ino)).and_then(|n| n.sident()).is_some_and(|i| i != ss.ident());
                let wrong_target = ps.kind() == FileKind::Symlink
                    && ss.kind() == FileKind::Symlink
                    && self.readlink_at(self.p(), pdir, name) != self.readlink_at(self.s(), sdir, name);
                // The primary's object has this one name, the secondary's has others: it is not this object
                // (RENAME_EXCHANGE of hard-linked files that failed on the secondary, for example). Repairing it
                // in place would overwrite the content of those other names.
                let shared = ps.kind() != FileKind::Dir && ps.nlink == 1 && ss.nlink > 1;
                if ps.kind() != ss.kind() || wrong_identity || shared || wrong_target {
                    self.quarantine_at(sdir, name, path, why);
                    self.remove_tree(sdir, name)?;
                    self.copy_tree(pdir, sdir, name, cc)
                } else {
                    let (pfd, pst) = self.p().lookup(pdir, name).map_err(|x| e("lookup primary", x))?;
                    let (sfd, sst) = self.s().lookup(sdir, name).map_err(|x| e("lookup secondary", x))?;
                    if pst.kind() != FileKind::Dir {
                        self.quarantine_fd(sfd.as_fd(), &sst, path, why);
                    }
                    // Directories: attributes only; their contents are
                    // repaired by their own mismatches.
                    self.copy_state(pfd.as_fd(), sfd.as_fd(), &pst)
                }
            }
        }
    }

    fn readlink_at(&self, be: &dyn Backend, dir: BorrowedFd<'_>, name: &CStr) -> Option<Vec<u8>> {
        be.lookup(dir, name).ok().and_then(|(fd, _)| be.readlink(fd.as_fd()).ok())
    }

    fn list(&self, be: &dyn Backend, node: BorrowedFd<'_>) -> Result<Vec<crate::backend::DirEntry>, String> {
        let d = be.opendir(node).map_err(|x| e("opendir", x))?;
        be.readdir(d.as_fd()).map_err(|x| e("readdir", x))
    }

    /// Looks for another name of primary inode `ino` under `scope` (bounded
    /// breadth-first walk) whose secondary counterpart exists, to recreate a
    /// hard link whose other names the kernel never looked up.
    fn find_link(&self, scope: (BorrowedFd<'_>, BorrowedFd<'_>), ino: u64, kind: FileKind) -> Option<OwnedFd> {
        const MAX_ENTRIES: usize = 20_000;
        let (p, s) = (self.p(), self.s());
        let mut queue = VecDeque::new();
        queue.push_back((crate::sys::dup(scope.0).ok()?, crate::sys::dup(scope.1).ok()?));
        let mut seen = 0;
        while let Some((pd, sd)) = queue.pop_front() {
            for ent in self.list(p, pd.as_fd()).ok()? {
                seen += 1;
                if seen > MAX_ENTRIES {
                    return None;
                }
                let Ok(c) = CString::new(ent.name) else { continue };
                if ent.ino == ino {
                    if let Ok((fd, st)) = s.lookup(sd.as_fd(), &c)
                        && st.kind() == kind && p.stat_at(pd.as_fd(), &c).is_ok_and(|x| x.ino == ino)
                        {
                            return Some(fd);
                        }
                } else if ent.kind.is_none_or(|k| k == FileKind::Dir)
                    && let (Ok((cp, st)), Ok((cs, _))) = (p.lookup(pd.as_fd(), &c), s.lookup(sd.as_fd(), &c))
                        && st.kind() == FileKind::Dir {
                            queue.push_back((cp, cs));
                        }
            }
        }
        None
    }

    /// Copies `pdir/name` (recursively) to `sdir/name`, which must not exist.
    fn copy_tree(
        &self,
        pdir: BorrowedFd<'_>,
        sdir: BorrowedFd<'_>,
        name: &CStr,
        cc: &mut CopyCtx<'_>,
    ) -> Result<(), String> {
        let links = &mut cc.links;
        let (p, s) = (self.p(), self.s());
        let (pfd, pst) = p.lookup(pdir, name).map_err(|x| e("lookup primary", x))?;
        if pst.kind() != FileKind::Dir && pst.nlink > 1 {
            // Preserve hard links: to a copy made by this repair, or to the secondary object of a node known to be
            // linked, or to another name found by a bounded search.
            let target = links.get(&pst.ident()).map(|f| f.as_fd().try_clone_to_owned());
            let target = match target {
                Some(t) => t.ok(),
                None => self
                    .nodes
                    .get(self.map_ino(pst.ino))
                    .and_then(|n| crate::sys::dup(n.try_s()?.as_fd()).ok())
                    // the node's secondary object may be gone (that can be the very divergence being repaired): an
                    // unlinked inode cannot be linked to
                    .filter(|fd| s.stat(fd.as_fd()).is_ok_and(|st| st.nlink > 0 && st.kind() == pst.kind()))
                    .or_else(|| self.find_link(cc.scope, pst.ino, pst.kind())),
            };
            if let Some(t) = target {
                match s.link(t.as_fd(), sdir, name) {
                    Ok(()) => return Ok(()),
                    // fall back to a copy; verification reports the link count if it still differs
                    Err(x) => tracing::warn!("cannot link {}: {}; copying instead", String::from_utf8_lossy(name.to_bytes()), fmt_errno(x)),
                }
            }
        }
        match pst.kind() {
            FileKind::Regular => {
                let out = s.create(sdir, name, libc::O_WRONLY | libc::O_EXCL, 0o600).map_err(|x| e("create", x))?;
                let inp = p.open(pfd.as_fd(), libc::O_RDONLY).map_err(|x| e("open primary", x))?;
                let mut buf = self.bufs.get(CHUNK);
                let mut off = 0u64;
                loop {
                    let k = p.pread(inp.as_fd(), &mut buf, off).map_err(|x| e("read primary", x))?;
                    if k == 0 {
                        break;
                    }
                    write_all(s, out.as_fd(), &buf[..k], off)?;
                    off += k as u64;
                }
                self.bufs.put(buf);
                drop(out);
                let (sfd, _) = s.lookup(sdir, name).map_err(|x| e("lookup copy", x))?;
                self.copy_attrs(pfd.as_fd(), sfd.as_fd(), &pst);
                if pst.nlink > 1 {
                    cc.links.insert(pst.ident(), sfd);
                }
                Ok(())
            }
            FileKind::Dir => {
                s.mkdir(sdir, name, 0o700).map_err(|x| e("mkdir", x))?;
                let (sfd, _) = s.lookup(sdir, name).map_err(|x| e("lookup copy", x))?;
                for ent in self.list(p, pfd.as_fd())? {
                    let c = CString::new(ent.name).map_err(|_| "name contains NUL".to_string())?;
                    self.copy_tree(pfd.as_fd(), sfd.as_fd(), &c, cc)?;
                }
                self.copy_attrs(pfd.as_fd(), sfd.as_fd(), &pst);
                Ok(())
            }
            FileKind::Symlink => {
                let t = p.readlink(pfd.as_fd()).map_err(|x| e("readlink", x))?;
                let t = CString::new(t).map_err(|_| "target contains NUL".to_string())?;
                s.symlink(&t, sdir, name).map_err(|x| e("symlink", x))?;
                let (sfd, _) = s.lookup(sdir, name).map_err(|x| e("lookup copy", x))?;
                self.copy_attrs(pfd.as_fd(), sfd.as_fd(), &pst);
                if pst.nlink > 1 {
                    cc.links.insert(pst.ident(), sfd);
                }
                Ok(())
            }
            _ => {
                s.mknod(sdir, name, pst.mode, pst.rdev).map_err(|x| e("mknod", x))?;
                let (sfd, _) = s.lookup(sdir, name).map_err(|x| e("lookup copy", x))?;
                self.copy_attrs(pfd.as_fd(), sfd.as_fd(), &pst);
                if pst.nlink > 1 {
                    cc.links.insert(pst.ident(), sfd);
                }
                Ok(())
            }
        }
    }

    /// Removes `sdir/name` from the secondary, recursively.
    fn remove_tree(&self, sdir: BorrowedFd<'_>, name: &CStr) -> Result<(), String> {
        let s = self.s();
        let st = s.stat_at(sdir, name).map_err(|x| e("stat", x))?;
        if st.kind() == FileKind::Dir {
            let (fd, _) = s.lookup(sdir, name).map_err(|x| e("lookup", x))?;
            for ent in self.list(s, fd.as_fd())? {
                let c = CString::new(ent.name).map_err(|_| "name contains NUL".to_string())?;
                self.remove_tree(fd.as_fd(), &c)?;
            }
            s.rmdir(sdir, name).map_err(|x| e("rmdir", x))
        } else {
            s.unlink(sdir, name).map_err(|x| e("unlink", x))
        }
    }

    // -------------------------------------------------------------- state

    /// Content (regular files) and attributes of one object.
    fn copy_state(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>, pst: &Stat) -> Result<(), String> {
        if pst.kind() == FileKind::Regular {
            self.copy_content(pfd, sfd, pst.size)?;
        }
        self.copy_attrs(pfd, sfd, pst);
        Ok(())
    }

    /// Rewrites only the chunks that differ, then fixes the size.
    fn copy_content(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>, size: u64) -> Result<(), String> {
        let (p, s) = (self.p(), self.s());
        let pf = p.open(pfd, libc::O_RDONLY).map_err(|x| e("open primary", x))?;
        // A read-only file (mode 0444, say) cannot be opened for writing by its owner without CAP_DAC_OVERRIDE:
        // give it the owner's write permission for the time of the repair (`copy_attrs` sets the mode afterwards,
        // also on the error path below).
        let mut restore = None;
        let sf = match s.open(sfd, libc::O_RDWR) {
            Err(libc::EACCES) => {
                if let Ok(st) = s.stat(sfd)
                    && s.chmod(sfd, FileKind::Regular, st.perm() | 0o200).is_ok()
                {
                    restore = Some(st.perm());
                }
                s.open(sfd, libc::O_RDWR)
            }
            r => r,
        };
        let sf = match sf {
            Ok(f) => f,
            Err(x) => {
                if let Some(m) = restore {
                    let _ = s.chmod(sfd, FileKind::Regular, m);
                }
                return Err(e("open secondary", x));
            }
        };
        let res = self.copy_content_to(p, pf.as_fd(), s, sf.as_fd(), sfd, size);
        if let Some(m) = restore {
            let _ = s.chmod(sfd, FileKind::Regular, m);
        }
        res
    }

    fn copy_content_to(
        &self,
        p: &dyn Backend,
        pf: BorrowedFd<'_>,
        s: &dyn Backend,
        sf: BorrowedFd<'_>,
        sfd: BorrowedFd<'_>,
        size: u64,
    ) -> Result<(), String> {
        // Only ranges holding data on either side need looking at (sparse
        // files); everything else is a hole on both sides.
        let ssize = s.stat(sf).map(|st| st.size).unwrap_or(0);
        let ranges = self.data_ranges(pf, sf, size.max(ssize));
        let mut a = self.bufs.get(CHUNK);
        let mut b = self.bufs.get(CHUNK);
        let mut res = Ok(());
        'ranges: for (start, end) in ranges {
            let mut off = start;
            while off < end {
                let want = ((end - off) as usize).min(CHUNK);
                let k = match p.pread(pf, &mut a[..want], off) {
                    Ok(k) => k,
                    Err(x) => {
                        res = Err(e("read primary", x));
                        break 'ranges;
                    }
                };
                if k == 0 {
                    break; // beyond the primary's end: truncated below
                }
                let same = matches!(s.pread(sf, &mut b[..k], off), Ok(j) if j == k && a[..k] == b[..k]);
                if !same && let Err(x) = write_all(s, sf, &a[..k], off) {
                    res = Err(x);
                    break 'ranges;
                }
                off += k as u64;
            }
        }
        self.bufs.put(a);
        self.bufs.put(b);
        res?;
        s.truncate(sfd, Some(sf), size).map_err(|x| e("truncate", x))?;
        s.fsync(sf, false).map_err(|x| e("fsync", x))
    }

    /// xattrs, owner, mode and times. Failures surface in verification.
    fn copy_attrs(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>, pst: &Stat) {
        let (p, s) = (self.p(), self.s());
        let kind = pst.kind();
        if let (Ok(XattrOut::Data(pl)), Ok(XattrOut::Data(sl))) = (p.listxattr(pfd, XATTR_MAX), s.listxattr(sfd, XATTR_MAX)) {
            let pn = compare::xattr_names(&pl);
            for name in compare::xattr_names(&sl) {
                if !pn.contains(&name)
                    && let Ok(c) = crate::sys::cstr(name) {
                        let _ = s.removexattr(sfd, &c);
                    }
            }
            for name in pn {
                let Ok(c) = crate::sys::cstr(name) else { continue };
                if let Ok(XattrOut::Data(v)) = p.getxattr(pfd, &c, XATTR_MAX)
                    && s.getxattr(sfd, &c, XATTR_MAX).ok() != Some(XattrOut::Data(v.clone())) {
                        let _ = s.setxattr(sfd, &c, &v, 0);
                    }
            }
        }
        let _ = s.chown(sfd, Some(pst.uid), Some(pst.gid));
        if kind != FileKind::Symlink {
            // after chown, which may clear setuid/setgid
            let _ = s.chmod(sfd, kind, pst.perm());
        }
        let _ = s.utimens(sfd, kind, None, TimeSpec::Set(pst.atime), TimeSpec::Set(pst.mtime));
    }

    // ------------------------------------------------------- verification

    fn verify_objects(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>, kind: FileKind) -> Result<(), String> {
        let pst = self.p().stat(pfd).map_err(|x| e("stat primary", x))?;
        let sst = self.s().stat(sfd).map_err(|x| e("stat secondary", x))?;
        let diffs = compare::diff_stat(&pst, &sst, &self.attr_rules);
        if !diffs.is_empty() {
            let d: Vec<String> = diffs.iter().map(|d| format!("{} {} vs {}", d.field, d.primary, d.secondary)).collect();
            return Err(format!("still differs: {}", d.join(", ")));
        }
        match kind {
            FileKind::Regular => self.compare_files(pfd, sfd)?,
            FileKind::Symlink => {
                if self.p().readlink(pfd).ok() != self.s().readlink(sfd).ok() {
                    return Err("link target still differs".into());
                }
            }
            FileKind::Dir => {
                let d = compare::diff_dir(&self.list(self.p(), pfd)?, &self.list(self.s(), sfd)?);
                if !d.is_empty() {
                    return Err(format!("listing still differs: {}", d.describe(5)));
                }
            }
            _ => {}
        }
        let names = |be: &dyn Backend, fd| match be.listxattr(fd, XATTR_MAX) {
            Ok(XattrOut::Data(v)) => compare::xattr_names(&v).into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        if names(self.p(), pfd) != names(self.s(), sfd) {
            return Err("xattr names still differ".into());
        }
        Ok(())
    }

    /// A directory's own attributes, without its link count and listing.
    fn verify_dir_attrs(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>) -> Result<(), String> {
        let pst = self.p().stat(pfd).map_err(|x| e("stat primary", x))?;
        let sst = self.s().stat(sfd).map_err(|x| e("stat secondary", x))?;
        let diffs: Vec<_> = compare::diff_stat(&pst, &sst, &self.attr_rules).into_iter().filter(|d| d.field != "nlink").collect();
        if !diffs.is_empty() {
            let d: Vec<String> = diffs.iter().map(|d| format!("{} {} vs {}", d.field, d.primary, d.secondary)).collect();
            return Err(format!("still differs: {}", d.join(", ")));
        }
        Ok(())
    }

    fn verify_entry(&self, pdir: BorrowedFd<'_>, sdir: BorrowedFd<'_>, name: &CStr) -> Result<(), String> {
        let pl = self.p().lookup(pdir, name).ok();
        let sl = self.s().lookup(sdir, name).ok();
        match (pl, sl) {
            (None, None) => Ok(()),
            (Some(_), None) => Err("still missing on the secondary".into()),
            (None, Some(_)) => Err("still present on the secondary".into()),
            (Some((pfd, pst)), Some((sfd, sst))) => {
                if pst.kind() != sst.kind() {
                    return Err("type still differs".into());
                }
                if let Some(si) = self.nodes.get(self.map_ino(pst.ino)).and_then(|n| n.sident())
                    && si != sst.ident() {
                        return Err("hard-link identity still differs".into());
                    }
                self.verify_objects(pfd.as_fd(), sfd.as_fd(), pst.kind())
            }
        }
    }

    fn compare_files(&self, pfd: BorrowedFd<'_>, sfd: BorrowedFd<'_>) -> Result<(), String> {
        let pf = self.p().open(pfd, libc::O_RDONLY).map_err(|x| e("open primary", x))?;
        let sf = self.s().open(sfd, libc::O_RDONLY).map_err(|x| e("open secondary", x))?;
        let (ps, ss) = (self.p().stat(pf.as_fd()), self.s().stat(sf.as_fd()));
        let (Ok(ps), Ok(ss)) = (ps, ss) else { return Err("stat error while verifying".into()) };
        if ps.size != ss.size {
            return Err(format!("size still differs: {} vs {}", ps.size, ss.size));
        }
        let mut a = self.bufs.get(CHUNK);
        let mut b = self.bufs.get(CHUNK);
        let mut res = Ok(());
        'ranges: for (start, end) in self.data_ranges(pf.as_fd(), sf.as_fd(), ps.size) {
            let mut off = start;
            while off < end {
                let want = ((end - off) as usize).min(CHUNK);
                let (Ok(k), Ok(j)) =
                    (self.p().pread(pf.as_fd(), &mut a[..want], off), self.s().pread(sf.as_fd(), &mut b[..want], off))
                else {
                    res = Err("read error while verifying".to_string());
                    break 'ranges;
                };
                if let Some(d) = compare::diff_data(off, &a[..k], &b[..j]) {
                    res = Err(format!("content still differs: {d}"));
                    break 'ranges;
                }
                if k == 0 {
                    break;
                }
                off += k as u64;
            }
        }
        self.bufs.put(a);
        self.bufs.put(b);
        res
    }

    // --------------------------------------------------------- quarantine

    fn quarantine_at(&self, sdir: BorrowedFd<'_>, name: &CStr, path: &str, why: &str) {
        if self.cfg.quarantine.is_none() {
            return;
        }
        if let Ok((fd, st)) = self.s().lookup(sdir, name) {
            self.quarantine_fd(fd.as_fd(), &st, path, why);
        }
    }

    /// Copies the secondary's version of an object (recursively, up to
    /// `quarantine_cap` bytes) into a new directory under `--quarantine`,
    /// with a `mismatch.txt` describing why.
    fn quarantine_fd(&self, sfd: BorrowedFd<'_>, sst: &Stat, path: &str, why: &str) {
        let Some(root) = &self.cfg.quarantine else { return };
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let seq = self.quarantine_seq.fetch_add(1, Relaxed);
        let mut slug: String = path
            .trim_start_matches('/')
            .chars()
            .map(|c| if c == '/' { '%' } else if c.is_control() { '_' } else { c })
            .collect();
        slug.truncate(120);
        let dir = root.join(format!("{secs}-{seq:05}-{slug}"));
        let mut notes = Vec::new();
        let res = std::fs::create_dir_all(&dir).map_err(|x| x.to_string()).and_then(|()| {
            let mut budget = self.cfg.quarantine_cap;
            self.save(sfd, sst, &dir.join("object"), &mut budget, &mut notes, 0);
            let mut f = std::fs::File::create(dir.join("mismatch.txt")).map_err(|x| x.to_string())?;
            let _ = writeln!(f, "path: {path}\nreason: {why}\nsecondary stat: {sst:?}");
            for n in &notes {
                let _ = writeln!(f, "note: {n}");
            }
            Ok(())
        });
        match res {
            Ok(()) => {
                self.stats.quarantined.fetch_add(1, Relaxed);
                tracing::warn!("{path}: secondary version saved to {}", dir.display());
            }
            Err(x) => tracing::error!("{path}: quarantine to {} failed: {x}", dir.display()),
        }
    }

    fn save(&self, sfd: BorrowedFd<'_>, st: &Stat, dest: &Path, budget: &mut u64, notes: &mut Vec<String>, depth: usize) {
        let s = self.s();
        let note = |notes: &mut Vec<String>, m: String| notes.push(format!("{}: {m}", dest.display()));
        match st.kind() {
            FileKind::Regular => {
                let r = s.open(sfd, libc::O_RDONLY).map_err(|x| e("open", x)).and_then(|f| {
                    let mut out = std::fs::File::create(dest).map_err(|x| x.to_string())?;
                    let mut buf = vec![0u8; CHUNK];
                    let mut off = 0u64;
                    while *budget > 0 {
                        let want = (CHUNK as u64).min(*budget) as usize;
                        let k = s.pread(f.as_fd(), &mut buf[..want], off).map_err(|x| e("read", x))?;
                        if k == 0 {
                            return Ok(());
                        }
                        out.write_all(&buf[..k]).map_err(|x| x.to_string())?;
                        off += k as u64;
                        *budget -= k as u64;
                    }
                    if off < st.size {
                        Err(format!("truncated at {off} of {} bytes (--quarantine-cap)", st.size))
                    } else {
                        Ok(())
                    }
                });
                if let Err(x) = r {
                    note(notes, x);
                }
            }
            FileKind::Dir if depth < 64 => {
                if let Err(x) = std::fs::create_dir_all(dest) {
                    return note(notes, x.to_string());
                }
                let ents = match self.list(s, sfd) {
                    Ok(v) => v,
                    Err(x) => return note(notes, x),
                };
                for ent in ents {
                    if *budget == 0 {
                        return note(notes, "quarantine budget exhausted".into());
                    }
                    let Ok(c) = CString::new(ent.name.clone()) else { continue };
                    if let Ok((fd, cst)) = s.lookup(sfd, &c) {
                        self.save(fd.as_fd(), &cst, &dest.join(OsStr::from_bytes(&ent.name)), budget, notes, depth + 1);
                    }
                }
            }
            FileKind::Symlink => match s.readlink(sfd) {
                Ok(t) => {
                    if let Err(x) = std::os::unix::fs::symlink(OsStr::from_bytes(&t), dest) {
                        note(notes, x.to_string());
                    }
                }
                Err(x) => note(notes, e("readlink", x)),
            },
            k => note(notes, format!("{} mode {:o} rdev {:#x} not copied", k.name(), st.mode, st.rdev)),
        }
    }
}

fn write_all(s: &dyn Backend, fd: BorrowedFd<'_>, mut data: &[u8], mut off: u64) -> Result<(), String> {
    while !data.is_empty() {
        let w = s.pwrite(fd, data, off).map_err(|x| e("write secondary", x))?;
        if w == 0 {
            return Err("write secondary: wrote 0 bytes".into());
        }
        data = &data[w..];
        off += w as u64;
    }
    Ok(())
}
