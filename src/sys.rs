//! Thin, allocation-free wrappers around the Linux syscalls xcheckfs needs.
//!
//! Every function returns `SysResult<T>` = `Result<T, i32>` where the error
//! is a raw errno. Errno values are what we compare between the two backends,
//! so they must never be mapped or normalised on the way.

use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

pub type SysResult<T> = Result<T, i32>;

#[inline]
pub fn errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[inline]
pub fn cvt(r: libc::c_int) -> SysResult<libc::c_int> {
    if r < 0 { Err(errno()) } else { Ok(r) }
}

#[inline]
pub fn cvt_size(r: libc::ssize_t) -> SysResult<usize> {
    if r < 0 { Err(errno()) } else { Ok(r as usize) }
}

#[inline]
pub fn owned(fd: libc::c_int) -> SysResult<OwnedFd> {
    if fd < 0 {
        Err(errno())
    } else {
        // SAFETY: the syscall just returned a fresh, owned descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// `/proc/self/fd/N` for an fd, used for operations that have no
/// `AT_EMPTY_PATH` variant (xattrs, chmod, truncate, open by O_PATH fd).
pub struct ProcPath([u8; 32]);

impl ProcPath {
    pub fn new(fd: RawFd) -> Self {
        let mut buf = [0u8; 32];
        let s = format!("/proc/self/fd/{fd}");
        buf[..s.len()].copy_from_slice(s.as_bytes());
        ProcPath(buf)
    }
    pub fn as_ptr(&self) -> *const libc::c_char {
        self.0.as_ptr() as *const libc::c_char
    }
}

pub fn proc_path(fd: BorrowedFd<'_>) -> ProcPath {
    ProcPath::new(fd.as_raw_fd())
}

pub const EMPTY: &CStr = c"";

/// Timestamp with nanosecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ts {
    pub sec: i64,
    pub nsec: u32,
}

impl Ts {
    pub fn as_nanos(&self) -> i128 {
        self.sec as i128 * 1_000_000_000 + self.nsec as i128
    }
    pub fn abs_diff_ns(&self, other: &Ts) -> u128 {
        (self.as_nanos() - other.as_nanos()).unsigned_abs()
    }
    pub fn to_system_time(self) -> std::time::SystemTime {
        use std::time::{Duration, UNIX_EPOCH};
        if self.sec >= 0 {
            UNIX_EPOCH + Duration::new(self.sec as u64, self.nsec)
        } else {
            UNIX_EPOCH - Duration::new((-self.sec) as u64, 0) + Duration::new(0, self.nsec)
        }
    }
    pub fn from_system_time(t: std::time::SystemTime) -> Ts {
        use std::time::UNIX_EPOCH;
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Ts { sec: d.as_secs() as i64, nsec: d.subsec_nanos() },
            Err(e) => {
                let d = e.duration();
                let mut sec = -(d.as_secs() as i64);
                let mut nsec = d.subsec_nanos();
                if nsec > 0 {
                    sec -= 1;
                    nsec = 1_000_000_000 - nsec;
                }
                Ts { sec, nsec }
            }
        }
    }
}

impl std::fmt::Display for Ts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{:09}", self.sec, self.nsec)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Regular,
    Dir,
    Symlink,
    Fifo,
    Socket,
    CharDev,
    BlockDev,
}

