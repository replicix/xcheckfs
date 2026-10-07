//! Mirrored POSIX record locks (fcntl F_SETLK/F_SETLKW/F_GETLK).
//!
//! Each lock owner (a process, as identified by the kernel's lock_owner)
//! gets its own open file description on both backends, and locks are taken
//! with *non-blocking* open-file-description locks (`F_OFD_SETLK`), so the
//! lock tables of both file systems evolve identically.
//!
//! Blocking requests are never blocked inside a backend: when both backends
//! report a conflict, the request is queued here, and every queued request
//! is retried in FIFO order after each successful lock change on the inode.
//! Blocking inside the backends would let them wake waiters in different,
//! unspecified orders, and would park a worker thread while it holds the
//! inode's lockstep lock.
//!
//! Limitations: no deadlock detection (EDEADLK), and the conflicting lock's
//! pid is reported as 0. BSD flock(2) locks are handled by the kernel.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use super::{Ctx, Cx, Engine, Node, OpResult, Side};
use crate::backend::Lock;
use crate::policy::MismatchKind;
use crate::stats::OpKind;
use crate::sys::{self, SysResult};

pub type LockReply = Box<dyn FnOnce(Result<(), i32>) + Send>;

struct OwnerFds {
    p: OwnedFd,
    s: Option<OwnedFd>,
    /// The FUSE file handle the owner first locked through. OFD lock owners
    /// are bound to one handle; the kernel sends no unlock for them on
    /// close, only RELEASE of that handle.
    fh: u64,
}

impl OwnerFds {
    fn fd(&self, side: Side) -> BorrowedFd<'_> {
        match side {
            Side::Primary => self.p.as_fd(),
            Side::Secondary => self.s.as_ref().expect("secondary owner fd").as_fd(),
        }
    }
}

struct Waiter {
    owner: u64,
    fh: u64,
    lock: Lock,
    reply: LockReply,
}

#[derive(Default)]
pub struct LockState {
    owners: HashMap<u64, OwnerFds>,
    waiters: VecDeque<Waiter>,
}

/// FUSE uses an inclusive end; OFFSET_MAX means "to EOF".
pub fn lock_from_range(typ: i32, start: u64, end: u64, pid: u32) -> Lock {
    let len = if end >= i64::MAX as u64 { 0 } else { end.saturating_sub(start) + 1 };
    Lock { typ, start, len, pid: pid as i32 }
}

pub fn lock_end(l: &Lock) -> u64 {
    if l.len == 0 { i64::MAX as u64 } else { l.start + l.len - 1 }
}

fn conflict(e: i32) -> bool {
    e == libc::EAGAIN || e == libc::EACCES
}

