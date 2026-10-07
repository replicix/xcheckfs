//! FUSE-level integration tests: the engine is mounted in-process (`fuser::Session`) as the current user and real
//! workloads run through the kernel with `std::fs` and `libc`. Every test asserts zero mismatches and that the primary
//! and secondary trees are identical afterwards. Skipped (with a message) when FUSE is not usable here.

mod common;

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use fuser::{Config, MountOption};
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::fusefs::XcheckFs;
use xcheckfs::policy::{Action, MismatchKind as K};
use xcheckfs::stats::OpKind;

const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

/// A mounted xcheckfs. Dropping unmounts, also when a test panics (frozen operations are released first).
pub struct Mount {
    session: Option<fuser::BackgroundSession>,
    pub h: Arc<Harness>,
    pub mnt: PathBuf,
    _mnt_dir: tempfile::TempDir,
    watchdog: Option<std::process::Child>,
}

/// Longest a single test may use its mount. A deadlock in the file system under test would otherwise leave the test
/// process (and the mount) stuck in uninterruptible sleep. The watchdog is a separate `bash` process (a thread of this
/// process could itself be wedged on the address-space lock): when the time is up it aborts the FUSE connection
/// through /sys/fs/fuse/connections (writable by the mounting user) and kills the test process. Dropping the
/// `Mount` closes the pipe the watchdog waits on, which makes it exit quietly.
const WATCHDOG_SECS: u64 = 120;

fn spawn_watchdog(mnt: &Path) -> Option<std::process::Child> {
    // SAFETY: plain stat of the freshly mounted (healthy) mount point.
    let minor = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::stat(cpath(mnt).as_ptr(), &mut st);
        libc::minor(st.st_dev)
    };
    let secs = std::env::var("XCHECKFS_WATCHDOG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(WATCHDOG_SECS * soak() as u64);
    let script = format!(
        "read -t {secs} _; rc=$?; if [ $rc -gt 128 ]; then \
           echo 'WATCHDOG: FUSE test hung for {secs}s: aborting connection {minor} and killing {pid}' >&2; \
           echo 1 > /sys/fs/fuse/connections/{minor}/abort; sleep 1; kill -9 {pid}; fi",
        pid = std::process::id()
    );
    std::process::Command::new("bash")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .ok()
}

impl Mount {
    fn p(&self, rel: &str) -> PathBuf {
        self.mnt.join(rel.trim_start_matches('/'))
    }

    /// Zero mismatches, trees identical, and the workload really went through the engine.
    #[track_caller]
    fn finish(&self) {
        // the kernel releases handles asynchronously after close(2); then nothing may be left open in the engine
        let t0 = std::time::Instant::now();
        let open = || (self.h.stats.open_files.load(Ordering::Relaxed), self.h.stats.open_dirs.load(Ordering::Relaxed));
        while open() != (0, 0) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(open(), (0, 0), "handles leaked in the engine (open files, open dirs)");
        self.h.assert_no_mismatches();
        self.h.assert_trees_equal();
        let ops: u64 = OpKind::ALL.iter().map(|&k| self.h.stats.op(k).count.load(Ordering::Relaxed)).sum();
        assert!(ops > 0, "no operation reached the engine: was the mount used?");
    }

    fn ops(&self, k: OpKind) -> u64 {
        self.h.stats.op(k).count.load(Ordering::Relaxed)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // never leave operations blocked in a freeze: unmounting would hang
        self.h.policy.set_mode(MismatchMode::Log);
        if let Some(s) = self.session.take()
            && s.umount_and_join().is_err() {
                let _ = std::process::Command::new("fusermount3").arg("-uz").arg(&self.mnt).status();
            }
        if let Some(mut w) = self.watchdog.take() {
            drop(w.stdin.take()); // EOF: the watchdog exits quietly
            let _ = w.wait();
        }
    }
}

fn is_fuse(p: &Path) -> bool {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid C string and out pointer.
    unsafe { libc::statfs(c.as_ptr(), &mut s) == 0 && s.f_type as i64 == FUSE_SUPER_MAGIC }
}

fn mount(level: CheckLevel, mode: MismatchMode) -> Option<Mount> {
    mount_with(Harness::builder().level(level).mode(mode))
}

fn mount_with(b: HarnessBuilder) -> Option<Mount> {
    mount_seeded(b, |_, _| {})
}

/// Like `mount_with`, with the trees pre-populated by `seed(primary_dir, secondary_dir)` (e.g. a diverged secondary).
fn mount_seeded(b: HarnessBuilder, seed: impl FnOnce(&Path, &Path)) -> Option<Mount> {
    if !Path::new("/dev/fuse").exists() {
        eprintln!("SKIP: /dev/fuse not available");
        return None;
    }
    if let Ok(f) = std::env::var("XCHECKFS_TEST_LOG") {
        // (the engine's log, e.g. XCHECKFS_TEST_LOG=warn cargo test --test fuse_mount -- --nocapture)
        let _ = tracing_subscriber::fmt().with_env_filter(f).with_test_writer().try_init();
    }
    let h = Arc::new(b.build_with(seed));
    let mnt_dir = tmp_in(&fast_base());
    let mnt = mnt_dir.path().to_path_buf();
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("xcheckfs-test".into()), MountOption::Subtype("xcheckfs".into())];
    cfg.n_threads = Some(8);
    match fuser::Session::new(XcheckFs { engine: h.engine.clone() }, &mnt, &cfg).and_then(|s| s.spawn()) {
        Ok(session) => {
            let mut m = Mount { session: Some(session), h, mnt, _mnt_dir: mnt_dir, watchdog: None };
            assert!(is_fuse(&m.mnt), "{} is not a FUSE mount", m.mnt.display());
            m.watchdog = spawn_watchdog(&m.mnt);
            Some(m)
        }
        Err(e) => {
            eprintln!("SKIP: cannot mount FUSE here: {e}");
            None
        }
    }
}

macro_rules! mnt {
    ($level:expr, $mode:expr) => {
        match mount($level, $mode) {
            Some(m) => m,
            None => return,
        }
    };
}

fn errno_of<T>(r: std::io::Result<T>) -> i32 {
    match r {
        Ok(_) => 0,
        Err(e) => e.raw_os_error().unwrap_or(-1),
    }
}

fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    pattern(seed, n)
}

const LEVELS: [CheckLevel; 3] = [CheckLevel::Basic, CheckLevel::Thorough, CheckLevel::Paranoid];

// ------------------------------------------------------------------------------------------- basic file ops

#[test]
fn mount_is_a_fuse_mount_and_unmounts_cleanly() {
    let Some(m) = mount(CheckLevel::Basic, MismatchMode::Log) else { return };
    let mnt = m.mnt.clone();
    fs::write(m.p("x"), b"x").unwrap();
    m.finish();
    drop(m);
    assert!(!is_fuse(&mnt), "still mounted after drop");
}

