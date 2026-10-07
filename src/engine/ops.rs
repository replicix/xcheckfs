//! The operations. Each one follows the pattern described in `engine/mod.rs`:
//! lock everything it observes or changes, run on both, compare, verify.

use std::ffi::{CStr, OsStr};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::node::Inserted;
use super::{Attr, Ctx, Cx, Engine, Fail, LockSet, Node, OpResult, OpenDir, OpenFile, ROOT_ID, SetAttr, Side, cname};
use crate::backend::{Backend, DirEntry, Lock, StatFs, TimeSpec, XattrOut};
use crate::compare;
use crate::policy::MismatchKind;
use crate::stats::OpKind;
use crate::sys::{self, FileKind, Stat, SysResult};

/// Largest range read back for verification of fallocate/copy_file_range.
const VERIFY_CAP: u64 = 16 << 20;
const XATTR_MAX: usize = 65536;

impl Engine {
    // ------------------------------------------------------------- helpers

    /// Locks `base` plus the children named in `children` (looked up on the
    /// primary), re-validating after locking that the names still refer to
    /// the same inodes. Returns the primary stats of the children.
    pub(crate) fn lock_children(
        &self,
        base: &[(u64, bool)],
        children: &[(&Node, &CStr)],
        excl: bool,
    ) -> (LockSet<'_>, Vec<Option<Stat>>) {
        let probe = |c: &[(&Node, &CStr)]| -> Vec<Option<Stat>> {
            c.iter().map(|(d, n)| self.b[0].stat_at(d.p(), n).ok()).collect()
        };
        let mut cands = probe(children);
        for attempt in 0.. {
            let mut ids: Vec<(u64, bool)> = base.to_vec();
            ids.extend(cands.iter().flatten().map(|st| (self.map_ino(st.ino), excl)));
            let l = self.lock(&ids);
            let now = probe(children);
            let same = now.iter().zip(cands.iter()).all(|(a, b)| a.map(|s| s.ident()) == b.map(|s| s.ident()));
            if same || attempt >= 16 {
                if !same {
                    tracing::warn!("name -> inode kept changing while locking; continuing");
                }
                return (l, now);
            }
            drop(l);
            cands = now;
        }
        unreachable!()
    }

    /// The node (other than `id`) that holds the secondary object `ident`, if it really does: a node whose primary
    /// object is gone (removed, but not forgotten yet) may still hold a secondary object that the secondary failed
    /// to remove and that a repair has since reused for a new file; such a claim does not count.
    fn secondary_claimed_by_other(&self, ident: (u64, u64), id: u64) -> Option<u64> {
        let other = self.nodes.by_secondary(ident).filter(|&o| o != id)?;
        let alive = self.nodes.get(other).is_some_and(|o| self.b[0].stat(o.p()).map_or(true, |st| st.nlink > 0));
        alive.then_some(other)
    }

    /// Checks of `finish_entry` for a node that is already known.
    #[allow(clippy::too_many_arguments)]
    fn check_existing(
        &self,
        cx: &mut Cx,
        parent: &Arc<Node>,
        name: &OsStr,
        n: &Arc<Node>,
        pst: &Stat,
        s: Option<(OwnedFd, Stat)>,
        excl: bool,
    ) -> OpResult<()> {
        let id = n.id;
        if n.pident != pst.ident() {
            tracing::error!("node {id} identity changed on the primary; refusing");
            return Err(Fail::Errno(libc::EIO));
        }
        if let (Some(a), Some((_, sst))) = (n.sident(), &s) {
            if a != sst.ident() {
                self.report_at(
                    cx,
                    parent,
                    Some(name),
                    MismatchKind::Identity,
                    None,
                    format!("same inode as {}", self.path_of(n)),
                    "a different inode".into(),
                    "hard-link structure differs".into(),
                    false,
                )?;
            }
        }
        if let Some((sfd, sst)) = s {
            if n.has_sec() {
                self.cmp_stat(cx, n, pst, &sst, excl, cx_what(cx.op))?;
            } else if sst.kind() == n.kind
                && self.secondary_claimed_by_other(sst.ident(), id).is_none()
                && !self.detached()
                && !self.repairs_exhausted(n)
            {
                // The object exists on the secondary again (repaired by a
                // resync of an ancestor): reconnect it. Safe under a shared
                // lock: operations that started without a secondary never
                // touch it. Connected first, so that a difference found now
                // can be repaired like any other.
                self.nodes.set_secondary(n, Some((sfd, sst.ident())));
                *n.ctimes.lock() = None;
                tracing::info!("{}: reconnected to the secondary", self.path_of(n));
                self.cmp_stat(cx, n, pst, &sst, excl, cx_what(cx.op))?;
            }
        }
        Ok(())
    }

    /// Turns a looked-up (primary, secondary) pair into a node + attributes,
    /// checking hard-link identity and attributes. Must be called with the
    /// parent locked; inserts the node only after all checks passed.
    #[allow(clippy::too_many_arguments)]
    fn finish_entry(
        &self,
        cx: &mut Cx,
        parent: &Arc<Node>,
        name: &OsStr,
        pfd: OwnedFd,
        pst: Stat,
        s: Option<(OwnedFd, Stat)>,
        excl: bool,
    ) -> OpResult<(Arc<Node>, Attr)> {
        if pst.dev != self.root_dev {
            tracing::error!(
                "{}: crosses a mount point on the primary; nested mounts are not supported",
                self.child_path(parent, name)
            );
            return Err(Fail::Errno(libc::EIO));
        }
        let id = self.map_ino(pst.ino);
        let attr = Attr { id, st: pst };
        // The lookup reference is taken atomically with finding the node: a FORGET for an older reference that
        // arrives while this lookup is held up (e.g. frozen on a mismatch) must not drop the node.
        if let Some(n) = self.nodes.get_ref(id) {
            if let Err(e) = self.check_existing(cx, parent, name, &n, &pst, s, excl) {
                // the reply will be an error (or the operation is retried): give the reference back
                self.nodes.forget(id, 1);
                return Err(e);
            }
            return Ok((n, attr));
        }
        if let Some((_, sst)) = &s {
            if let Some(other) = self.secondary_claimed_by_other(sst.ident(), id) {
                let other_path = self.nodes.get(other).map(|o| self.path_of(&o)).unwrap_or_default();
                self.report_at(
                    cx,
                    parent,
                    Some(name),
                    MismatchKind::Identity,
                    None,
                    "a different inode".into(),
                    format!("same inode as {other_path}"),
                    "hard-link structure differs".into(),
                    false,
                )?;
            }
        }
        let (sfd, sident, sst) = match s {
            Some((fd, st)) => (Some(fd), Some(st.ident()), Some(st)),
            None => (None, None, None),
        };
        let node = Arc::new(Node::new(id, pst.kind(), pfd, sfd, pst.ident(), sident, Engine::child_hint(parent, name)));
        if let Some(sst) = &sst {
            self.cmp_stat(cx, &node, &pst, sst, excl, cx_what(cx.op))?;
        }
        match self.nodes.insert_or_get(node) {
            Inserted::New(n) => {
                if !super::is_read_only(cx.op) {
                    Engine::touched_new(n.id);
                }
                self.stats.nodes.store(self.nodes.len() as u64, Relaxed);
                Ok((n, attr))
            }
            Inserted::Existing(n) => Ok((n, attr)),
        }
    }

