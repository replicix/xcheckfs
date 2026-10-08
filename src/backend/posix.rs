//! The real backend: a directory on any POSIX file system.
//!
//! The root directory is opened (O_PATH) when the backend is constructed,
//! i.e. before the FUSE mount exists. That makes it possible to mount
//! xcheckfs on top of the primary directory itself: the descriptor keeps
//! referring to the underlying directory.

use std::ffi::CStr;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use super::{Backend, DirEntry, Lock, StatFs, TimeSpec, XattrOut};
use crate::sys::{self, FileKind, Stat, SysResult, cvt, cvt_size, owned, proc_path};

pub struct PosixBackend {
    label: String,
    path: PathBuf,
    root: OwnedFd,
}

impl PosixBackend {
    pub fn open(label: &str, path: &Path) -> std::io::Result<PosixBackend> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: valid C string.
        let fd = unsafe {
            libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        };
        let root = owned(fd).map_err(std::io::Error::from_raw_os_error)?;
        Ok(PosixBackend { label: label.to_string(), path: path.to_path_buf(), root })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root_fd(&self) -> BorrowedFd<'_> {
        self.root.as_fd()
    }
}

fn timespec(t: TimeSpec) -> libc::timespec {
    match t {
        TimeSpec::Omit => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        TimeSpec::Now => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
        TimeSpec::Set(ts) => libc::timespec { tv_sec: ts.sec, tv_nsec: ts.nsec as i64 },
    }
}

fn xattr_result(r: libc::ssize_t, mut buf: Vec<u8>, size: usize) -> SysResult<XattrOut> {
    let n = cvt_size(r)?;
    if size == 0 {
        Ok(XattrOut::Size(n))
    } else {
        buf.truncate(n);
        Ok(XattrOut::Data(buf))
    }
}

impl Backend for PosixBackend {
    fn label(&self) -> &str {
        &self.label
    }

    fn root(&self) -> SysResult<OwnedFd> {
        sys::dup(self.root.as_fd())
    }