#[test]
fn files_create_write_read_append_truncate_rename() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        let big = data(1, 3_000_000); // several 1 MiB write requests
        fs::write(m.p("big"), &big).unwrap();
        assert_eq!(fs::read(m.p("big")).unwrap(), big);

        // append, O_APPEND semantics with several writers in a row
        let mut f = OpenOptions::new().append(true).open(m.p("big")).unwrap();
        f.write_all(b"tail1").unwrap();
        f.write_all(b"tail2").unwrap();
        drop(f);
        assert_eq!(fs::metadata(m.p("big")).unwrap().len(), 3_000_010);

        // pwrite / pread at odd offsets, in place and past EOF
        let f = OpenOptions::new().read(true).write(true).open(m.p("big")).unwrap();
        f.write_all_at(b"PATCH", 1_234_567).unwrap();
        let mut b = [0u8; 5];
        f.read_exact_at(&mut b, 1_234_567).unwrap();
        assert_eq!(&b, b"PATCH");
        f.write_all_at(b"beyond", 3_500_000).unwrap();
        f.set_len(100_000).unwrap(); // ftruncate through the fd
        assert_eq!(f.metadata().unwrap().len(), 100_000);
        drop(f);
        fs::OpenOptions::new().write(true).open(m.p("big")).unwrap().set_len(5).unwrap();
        assert_eq!(fs::read(m.p("big")).unwrap(), &big[..5]);

        // O_TRUNC, O_EXCL, truncate(2) by path
        fs::write(m.p("big"), b"fresh").unwrap();
        assert_eq!(errno_of(OpenOptions::new().write(true).create_new(true).open(m.p("big"))), libc::EEXIST);
        fs::OpenOptions::new().write(true).open(m.p("big")).unwrap().set_len(0).unwrap();
        assert_eq!(fs::read(m.p("big")).unwrap(), b"");
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::truncate(cpath(&m.p("big")).as_ptr(), 12345) }, 0);
        assert_eq!(fs::metadata(m.p("big")).unwrap().len(), 12345);

        // rename: plain, over an existing file, within and across directories
        fs::write(m.p("a"), b"A").unwrap();
        fs::write(m.p("b"), b"B").unwrap();
        fs::rename(m.p("a"), m.p("b")).unwrap();
        assert_eq!(fs::read(m.p("b")).unwrap(), b"A");
        assert!(!m.p("a").exists());
        fs::create_dir(m.p("dir")).unwrap();
        fs::rename(m.p("b"), m.p("dir/moved")).unwrap();
        assert_eq!(fs::read(m.p("dir/moved")).unwrap(), b"A");
        m.finish();
        assert!(m.ops(OpKind::Write) > 3 && m.ops(OpKind::Rename) == 2);
    }
}

#[test]
fn errno_agreement_for_failing_operations() {
    let m = mnt!(CheckLevel::Thorough, MismatchMode::Log);
    fs::create_dir(m.p("d")).unwrap();
    fs::write(m.p("d/f"), b"x").unwrap();
    assert_eq!(errno_of(fs::create_dir(m.p("d"))), libc::EEXIST);
    assert_eq!(errno_of(fs::remove_dir(m.p("d"))), libc::ENOTEMPTY);
    assert_eq!(errno_of(fs::remove_file(m.p("d"))), libc::EISDIR);
    assert_eq!(errno_of(fs::remove_dir(m.p("d/f"))), libc::ENOTDIR);
    assert_eq!(errno_of(fs::read(m.p("nope"))), libc::ENOENT);
    assert_eq!(errno_of(fs::read(m.p("d/f/x"))), libc::ENOTDIR);
    assert_eq!(errno_of(fs::rename(m.p("nope"), m.p("n2"))), libc::ENOENT);
    assert_eq!(errno_of(fs::hard_link(m.p("d"), m.p("dl"))), libc::EPERM);
    assert_eq!(errno_of(fs::rename(m.p("d"), m.p("d/inner"))), libc::EINVAL);
    assert_eq!(errno_of(File::create(m.p("d")).map(|_| ())), libc::EISDIR);
    assert_eq!(errno_of(fs::create_dir(m.p("x/y"))), libc::ENOENT);
    let long = "n".repeat(300);
    assert_eq!(errno_of(File::create(m.p(&long))), libc::ENAMETOOLONG);
    let ok255 = "n".repeat(255);
    File::create(m.p(&ok255)).unwrap();
    assert_eq!(errno_of(fs::read_link(m.p("d/f"))), libc::EINVAL);
    m.finish();
}

#[test]
fn names_with_unusual_bytes() {
    let m = mnt!(CheckLevel::Thorough, MismatchMode::Log);
    for n in ["with space", "tab\there", "new\nline", "ünï©ødé", "-leading-dash", ".hidden", "a\\b", "quote\"s", "😀"] {
        fs::write(m.p(n), n.as_bytes()).unwrap();
        assert_eq!(fs::read(m.p(n)).unwrap(), n.as_bytes());
    }
    let mut names: Vec<_> = fs::read_dir(&m.mnt).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names.len(), 9);
    m.finish();
}

#[test]
fn attributes_permissions_and_times() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        fs::write(m.p("f"), b"x").unwrap();
        for mode in [0o600, 0o755, 0o4755, 0o2755, 0o1777, 0o644] {
            fs::set_permissions(m.p("f"), fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(fs::metadata(m.p("f")).unwrap().mode() & 0o7777, mode, "mode {mode:o}");
        }
        // utimensat with explicit times and UTIME_NOW / UTIME_OMIT
        let ts = [libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 123 }, libc::timespec { tv_sec: 1_100_000_000, tv_nsec: 456 }];
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, cpath(&m.p("f")).as_ptr(), ts.as_ptr(), 0) }, 0);
        let md = fs::metadata(m.p("f")).unwrap();
        assert_eq!((md.mtime(), md.atime()), (1_100_000_000, 1_000_000_000));
        let now = [libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT }, libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW }];
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, cpath(&m.p("f")).as_ptr(), now.as_ptr(), 0) }, 0);
        assert!(fs::metadata(m.p("f")).unwrap().mtime() > 1_500_000_000);
        // chown to ourselves is allowed for everybody
        let md = fs::metadata(m.p("f")).unwrap();
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::chown(cpath(&m.p("f")).as_ptr(), md.uid(), md.gid()) }, 0);
        // ownership of new objects is the caller's
        // SAFETY: trivial.
        assert_eq!(md.uid(), unsafe { libc::geteuid() });
        // access(2) and statfs
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::access(cpath(&m.p("f")).as_ptr(), libc::R_OK | libc::W_OK) }, 0);
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::access(cpath(&m.p("f")).as_ptr(), libc::X_OK) }, -1);
        let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::statvfs(cpath(&m.mnt).as_ptr(), &mut sv) }, 0);
        assert!(sv.f_bsize > 0);
        m.finish();
    }
}

#[test]
fn links_symlinks_and_special_files() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        fs::write(m.p("orig"), b"content").unwrap();
        fs::hard_link(m.p("orig"), m.p("hl1")).unwrap();
        fs::create_dir(m.p("d")).unwrap();
        fs::hard_link(m.p("hl1"), m.p("d/hl2")).unwrap();
        let (a, b, c) = (
            fs::metadata(m.p("orig")).unwrap(),
            fs::metadata(m.p("hl1")).unwrap(),
            fs::metadata(m.p("d/hl2")).unwrap(),
        );
        assert!(a.ino() == b.ino() && b.ino() == c.ino() && a.nlink() == 3);
        fs::write(m.p("d/hl2"), b"changed through a link").unwrap();
        assert_eq!(fs::read(m.p("orig")).unwrap(), b"changed through a link");
        fs::remove_file(m.p("orig")).unwrap();
        assert_eq!(fs::metadata(m.p("hl1")).unwrap().nlink(), 2);
        fs::rename(m.p("hl1"), m.p("d/hl3")).unwrap();
        assert_eq!(fs::metadata(m.p("d/hl3")).unwrap().ino(), c.ino());

        std::os::unix::fs::symlink("d/hl3", m.p("sl")).unwrap();
        std::os::unix::fs::symlink("/no/such/thing", m.p("dangling")).unwrap();
        assert_eq!(fs::read_link(m.p("sl")).unwrap(), Path::new("d/hl3"));
        assert_eq!(fs::read(m.p("sl")).unwrap(), b"changed through a link");
        assert_eq!(fs::read_link(m.p("dangling")).unwrap(), Path::new("/no/such/thing"));
        assert!(fs::symlink_metadata(m.p("sl")).unwrap().file_type().is_symlink());
        assert_eq!(errno_of(fs::read(m.p("dangling"))), libc::ENOENT);

        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(cpath(&m.p("fifo")).as_ptr(), 0o644) }, 0);
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&fs::metadata(m.p("fifo")).unwrap().file_type()));
        fs::remove_file(m.p("fifo")).unwrap();
        m.finish();
    }
}