impl FileKind {
    pub fn from_mode(mode: u32) -> FileKind {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => FileKind::Dir,
            libc::S_IFLNK => FileKind::Symlink,
            libc::S_IFIFO => FileKind::Fifo,
            libc::S_IFSOCK => FileKind::Socket,
            libc::S_IFCHR => FileKind::CharDev,
            libc::S_IFBLK => FileKind::BlockDev,
            _ => FileKind::Regular,
        }
    }
    pub fn from_dtype(d: u8) -> Option<FileKind> {
        Some(match d {
            libc::DT_REG => FileKind::Regular,
            libc::DT_DIR => FileKind::Dir,
            libc::DT_LNK => FileKind::Symlink,
            libc::DT_FIFO => FileKind::Fifo,
            libc::DT_SOCK => FileKind::Socket,
            libc::DT_CHR => FileKind::CharDev,
            libc::DT_BLK => FileKind::BlockDev,
            _ => return None,
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            FileKind::Regular => "file",
            FileKind::Dir => "dir",
            FileKind::Symlink => "symlink",
            FileKind::Fifo => "fifo",
            FileKind::Socket => "socket",
            FileKind::CharDev => "chardev",
            FileKind::BlockDev => "blockdev",
        }
    }
}

/// The subset of `struct stat` xcheckfs cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u64,
    pub blocks: u64,
    pub atime: Ts,
    pub mtime: Ts,
    pub ctime: Ts,
}

impl Stat {
    pub fn from_libc(s: &libc::stat) -> Stat {
        Stat {
            dev: s.st_dev,
            ino: s.st_ino,
            mode: s.st_mode,
            nlink: s.st_nlink,
            uid: s.st_uid,
            gid: s.st_gid,
            rdev: s.st_rdev,
            size: s.st_size as u64,
            blksize: s.st_blksize as u64,
            blocks: s.st_blocks as u64,
            atime: Ts { sec: s.st_atime, nsec: s.st_atime_nsec as u32 },
            mtime: Ts { sec: s.st_mtime, nsec: s.st_mtime_nsec as u32 },
            ctime: Ts { sec: s.st_ctime, nsec: s.st_ctime_nsec as u32 },
        }
    }
    pub fn kind(&self) -> FileKind {
        FileKind::from_mode(self.mode)
    }
    /// Permission bits including setuid/setgid/sticky.
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }
    pub fn ident(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

pub fn fstatat(dir: BorrowedFd<'_>, name: &CStr, flags: i32) -> SysResult<Stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid fd, valid C string, st is a valid out pointer.
    cvt(unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), flags) })?;
    // SAFETY: fstatat succeeded and initialised the buffer.
    let st = unsafe { st.assume_init() };
    Ok(Stat::from_libc(&st))
}

pub fn fstat(fd: BorrowedFd<'_>) -> SysResult<Stat> {
    fstatat(fd, EMPTY, libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW)
}

pub fn cstr(bytes: &[u8]) -> SysResult<CString> {
    CString::new(bytes).map_err(|_| libc::EINVAL)
}

/// Name of an errno, e.g. `ENOENT`, for logs and rule files.
pub fn errno_name(e: i32) -> &'static str {
    match e {
        0 => "OK",
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::ESRCH => "ESRCH",
        libc::EINTR => "EINTR",
        libc::EIO => "EIO",
        libc::ENXIO => "ENXIO",
        libc::E2BIG => "E2BIG",
        libc::EBADF => "EBADF",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EBUSY => "EBUSY",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENODEV => "ENODEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EISDIR => "EISDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ENOTTY => "ENOTTY",
        libc::ETXTBSY => "ETXTBSY",
        libc::EFBIG => "EFBIG",
        libc::ENOSPC => "ENOSPC",
        libc::ESPIPE => "ESPIPE",
        libc::EROFS => "EROFS",
        libc::EMLINK => "EMLINK",
        libc::EPIPE => "EPIPE",
        libc::ERANGE => "ERANGE",
        libc::EDEADLK => "EDEADLK",
        libc::ENAMETOOLONG => "ENAMETOOLONG",
        libc::ENOLCK => "ENOLCK",
        libc::ENOSYS => "ENOSYS",
        libc::ENOTEMPTY => "ENOTEMPTY",
        libc::ELOOP => "ELOOP",
        libc::ENODATA => "ENODATA",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ENOTCONN => "ENOTCONN",
        libc::ESTALE => "ESTALE",
        libc::EDQUOT => "EDQUOT",
        libc::ECANCELED => "ECANCELED",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::EBADMSG => "EBADMSG",
        libc::EILSEQ => "EILSEQ",
        _ => "E?",
    }
}