    fn lookup(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<(OwnedFd, Stat)> {
        // SAFETY: valid fd and C string.
        let fd = owned(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let st = sys::fstat(fd.as_fd())?;
        Ok((fd, st))
    }

    fn stat_at(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<Stat> {
        sys::fstatat(dir, name, libc::AT_SYMLINK_NOFOLLOW)
    }

    fn stat(&self, fd: BorrowedFd<'_>) -> SysResult<Stat> {
        sys::fstat(fd)
    }

    fn chmod(&self, node: BorrowedFd<'_>, kind: FileKind, mode: u32) -> SysResult<()> {
        if kind == FileKind::Symlink {
            // Linux has no symlink permissions; fchmodat(AT_SYMLINK_NOFOLLOW)
            // reports the canonical error.
            return Err(libc::EOPNOTSUPP);
        }
        let p = proc_path(node);
        // SAFETY: valid C string.
        cvt(unsafe { libc::chmod(p.as_ptr(), mode as libc::mode_t) }).map(drop)
    }

    fn chown(&self, node: BorrowedFd<'_>, uid: Option<u32>, gid: Option<u32>) -> SysResult<()> {
        let u = uid.unwrap_or(u32::MAX);
        let g = gid.unwrap_or(u32::MAX);
        // SAFETY: valid fd.
        cvt(unsafe {
            libc::fchownat(
                node.as_raw_fd(),
                sys::EMPTY.as_ptr(),
                u,
                g,
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map(drop)
    }

    fn truncate(&self, node: BorrowedFd<'_>, file: Option<BorrowedFd<'_>>, size: u64) -> SysResult<()> {
        // SAFETY: valid fds / C string.
        cvt(unsafe {
            match file {
                Some(f) => libc::ftruncate(f.as_raw_fd(), size as libc::off_t),
                None => libc::truncate(proc_path(node).as_ptr(), size as libc::off_t),
            }
        })
        .map(drop)
    }

    fn utimens(
        &self,
        node: BorrowedFd<'_>,
        kind: FileKind,
        file: Option<BorrowedFd<'_>>,
        atime: TimeSpec,
        mtime: TimeSpec,
    ) -> SysResult<()> {
        let ts = [timespec(atime), timespec(mtime)];
        // SAFETY: valid fds, valid timespec array.
        cvt(unsafe {
            match file {
                Some(f) => libc::futimens(f.as_raw_fd(), ts.as_ptr()),
                None if kind == FileKind::Symlink => libc::utimensat(
                    node.as_raw_fd(),
                    sys::EMPTY.as_ptr(),
                    ts.as_ptr(),
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                ),
                None => libc::utimensat(libc::AT_FDCWD, proc_path(node).as_ptr(), ts.as_ptr(), 0),
            }
        })
        .map(drop)
    }

    fn readlink(&self, node: BorrowedFd<'_>) -> SysResult<Vec<u8>> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        // SAFETY: valid fd and buffer.
        let n = cvt_size(unsafe {
            libc::readlinkat(
                node.as_raw_fd(),
                sys::EMPTY.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        })?;
        buf.truncate(n);
        Ok(buf)
    }

    fn mknod(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32, rdev: u64) -> SysResult<()> {
        // SAFETY: valid fd / C string.
        cvt(unsafe {
            libc::mknodat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t)
        })
        .map(drop)
    }

    fn mkdir(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> SysResult<()> {
        // SAFETY: valid fd / C string.
        cvt(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) }).map(drop)
    }

    fn unlink(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        // SAFETY: valid fd / C string.
        cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
    }

    fn rmdir(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        // SAFETY: valid fd / C string.
        cvt(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }).map(drop)
    }

    fn symlink(&self, target: &CStr, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        // SAFETY: valid fd / C strings.
        cvt(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) }).map(drop)
    }

    fn rename(
        &self,
        dir: BorrowedFd<'_>,
        name: &CStr,
        newdir: BorrowedFd<'_>,
        newname: &CStr,
        flags: u32,
    ) -> SysResult<()> {
        // A raw syscall: musl has no renameat2() wrapper.
        // SAFETY: valid fds / C strings.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                dir.as_raw_fd(),
                name.as_ptr(),
                newdir.as_raw_fd(),
                newname.as_ptr(),
                flags,
            )
        };
        if r < 0 { Err(sys::errno()) } else { Ok(()) }
    }