#[test]
fn directories_and_large_listings() {
    for level in [CheckLevel::Basic, CheckLevel::Paranoid] {
        let m = mnt!(level, MismatchMode::Log);
        fs::create_dir_all(m.p("a/b/c/d")).unwrap();
        assert_eq!(errno_of(fs::remove_dir(m.p("a"))), libc::ENOTEMPTY);
        // enough long names that the kernel needs many readdir requests (offset handling)
        fs::create_dir(m.p("many")).unwrap();
        for i in 0..2500 {
            File::create(m.p(&format!("many/entry_with_a_fairly_long_name_to_fill_buffers_{i:05}"))).unwrap();
        }
        let n = fs::read_dir(m.p("many")).unwrap().count();
        assert_eq!(n, 2500);
        // delete while listing: entries removed after the listing started may or may not be returned, never twice
        let mut seen = std::collections::HashSet::new();
        for (i, e) in fs::read_dir(m.p("many")).unwrap().enumerate() {
            let e = e.unwrap();
            assert!(seen.insert(e.file_name()), "duplicate entry");
            if i % 3 == 0 {
                fs::remove_file(e.path()).unwrap();
            }
        }
        // the directory's own link count follows its subdirectories
        assert_eq!(fs::metadata(m.p("a")).unwrap().nlink(), 3);
        fs::remove_dir(m.p("a/b/c/d")).unwrap();
        fs::remove_dir(m.p("a/b/c")).unwrap();
        assert_eq!(fs::metadata(m.p("a/b")).unwrap().nlink(), 2);
        m.finish();
    }
}

/// Open-but-unlinked files, and files renamed while open: the engine addresses objects by descriptor, never by path.
#[test]
fn open_files_survive_unlink_and_rename() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(m.p("victim")).unwrap();
        f.write_all(b"before").unwrap();
        fs::remove_file(m.p("victim")).unwrap();
        f.write_all(b" and after").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "before and after");
        assert_eq!(f.metadata().unwrap().nlink(), 0);
        f.set_len(3).unwrap();
        drop(f);
        assert!(!m.p("victim").exists());

        fs::create_dir(m.p("d1")).unwrap();
        let mut g = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(m.p("d1/f")).unwrap();
        g.write_all(b"one").unwrap();
        fs::rename(m.p("d1"), m.p("d2")).unwrap();
        g.write_all(b"two").unwrap();
        drop(g);
        assert_eq!(fs::read(m.p("d2/f")).unwrap(), b"onetwo");
        m.finish();
    }
}

#[test]
fn rename_flags_symlink_times_and_o_direct() {
    use std::os::unix::fs::OpenOptionsExt;
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        fs::write(m.p("a"), b"A").unwrap();
        fs::write(m.p("b"), b"B").unwrap();
        fs::create_dir(m.p("d1")).unwrap();
        fs::create_dir(m.p("d2")).unwrap();
        fs::write(m.p("d2/inner"), b"I").unwrap();
        let renameat2 = |from: &str, to: &str, flags: u32| -> i32 {
            let (f, t) = (cpath(&m.p(from)), cpath(&m.p(to)));
            // SAFETY: valid C strings.
            let r = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), flags) };
            if r == 0 { 0 } else { std::io::Error::last_os_error().raw_os_error().unwrap() }
        };
        assert_eq!(renameat2("a", "b", libc::RENAME_EXCHANGE), 0);
        assert_eq!((fs::read(m.p("a")).unwrap(), fs::read(m.p("b")).unwrap()), (b"B".to_vec(), b"A".to_vec()));
        assert_eq!(renameat2("d1", "d2", libc::RENAME_EXCHANGE), 0, "directories exchange too");
        assert!(m.p("d1/inner").exists() && !m.p("d2/inner").exists());
        assert_eq!(renameat2("a", "b", libc::RENAME_NOREPLACE), libc::EEXIST);
        assert_eq!(renameat2("a", "fresh", libc::RENAME_NOREPLACE), 0);

        // times and ownership of a symlink itself
        std::os::unix::fs::symlink("fresh", m.p("sl")).unwrap();
        let ts = [libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 }, libc::timespec { tv_sec: 1_100_000_000, tv_nsec: 0 }];
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::utimensat(libc::AT_FDCWD, cpath(&m.p("sl")).as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) }, 0);
        assert_eq!(fs::symlink_metadata(m.p("sl")).unwrap().mtime(), 1_100_000_000);
        let md = fs::symlink_metadata(m.p("sl")).unwrap();
        // SAFETY: valid args.
        assert_eq!(unsafe { libc::lchown(cpath(&m.p("sl")).as_ptr(), md.uid(), md.gid()) }, 0);

        // O_DIRECT is not passed to the backends (alignment); the data still arrives intact
        let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(true).custom_flags(libc::O_DIRECT).open(m.p("direct")).unwrap();
        let mut buf = vec![0u8; 8192 + 4096];
        let off = buf.as_ptr().align_offset(4096);
        let aligned = &mut buf[off..off + 8192];
        aligned.copy_from_slice(&data(12, 8192));
        f.write_all(aligned).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut back = vec![0u8; 8192 + 4096];
        let off = back.as_ptr().align_offset(4096);
        f.read_exact(&mut back[off..off + 8192]).unwrap();
        assert_eq!(&back[off..off + 8192], &data(12, 8192)[..]);
        drop(f);
        m.finish();
    }
}

// ----------------------------------------------------------------------------------------------- xattrs

