//! FUSE adapter: translates fuser requests into engine calls.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner, OpenFlags, PollEvents, PollFlags,
    PollNotifier, RenameFlags, ReplyAttr, ReplyBmap, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyIoctl, ReplyLock, ReplyLseek, ReplyOpen, ReplyPoll, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};

use crate::backend::{TimeSpec, XattrOut};
use crate::engine::locks::{lock_end, lock_from_range};
use crate::engine::{Attr, Ctx, Engine, SetAttr};
use crate::stats::OpKind;
use crate::sys::{FileKind, Ts};

pub struct XcheckFs {
    pub engine: Arc<Engine>,
}

fn ctx(req: &Request) -> Ctx {
    Ctx { uid: req.uid(), gid: req.gid(), pid: req.pid() }
}

fn err(e: i32) -> Errno {
    Errno::from_i32(e)
}

fn ftype(k: FileKind) -> FileType {
    match k {
        FileKind::Regular => FileType::RegularFile,
        FileKind::Dir => FileType::Directory,
        FileKind::Symlink => FileType::Symlink,
        FileKind::Fifo => FileType::NamedPipe,
        FileKind::Socket => FileType::Socket,
        FileKind::CharDev => FileType::CharDevice,
        FileKind::BlockDev => FileType::BlockDevice,
    }
}

pub fn file_attr(a: &Attr) -> FileAttr {
    let st = &a.st;
    FileAttr {
        ino: INodeNo(a.id),
        size: st.size,
        blocks: st.blocks,
        atime: st.atime.to_system_time(),
        mtime: st.mtime.to_system_time(),
        ctime: st.ctime.to_system_time(),
        crtime: UNIX_EPOCH,
        kind: ftype(st.kind()),
        perm: (st.mode & 0o7777) as u16,
        nlink: st.nlink.min(u32::MAX as u64) as u32,
        uid: st.uid,
        gid: st.gid,
        rdev: st.rdev as u32,
        blksize: st.blksize as u32,
        flags: 0,
    }
}

fn timespec(t: Option<TimeOrNow>) -> Option<TimeSpec> {
    t.map(|t| match t {
        TimeOrNow::Now => TimeSpec::Now,
        TimeOrNow::SpecificTime(s) => TimeSpec::Set(Ts::from_system_time(s)),
    })
}

impl XcheckFs {
    fn attr_ttl(&self) -> Duration {
        self.engine.cfg.attr_ttl
    }
    fn entry(&self, reply: ReplyEntry, r: Result<Attr, i32>) {
        match r {
            Ok(a) => reply.entry_with_ttls(&self.attr_ttl(), &self.engine.cfg.entry_ttl, &file_attr(&a), Generation(0)),
            Err(e) => reply.error(err(e)),
        }
    }
    fn empty(reply: ReplyEmpty, r: Result<(), i32>) {
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }
    fn open_flags(&self) -> FopenFlags {
        if self.engine.cfg.direct_io { FopenFlags::FOPEN_DIRECT_IO } else { FopenFlags::empty() }
    }
}

