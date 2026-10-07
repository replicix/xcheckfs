//! Backends: the two file systems xcheckfs drives in lockstep.
//!
//! A backend works purely on file descriptors handed out by itself: O_PATH
//! descriptors for inodes ("nodes") and regular descriptors for open files
//! and directories. The engine never touches paths, so renames performed
//! through the mount cannot make a backend operate on the wrong object.
//!
//! [`posix::PosixBackend`] is the real implementation (any POSIX file system
//! reachable through a directory). [`fault::FaultBackend`] wraps it to
//! inject defects for the test suite.

use std::ffi::CStr;
use std::os::fd::{BorrowedFd, OwnedFd};

use crate::sys::{FileKind, Stat, SysResult, Ts};

pub mod posix;

#[cfg(any(test, feature = "fault-injection"))]
pub mod fault;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: Vec<u8>,
    pub ino: u64,
    /// `None` when the file system returned `DT_UNKNOWN`.
    pub kind: Option<FileKind>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSpec {
    Omit,
    Now,
    Set(Ts),
}

/// A byte-range lock in `struct flock` terms: `len == 0` means "to EOF".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lock {
    pub typ: i32,
    pub start: u64,
    pub len: u64,
    pub pid: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XattrOut {
    /// Reply to a size probe (`size == 0` request).
    Size(usize),
    Data(Vec<u8>),
}

/// One of the two file systems. All methods are synchronous syscall-shaped
/// operations; errors are raw errno values and must be passed through
/// unchanged, since the engine compares them.
pub trait Backend: Send + Sync {
    fn label(&self) -> &str;

    /// O_PATH descriptor of the backend's root directory.
    fn root(&self) -> SysResult<OwnedFd>;

    /// Opens `name` in `dir` as an O_PATH|O_NOFOLLOW descriptor and stats it.
    fn lookup(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<(OwnedFd, Stat)>;
    /// Stats `name` in `dir` without following symlinks.
    fn stat_at(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<Stat>;
    /// Stats a node or an open file.
    fn stat(&self, fd: BorrowedFd<'_>) -> SysResult<Stat>;

    fn chmod(&self, node: BorrowedFd<'_>, kind: FileKind, mode: u32) -> SysResult<()>;
    fn chown(&self, node: BorrowedFd<'_>, uid: Option<u32>, gid: Option<u32>) -> SysResult<()>;
    fn truncate(&self, node: BorrowedFd<'_>, file: Option<BorrowedFd<'_>>, size: u64) -> SysResult<()>;
    fn utimens(
        &self,
        node: BorrowedFd<'_>,
        kind: FileKind,
        file: Option<BorrowedFd<'_>>,
        atime: TimeSpec,
        mtime: TimeSpec,
    ) -> SysResult<()>;
    fn readlink(&self, node: BorrowedFd<'_>) -> SysResult<Vec<u8>>;

    fn mknod(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32, rdev: u64) -> SysResult<()>;
    fn mkdir(&self, dir: BorrowedFd<'_>, name: &CStr, mode: u32) -> SysResult<()>;
    fn unlink(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()>;
    fn rmdir(&self, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()>;
    fn symlink(&self, target: &CStr, dir: BorrowedFd<'_>, name: &CStr) -> SysResult<()>;
    fn rename(
        &self,
        dir: BorrowedFd<'_>,
        name: &CStr,
        newdir: BorrowedFd<'_>,
        newname: &CStr,
        flags: u32,
    ) -> SysResult<()>;
    fn link(&self, node: BorrowedFd<'_>, newdir: BorrowedFd<'_>, newname: &CStr) -> SysResult<()>;

    /// Opens an existing node. `flags` never contains O_CREAT.
    fn open(&self, node: BorrowedFd<'_>, flags: i32) -> SysResult<OwnedFd>;
    /// `openat(dir, name, flags | O_CREAT, mode)`.
    fn create(&self, dir: BorrowedFd<'_>, name: &CStr, flags: i32, mode: u32) -> SysResult<OwnedFd>;
    fn pread(&self, file: BorrowedFd<'_>, buf: &mut [u8], off: u64) -> SysResult<usize>;
    fn pwrite(&self, file: BorrowedFd<'_>, data: &[u8], off: u64) -> SysResult<usize>;
    /// Called on every close(2) of a file descriptor in the mount.
    fn flush(&self, file: BorrowedFd<'_>) -> SysResult<()>;
    fn fsync(&self, file: BorrowedFd<'_>, datasync: bool) -> SysResult<()>;

    fn opendir(&self, node: BorrowedFd<'_>) -> SysResult<OwnedFd>;
    /// The complete directory listing, from the start, without `.`/`..`.
    fn readdir(&self, dir: BorrowedFd<'_>) -> SysResult<Vec<DirEntry>>;

    fn statfs(&self, node: BorrowedFd<'_>) -> SysResult<StatFs>;

    fn setxattr(&self, node: BorrowedFd<'_>, name: &CStr, value: &[u8], flags: i32) -> SysResult<()>;
    fn getxattr(&self, node: BorrowedFd<'_>, name: &CStr, size: usize) -> SysResult<XattrOut>;
    fn listxattr(&self, node: BorrowedFd<'_>, size: usize) -> SysResult<XattrOut>;
    fn removexattr(&self, node: BorrowedFd<'_>, name: &CStr) -> SysResult<()>;

    fn access(&self, node: BorrowedFd<'_>, mask: i32) -> SysResult<()>;
    fn fallocate(&self, file: BorrowedFd<'_>, mode: i32, off: u64, len: u64) -> SysResult<()>;
    fn lseek(&self, file: BorrowedFd<'_>, off: i64, whence: i32) -> SysResult<i64>;
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        fin: BorrowedFd<'_>,
        off_in: u64,
        fout: BorrowedFd<'_>,
        off_out: u64,
        len: usize,
        flags: u32,
    ) -> SysResult<usize>;

    /// Non-blocking open-file-description lock test (`F_OFD_GETLK`).
    fn getlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<Lock>;
    /// Non-blocking open-file-description lock (`F_OFD_SETLK`).
    fn setlk(&self, file: BorrowedFd<'_>, lock: &Lock) -> SysResult<()>;
}