#[test]
fn xattrs_through_the_mount() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        if !xattrs_supported(m.h.p_root()) || !xattrs_supported(m.h.s_root()) {
            eprintln!("SKIP: no user xattrs on the backing file systems");
            return;
        }
        fs::write(m.p("f"), b"x").unwrap();
        fs::create_dir(m.p("d")).unwrap();
        std::os::unix::fs::symlink("f", m.p("sl")).unwrap();
        for p in ["f", "d"] {
            raw_setxattr(&m.p(p), "user.a", b"alpha").unwrap();
            raw_setxattr(&m.p(p), "user.big", &data(2, 4000)).unwrap();
            assert_eq!(raw_getxattr(&m.p(p), "user.a").unwrap(), b"alpha");
            assert_eq!(raw_getxattr(&m.p(p), "user.big").unwrap(), data(2, 4000));
            assert_eq!(raw_listxattr(&m.p(p)).unwrap(), vec!["user.a", "user.big"]);
            raw_setxattr(&m.p(p), "user.a", b"replaced").unwrap();
            assert_eq!(raw_getxattr(&m.p(p), "user.a").unwrap(), b"replaced");
            assert_eq!(raw_getxattr(&m.p(p), "user.none").unwrap_err().raw_os_error(), Some(libc::ENODATA));
            // flags
            let (c, n) = (cpath(&m.p(p)), CString::new("user.a").unwrap());
            // SAFETY: valid C strings and buffer.
            let r = unsafe { libc::lsetxattr(c.as_ptr(), n.as_ptr(), b"v".as_ptr() as *const _, 1, libc::XATTR_CREATE) };
            assert_eq!((r, errno_of::<()>(Err(std::io::Error::last_os_error()))), (-1, libc::EEXIST));
            let n2 = CString::new("user.missing").unwrap();
            // SAFETY: valid C strings and buffer.
            let r = unsafe { libc::lsetxattr(c.as_ptr(), n2.as_ptr(), b"v".as_ptr() as *const _, 1, libc::XATTR_REPLACE) };
            assert_eq!((r, errno_of::<()>(Err(std::io::Error::last_os_error()))), (-1, libc::ENODATA));
            // size probe
            // SAFETY: valid C strings, null buffer with size 0 is the probe form.
            let sz = unsafe { libc::lgetxattr(c.as_ptr(), n.as_ptr(), std::ptr::null_mut(), 0) };
            assert_eq!(sz, 8);
            // SAFETY: valid C strings.
            assert_eq!(unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) }, 0);
            // SAFETY: valid C strings.
            assert_eq!(unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) }, -1);
        }
        // xattrs survive rename and are shared by hard links
        fs::hard_link(m.p("f"), m.p("hl")).unwrap();
        assert_eq!(raw_getxattr(&m.p("hl"), "user.big").unwrap(), data(2, 4000));
        fs::rename(m.p("f"), m.p("f2")).unwrap();
        assert_eq!(raw_listxattr(&m.p("f2")).unwrap(), vec!["user.big"]);
        m.finish();
    }
}

// ------------------------------------------------------------------ fallocate / copy_file_range / mmap / sparse

#[test]
fn fallocate_copy_file_range_and_sparse_files() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        let src = data(3, 200_000);
        fs::write(m.p("src"), &src).unwrap();
        let f = OpenOptions::new().read(true).write(true).open(m.p("src")).unwrap();
        let fd = f.as_raw_fd();
        // SAFETY: valid fd.
        unsafe {
            assert_eq!(libc::fallocate(fd, 0, 0, 300_000), 0);
            assert_eq!(f.metadata().unwrap().len(), 300_000);
            assert_eq!(libc::fallocate(fd, libc::FALLOC_FL_KEEP_SIZE, 0, 400_000), 0);
            assert_eq!(f.metadata().unwrap().len(), 300_000);
            assert_eq!(libc::fallocate(fd, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 4096, 8192), 0);
            // ZERO_RANGE is not supported everywhere: only the agreement of both sides matters
            let _ = libc::fallocate(fd, libc::FALLOC_FL_ZERO_RANGE, 50_000, 10_000);
            assert_eq!(libc::posix_fallocate(fd, 0, 310_000), 0);
        }
        let got = fs::read(m.p("src")).unwrap();
        assert_eq!(got.len(), 310_000);
        assert!(got[4096..4096 + 8192].iter().all(|&b| b == 0), "hole punched");
        assert_eq!(&got[..4096], &src[..4096]);

        // copy_file_range between two files, overlapping ranges within one file, short copy at EOF
        let dst = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(m.p("dst")).unwrap();
        let (mut off_in, mut off_out) = (1000i64, 10i64);
        // SAFETY: valid fds and offset pointers.
        let n = unsafe { libc::copy_file_range(fd, &mut off_in, dst.as_raw_fd(), &mut off_out, 100_000, 0) };
        assert_eq!(n, 100_000);
        let (mut off_in, mut off_out) = (300_000i64, 0i64);
        // SAFETY: valid fds and offset pointers.
        let n = unsafe { libc::copy_file_range(fd, &mut off_in, dst.as_raw_fd(), &mut off_out, 100_000, 0) };
        assert_eq!(n, 10_000, "short copy at EOF");
        let (mut off_in, mut off_out) = (0i64, 150_000i64);
        // SAFETY: valid fds and offset pointers.
        let n = unsafe { libc::copy_file_range(fd, &mut off_in, fd, &mut off_out, 50_000, 0) };
        assert_eq!(n, 50_000);
        drop((f, dst));

        // sparse file: a hole of 8 MiB, SEEK_DATA/SEEK_HOLE agree with the backing file system's answer
        let sp = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(m.p("sparse")).unwrap();
        sp.write_all_at(b"data-at-start", 0).unwrap();
        sp.write_all_at(b"data-at-end", 8 << 20).unwrap();
        assert_eq!(sp.metadata().unwrap().len(), (8 << 20) + 11);
        let backing = File::open(m.h.p_path("sparse")).unwrap();
        for (off, whence) in [(0i64, libc::SEEK_DATA), (0, libc::SEEK_HOLE), (100, libc::SEEK_DATA), (5 << 20, libc::SEEK_DATA), (5 << 20, libc::SEEK_HOLE)] {
            // SAFETY: valid fds.
            let (a, b) = unsafe { (libc::lseek(sp.as_raw_fd(), off, whence), libc::lseek(backing.as_raw_fd(), off, whence)) };
            assert_eq!(a, b, "lseek({off}, {whence})");
        }
        let mut hole = vec![1u8; 4096];
        sp.read_exact_at(&mut hole, 4 << 20).unwrap();
        assert!(hole.iter().all(|&b| b == 0));
        sp.set_len(16 << 20).unwrap(); // extend with a hole
        sp.set_len(1 << 20).unwrap();
        drop(sp);
        m.finish();
    }
}

/// The mappings are touched by forked children, never by this process: a page fault on a FUSE-backed mapping waits for
/// the file system to answer while holding the faulting process's address-space lock, and an in-process file system
/// (our server threads, which allocate memory) needs that very lock: a deadlock. A child has its own address space.
/// (A real `xcheckfs mount` runs in its own process and is not affected.) Children only call async-signal-safe libc
/// functions on buffers prepared before the fork.
#[test]
fn mmap_write_and_msync() {
    for level in LEVELS {
        let m = mnt!(level, MismatchMode::Log);
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(m.p("mapped")).unwrap();
        let len = 1usize << 20;
        f.set_len(len as u64).unwrap();
        let want = data(4, len);
        let patch = *b"AFTER-SYNC";
        let fd = f.as_raw_fd();
        // SAFETY: the child only maps, copies, syncs and exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: async-signal-safe calls only; accesses stay inside the mapping.
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
                if p == libc::MAP_FAILED {
                    libc::_exit(10);
                }
                std::ptr::copy_nonoverlapping(want.as_ptr(), p as *mut u8, len);
                if libc::msync(p, len, libc::MS_SYNC) != 0 {
                    libc::_exit(11);
                }
                // modify a few bytes again, then sync again
                std::ptr::copy_nonoverlapping(patch.as_ptr(), (p as *mut u8).add(8192), 10);
                if libc::msync(p, len, libc::MS_SYNC) != 0 {
                    libc::_exit(12);
                }
                libc::_exit(if libc::munmap(p, len) == 0 { 0 } else { 13 });
            }
        }
        assert!(pid > 0);
        assert_eq!(wait_child(pid, Duration::from_secs(60)), Some(0), "mmap writer child failed");
        drop(f);
        let got = fs::read(m.p("mapped")).unwrap();
        assert_eq!(&got[8192..8202], b"AFTER-SYNC");
        assert_eq!(&got[..8192], &want[..8192]);
        assert_eq!(&got[9000..], &want[9000..]);

        // a read-only private mapping of the existing file, compared in a child as well
        let f = File::open(m.p("mapped")).unwrap();
        let fd = f.as_raw_fd();
        // SAFETY: as above.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: async-signal-safe calls only.
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, fd, 0);
                if p == libc::MAP_FAILED {
                    libc::_exit(20);
                }
                let same = libc::memcmp((p as *const u8).add(9000) as *const _, want.as_ptr().add(9000) as *const _, 4096) == 0;
                libc::_exit(if same { 0 } else { 21 });
            }
        }
        assert!(pid > 0);
        assert_eq!(wait_child(pid, Duration::from_secs(60)), Some(0), "mmap reader child failed");
        drop(f);
        m.finish();
    }
}