/// Inverse of [`errno_name`]; accepts `ENOENT`, `enoent` or a number.
pub fn errno_from_name(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return Some(n);
    }
    let up = s.to_ascii_uppercase();
    if up == "ENOTSUP" {
        return Some(libc::ENOTSUP);
    }
    if up == "ENOATTR" {
        return Some(libc::ENODATA);
    }
    (0..200).find(|&e| errno_name(e) == up)
}

pub fn fmt_errno(e: i32) -> String {
    format!("{} ({})", errno_name(e), e)
}

pub fn fmt_result<T>(r: &SysResult<T>) -> String {
    match r {
        Ok(_) => "OK".to_string(),
        Err(e) => fmt_errno(*e),
    }
}

/// Per-thread filesystem credentials.
///
/// `setfsuid`/`setfsgid`/`setgroups` are invoked as raw syscalls: glibc's
/// `setgroups` wrapper would broadcast to every thread of the process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Creds {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

pub struct CredsGuard {
    active: bool,
}

impl CredsGuard {
    pub fn none() -> CredsGuard {
        CredsGuard { active: false }
    }

    /// Switches the calling thread to `c`. The guard switches back to root.
    pub fn switch(c: &Creds) -> SysResult<CredsGuard> {
        // SAFETY: plain syscalls with integer/pointer arguments.
        unsafe {
            if libc::syscall(
                libc::SYS_setgroups,
                c.groups.len() as libc::size_t,
                c.groups.as_ptr(),
            ) < 0
            {
                return Err(errno());
            }
            libc::syscall(libc::SYS_setfsgid, c.gid as libc::c_long);
            libc::syscall(libc::SYS_setfsuid, c.uid as libc::c_long);
            // setfs*id return the previous value and cannot report failure;
            // read the value back to make sure the switch happened.
            let cur_uid = libc::syscall(libc::SYS_setfsuid, -1i64 as libc::c_long) as u32;
            let cur_gid = libc::syscall(libc::SYS_setfsgid, -1i64 as libc::c_long) as u32;
            let g = CredsGuard { active: true };
            if cur_uid != c.uid || cur_gid != c.gid {
                drop(g);
                return Err(libc::EPERM);
            }
            Ok(g)
        }
    }
}

impl Drop for CredsGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        // SAFETY: restoring root credentials on this thread.
        unsafe {
            libc::syscall(libc::SYS_setfsuid, 0 as libc::c_long);
            libc::syscall(libc::SYS_setfsgid, 0 as libc::c_long);
            let root: [u32; 1] = [0];
            libc::syscall(libc::SYS_setgroups, 1 as libc::size_t, root.as_ptr());
        }
    }
}

/// Supplementary groups of a process, read from `/proc/<pid>/status`.
pub fn proc_groups(pid: u32) -> Option<Vec<u32>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = s.lines().find(|l| l.starts_with("Groups:"))?;
    Some(
        line["Groups:".len()..]
            .split_whitespace()
            .filter_map(|g| g.parse().ok())
            .collect(),
    )
}

pub fn is_root() -> bool {
    // SAFETY: trivial syscall.
    unsafe { libc::geteuid() == 0 }
}

/// Raises RLIMIT_NOFILE to the hard limit. xcheckfs keeps two O_PATH
/// descriptors per inode the kernel has cached.
pub fn raise_nofile_limit() -> u64 {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: valid out pointer.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            rl.rlim_cur = rl.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
    rl.rlim_cur
}

pub fn dup(fd: BorrowedFd<'_>) -> SysResult<OwnedFd> {
    // SAFETY: valid fd.
    owned(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })
}

/// Reads the path an O_PATH descriptor currently refers to.
pub fn fd_path(fd: BorrowedFd<'_>) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()
}