    /// Reports when exactly one side fails an expectation; warns when both do.
    #[allow(clippy::too_many_arguments)]
    fn verify_sides(
        &self,
        cx: &mut Cx,
        node: &Arc<Node>,
        name: Option<&OsStr>,
        what: &str,
        p_ok: Result<(), String>,
        s_ok: Option<Result<(), String>>,
        excl: bool,
    ) -> OpResult<()> {
        self.stats.verifications.fetch_add(1, Relaxed);
        let Some(s_ok) = s_ok else { return Ok(()) };
        match (&p_ok, &s_ok) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(pe), Err(se)) => {
                tracing::warn!(
                    "{} verification '{what}' failed on BOTH file systems (primary: {pe}; secondary: {se})",
                    cx.op.name()
                );
                Ok(())
            }
            _ => self.report_at(
                cx,
                node,
                name,
                MismatchKind::Verify,
                Some(what.into()),
                p_ok.err().unwrap_or_else(|| "ok".into()),
                s_ok.err().unwrap_or_else(|| "ok".into()),
                String::new(),
                excl,
            ),
        }
    }

    /// Compares the complete listing of `dir` (paranoid mode).
    fn compare_listing(&self, cx: &mut Cx, dir: &Arc<Node>, what: &str) -> OpResult<()> {
        if !self.paranoid() || !self.sec_for(&[dir]) {
            return Ok(());
        }
        self.stats.verifications.fetch_add(1, Relaxed);
        let (p, s) = self.both(cx, true, None, |side, be| {
            let d = be.opendir(dir.fd(side).as_fd())?;
            be.readdir(d.as_fd())
        });
        if let (Ok(p), Some(Ok(s))) = (&p, &s) {
            self.cmp_dir(cx, dir, p, s, what)?;
        }
        Ok(())
    }

    /// Compares the complete content of a regular file (paranoid mode).
    pub(crate) fn compare_content(&self, cx: &mut Cx, n: &Arc<Node>, excl: bool) -> OpResult<()> {
        if n.kind != FileKind::Regular || !self.sec_for(&[n]) {
            return Ok(());
        }
        self.stats.verifications.fetch_add(1, Relaxed);
        let (p, s) = self.both(cx, true, None, |side, be| be.open(n.fd(side).as_fd(), libc::O_RDONLY));
        let (Ok(pf), Some(Ok(sf))) = (p, s) else { return Ok(()) };
        const CHUNK: usize = 1 << 20;
        let mut off = 0u64;
        loop {
            let (a, b) = self.both(cx, true, None, |side, be| {
                let fd = if side == Side::Primary { pf.as_fd() } else { sf.as_fd() };
                let mut buf = self.bufs.get(CHUNK);
                let k = be.pread(fd, &mut buf, off)?;
                buf.truncate(k);
                Ok(buf)
            });
            let (Ok(a), Some(Ok(b))) = (a, b) else { return Ok(()) };
            if let Some(d) = compare::diff_data(off, &a, &b) {
                return self.report(cx, n, MismatchKind::Content, None, format!("{} bytes read", a.len()), format!("{} bytes read", b.len()), d, excl);
            }
            let done = a.len() < CHUNK;
            off += a.len() as u64;
            self.bufs.put(a);
            self.bufs.put(b);
            if done {
                return Ok(());
            }
        }
    }

    /// Reads `len` bytes at `off` through a readable descriptor of `n`.
    fn read_back(&self, be: &dyn Backend, n: &Node, side: Side, off: u64, len: usize) -> SysResult<Vec<u8>> {
        let f = be.open(n.fd(side).as_fd(), libc::O_RDONLY)?;
        let mut buf = vec![0u8; len];
        let k = be.pread(f.as_fd(), &mut buf, off)?;
        buf.truncate(k);
        Ok(buf)
    }

    // -------------------------------------------------------------- lookup

    pub fn lookup(&self, _ctx: &Ctx, parent: u64, name: &OsStr) -> Result<Attr, i32> {
        self.run(OpKind::Lookup, parent, || self.detail_name(parent, name), |cx| {
            let pn = self.node(parent)?;
            let c = cname(name)?;
            let (_l, _) = self.lock_children(&[(parent, false)], &[(&pn, &c)], false);
            let sec = self.sec_for(&[&pn]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.lookup(pn.fd(side).as_fd(), &c));
            self.cmp_result(cx, &pn, Some(name), &p, &s, false)?;
            let (pfd, pst) = p?;
            let s = s.and_then(|r| r.ok());
            let (_, attr) = self.finish_entry(cx, &pn, name, pfd, pst, s, false)?;
            Ok(attr)
        })
    }

    pub fn forget(&self, ino: u64, nlookup: u64) {
        let st = self.stats.op(OpKind::Forget);
        st.count.fetch_add(1, Relaxed);
        self.nodes.forget(ino, nlookup);
        self.stats.nodes.store(self.nodes.len() as u64, Relaxed);
    }

    pub fn getattr(&self, _ctx: &Ctx, ino: u64) -> Result<Attr, i32> {
        self.run(OpKind::Getattr, ino, || self.detail_ino(ino), |cx| {
            let n = self.node(ino)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.stat(n.fd(side).as_fd()));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let pst = p?;
            if let Some(Ok(sst)) = &s {
                self.cmp_stat(cx, &n, &pst, sst, false, "getattr")?;
            }
            Ok(Attr { id: n.id, st: pst })
        })
    }

    pub fn setattr(&self, ctx: &Ctx, ino: u64, a: SetAttr, fh: Option<u64>) -> Result<Attr, i32> {
        self.run(OpKind::Setattr, ino, || format!("{} {:?}", self.detail_ino(ino), a), |cx| {
            let n = self.node(ino)?;
            let file = fh.and_then(|h| self.files.get(h));
            let _l = self.lock(&[(ino, true)]);
            let sec = self.sec_for(&[&n]);
            let creds = self.creds(ctx);
            let creds = creds.as_ref();
            if let Some(mode) = a.mode {
                let (p, s) = self.both(cx, sec, creds, |side, be| be.chmod(n.fd(side).as_fd(), n.kind, mode & 0o7777));
                self.cmp_result(cx, &n, None, &p, &s, true)?;
                p?;
            }
            if a.uid.is_some() || a.gid.is_some() {
                let (p, s) = self.both(cx, sec, creds, |side, be| be.chown(n.fd(side).as_fd(), a.uid, a.gid));
                self.cmp_result(cx, &n, None, &p, &s, true)?;
                p?;
            }
            if let Some(size) = a.size {
                let (p, s) = self.both(cx, sec, creds, |side, be| {
                    let f = file.as_ref().filter(|f| side == Side::Primary || f.has_sec());
                    be.truncate(n.fd(side).as_fd(), f.map(|f| f.fd(side)), size)
                });
                self.cmp_result(cx, &n, None, &p, &s, true)?;
                p?;
            }
            if a.atime.is_some() || a.mtime.is_some() {
                let at = a.atime.unwrap_or(TimeSpec::Omit);
                let mt = a.mtime.unwrap_or(TimeSpec::Omit);
                let (p, s) = self.both(cx, sec, creds, |side, be| be.utimens(n.fd(side).as_fd(), n.kind, None, at, mt));
                self.cmp_result(cx, &n, None, &p, &s, true)?;
                p?;
            }
            let (p, s) = self.both(cx, sec, None, |side, be| be.stat(n.fd(side).as_fd()));
            let pst = p?;
            if let Some(Ok(sst)) = &s {
                self.cmp_stat(cx, &n, &pst, sst, true, "after setattr")?;
                if self.thorough() {
                    let check = |st: &Stat| -> Result<(), String> {
                        let mut bad = Vec::new();
                        if let Some(m) = a.mode {
                            // setgid may legitimately be dropped by the kernel
                            if st.perm() & !0o2000 != (m & 0o7777) & !0o2000 {
                                bad.push(format!("mode {:04o} != requested {:04o}", st.perm(), m & 0o7777));
                            }
                        }
                        if a.uid.is_some_and(|u| u != st.uid) {
                            bad.push(format!("uid {} != requested {}", st.uid, a.uid.unwrap()));
                        }
                        if a.gid.is_some_and(|g| g != st.gid) {
                            bad.push(format!("gid {} != requested {}", st.gid, a.gid.unwrap()));
                        }
                        if a.size.is_some_and(|z| z != st.size) {
                            bad.push(format!("size {} != requested {}", st.size, a.size.unwrap()));
                        }
                        if let Some(TimeSpec::Set(t)) = a.mtime {
                            if st.mtime != t {
                                bad.push(format!("mtime {} != requested {t}", st.mtime));
                            }
                        }
                        if let Some(TimeSpec::Set(t)) = a.atime {
                            if st.atime != t {
                                bad.push(format!("atime {} != requested {t}", st.atime));
                            }
                        }
                        if bad.is_empty() { Ok(()) } else { Err(bad.join(", ")) }
                    };
                    self.verify_sides(cx, &n, None, "setattr applied", check(&pst), Some(check(sst)), true)?;
                }
            }
            Ok(Attr { id: n.id, st: pst })
        })
    }

    pub fn readlink(&self, _ctx: &Ctx, ino: u64) -> Result<Vec<u8>, i32> {
        self.run(OpKind::Readlink, ino, || self.detail_ino(ino), |cx| {
            let n = self.node(ino)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.readlink(n.fd(side).as_fd()));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let pt = p?;
            if let Some(Ok(st)) = &s {
                if &pt != st {
                    self.report(
                        cx,
                        &n,
                        MismatchKind::Readlink,
                        None,
                        String::from_utf8_lossy(&pt).into_owned(),
                        String::from_utf8_lossy(st).into_owned(),
                        String::new(),
                        false,
                    )?;
                }
            }
            Ok(pt)
        })
    }

    // ------------------------------------------------------- create family

    /// Runs a creating operation in `parent` and looks the new entry up.
    #[allow(clippy::type_complexity)]
    fn create_like(
        &self,
        cx: &mut Cx,
        ctx: &Ctx,
        parent: u64,
        name: &OsStr,
        op: &(dyn Fn(Side, &dyn Backend, std::os::fd::BorrowedFd<'_>, &CStr) -> SysResult<Option<OwnedFd>> + Sync),
    ) -> OpResult<(Arc<Node>, Attr, Option<OwnedFd>, Option<OwnedFd>)> {
        let pn = self.node(parent)?;
        let c = cname(name)?;
        let (_l, _) = self.lock_children(&[(parent, true)], &[(&pn, &c)], true);
        let sec = self.sec_for(&[&pn]);
        let creds = self.creds(ctx);
        let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| op(side, be, pn.fd(side).as_fd(), &c));
        self.cmp_result(cx, &pn, Some(name), &p, &s, true)?;
        let popen = p?;
        let (s_ok, sopen) = match s {
            Some(Ok(f)) => (true, f),
            _ => (false, None),
        };
        let (lp, ls) = self.both(cx, s_ok, None, |side, be| be.lookup(pn.fd(side).as_fd(), &c));
        // The secondary claimed success but the new name is not there (or the other way round).
        self.cmp_result(cx, &pn, Some(name), &lp, &ls, true)?;
        let (pfd, pst) = lp?;
        let ls = ls.and_then(|r| r.ok());
        let (n, attr) = self.finish_entry(cx, &pn, name, pfd, pst, ls, true)?;
        self.compare_listing(cx, &pn, "parent after create")?;
        Ok((n, attr, popen, sopen))
    }

    pub fn mknod(&self, ctx: &Ctx, parent: u64, name: &OsStr, mode: u32, rdev: u64) -> Result<Attr, i32> {
        self.run(OpKind::Mknod, parent, || format!("{} mode={mode:o}", self.detail_name(parent, name)), |cx| {
            let (_, attr, _, _) = self.create_like(cx, ctx, parent, name, &|_, be, d, c| be.mknod(d, c, mode, rdev).map(|_| None))?;
            Ok(attr)
        })
    }

    pub fn mkdir(&self, ctx: &Ctx, parent: u64, name: &OsStr, mode: u32) -> Result<Attr, i32> {
        self.run(OpKind::Mkdir, parent, || format!("{} mode={mode:o}", self.detail_name(parent, name)), |cx| {
            let (_, attr, _, _) = self.create_like(cx, ctx, parent, name, &|_, be, d, c| be.mkdir(d, c, mode).map(|_| None))?;
            Ok(attr)
        })
    }

    pub fn symlink(&self, ctx: &Ctx, parent: u64, name: &OsStr, target: &OsStr) -> Result<Attr, i32> {
        self.run(OpKind::Symlink, parent, || format!("{} -> {}", self.detail_name(parent, name), target.to_string_lossy()), |cx| {
            let t = cname(target)?;
            let (n, attr, _, _) = self.create_like(cx, ctx, parent, name, &|_, be, d, c| be.symlink(&t, d, c).map(|_| None))?;
            if self.thorough() && self.sec_for(&[&n]) {
                let (p, s) = self.both(cx, true, None, |side, be| be.readlink(n.fd(side).as_fd()));
                let chk = |r: SysResult<Vec<u8>>| match r {
                    Ok(v) if v == target.as_bytes() => Ok(()),
                    Ok(v) => Err(format!("target {:?}", String::from_utf8_lossy(&v))),
                    Err(e) => Err(sys::fmt_errno(e)),
                };
                self.verify_sides(cx, &n, None, "symlink target", chk(p), s.map(chk), true)?;
            }
            Ok(attr)
        })
    }

    pub fn create(&self, ctx: &Ctx, parent: u64, name: &OsStr, mode: u32, flags: i32) -> Result<(Attr, u64), i32> {
        self.run(OpKind::Create, parent, || format!("{} mode={mode:o} flags={flags:#x}", self.detail_name(parent, name)), |cx| {
            let fl = sanitize_open_flags(flags);
            let (n, attr, pf, sf) =
                self.create_like(cx, ctx, parent, name, &|_, be, d, c| be.create(d, c, fl, mode).map(Some))?;
            let pf = pf.ok_or(Fail::Errno(libc::EIO))?;
            let fh = self.install_file(n, pf, sf, fl);
            Ok((attr, fh))
        })
    }

    fn install_file(&self, n: Arc<Node>, pfd: OwnedFd, sfd: Option<OwnedFd>, flags: i32) -> u64 {
        let fh = self.alloc_fh();
        self.nodes.opened(&n);
        self.files.insert(fh, Arc::new(OpenFile { sec_gen: n.sec_gen(), node: n, pfd, sfd, flags, written: AtomicBool::new(false) }));
        self.stats.open_files.fetch_add(1, Relaxed);
        fh
    }

    // ------------------------------------------------------ remove family

    fn remove_like(&self, cx: &mut Cx, ctx: &Ctx, parent: u64, name: &OsStr, dir: bool) -> OpResult<()> {
        let pn = self.node(parent)?;
        let c = cname(name)?;
        let (_l, cands) = self.lock_children(&[(parent, true)], &[(&pn, &c)], true);
        let sec = self.sec_for(&[&pn]);
        let creds = self.creds(ctx);
        let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| {
            if dir { be.rmdir(pn.fd(side).as_fd(), &c) } else { be.unlink(pn.fd(side).as_fd(), &c) }
        });
        self.cmp_result(cx, &pn, Some(name), &p, &s, true)?;
        p?;
        if self.thorough() {
            let s_ok = matches!(s, Some(Ok(())));
            let (gp, gs) = self.both(cx, s_ok, None, |side, be| be.stat_at(pn.fd(side).as_fd(), &c));
            let chk = |r: SysResult<Stat>| match r {
                Err(libc::ENOENT) => Ok(()),
                Ok(_) => Err("name still exists".to_string()),
                Err(e) => Err(sys::fmt_errno(e)),
            };
            self.verify_sides(cx, &pn, Some(name), "removed", chk(gp), gs.map(chk), true)?;
            // The object itself may live on (hard links, open files): its
            // link count and ctime must have changed the same way.
            if let Some(child) = cands[0].and_then(|st| self.nodes.get(self.map_ino(st.ino))) {
                if self.sec_for(&[&child]) {
                    let (a, b) = self.both(cx, true, None, |side, be| be.stat(child.fd(side).as_fd()));
                    if let (Ok(a), Some(Ok(b))) = (a, b) {
                        self.cmp_stat(cx, &child, &a, &b, true, "after remove")?;
                    }
                }
            }
        }
        self.compare_listing(cx, &pn, "parent after remove")?;
        Ok(())
    }

    pub fn unlink(&self, ctx: &Ctx, parent: u64, name: &OsStr) -> Result<(), i32> {
        self.run(OpKind::Unlink, parent, || self.detail_name(parent, name), |cx| self.remove_like(cx, ctx, parent, name, false))
    }

    pub fn rmdir(&self, ctx: &Ctx, parent: u64, name: &OsStr) -> Result<(), i32> {
        self.run(OpKind::Rmdir, parent, || self.detail_name(parent, name), |cx| self.remove_like(cx, ctx, parent, name, true))
    }

    pub fn rename(
        &self,
        ctx: &Ctx,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
    ) -> Result<(), i32> {
        self.run(
            OpKind::Rename,
            parent,
            || format!("{} -> {} flags={flags:#x}", self.detail_name(parent, name), self.detail_name(newparent, newname)),
            |cx| {
                let pn = self.node(parent)?;
                let npn = self.node(newparent)?;
                let c = cname(name)?;
                let nc = cname(newname)?;
                let (_l, cands) =
                    self.lock_children(&[(parent, true), (newparent, true)], &[(&pn, &c), (&npn, &nc)], true);
                let sec = self.sec_for(&[&pn, &npn]);
                let thorough = self.thorough() && sec;
                // identities before the rename, per side: (src, dst)
                let pre = if thorough {
                    let (a, b) = self.both(cx, true, None, |side, be| {
                        Ok((
                            be.stat_at(pn.fd(side).as_fd(), &c).ok().map(|s| s.ident()),
                            be.stat_at(npn.fd(side).as_fd(), &nc).ok().map(|s| s.ident()),
                        ))
                    });
                    Some((a.unwrap(), b.map(|r| r.unwrap())))
                } else {
                    None
                };
                let creds = self.creds(ctx);
                // A repair after a rename mismatch must fix the target name too.
                cx.resync_extra = Some(super::resync::ResyncReq::entry(&npn, newname));
                let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| {
                    be.rename(pn.fd(side).as_fd(), &c, npn.fd(side).as_fd(), &nc, flags)
                });
                self.cmp_result(cx, &pn, Some(name), &p, &s, true)?;
                p?;
                if let Some(src) = cands[0].and_then(|st| self.nodes.get(self.map_ino(st.ino))) {
                    src.set_hint(Engine::child_hint(&npn, newname));
                }
                if flags & libc::RENAME_EXCHANGE != 0 {
                    if let Some(dst) = cands[1].and_then(|st| self.nodes.get(self.map_ino(st.ino))) {
                        dst.set_hint(Engine::child_hint(&pn, name));
                    }
                }
                if let (Some((pp, ps)), true) = (pre, matches!(s, Some(Ok(())))) {
                    let (ap, as_) = self.both(cx, true, None, |side, be| {
                        Ok((
                            be.stat_at(pn.fd(side).as_fd(), &c).ok().map(|s| s.ident()),
                            be.stat_at(npn.fd(side).as_fd(), &nc).ok().map(|s| s.ident()),
                        ))
                    });
                    let chk = |pre: NamePair, post: NamePair| {
                        let ok = if flags & libc::RENAME_EXCHANGE != 0 {
                            post.0 == pre.1 && post.1 == pre.0
                        } else if pre.0.is_some() && pre.0 == pre.1 {
                            post == pre // rename between links of one inode: no-op
                        } else {
                            post.0.is_none() && post.1 == pre.0
                        };
                        if ok { Ok(()) } else { Err(format!("before {pre:?}, after {post:?}")) }
                    };
                    let ps = ps.unwrap();
                    self.verify_sides(cx, &pn, Some(name), "renamed", chk(pp, ap.unwrap()), Some(chk(ps, as_.unwrap().unwrap())), true)?;
                    if let Some(src) = cands[0].and_then(|st| self.nodes.get(self.map_ino(st.ino))) {
                        if self.sec_for(&[&src]) {
                            let (a, b) = self.both(cx, true, None, |side, be| be.stat(src.fd(side).as_fd()));
                            if let (Ok(a), Some(Ok(b))) = (a, b) {
                                self.cmp_stat(cx, &src, &a, &b, true, "after rename")?;
                            }
                        }
                    }
                }
                cx.resync_extra = None;
                self.compare_listing(cx, &pn, "source dir after rename")?;
                if newparent != parent {
                    self.compare_listing(cx, &npn, "target dir after rename")?;
                }
                Ok(())
            },
        )
    }

    pub fn link(&self, ctx: &Ctx, ino: u64, newparent: u64, newname: &OsStr) -> Result<Attr, i32> {
        self.run(OpKind::Link, ino, || format!("{} -> {}", self.detail_ino(ino), self.detail_name(newparent, newname)), |cx| {
            let n = self.node(ino)?;
            let npn = self.node(newparent)?;
            let nc = cname(newname)?;
            let (_l, _) = self.lock_children(&[(ino, true), (newparent, true)], &[(&npn, &nc)], true);
            let sec = self.sec_for(&[&n, &npn]);
            let creds = self.creds(ctx);
            let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| be.link(n.fd(side).as_fd(), npn.fd(side).as_fd(), &nc));
            self.cmp_result(cx, &npn, Some(newname), &p, &s, true)?;
            p?;
            let s_ok = matches!(s, Some(Ok(())));
            let (lp, ls) = self.both(cx, s_ok, None, |side, be| be.lookup(npn.fd(side).as_fd(), &nc));
            self.cmp_result(cx, &npn, Some(newname), &lp, &ls, true)?;
            let (pfd, pst) = lp?;
            if pst.ident() != n.pident {
                tracing::error!("link: new primary entry is not the linked inode");
                return Err(Fail::Errno(libc::EIO));
            }
            let (_, attr) = self.finish_entry(cx, &npn, newname, pfd, pst, ls.and_then(|r| r.ok()), true)?;
            self.compare_listing(cx, &npn, "dir after link")?;
            Ok(attr)
        })
    }

    // --------------------------------------------------------------- files

    pub fn open(&self, ctx: &Ctx, ino: u64, flags: i32) -> Result<u64, i32> {
        self.run(OpKind::Open, ino, || format!("{} flags={flags:#x}", self.detail_ino(ino)), |cx| {
            let n = self.node(ino)?;
            let fl = sanitize_open_flags(flags);
            let trunc = fl & libc::O_TRUNC != 0;
            let _l = self.lock(&[(ino, trunc)]);
            let sec = self.sec_for(&[&n]);
            let creds = self.creds(ctx);
            let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| be.open(n.fd(side).as_fd(), fl));
            self.cmp_result(cx, &n, None, &p, &s, trunc)?;
            let pf = p?;
            let sf = s.and_then(|r| r.ok());
            if trunc && self.thorough() && sf.is_some() {
                let (a, b) = self.both(cx, true, None, |side, be| be.stat(n.fd(side).as_fd()));
                let chk = |r: SysResult<Stat>| match r {
                    Ok(st) if st.size == 0 => Ok(()),
                    Ok(st) => Err(format!("size {} after O_TRUNC", st.size)),
                    Err(e) => Err(sys::fmt_errno(e)),
                };
                self.verify_sides(cx, &n, None, "truncated", chk(a), b.map(chk), true)?;
            }
            Ok(self.install_file(n, pf, sf, fl))
        })
    }

    /// Reads `size` bytes at `off`; `out` receives the primary's data.
    pub fn read(&self, _ctx: &Ctx, ino: u64, fh: u64, off: u64, size: u32, out: &mut dyn FnMut(&[u8])) -> Result<(), i32> {
        self.run(OpKind::Read, ino, || format!("{} off={off} len={size}", self.detail_ino(ino)), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, false)]);
            let sec = !self.detached() && f.has_sec();
            if !sec && !self.detached() {
                self.stats.secondary_skipped.fetch_add(1, Relaxed);
            }
            let (p, s) = self.both(cx, sec, None, |side, be| {
                let mut buf = self.bufs.get(size as usize);
                match be.pread(f.fd(side).as_fd(), &mut buf, off) {
                    Ok(k) => {
                        buf.truncate(k);
                        Ok(buf)
                    }
                    Err(e) => {
                        self.bufs.put(buf);
                        Err(e)
                    }
                }
            });
            self.cmp_result(cx, n, None, &p, &s, false)?;
            let pb = p?;
            let mut res = Ok(());
            if let Some(Ok(sb)) = &s {
                if pb.len() != sb.len() {
                    res = self.report(cx, n, MismatchKind::Length, None, pb.len().to_string(), sb.len().to_string(), format!("read at offset {off}, {size} requested"), false);
                } else if let Some(d) = compare::diff_data(off, &pb, sb) {
                    res = self.report(cx, n, MismatchKind::Data, None, format!("xxh3 {:016x}", xxhash_rust::xxh3::xxh3_64(&pb)), format!("xxh3 {:016x}", xxhash_rust::xxh3::xxh3_64(sb)), d, false);
                }
            }
            if let Some(Ok(sb)) = s {
                self.bufs.put(sb);
            }
            if let Err(e) = res {
                self.bufs.put(pb);
                return Err(e);
            }
            cx.bytes = pb.len() as u64;
            self.stats.bytes_read.fetch_add(pb.len() as u64, Relaxed);
            out(&pb);
            self.bufs.put(pb);
            Ok(())
        })
    }

    pub fn write(&self, _ctx: &Ctx, ino: u64, fh: u64, off: u64, data: &[u8]) -> Result<u32, i32> {
        self.run(OpKind::Write, ino, || format!("{} off={off} len={}", self.detail_ino(ino), data.len()), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, true)]);
            let sec = !self.detached() && f.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.pwrite(f.fd(side).as_fd(), data, off));
            self.cmp_result(cx, n, None, &p, &s, true)?;
            let pn = p?;
            f.written.store(true, Relaxed);
            cx.bytes = pn as u64;
            self.stats.bytes_written.fetch_add(pn as u64, Relaxed);
            let sn = match &s {
                Some(Ok(sn)) => Some(*sn),
                _ => None,
            };
            if let Some(sn) = sn {
                if sn != pn {
                    self.report(cx, n, MismatchKind::Length, None, pn.to_string(), sn.to_string(), format!("write at offset {off}, {} bytes", data.len()), true)?;
                }
            }
            if self.thorough() && sn.is_some() && n.has_sec() {
                let append = f.flags & libc::O_APPEND != 0;
                let (a, b) = self.both(cx, true, None, |side, be| {
                    let len = if side == Side::Primary { pn } else { sn.unwrap() };
                    let at = if append { be.stat(f.fd(side).as_fd())?.size.saturating_sub(len as u64) } else { off };
                    self.read_back(be, n, side, at, len)
                });
                let chk = |r: SysResult<Vec<u8>>, len: usize| match r {
                    Ok(v) => match compare::diff_data(off, &data[..len], &v) {
                        None => Ok(()),
                        Some(d) => Err(format!("read-back differs from written data: {d}")),
                    },
                    Err(e) => Err(sys::fmt_errno(e)),
                };
                self.verify_sides(cx, n, None, "write read-back", chk(a, pn), b.map(|b| chk(b, sn.unwrap())), true)?;
            }
            Ok(pn as u32)
        })
    }

    pub fn flush(&self, _ctx: &Ctx, ino: u64, fh: u64, lock_owner: u64) -> Result<(), i32> {
        self.run(OpKind::Flush, ino, || self.detail_ino(ino), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, false)]);
            let sec = !self.detached() && f.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.flush(f.fd(side).as_fd()));
            self.cmp_result(cx, n, None, &p, &s, false)?;
            if self.cfg.mirror_locks {
                self.release_owner(cx, n, lock_owner);
            }
            p?;
            Ok(())
        })
    }

    pub fn release(&self, _ctx: &Ctx, ino: u64, fh: u64) -> Result<(), i32> {
        self.run(OpKind::Release, ino, || self.detail_ino(ino), |cx| {
            let Some(f) = self.files.remove(fh) else { return Err(Fail::Errno(libc::EBADF)) };
            self.stats.open_files.fetch_sub(1, Relaxed);
            let n = f.node.clone();
            let res = if self.paranoid() && f.written.load(Relaxed) && f.has_sec() {
                let _l = self.lock(&[(n.id, false)]);
                self.compare_content(cx, &n, false)
            } else {
                Ok(())
            };
            drop(f);
            if self.cfg.mirror_locks {
                self.release_handle(cx, &n, fh);
            }
            self.nodes.closed(&n);
            match res {
                Err(Fail::Errno(e)) => Err(Fail::Errno(e)),
                _ => Ok(()), // the handle is gone: never retry a release
            }
        })
    }

    pub fn fsync(&self, _ctx: &Ctx, ino: u64, fh: u64, datasync: bool) -> Result<(), i32> {
        self.run(OpKind::Fsync, ino, || self.detail_ino(ino), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, false)]);
            let sec = !self.detached() && f.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.fsync(f.fd(side).as_fd(), datasync));
            self.cmp_result(cx, n, None, &p, &s, false)?;
            Ok(p?)
        })
    }

    pub fn fallocate(&self, _ctx: &Ctx, ino: u64, fh: u64, off: u64, len: u64, mode: i32) -> Result<(), i32> {
        self.run(OpKind::Fallocate, ino, || format!("{} off={off} len={len} mode={mode:#x}", self.detail_ino(ino)), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, true)]);
            let sec = !self.detached() && f.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.fallocate(f.fd(side).as_fd(), mode, off, len));
            self.cmp_result(cx, n, None, &p, &s, true)?;
            p?;
            f.written.store(true, Relaxed);
            if self.thorough() && matches!(s, Some(Ok(()))) && n.has_sec() {
                let (a, b) = self.both(cx, true, None, |side, be| be.stat(n.fd(side).as_fd()));
                if let (Ok(a), Some(Ok(b))) = (a, b) {
                    self.cmp_stat(cx, n, &a, &b, true, "after fallocate")?;
                }
                if mode & (libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_ZERO_RANGE) != 0 {
                    let l = len.min(VERIFY_CAP) as usize;
                    let (a, b) = self.both(cx, true, None, |side, be| self.read_back(be, n, side, off, l));
                    let chk = |r: SysResult<Vec<u8>>| match r {
                        Ok(v) => match v.iter().position(|&x| x != 0) {
                            None => Ok(()),
                            Some(i) => Err(format!("non-zero byte at offset {}", off + i as u64)),
                        },
                        Err(e) => Err(sys::fmt_errno(e)),
                    };
                    self.verify_sides(cx, n, None, "zeroed range", chk(a), b.map(chk), true)?;
                }
            }
            Ok(())
        })
    }

    pub fn lseek(&self, _ctx: &Ctx, ino: u64, fh: u64, off: i64, whence: i32) -> Result<i64, i32> {
        self.run(OpKind::Lseek, ino, || format!("{} off={off} whence={whence}", self.detail_ino(ino)), |cx| {
            let f = self.file(fh)?;
            let n = &f.node;
            let _l = self.lock(&[(n.id, false)]);
            let sec = !self.detached() && f.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.lseek(f.fd(side).as_fd(), off, whence));
            self.cmp_result(cx, n, None, &p, &s, false)?;
            let po = p?;
            if let Some(Ok(so)) = s {
                if so != po {
                    if whence == libc::SEEK_DATA || whence == libc::SEEK_HOLE {
                        // Hole detection granularity is file-system specific.
                        tracing::debug!("lseek {whence} differs: {po} vs {so} (not a mismatch)");
                    } else {
                        self.report(cx, n, MismatchKind::Length, Some("offset".into()), po.to_string(), so.to_string(), String::new(), false)?;
                    }
                }
            }
            Ok(po)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_file_range(
        &self,
        _ctx: &Ctx,
        fh_in: u64,
        off_in: u64,
        fh_out: u64,
        off_out: u64,
        len: u64,
        flags: u32,
    ) -> Result<u32, i32> {
        self.run(OpKind::CopyFileRange, 0, || format!("fh {fh_in}@{off_in} -> fh {fh_out}@{off_out} len={len}"), |cx| {
            let fi = self.file(fh_in)?;
            let fo = self.file(fh_out)?;
            let _l = self.lock(&[(fi.node.id, false), (fo.node.id, true)]);
            let sec = !self.detached() && fi.has_sec() && fo.has_sec();
            let l = len.min(u32::MAX as u64) as usize;
            let (p, s) = self.both(cx, sec, None, |side, be| be.copy_file_range(fi.fd(side).as_fd(), off_in, fo.fd(side).as_fd(), off_out, l, flags));
            self.cmp_result(cx, &fo.node, None, &p, &s, true)?;
            let pn = p?;
            fo.written.store(true, Relaxed);
            cx.bytes = pn as u64;
            if let Some(Ok(sn)) = s {
                if sn != pn {
                    self.report(cx, &fo.node, MismatchKind::Length, None, pn.to_string(), sn.to_string(), String::new(), true)?;
                } else if self.thorough() && fo.node.has_sec() {
                    let k = (pn as u64).min(VERIFY_CAP) as usize;
                    let (a, b) = self.both(cx, true, None, |side, be| self.read_back(be, &fo.node, side, off_out, k));
                    if let (Ok(a), Some(Ok(b))) = (a, b) {
                        self.stats.verifications.fetch_add(1, Relaxed);
                        if let Some(d) = compare::diff_data(off_out, &a, &b) {
                            self.report(cx, &fo.node, MismatchKind::Verify, Some("copied range".into()), String::new(), String::new(), d, true)?;
                        }
                    }
                }
            }
            Ok(pn as u32)
        })
    }

    // --------------------------------------------------------- directories

    pub fn opendir(&self, _ctx: &Ctx, ino: u64) -> Result<u64, i32> {
        self.run(OpKind::Opendir, ino, || self.detail_ino(ino), |cx| {
            let n = self.node(ino)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.opendir(n.fd(side).as_fd()));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let pfd = p?;
            let sfd = s.and_then(|r| r.ok());
            let fh = self.alloc_fh();
            self.nodes.opened(&n);
            self.dirs.insert(fh, Arc::new(OpenDir { sec_gen: n.sec_gen(), node: n, pfd, sfd, entries: parking_lot::Mutex::new(None) }));
            self.stats.open_dirs.fetch_add(1, Relaxed);
            Ok(fh)
        })
    }

    /// Lists entries from `offset`; `add(ino, next_offset, kind, name)`
    /// returns true when the reply buffer is full.
    pub fn readdir(
        &self,
        _ctx: &Ctx,
        ino: u64,
        fh: u64,
        offset: u64,
        add: &mut DirFiller<'_>,
    ) -> Result<(), i32> {
        self.run(OpKind::Readdir, ino, || format!("{} off={offset}", self.detail_ino(ino)), |cx| {
            let d = self.dir(fh)?;
            let n = &d.node;
            let snap = {
                let cur = d.entries.lock().clone();
                match cur {
                    Some(e) if offset != 0 => e,
                    _ => {
                        let e = Arc::new(self.snapshot_dir(cx, &d)?);
                        *d.entries.lock() = Some(e.clone());
                        e
                    }
                }
            };
            let _ = n;
            for (i, (name, ino, kind)) in snap.iter().enumerate().skip(offset as usize) {
                if add(*ino, i as u64 + 1, *kind, name) {
                    break;
                }
            }
            Ok(())
        })
    }

    fn snapshot_dir(&self, cx: &mut Cx, d: &OpenDir) -> OpResult<Vec<(Vec<u8>, u64, FileKind)>> {
        let n = &d.node;
        let _l = self.lock(&[(n.id, false)]);
        let sec = !self.detached() && d.has_sec();
        let (p, s) = self.both(cx, sec, None, |side, be| be.readdir(d.fd(side).as_fd()));
        self.cmp_result(cx, n, None, &p, &s, false)?;
        let pe: Vec<DirEntry> = p?;
        if let Some(Ok(se)) = &s {
            self.cmp_dir(cx, n, &pe, se, "readdir")?;
        }
        let parent_ino = if n.id == ROOT_ID {
            ROOT_ID
        } else {
            self.b[0].stat_at(n.p(), c"..").map(|st| self.map_ino(st.ino)).unwrap_or(ROOT_ID)
        };
        let mut out = Vec::with_capacity(pe.len() + 2);
        out.push((b".".to_vec(), n.id, FileKind::Dir));
        out.push((b"..".to_vec(), parent_ino, FileKind::Dir));
        for e in pe {
            let kind = match e.kind {
                Some(k) => k,
                None => sys::cstr(&e.name)
                    .ok()
                    .and_then(|c| self.b[0].stat_at(n.p(), &c).ok())
                    .map(|st| st.kind())
                    .unwrap_or(FileKind::Regular),
            };
            out.push((e.name, self.map_ino(e.ino), kind));
        }
        Ok(out)
    }

    pub fn releasedir(&self, _ctx: &Ctx, ino: u64, fh: u64) -> Result<(), i32> {
        self.run(OpKind::Releasedir, ino, || self.detail_ino(ino), |_cx| {
            let Some(d) = self.dirs.remove(fh) else { return Err(Fail::Errno(libc::EBADF)) };
            self.stats.open_dirs.fetch_sub(1, Relaxed);
            let n = d.node.clone();
            drop(d);
            self.nodes.closed(&n);
            Ok(())
        })
    }

    pub fn fsyncdir(&self, _ctx: &Ctx, ino: u64, fh: u64, datasync: bool) -> Result<(), i32> {
        self.run(OpKind::Fsyncdir, ino, || self.detail_ino(ino), |cx| {
            let d = self.dir(fh)?;
            let n = &d.node;
            let _l = self.lock(&[(n.id, false)]);
            let sec = !self.detached() && d.has_sec();
            let (p, s) = self.both(cx, sec, None, |side, be| be.fsync(d.fd(side).as_fd(), datasync));
            self.cmp_result(cx, n, None, &p, &s, false)?;
            Ok(p?)
        })
    }

    pub fn statfs(&self, _ctx: &Ctx, ino: u64) -> Result<StatFs, i32> {
        self.run(OpKind::Statfs, ino, || self.detail_ino(ino), |cx| {
            let n = self.node(ino)?;
            let sec = self.sec_for(&[&n]);
            // Capacities differ by nature: only the outcome is compared.
            let (p, s) = self.both(cx, sec, None, |side, be| be.statfs(n.fd(side).as_fd()));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            Ok(p?)
        })
    }

    // -------------------------------------------------------------- xattrs

    pub fn setxattr(&self, ctx: &Ctx, ino: u64, name: &OsStr, value: &[u8], flags: i32) -> Result<(), i32> {
        self.run(OpKind::Setxattr, ino, || format!("{} {} ({} bytes)", self.detail_ino(ino), name.to_string_lossy(), value.len()), |cx| {
            let n = self.node(ino)?;
            let c = cname(name)?;
            let _l = self.lock(&[(ino, true)]);
            let sec = self.sec_for(&[&n]);
            let creds = self.creds(ctx);
            let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| be.setxattr(n.fd(side).as_fd(), &c, value, flags));
            self.cmp_result(cx, &n, None, &p, &s, true)?;
            p?;
            if self.thorough() && matches!(s, Some(Ok(()))) {
                let (a, b) = self.both(cx, true, None, |side, be| be.getxattr(n.fd(side).as_fd(), &c, XATTR_MAX));
                let chk = |r: SysResult<XattrOut>| match r {
                    Ok(XattrOut::Data(v)) if v == value => Ok(()),
                    Ok(XattrOut::Data(v)) => Err(format!("value differs ({} bytes)", v.len())),
                    Ok(XattrOut::Size(_)) => Err("size reply".into()),
                    Err(e) => Err(sys::fmt_errno(e)),
                };
                self.verify_sides(cx, &n, None, "xattr set", chk(a), b.map(chk), true)?;
            }
            Ok(())
        })
    }

    pub fn getxattr(&self, _ctx: &Ctx, ino: u64, name: &OsStr, size: u32) -> Result<XattrOut, i32> {
        self.run(OpKind::Getxattr, ino, || format!("{} {}", self.detail_ino(ino), name.to_string_lossy()), |cx| {
            let n = self.node(ino)?;
            let c = cname(name)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.getxattr(n.fd(side).as_fd(), &c, size as usize));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let pv = p?;
            if let Some(Ok(sv)) = &s {
                if &pv != sv {
                    self.report(cx, &n, MismatchKind::Xattr, Some(name.to_string_lossy().into_owned()), xattr_desc(&pv), xattr_desc(sv), String::new(), false)?;
                }
            }
            Ok(pv)
        })
    }

    pub fn listxattr(&self, _ctx: &Ctx, ino: u64, size: u32) -> Result<XattrOut, i32> {
        self.run(OpKind::Listxattr, ino, || self.detail_ino(ino), |cx| {
            let n = self.node(ino)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let (p, s) = self.both(cx, sec, None, |side, be| be.listxattr(n.fd(side).as_fd(), size as usize));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let pv = p?;
            if let Some(Ok(sv)) = &s {
                let same = match (&pv, sv) {
                    (XattrOut::Data(a), XattrOut::Data(b)) => compare::xattr_names(a) == compare::xattr_names(b),
                    (a, b) => a == b,
                };
                if !same {
                    let names = |x: &XattrOut| match x {
                        XattrOut::Size(z) => format!("size {z}"),
                        XattrOut::Data(d) => compare::xattr_names(d).iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>().join(","),
                    };
                    self.report(cx, &n, MismatchKind::Xattr, Some("list".into()), names(&pv), names(sv), String::new(), false)?;
                }
            }
            Ok(pv)
        })
    }

    pub fn removexattr(&self, ctx: &Ctx, ino: u64, name: &OsStr) -> Result<(), i32> {
        self.run(OpKind::Removexattr, ino, || format!("{} {}", self.detail_ino(ino), name.to_string_lossy()), |cx| {
            let n = self.node(ino)?;
            let c = cname(name)?;
            let _l = self.lock(&[(ino, true)]);
            let sec = self.sec_for(&[&n]);
            let creds = self.creds(ctx);
            let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| be.removexattr(n.fd(side).as_fd(), &c));
            self.cmp_result(cx, &n, None, &p, &s, true)?;
            p?;
            if self.thorough() && matches!(s, Some(Ok(()))) {
                let (a, b) = self.both(cx, true, None, |side, be| be.getxattr(n.fd(side).as_fd(), &c, 0));
                let chk = |r: SysResult<XattrOut>| match r {
                    Err(libc::ENODATA) => Ok(()),
                    Ok(_) => Err("attribute still present".to_string()),
                    Err(e) => Err(sys::fmt_errno(e)),
                };
                self.verify_sides(cx, &n, None, "xattr removed", chk(a), b.map(chk), true)?;
            }
            Ok(())
        })
    }

    pub fn access(&self, ctx: &Ctx, ino: u64, mask: i32) -> Result<(), i32> {
        self.run(OpKind::Access, ino, || format!("{} mask={mask:o}", self.detail_ino(ino)), |cx| {
            let n = self.node(ino)?;
            let _l = self.lock(&[(ino, false)]);
            let sec = self.sec_for(&[&n]);
            let creds = self.creds(ctx);
            let (p, s) = self.both(cx, sec, creds.as_ref(), |side, be| be.access(n.fd(side).as_fd(), mask));
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            Ok(p?)
        })
    }

    /// Counts an operation xcheckfs deliberately does not implement.
    pub fn unsupported(&self, op: OpKind) -> i32 {
        let st = self.stats.op(op);
        st.count.fetch_add(1, Relaxed);
        st.errors.fetch_add(1, Relaxed);
        libc::ENOSYS
    }

    // ------------------------------------------------------------- details

    fn detail_ino(&self, ino: u64) -> String {
        match self.nodes.get(ino) {
            Some(n) if ino == ROOT_ID => format!("/ [{}]", n.id),
            Some(n) => format!("{} [{}]", n.hint(), n.id),
            None => format!("[{ino}]"),
        }
    }

    fn detail_name(&self, parent: u64, name: &OsStr) -> String {
        match self.nodes.get(parent) {
            Some(n) => format!("{}/{}", n.hint(), name.to_string_lossy()),
            None => format!("[{parent}]/{}", name.to_string_lossy()),
        }
    }

    /// Internal: used by the lock module for getlk/setlk.
    pub(crate) fn lock_types_ok(l: &Lock) -> bool {
        matches!(l.typ, libc::F_RDLCK | libc::F_WRLCK | libc::F_UNLCK)
    }
}

/// Identities of (source name, target name) around a rename.
type NamePair = (Option<(u64, u64)>, Option<(u64, u64)>);

/// `add(ino, next_offset, kind, name)` -> reply buffer full.
pub type DirFiller<'a> = dyn FnMut(u64, u64, FileKind, &[u8]) -> bool + 'a;

fn xattr_desc(x: &XattrOut) -> String {
    match x {
        XattrOut::Size(n) => format!("size {n}"),
        XattrOut::Data(d) => format!("{} bytes xxh3 {:016x}", d.len(), xxhash_rust::xxh3::xxh3_64(d)),
    }
}

fn cx_what(op: OpKind) -> &'static str {
    match op {
        OpKind::Lookup => "lookup",
        OpKind::Link => "after link",
        _ => "new entry",
    }
}

/// O_DIRECT is not passed to the backends (it would require aligned
/// buffers and only changes caching, not semantics); the kernel's FUSE
/// layer handles the caller-visible part.
fn sanitize_open_flags(flags: i32) -> i32 {
    flags & !(libc::O_DIRECT | libc::O_NOCTTY)
}