// --------------------------------------------------------------------------------------------------- locks

fn flock_of(typ: i32, start: i64, len: i64) -> libc::flock {
    // SAFETY: flock is plain data.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = typ as i16;
    fl.l_whence = libc::SEEK_SET as i16;
    fl.l_start = start;
    fl.l_len = len;
    fl
}

fn fcntl_lock(fd: i32, cmd: i32, fl: &mut libc::flock) -> i32 {
    // SAFETY: valid fd and flock.
    if unsafe { libc::fcntl(fd, cmd, fl as *mut libc::flock) } == 0 { 0 } else { std::io::Error::last_os_error().raw_os_error().unwrap() }
}

#[test]
fn ofd_locks_between_two_descriptors() {
    let m = mnt!(CheckLevel::Thorough, MismatchMode::Log);
    fs::write(m.p("lockfile"), vec![0u8; 4096]).unwrap();
    let f1 = OpenOptions::new().read(true).write(true).open(m.p("lockfile")).unwrap();
    let f2 = OpenOptions::new().read(true).write(true).open(m.p("lockfile")).unwrap();
    let (a, b) = (f1.as_raw_fd(), f2.as_raw_fd());
    assert_eq!(fcntl_lock(a, libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 0, 100)), 0);
    // conflicting request from the other description
    let e = fcntl_lock(b, libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 50, 100));
    assert!(e == libc::EAGAIN || e == libc::EACCES, "errno {e}");
    assert_eq!(fcntl_lock(b, libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 100, 50)), 0, "adjacent range");
    // F_OFD_GETLK describes the blocker
    let mut q = flock_of(libc::F_WRLCK, 0, 10);
    assert_eq!(fcntl_lock(b, libc::F_OFD_GETLK, &mut q), 0);
    assert_eq!((q.l_type as i32, q.l_start, q.l_len), (libc::F_WRLCK, 0, 100));
    let mut q = flock_of(libc::F_WRLCK, 1000, 10);
    assert_eq!(fcntl_lock(b, libc::F_OFD_GETLK, &mut q), 0);
    assert_eq!(q.l_type as i32, libc::F_UNLCK);

    // a blocking request is queued by the engine and granted when the holder unlocks
    let waiter = {
        let fd = b;
        std::thread::scope(|s| {
            let t = s.spawn(move || fcntl_lock(fd, libc::F_OFD_SETLKW, &mut flock_of(libc::F_WRLCK, 0, 10)));
            let t0 = std::time::Instant::now();
            while m.h.stats.lock_waiters.load(Ordering::Relaxed) == 0 {
                assert!(t0.elapsed() < Duration::from_secs(10), "request never queued");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!t.is_finished());
            assert_eq!(fcntl_lock(a, libc::F_OFD_SETLK, &mut flock_of(libc::F_UNLCK, 0, 100)), 0);
            t.join().unwrap()
        })
    };
    assert_eq!(waiter, 0);
    assert_eq!(m.h.stats.lock_waiters.load(Ordering::Relaxed), 0);
    // b (the waiter, now the holder) unlocks explicitly; a can lock everything then
    assert_eq!(fcntl_lock(b, libc::F_OFD_SETLK, &mut flock_of(libc::F_UNLCK, 0, 0)), 0);
    assert_eq!(fcntl_lock(a, libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 0, 200)), 0);
    drop(f2);
    drop(f1);
    m.finish();
    assert!(m.ops(OpKind::Setlk) >= 6);
}

/// The kernel sends no unlock for OFD locks when their description is closed (only RELEASE): the engine must
/// release the owners bound to the handle (a native file system drops the lock during close; through FUSE it
/// happens right after, when the asynchronous RELEASE arrives).
#[test]
fn ofd_lock_is_released_when_the_description_is_closed() {
    let m = mnt!(CheckLevel::Basic, MismatchMode::Log);
    fs::write(m.p("lockfile"), vec![0u8; 4096]).unwrap();
    let f1 = OpenOptions::new().read(true).write(true).open(m.p("lockfile")).unwrap();
    let f2 = OpenOptions::new().read(true).write(true).open(m.p("lockfile")).unwrap();
    assert_eq!(fcntl_lock(f2.as_raw_fd(), libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 0, 100)), 0);
    drop(f2);
    // FUSE sends RELEASE asynchronously after close(2) returns: the lock goes
    // away shortly after the close, not during it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if fcntl_lock(f1.as_raw_fd(), libc::F_OFD_SETLK, &mut flock_of(libc::F_WRLCK, 0, 100)) == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "OFD lock not released after close (releases seen: {}, setlk: {}, flush: {})",
            m.ops(OpKind::Release),
            m.ops(OpKind::Setlk),
            m.ops(OpKind::Flush)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(f1);
    m.finish();
}

/// Classic POSIX (per-process) record locks need two processes: fork, with only async-signal-safe calls in the child.
#[test]
fn posix_locks_between_two_processes() {
    let m = mnt!(CheckLevel::Basic, MismatchMode::Log);
    fs::write(m.p("lockfile"), vec![0u8; 4096]).unwrap();
    let f = OpenOptions::new().read(true).write(true).open(m.p("lockfile")).unwrap();
    let fd = f.as_raw_fd();
    assert_eq!(fcntl_lock(fd, libc::F_SETLK, &mut flock_of(libc::F_WRLCK, 0, 100)), 0);

    // child 1: conflicting non-blocking request fails, F_GETLK sees the parent's lock
    // SAFETY: the child only calls fcntl and _exit.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: async-signal-safe calls only.
        unsafe {
            let mut fl = flock_of(libc::F_WRLCK, 50, 100);
            let r = libc::fcntl(fd, libc::F_SETLK, &mut fl as *mut libc::flock);
            let e = *libc::__errno_location();
            if r != -1 || (e != libc::EAGAIN && e != libc::EACCES) {
                libc::_exit(10);
            }
            let mut fl = flock_of(libc::F_WRLCK, 0, 10);
            if libc::fcntl(fd, libc::F_GETLK, &mut fl as *mut libc::flock) != 0 {
                libc::_exit(11);
            }
            if fl.l_type as i32 != libc::F_WRLCK || fl.l_start != 0 || fl.l_len != 100 {
                libc::_exit(12);
            }
            let mut fl = flock_of(libc::F_WRLCK, 200, 10);
            if libc::fcntl(fd, libc::F_SETLK, &mut fl as *mut libc::flock) != 0 {
                libc::_exit(13); // a free range is granted
            }
            libc::_exit(0);
        }
    }
    assert!(pid > 0);
    assert_eq!(wait_child(pid, Duration::from_secs(20)), Some(0), "child 1 failed");

    // child 2: blocks in F_SETLKW until the parent unlocks
    // SAFETY: as above.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: async-signal-safe calls only.
        unsafe {
            let mut fl = flock_of(libc::F_WRLCK, 0, 10);
            libc::_exit(if libc::fcntl(fd, libc::F_SETLKW, &mut fl as *mut libc::flock) == 0 { 0 } else { 20 });
        }
    }
    let t0 = std::time::Instant::now();
    while m.h.stats.lock_waiters.load(Ordering::Relaxed) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(10), "blocking request never queued");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(wait_child(pid, Duration::from_millis(200)), None, "the child must still be blocked");
    assert_eq!(fcntl_lock(fd, libc::F_SETLK, &mut flock_of(libc::F_UNLCK, 0, 100)), 0);
    assert_eq!(wait_child(pid, Duration::from_secs(20)), Some(0), "child 2 failed");
    drop(f);
    m.finish();
}