impl Filesystem for XcheckFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let mut want = InitFlags::FUSE_PARALLEL_DIROPS;
        if self.engine.cfg.mirror_locks {
            want |= InitFlags::FUSE_POSIX_LOCKS;
        }
        for flag in [InitFlags::FUSE_PARALLEL_DIROPS, InitFlags::FUSE_POSIX_LOCKS] {
            if want.contains(flag) {
                if let Err(missing) = config.add_capabilities(flag) {
                    tracing::warn!("kernel does not support {missing:?}");
                }
            }
        }
        let _ = config.set_max_write(1 << 20);
        let _ = config.set_time_granularity(Duration::from_nanos(1));
        Ok(())
    }

    fn destroy(&mut self) {
        tracing::info!("file system destroyed (unmounted)");
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.entry(reply, self.engine.lookup(&ctx(req), parent.0, name));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.engine.forget(ino.0, nlookup);
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.engine.getattr(&ctx(req), ino.0) {
            Ok(a) => reply.attr(&self.attr_ttl(), &file_attr(&a)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let a = SetAttr { mode, uid, gid, size, atime: timespec(atime), mtime: timespec(mtime) };
        match self.engine.setattr(&ctx(req), ino.0, a, fh.map(|f| f.0)) {
            Ok(a) => reply.attr(&self.attr_ttl(), &file_attr(&a)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.engine.readlink(&ctx(req), ino.0) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, rdev: u32, reply: ReplyEntry) {
        self.entry(reply, self.engine.mknod(&ctx(req), parent.0, name, mode, rdev as u64));
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, reply: ReplyEntry) {
        self.entry(reply, self.engine.mkdir(&ctx(req), parent.0, name, mode));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.unlink(&ctx(req), parent.0, name));
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.rmdir(&ctx(req), parent.0, name));
    }

    fn symlink(&self, req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        self.entry(reply, self.engine.symlink(&ctx(req), parent.0, link_name, target.as_os_str()));
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        Self::empty(reply, self.engine.rename(&ctx(req), parent.0, name, newparent.0, newname, flags.bits()));
    }

    fn link(&self, req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        self.entry(reply, self.engine.link(&ctx(req), ino.0, newparent.0, newname));
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self.engine.open(&ctx(req), ino.0, flags.0) {
            Ok(fh) => reply.opened(FileHandle(fh), self.open_flags()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut reply = Some(reply);
        let r = self.engine.read(&ctx(req), ino.0, fh.0, offset, size, &mut |d| {
            if let Some(r) = reply.take() {
                r.data(d)
            }
        });
        if let (Err(e), Some(r)) = (r, reply) {
            r.error(err(e));
        }
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.engine.write(&ctx(req), ino.0, fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(err(e)),
        }
    }

    fn flush(&self, req: &Request, ino: INodeNo, fh: FileHandle, lock_owner: LockOwner, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.flush(&ctx(req), ino.0, fh.0, lock_owner.0));
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        Self::empty(reply, self.engine.release(&ctx(req), ino.0, fh.0));
    }

    fn fsync(&self, req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.fsync(&ctx(req), ino.0, fh.0, datasync));
    }

    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.engine.opendir(&ctx(req), ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readdir(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let r = self.engine.readdir(&ctx(req), ino.0, fh.0, offset, &mut |ino, off, kind, name| {
            reply.add(INodeNo(ino), off, ftype(kind), OsStr::from_bytes(name))
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }

    fn releasedir(&self, req: &Request, ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.releasedir(&ctx(req), ino.0, fh.0));
    }

    fn fsyncdir(&self, req: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.fsyncdir(&ctx(req), ino.0, fh.0, datasync));
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        match self.engine.statfs(&ctx(req), ino.0) {
            Ok(s) => reply.statfs(s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.frsize),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        if position != 0 {
            return reply.error(err(libc::EINVAL));
        }
        Self::empty(reply, self.engine.setxattr(&ctx(req), ino.0, name, value, flags));
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        match self.engine.getxattr(&ctx(req), ino.0, name, size) {
            Ok(XattrOut::Size(n)) => reply.size(n as u32),
            Ok(XattrOut::Data(d)) => reply.data(&d),
            Err(e) => reply.error(err(e)),
        }
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.engine.listxattr(&ctx(req), ino.0, size) {
            Ok(XattrOut::Size(n)) => reply.size(n as u32),
            Ok(XattrOut::Data(d)) => reply.data(&d),
            Err(e) => reply.error(err(e)),
        }
    }

    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.removexattr(&ctx(req), ino.0, name));
    }

    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.access(&ctx(req), ino.0, mask.bits()));
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self.engine.create(&ctx(req), parent.0, name, mode, flags) {
            Ok((a, fh)) => reply.created(&self.attr_ttl(), &file_attr(&a), Generation(0), FileHandle(fh), self.open_flags()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn getlk(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        match self.engine.getlk(&ctx(req), ino.0, lock_owner.0, lock_from_range(typ, start, end, pid)) {
            Ok(l) => reply.locked(l.start, lock_end(&l), l.typ, l.pid as u32),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setlk(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        self.engine.setlk(
            &ctx(req),
            ino.0,
            fh.0,
            lock_owner.0,
            lock_from_range(typ, start, end, pid),
            sleep,
            Box::new(move |r| Self::empty(reply, r)),
        );
    }

    fn bmap(&self, _req: &Request, _ino: INodeNo, _blocksize: u32, _idx: u64, reply: ReplyBmap) {
        reply.error(err(self.engine.unsupported(OpKind::Bmap)));
    }

    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        // Arbitrary ioctls cannot be mirrored safely (opaque in/out data).
        reply.error(err(libc::ENOTTY));
        self.engine.unsupported(OpKind::Ioctl);
    }

    fn poll(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _ph: PollNotifier,
        _events: PollEvents,
        _flags: PollFlags,
        reply: ReplyPoll,
    ) {
        reply.error(err(self.engine.unsupported(OpKind::Poll)));
    }

    fn fallocate(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, length: u64, mode: i32, reply: ReplyEmpty) {
        Self::empty(reply, self.engine.fallocate(&ctx(req), ino.0, fh.0, offset, length, mode));
    }

    fn lseek(&self, req: &Request, ino: INodeNo, fh: FileHandle, offset: i64, whence: i32, reply: ReplyLseek) {
        match self.engine.lseek(&ctx(req), ino.0, fh.0, offset, whence) {
            Ok(o) => reply.offset(o),
            Err(e) => reply.error(err(e)),
        }
    }

    fn copy_file_range(
        &self,
        req: &Request,
        _ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        _ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        match self.engine.copy_file_range(&ctx(req), fh_in.0, offset_in, fh_out.0, offset_out, len, flags.bits() as u32) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(err(e)),
        }
    }
}