impl Engine {
    fn owner_fds<'a>(&self, n: &Node, st: &'a mut LockState, owner: u64, fh: u64, typ: i32) -> SysResult<&'a OwnerFds> {
        if let std::collections::hash_map::Entry::Vacant(e) = st.owners.entry(owner) {
            let open = |side: Side| -> SysResult<OwnedFd> {
                let be = &*self.b[side as usize];
                be.open(n.fd(side).as_fd(), libc::O_RDWR).or_else(|_| {
                    be.open(n.fd(side).as_fd(), if typ == libc::F_WRLCK { libc::O_WRONLY } else { libc::O_RDONLY })
                })
            };
            let p = open(Side::Primary)?;
            let s = if n.has_sec() && !self.detached() { open(Side::Secondary).ok() } else { None };
            e.insert(OwnerFds { p, s, fh });
        }
        Ok(&st.owners[&owner])
    }

    /// One non-blocking attempt on both backends. Returns the primary's
    /// result.
    fn try_lock(&self, cx: &mut Cx, n: &Arc<Node>, st: &mut LockState, owner: u64, fh: u64, l: &Lock) -> OpResult<SysResult<()>> {
        if l.typ == libc::F_UNLCK && !st.owners.contains_key(&owner) {
            return Ok(Ok(()));
        }
        let fds = match self.owner_fds(n, st, owner, fh, l.typ) {
            Ok(f) => f,
            Err(e) => return Ok(Err(e)),
        };
        let sec = !self.detached() && fds.s.is_some();
        let (p, s) = self.both(cx, sec, None, |side, be| be.setlk(fds.fd(side).as_fd(), l));
        if let Some(s) = &s {
            let norm = |r: &SysResult<()>| match r {
                Ok(()) => 0,
                Err(e) if conflict(*e) => libc::EAGAIN,
                Err(e) => *e,
            };
            let (a, b) = (norm(&p), norm(s));
            if a != b {
                let desc = |e: i32| if e == libc::EAGAIN { "conflict".to_string() } else { sys::errno_name(e).to_string() };
                self.report(
                    cx,
                    n,
                    MismatchKind::Lock,
                    Some("setlk".into()),
                    desc(a),
                    desc(b),
                    format!("owner {owner:#x} type {} range {}+{}", l.typ, l.start, l.len),
                    true,
                )?;
            }
        }
        Ok(p)
    }

    /// Retries queued blocking requests in FIFO order.
    fn wake_waiters(&self, cx: &mut Cx, n: &Arc<Node>, st: &mut LockState) {
        let mut i = 0;
        while i < st.waiters.len() {
            let (owner, fh, lock) = (st.waiters[i].owner, st.waiters[i].fh, st.waiters[i].lock);
            match self.try_lock(cx, n, st, owner, fh, &lock) {
                Ok(Err(e)) if conflict(e) => i += 1,
                Ok(r) => {
                    let w = st.waiters.remove(i).unwrap();
                    self.stats.lock_waiters.fetch_sub(1, Relaxed);
                    (w.reply)(r);
                }
                Err(_) => {
                    let w = st.waiters.remove(i).unwrap();
                    self.stats.lock_waiters.fetch_sub(1, Relaxed);
                    (w.reply)(Err(libc::EIO));
                }
            }
        }
    }

    pub fn getlk(&self, _ctx: &Ctx, ino: u64, owner: u64, l: Lock) -> Result<Lock, i32> {
        self.run(OpKind::Getlk, ino, || format!("[{ino}] owner={owner:#x} type={} {}+{}", l.typ, l.start, l.len), |cx| {
            let n = self.node(ino)?;
            if !Engine::lock_types_ok(&l) {
                return Err(libc::EINVAL.into());
            }
            let st = n.locks.lock();
            let temp;
            let (pfd, sfd) = match st.owners.get(&owner) {
                Some(f) => (f.p.as_fd(), f.s.as_ref().map(|s| s.as_fd())),
                None => {
                    let open = |side: Side| -> SysResult<OwnedFd> {
                        let be = &*self.b[side as usize];
                        be.open(n.fd(side).as_fd(), libc::O_RDWR).or_else(|_| be.open(n.fd(side).as_fd(), libc::O_RDONLY))
                    };
                    let p = open(Side::Primary)?;
                    let s = if n.has_sec() && !self.detached() { open(Side::Secondary).ok() } else { None };
                    temp = (p, s);
                    (temp.0.as_fd(), temp.1.as_ref().map(|s| s.as_fd()))
                }
            };
            let sec = !self.detached() && sfd.is_some();
            let (p, s) = self.both(cx, sec, None, |side, be| {
                be.getlk(if side == Side::Primary { pfd } else { sfd.unwrap() }, &l)
            });
            self.cmp_result(cx, &n, None, &p, &s, false)?;
            let mut pl = p?;
            if let Some(Ok(sl)) = s {
                if (pl.typ, pl.start, pl.len) != (sl.typ, sl.start, sl.len) {
                    let d = |x: &Lock| format!("type {} {}+{}", x.typ, x.start, x.len);
                    self.report(cx, &n, MismatchKind::Lock, Some("getlk".into()), d(&pl), d(&sl), String::new(), false)?;
                }
            }
            drop(st);
            if pl.typ == libc::F_UNLCK {
                pl.start = l.start;
                pl.len = l.len;
            }
            pl.pid = 0;
            Ok(pl)
        })
    }

    /// Sets or clears a lock through file handle `fh`. `reply` is called
    /// exactly once, possibly later from another thread when `sleep` is set
    /// and the lock is busy.
    #[allow(clippy::too_many_arguments)]
    pub fn setlk(&self, _ctx: &Ctx, ino: u64, fh: u64, owner: u64, l: Lock, sleep: bool, reply: LockReply) {
        let mut reply = Some(reply);
        let res = self.run(
            OpKind::Setlk,
            ino,
            || format!("[{ino}] owner={owner:#x} type={} {}+{} sleep={sleep}", l.typ, l.start, l.len),
            |cx| {
                let n = self.node(ino)?;
                if !Engine::lock_types_ok(&l) {
                    return Err(libc::EINVAL.into());
                }
                let mut st = n.locks.lock();
                let r = self.try_lock(cx, &n, &mut st, owner, fh, &l)?;
                match r {
                    Err(e) if sleep && conflict(e) => {
                        st.waiters.push_back(Waiter { owner, fh, lock: l, reply: reply.take().unwrap() });
                        self.stats.lock_waiters.fetch_add(1, Relaxed);
                        Ok(None)
                    }
                    Ok(()) => {
                        self.wake_waiters(cx, &n, &mut st);
                        Ok(Some(Ok(())))
                    }
                    Err(e) => Ok(Some(Err(e))),
                }
            },
        );
        match res {
            Ok(None) => {} // queued
            Ok(Some(r)) => (reply.take().unwrap())(r),
            Err(e) => {
                if let Some(r) = reply.take() {
                    r(Err(e))
                }
            }
        }
    }

    /// The owner closed a descriptor of this file: POSIX releases all of its
    /// locks on the file. Pending blocking requests of the owner are
    /// cancelled with EINTR (the process is closing or exiting).
    pub(crate) fn release_owner(&self, cx: &mut Cx, n: &Arc<Node>, owner: u64) {
        let mut st = n.locks.lock();
        let mut i = 0;
        while i < st.waiters.len() {
            if st.waiters[i].owner == owner {
                let w = st.waiters.remove(i).unwrap();
                self.stats.lock_waiters.fetch_sub(1, Relaxed);
                (w.reply)(Err(libc::EINTR));
            } else {
                i += 1;
            }
        }
        if st.owners.remove(&owner).is_some() {
            // Dropping the descriptors released the locks on both sides.
            self.wake_waiters(cx, n, &mut st);
        }
    }

    /// A file handle was released (its last descriptor closed). Owners bound
    /// to it lose their locks: this is what releases OFD locks. POSIX owners
    /// that locked through it were already released by the FLUSH the
    /// closing process sent first, so dropping them here is equivalent.
    pub(crate) fn release_handle(&self, cx: &mut Cx, n: &Arc<Node>, fh: u64) {
        let mut st = n.locks.lock();
        let mut i = 0;
        while i < st.waiters.len() {
            if st.waiters[i].fh == fh {
                let w = st.waiters.remove(i).unwrap();
                self.stats.lock_waiters.fetch_sub(1, Relaxed);
                (w.reply)(Err(libc::EINTR));
            } else {
                i += 1;
            }
        }
        let before = st.owners.len();
        st.owners.retain(|_, o| o.fh != fh);
        if st.owners.len() != before {
            self.wake_waiters(cx, n, &mut st);
        }
    }
}