fn wait_child(pid: i32, timeout: Duration) -> Option<i32> {
    let t0 = std::time::Instant::now();
    loop {
        let mut st = 0;
        // SAFETY: valid pid and out pointer.
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid {
            return Some(if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) });
        }
        if t0.elapsed() > timeout {
            if timeout > Duration::from_secs(5) {
                // SAFETY: valid pid.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---------------------------------------------------------------------------------------------- stress

/// Random operations through the kernel from many threads over a small shared namespace.
fn fs_random(m: &Mount, seed: u64, ops: usize, xattrs: bool) {
    let mut rng = Rng::new(seed);
    let dirs = ["", "a", "b", "a/x", "b/y"];
    let names = ["f0", "f1", "f2", "f3", "g0", "g1", "dd", "ee"];
    let pick = |rng: &mut Rng| {
        let d = *rng.pick(&dirs);
        let n = *rng.pick(&names);
        m.p(&if d.is_empty() { n.to_string() } else { format!("{d}/{n}") })
    };
    for _ in 0..ops {
        let p = pick(&mut rng);
        match rng.below(17) {
            0 | 1 => {
                let _ = fs::write(&p, rng.blob(70_000));
            }
            2 => {
                if let Ok(f) = OpenOptions::new().read(true).write(true).open(&p) {
                    let _ = f.write_all_at(&rng.blob(9000), rng.below(20_000));
                    let mut b = vec![0u8; 5000];
                    let _ = f.read_at(&mut b, rng.below(10_000));
                }
            }
            3 => {
                let _ = fs::read(&p);
            }
            4 => {
                let _ = fs::symlink_metadata(&p);
            }
            5 => {
                let _ = fs::rename(&p, pick(&mut rng));
            }
            6 => {
                let _ = fs::remove_file(&p);
            }
            7 => {
                let _ = fs::create_dir(&p);
            }
            8 => {
                let _ = fs::remove_dir(&p);
            }
            9 => {
                let _ = fs::hard_link(&p, pick(&mut rng));
            }
            10 => {
                let _ = std::os::unix::fs::symlink(pick(&mut rng), &p);
            }
            11 => {
                let _ = fs::read_link(&p);
            }
            12 => {
                if let Ok(f) = OpenOptions::new().write(true).open(&p) {
                    let _ = f.set_len(rng.below(30_000));
                }
            }
            13 => {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(*rng.pick(&[0o644, 0o600, 0o755, 0o444])));
            }
            14 => {
                let dir = *rng.pick(&dirs);
                let d = m.p(dir);
                if let Ok(rd) = fs::read_dir(d) {
                    for e in rd.flatten() {
                        let _ = e.metadata();
                    }
                }
            }
            15 if xattrs => {
                let n = format!("user.k{}", rng.below(3));
                match rng.below(3) {
                    0 => {
                        let _ = raw_setxattr(&p, &n, &rng.blob(100));
                    }
                    1 => {
                        let _ = raw_getxattr(&p, &n);
                        let _ = raw_listxattr(&p);
                    }
                    _ => {
                        let c = (cpath(&p), CString::new(n).unwrap());
                        // SAFETY: valid C strings.
                        unsafe { libc::lremovexattr(c.0.as_ptr(), c.1.as_ptr()) };
                    }
                }
            }
            _ => {
                if let (Ok(a), Ok(b)) = (File::open(&p), OpenOptions::new().write(true).open(pick(&mut rng))) {
                    let (mut oi, mut oo) = (rng.below(5000) as i64, rng.below(5000) as i64);
                    // SAFETY: valid fds and offsets.
                    unsafe { libc::copy_file_range(a.as_raw_fd(), &mut oi, b.as_raw_fd(), &mut oo, 20_000, 0) };
                }
            }
        }
    }
}

fn stress(level: CheckLevel, threads: usize, ops: usize) {
    let ops = ops * soak();
    let Some(m) = mount(level, MismatchMode::Log) else { return };
    let m = Arc::new(m);
    let xattrs = xattrs_supported(m.h.p_root()) && xattrs_supported(m.h.s_root());
    for d in ["a", "b", "a/x", "b/y"] {
        fs::create_dir(m.p(d)).unwrap();
    }
    let ts: Vec<_> = (0..threads)
        .map(|t| {
            let m = m.clone();
            std::thread::spawn(move || fs_random(&m, 5000 + t as u64, ops, xattrs))
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    let total: u64 = OpKind::ALL.iter().map(|&k| m.ops(k)).sum();
    let errors: u64 = OpKind::ALL.iter().map(|&k| m.h.stats.op(k).errors.load(Ordering::Relaxed)).sum();
    eprintln!("fuse stress {level:?}: {total} engine ops, {errors} error results");
    assert!(total > (threads * ops) as u64);
    m.finish();
}

#[test]
fn parallel_stress_basic() {
    stress(CheckLevel::Basic, 8, 1500);
}

#[test]
fn parallel_stress_thorough() {
    stress(CheckLevel::Thorough, 8, 1000);
}

#[test]
fn parallel_stress_paranoid() {
    stress(CheckLevel::Paranoid, 6, 600);
}

/// Readers, writers, renamers and unlinkers hammering a handful of files in one directory through the kernel.
#[test]
fn parallel_hot_directory() {
    let Some(m) = mount(CheckLevel::Thorough, MismatchMode::Log) else { return };
    let m = Arc::new(m);
    fs::create_dir(m.p("hot")).unwrap();
    for i in 0..4 {
        fs::write(m.p(&format!("hot/f{i}")), data(i, 20_000)).unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut ts = Vec::new();
    for role in 0..9u64 {
        let (m, stop) = (m.clone(), stop.clone());
        ts.push(std::thread::spawn(move || {
            let mut rng = Rng::new(role + 1);
            while !stop.load(Ordering::Relaxed) {
                let a = m.p(&format!("hot/f{}", rng.below(4)));
                let b = m.p(&format!("hot/f{}", rng.below(4)));
                match role % 3 {
                    0 => {
                        if let Ok(f) = OpenOptions::new().write(true).open(&a) {
                            let _ = f.write_all_at(&rng.blob(4000), rng.below(30_000));
                        }
                    }
                    1 => match rng.below(3) {
                        0 => drop(fs::read(&a)),
                        1 => drop(fs::metadata(&a)),
                        _ => drop(fs::read_dir(m.p("hot")).map(|r| r.count())),
                    },
                    _ => match rng.below(4) {
                        0 => drop(fs::rename(&a, &b)),
                        1 => drop(fs::remove_file(&a)),
                        2 => drop(fs::write(&a, rng.blob(5000))),
                        _ => drop(fs::hard_link(&a, &b)),
                    },
                }
            }
        }));
    }
    std::thread::sleep(Duration::from_millis(2000 * soak() as u64));
    stop.store(true, Ordering::Relaxed);
    for t in ts {
        t.join().unwrap();
    }
    m.finish();
}

// ----------------------------------------------------------------------------------- faults via the mount

fn direct_io_mount(level: CheckLevel, mode: MismatchMode) -> Option<Mount> {
    // direct_io keeps reads away from the kernel page cache so every read reaches the engine
    mount_with(Harness::builder().level(level).mode(mode).config(|c| c.direct_io = true))
}

#[test]
fn corruption_surfaces_through_the_mount_log_mode() {
    let Some(m) = direct_io_mount(CheckLevel::Basic, MismatchMode::Log) else { return };
    let good = data(8, 50_000);
    fs::write(m.p("victim"), &good).unwrap();
    fs::write(m.p("healthy"), &good).unwrap();
    m.h.mark();
    m.h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 17 }).path("/victim"));
    // the application still gets the primary's data ...
    assert_eq!(fs::read(m.p("victim")).unwrap(), good);
    assert_eq!(fs::read(m.p("healthy")).unwrap(), good);
    // ... and the corruption is on record, with the path
    let ms = m.h.new_mismatches();
    assert_eq!(ms.len(), 1, "{}", m.h.describe_mismatches());
    assert_eq!((ms[0].kind, ms[0].op), (K::Data, OpKind::Read));
    assert!(ms[0].path.ends_with("/victim"), "{}", ms[0].summary());
    assert_eq!(m.h.stats.mismatches.load(Ordering::Relaxed), 1);
}

#[test]
fn corruption_surfaces_through_the_mount_fail_mode() {
    let Some(m) = direct_io_mount(CheckLevel::Thorough, MismatchMode::Fail) else { return };
    let good = data(9, 20_000);
    fs::write(m.p("victim"), &good).unwrap();
    m.h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 100 }).path("/victim"));
    assert_eq!(errno_of(fs::read(m.p("victim"))), libc::EIO, "the read fails with EIO");
    m.h.mark();
    // a silently dropped write is caught by thorough read-back and fails the write(2)
    m.h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/dropper"));
    let mut f = File::create(m.p("dropper")).unwrap();
    assert_eq!(errno_of(f.write_all(b"hello")), libc::EIO);
    drop(f);
    // the primary did apply it
    assert_eq!(fs::read(m.h.p_path("dropper")).unwrap(), b"hello");
    // everything not involved keeps working
    fs::write(m.p("other"), b"fine").unwrap();
    assert_eq!(fs::read(m.p("other")).unwrap(), b"fine");
}