    fn link(&self, node: BorrowedFd<'_>, newdir: BorrowedFd<'_>, newname: &CStr) -> SysResult<()> {
        let p = proc_path(node);
        // SAFETY: valid fds / C strings.
        cvt(unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                p.as_ptr(),
                newdir.as_raw_fd(),
                newname.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        })
        .map(drop)
    }

    fn open(&self, node: BorrowedFd<'_>, flags: i32) -> SysResult<OwnedFd> {
        let flags = (flags & !(libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW)) | libc::O_CLOEXEC;
        let p = proc_path(node);
        // SAFETY: valid C string.
        owned(unsafe { libc::open(p.as_ptr(), flags) })
    }

    fn create(&self, dir: BorrowedFd<'_>, name: &CStr, flags: i32, mode: u32) -> SysResult<OwnedFd> {
        // SAFETY: valid fd / C string.
        owned(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CREAT | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        })
    }

    fn pread(&self, file: BorrowedFd<'_>, buf: &mut [u8], off: u64) -> SysResult<usize> {
        // Loop over short reads: a single pread may legitimately return
        // less than asked before EOF (e.g. on FUSE or network backends), and
        // FUSE read semantics require the full amount unless at EOF.
        let mut done = 0;
        while done < buf.len() {
            // SAFETY: valid fd, buffer bounds checked.
            let n = cvt_size(unsafe {
                libc::pread(
                    file.as_raw_fd(),
                    buf[done..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - done,
                    (off + done as u64) as libc::off_t,
                )
            });
            match n {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(libc::EINTR) => continue,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    }

    fn pwrite(&self, file: BorrowedFd<'_>, data: &[u8], off: u64) -> SysResult<usize> {
        loop {
            // SAFETY: valid fd and buffer.
            match cvt_size(unsafe {
                libc::pwrite(
                    file.as_raw_fd(),
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    off as libc::off_t,
                )
            }) {
                Err(libc::EINTR) => continue,
                r => return r,
            }
        }
    }

    fn flush(&self, file: BorrowedFd<'_>) -> SysResult<()> {
        // close(dup(fd)) makes the backend see a close(2), so a FUSE or
        // network backend runs its own flush logic and reports its errors.
        // Best effort: when xcheckfs itself is out of descriptors the flush
        // is skipped (the backend still sees the close at release), since a
        // native close(2) never fails with EMFILE.
        let d = match sys::dup(file) {
            Err(libc::EMFILE | libc::ENFILE) => return Ok(()),
            r => r?,
        };
        let raw = std::os::fd::IntoRawFd::into_raw_fd(d);
        // SAFETY: we own `raw`.
        cvt(unsafe { libc::close(raw) }).map(drop)
    }

    fn fsync(&self, file: BorrowedFd<'_>, datasync: bool) -> SysResult<()> {
        // SAFETY: valid fd.
        cvt(unsafe {
            if datasync { libc::fdatasync(file.as_raw_fd()) } else { libc::fsync(file.as_raw_fd()) }
        })
        .map(drop)
    }

    fn opendir(&self, node: BorrowedFd<'_>) -> SysResult<OwnedFd> {
        // Through the magic link, like `open`: opening "." relative to the descriptor would demand search (x)
        // permission on the directory, while listing it requires read permission only.
        let p = proc_path(node);
        // SAFETY: valid C string.
        owned(unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) })
    }

    fn readdir(&self, dir: BorrowedFd<'_>) -> SysResult<Vec<DirEntry>> {
        // Work on a private duplicate so the stream position of the handle
        // is irrelevant and concurrent readdirs cannot interfere.
        let d = sys::dup(dir)?;
        let raw = std::os::fd::IntoRawFd::into_raw_fd(d);
        // SAFETY: fdopendir takes ownership of `raw` on success.
        let dp = unsafe { libc::fdopendir(raw) };
        if dp.is_null() {
            let e = sys::errno();
            // SAFETY: fdopendir failed, we still own raw.
            unsafe { libc::close(raw) };
            return Err(e);
        }
        struct Closer(*mut libc::DIR);
        impl Drop for Closer {
            fn drop(&mut self) {
                // SAFETY: valid DIR*.
                unsafe { libc::closedir(self.0) };
            }
        }
        let _c = Closer(dp);
        // SAFETY: valid DIR*.
        unsafe { libc::rewinddir(dp) };
        let mut out = Vec::new();
        loop {
            // SAFETY: errno is thread-local.
            unsafe { *libc::__errno_location() = 0 };
            // SAFETY: valid DIR*.
            let ent = unsafe { libc::readdir64(dp) };
            if ent.is_null() {
                let e = sys::errno();
                if e != 0 {
                    return Err(e);
                }
                break;
            }
            // SAFETY: readdir returned a valid entry.
            let ent = unsafe { &*ent };
            // SAFETY: d_name is NUL-terminated.
            let name = unsafe { CStr::from_ptr(ent.d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            out.push(DirEntry {
                name: name.to_vec(),
                ino: ent.d_ino,
                kind: FileKind::from_dtype(ent.d_type),
            });
        }
        Ok(out)
    }

    fn statfs(&self, node: BorrowedFd<'_>) -> SysResult<StatFs> {
        let mut s = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid fd, valid out pointer.
        cvt(unsafe { libc::fstatvfs(node.as_raw_fd(), s.as_mut_ptr()) })?;
        // SAFETY: initialised by fstatvfs.
        let s = unsafe { s.assume_init() };
        Ok(StatFs {
            blocks: s.f_blocks,
            bfree: s.f_bfree,
            bavail: s.f_bavail,
            files: s.f_files,
            ffree: s.f_ffree,
            bsize: s.f_bsize as u32,
            namelen: s.f_namemax as u32,
            frsize: s.f_frsize as u32,
        })
    }

    fn setxattr(&self, node: BorrowedFd<'_>, name: &CStr, value: &[u8], flags: i32) -> SysResult<()> {
        let p = proc_path(node);
        // SAFETY: valid C strings and buffer.
        cvt(unsafe {
            libc::setxattr(
                p.as_ptr(),
                name.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                flags,
            )
        })
        .map(drop)
    }

    fn getxattr(&self, node: BorrowedFd<'_>, name: &CStr, size: usize) -> SysResult<XattrOut> {
        let p = proc_path(node);
        let mut buf = vec![0u8; size];
        // SAFETY: valid C strings and buffer.
        let r = unsafe {
            libc::getxattr(
                p.as_ptr(),
                name.as_ptr(),
                if size == 0 { std::ptr::null_mut() } else { buf.as_mut_ptr() as *mut libc::c_void },
                size,
            )
        };
        xattr_result(r, buf, size)
    }

    fn listxattr(&self, node: BorrowedFd<'_>, size: usize) -> SysResult<XattrOut> {
        let p = proc_path(node);
        let mut buf = vec![0u8; size];
        // SAFETY: valid C string and buffer.
        let r = unsafe {
            libc::listxattr(
                p.as_ptr(),
                if size == 0 { std::ptr::null_mut() } else { buf.as_mut_ptr() as *mut libc::c_char },
                size,
            )
        };
        xattr_result(r, buf, size)
    }

    fn removexattr(&self, node: BorrowedFd<'_>, name: &CStr) -> SysResult<()> {
        let p = proc_path(node);
        // SAFETY: valid C strings.
        cvt(unsafe { libc::removexattr(p.as_ptr(), name.as_ptr()) }).map(drop)
    }

    fn access(&self, node: BorrowedFd<'_>, mask: i32) -> SysResult<()> {
        let p = proc_path(node);
        // AT_EACCESS: check with the (per-thread) fs credentials.
        // SAFETY: valid C string.
        cvt(unsafe { libc::faccessat(libc::AT_FDCWD, p.as_ptr(), mask, libc::AT_EACCESS) }).map(drop)
    }

    fn fallocate(&self, file: BorrowedFd<'_>, mode: i32, off: u64, len: u64) -> SysResult<()> {
        // SAFETY: valid fd.
        cvt(unsafe {
            libc::fallocate(file.as_raw_fd(), mode, off as libc::off_t, len as libc::off_t)
        })
        .map(drop)
    }

    fn lseek(&self, file: BorrowedFd<'_>, off: i64, whence: i32) -> SysResult<i64> {
        // A private description: lseek must not move a shared offset.
        // SAFETY: valid fd.
        let r = unsafe { libc::lseek(file.as_raw_fd(), off, whence) };
        if r < 0 { Err(sys::errno()) } else { Ok(r) }
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
        let mut oi = off_in as libc::loff_t;
        let mut oo = off_out as libc::loff_t;
        // A raw syscall: not every libc (musl) has a wrapper.
        // SAFETY: valid fds and offset pointers.
        let r = unsafe {
            libc::syscall(
                libc::SYS_copy_file_range,
                fin.as_raw_fd(),
                &mut oi as *mut libc::loff_t,
                fout.as_raw_fd(),
                &mut oo as *mut libc::loff_t,
                len,
                flags,
            )
        };
        if r < 0 { Err(sys::errno()) } else { Ok(r as usize) }
    }

    fn getlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<Lock> {
        let mut fl = to_flock(lock);
        // SAFETY: valid fd and flock.
        cvt(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut fl) })?;
        Ok(from_flock(&fl))
    }

    fn setlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<()> {
        let mut fl = to_flock(lock);
        // SAFETY: valid fd and flock.
        cvt(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &mut fl) }).map(drop)
    }
}

fn to_flock(l: &Lock) -> libc::flock {
    // SAFETY: flock is plain data.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = l.typ as libc::c_short;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = l.start as libc::off_t;
    fl.l_len = l.len as libc::off_t;
    fl.l_pid = 0; // required for OFD locks
    fl
}

fn from_flock(fl: &libc::flock) -> Lock {
    Lock { typ: fl.l_type as i32, start: fl.l_start as u64, len: fl.l_len as u64, pid: fl.l_pid }
}