#[test]
fn namespace_faults_surface_through_the_mount() {
    let Some(m) = mount(CheckLevel::Thorough, MismatchMode::Log) else { return };
    fs::write(m.p("a"), b"a").unwrap();
    m.h.mark();
    m.h.inject(Fault::new(FaultOp::Unlink, Effect::Skip));
    fs::remove_file(m.p("a")).unwrap(); // succeeds for the application (primary's result) ...
    assert!(!m.p("a").exists());
    m.h.expect_mismatch(K::Verify, Some("removed")); // ... but the unfaithful secondary is on record
    m.h.clear_faults();
    m.h.mark();
    m.h.inject(Fault::new(FaultOp::Readdir, Effect::DropEntry(b"vanishing".to_vec())));
    fs::write(m.p("vanishing"), b"x").unwrap();
    assert_eq!(fs::read_dir(&m.mnt).unwrap().count(), 1);
    m.h.expect_mismatch(K::Readdir, None);
}

#[test]
fn freeze_through_the_mount_blocks_until_resolved() {
    let Some(m) = direct_io_mount(CheckLevel::Basic, MismatchMode::Freeze) else { return };
    let m = Arc::new(m);
    let good = data(10, 10_000);
    fs::write(m.p("victim"), &good).unwrap();
    m.h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/victim"));
    let reader = {
        let m = m.clone();
        std::thread::spawn(move || fs::read(m.p("victim")))
    };
    let id = m.h.wait_pending(Duration::from_secs(10));
    // every other application is frozen too
    let other = {
        let m = m.clone();
        std::thread::spawn(move || fs::write(m.p("other"), b"x"))
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(!reader.is_finished() && !other.is_finished());
    // (never touch the mount from this thread now: it is frozen; look at the primary tree instead)
    assert!(!m.h.p_path("other").exists());
    assert!(m.h.policy.resolve(id, Action::Continue));
    assert_eq!(reader.join().unwrap().unwrap(), good);
    other.join().unwrap().unwrap();
}

#[test]
fn detach_through_the_mount_keeps_the_application_running() {
    let Some(m) = direct_io_mount(CheckLevel::Basic, MismatchMode::Detach) else { return };
    fs::write(m.p("victim"), data(11, 5000)).unwrap();
    m.h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/victim"));
    assert_eq!(fs::read(m.p("victim")).unwrap(), data(11, 5000));
    assert!(m.h.stats.detached.load(Ordering::Relaxed));
    m.h.fault.reset_calls();
    fs::create_dir(m.p("d")).unwrap();
    fs::write(m.p("d/new"), b"after detach").unwrap();
    assert_eq!(fs::read(m.p("d/new")).unwrap(), b"after detach");
    fs::rename(m.p("d/new"), m.p("d/new2")).unwrap();
    assert_eq!(m.h.fault.total_calls(), 0);
    assert!(m.h.p_path("d/new2").exists() && !m.h.s_path("d/new2").exists());
}

use xcheckfs::backend::fault::{Effect, Fault, FaultOp};

// ------------------------------------------------------------------------------------------------ resync

/// Reads everything below `dir` through the mount: listing, attributes, symlink targets, file content.
fn read_everything(dir: &Path) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let md = fs::symlink_metadata(e.path()).unwrap();
        if md.is_dir() {
            read_everything(&e.path());
        } else if md.file_type().is_symlink() {
            fs::read_link(e.path()).unwrap();
        } else if md.is_file() {
            fs::read(e.path()).unwrap();
        }
    }
}

#[test]
fn resync_repairs_a_diverged_secondary_through_the_mount() {
    let content = data(120, 300_000);
    let Some(m) = mount_seeded(
        // direct_io: every read reaches the engine (no page cache), so the second round below is seen too
        Harness::builder().mode(MismatchMode::Resync).config(|c| {
            c.direct_io = true;
            c.dir_nlink = false;
        }),
        |p, s| {
            for r in [p, s] {
                fs::create_dir_all(r.join("dir/sub")).unwrap();
                fs::write(r.join("dir/file"), data(120, 300_000)).unwrap();
                fs::write(r.join("dir/sized"), data(121, 5000)).unwrap();
                fs::write(r.join("dir/a"), b"linked").unwrap();
                std::os::unix::fs::symlink("good-target", r.join("dir/link")).unwrap();
            }
            fs::hard_link(p.join("dir/a"), p.join("dir/b")).unwrap();
            fs::write(s.join("dir/b"), b"linked").unwrap(); // a copy where the primary has a link
            // damage on the secondary
            let mut bad = data(120, 300_000);
            bad[4] ^= 0xff;
            bad[299_999] ^= 0xff;
            fs::write(s.join("dir/file"), bad).unwrap();
            fs::write(s.join("dir/sized"), data(121, 4000)).unwrap();
            fs::remove_file(s.join("dir/link")).unwrap();
            std::os::unix::fs::symlink("bad-target", s.join("dir/link")).unwrap();
            fs::set_permissions(s.join("dir/sub"), fs::Permissions::from_mode(0o700)).unwrap();
            // missing on the secondary: a subtree; extra on the secondary: files and a directory
            fs::create_dir_all(p.join("dir/only_primary/deeper")).unwrap();
            fs::write(p.join("dir/only_primary/deeper/f"), b"copy me").unwrap();
            fs::write(s.join("dir/extra_file"), b"x").unwrap();
            fs::create_dir_all(s.join("dir/extra_dir/x")).unwrap();
        },
    ) else {
        return;
    };
    let quiet_before = m.h.stats.resyncs.load(Ordering::Relaxed);
    // the application just reads: it always gets the primary's data, and the file systems converge
    assert_eq!(fs::read(m.p("dir/file")).unwrap(), content);
    assert_eq!(fs::read_link(m.p("dir/link")).unwrap(), Path::new("good-target"));
    assert_eq!(fs::metadata(m.p("dir/sized")).unwrap().len(), 5000);
    assert_eq!(fs::read(m.p("dir/only_primary/deeper/f")).unwrap(), b"copy me");
    let names: Vec<_> = {
        let mut v: Vec<_> = fs::read_dir(m.p("dir")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    };
    assert_eq!(names, ["a", "b", "file", "link", "only_primary", "sized", "sub"]);
    read_everything(&m.mnt);
    m.h.assert_trees_equal();
    let st = &m.h.stats;
    assert!(st.mismatches.load(Ordering::Relaxed) >= 5, "the divergence was reported");
    assert!(st.resyncs.load(Ordering::Relaxed) > quiet_before + 4, "and repaired");
    assert_eq!(st.resync_failures.load(Ordering::Relaxed), 0);
    assert_eq!(fs::read(m.h.s_path("dir/file")).unwrap(), content);
    assert_eq!(fs::metadata(m.h.s_path("dir/a")).unwrap().ino(), fs::metadata(m.h.s_path("dir/b")).unwrap().ino());

    // comparisons resume: damage the secondary again behind the mount's back; the next read notices and repairs
    let (mism, resyncs) = (st.mismatches.load(Ordering::Relaxed), st.resyncs.load(Ordering::Relaxed));
    let mut bad = data(120, 300_000);
    bad[1000] ^= 0xff;
    let f = OpenOptions::new().write(true).open(m.h.s_path("dir/file")).unwrap();
    f.write_all_at(&bad[1000..1001], 1000).unwrap();
    drop(f);
    assert_eq!(fs::read(m.p("dir/file")).unwrap(), content);
    assert!(st.repeats.load(Ordering::Relaxed) >= 1, "the same problem again is counted as a repeat");
    assert_eq!(st.mismatches.load(Ordering::Relaxed), mism);
    assert!(st.resyncs.load(Ordering::Relaxed) > resyncs);
    m.h.assert_trees_equal();

    // ... and without damage nothing is reported or repaired any more; normal work is mirrored
    let (rep, res) = (st.repeats.load(Ordering::Relaxed), st.resyncs.load(Ordering::Relaxed));
    read_everything(&m.mnt);
    fs::write(m.p("dir/new"), b"new file").unwrap();
    fs::rename(m.p("dir/new"), m.p("dir/sub/moved")).unwrap();
    assert_eq!(fs::read(m.p("dir/sub/moved")).unwrap(), b"new file");
    assert_eq!((st.repeats.load(Ordering::Relaxed), st.resyncs.load(Ordering::Relaxed)), (rep, res));
    assert_eq!(st.mismatches.load(Ordering::Relaxed), mism);
    m.h.assert_trees_equal();
}

// ------------------------------------------------------------------------------------- misc end-to-end

/// The classic "build a tree, copy it, compare" workload: `cp -a`-like recursive copy inside the mount.
#[test]
fn recursive_copy_and_delete_of_a_tree() {
    let m = mnt!(CheckLevel::Thorough, MismatchMode::Log);
    let mut rng = Rng::new(31337);
    fs::create_dir_all(m.p("src/sub1/deep")).unwrap();
    fs::create_dir_all(m.p("src/sub2")).unwrap();
    for i in 0..60 {
        let d = *rng.pick(&["src", "src/sub1", "src/sub1/deep", "src/sub2"]);
        fs::write(m.p(&format!("{d}/file{i}")), rng.blob(200_000)).unwrap();
    }
    std::os::unix::fs::symlink("../file0", m.p("src/sub2/link")).unwrap();
    fn copy_tree(from: &Path, to: &Path) {
        fs::create_dir(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let t = to.join(e.file_name());
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                copy_tree(&e.path(), &t);
            } else if ft.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(e.path()).unwrap(), &t).unwrap();
            } else {
                fs::copy(e.path(), &t).unwrap();
            }
        }
    }
    copy_tree(&m.p("src"), &m.p("dst"));
    let d = tree_diff(&m.p("src"), &m.p("dst"), &TreeOpts { mtime: None, owner: true, dir_nlink: true, ..Default::default() });
    assert!(d.is_empty(), "{d:?}");
    fs::remove_dir_all(m.p("src")).unwrap();
    assert!(!m.p("src").exists());
    m.finish();
}

/// The same random workload with two *different* file systems behind the mount (tmpfs vs the disk-backed temp dir).
#[test]
fn parallel_stress_cross_filesystem() {
    let shm = PathBuf::from("/dev/shm");
    let cands = [std::env::temp_dir(), PathBuf::from("/var/tmp")];
    let Some(other) = cands.iter().find(|c| c.is_dir() && dev_of(c) != dev_of(&shm) && dev_of(&shm) != 0) else {
        eprintln!("SKIP: no second file system next to /dev/shm");
        return;
    };
    for level in [CheckLevel::Basic, CheckLevel::Thorough] {
        let b = Harness::builder().level(level).bases(&shm, other).config(|c| c.dir_nlink = false);
        let Some(m) = mount_with(b) else { return };
        let m = Arc::new(m);
        let xattrs = xattrs_supported(m.h.p_root()) && xattrs_supported(m.h.s_root());
        for d in ["a", "b", "a/x", "b/y"] {
            fs::create_dir(m.p(d)).unwrap();
        }
        let ts: Vec<_> = (0..6)
            .map(|t| {
                let m = m.clone();
                std::thread::spawn(move || fs_random(&m, 7000 + t as u64, 800 * soak(), xattrs))
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        m.h.assert_no_mismatches();
        let d = tree_diff(m.h.p_root(), m.h.s_root(), &TreeOpts { dir_nlink: false, ..Default::default() });
        assert!(d.is_empty(), "trees differ ({level:?}): {d:?}");
    }
}

/// Self-test of the watchdog: hangs on purpose (stat on a frozen mount from the only thread that could resolve the
/// freeze). Run manually: `XCHECKFS_WATCHDOG_SECS=3 cargo test --test fuse_mount watchdog -- --ignored`; the process
/// must be killed after about 3 seconds and the mount must disappear.
#[test]
#[ignore = "hangs on purpose to prove that the watchdog works"]
fn watchdog_self_test() {
    let Some(m) = direct_io_mount(CheckLevel::Basic, MismatchMode::Freeze) else { return };
    fs::write(m.p("victim"), data(13, 1000)).unwrap();
    m.h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/victim"));
    let _ = fs::read(m.p("victim")); // freezes here, nobody can resolve it
    unreachable!("the watchdog should have killed this process");
}
