//! End-to-end smoke tests of the real `xcheckfs` binary: argument parsing and validation, the background mount
//! lifecycle (pid file, log file, signals, exit codes), mismatch repair and the freeze/resolve workflow through the
//! control socket, `ctl` and `verify`. Everything runs as an unprivileged user through the CLI only; the file
//! system is never mounted in-process (see `fuse_mount.rs` for that).
//!
//! The tests that need a mount are skipped (with a message) when FUSE is not usable here (no `/dev/fuse`, no
//! `fusermount3`, or a probe mount fails). Set `XCHECKFS_CLI_REQUIRE_FUSE=1` to turn the skip into a failure (CI).
//!
//! Every test owns a temp dir with its own trees, mount point, control socket, pid and log file, so the tests run
//! in parallel. The `Env` guard (and the `Daemon`/`Fg` guards) kill whatever was started and `fusermount3 -u -z`
//! the mount points when the test ends, also when it fails; a `bash` watchdog does the same, and kills the test
//! process, if a test hangs.
//!
//! This process makes itself a child subreaper, so that the daemon started by `mount -b` (whose parent exits as
//! soon as the mount is up) is re-parented to the test, which can then `waitpid` it and see its real exit status.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_xcheckfs");

/// Longest a test may live before the watchdog aborts it.
const WATCHDOG_SECS: u64 = 120;

/// Flags that make every read, write and stat reach xcheckfs (no kernel caching in front of it).
const NOCACHE: [&str; 5] = ["--direct-io", "--attr-timeout", "0", "--entry-timeout", "0"];

// ------------------------------------------------------------------------------------------------ plumbing

fn s(p: &Path) -> String {
    p.to_str().expect("utf-8 path").to_string()
}

fn is_root() -> bool {
    // SAFETY: trivial syscall.
    unsafe { libc::geteuid() == 0 }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8).collect()
}

/// Polls `f` every 10 ms until it returns `Some`, for at most `secs` seconds.
fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_cond(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    wait_for(secs, || f().then_some(())).is_some()
}

/// Is a FUSE file system mounted exactly at `p`? (Reads /proc, never touches the mount itself, so it is safe while
/// the file system is frozen.)
fn mounted(p: &Path) -> bool {
    let want = s(p);
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else { return false };
    text.lines().any(|l| {
        let f: Vec<&str> = l.split(' ').collect();
        f.get(4) == Some(&want.as_str()) && l.split(" - ").nth(1).is_some_and(|r| r.starts_with("fuse"))
    })
}

/// Has the process installed handlers for SIGHUP, SIGINT, SIGUSR1 and SIGTERM (`SigCgt` in /proc)? xcheckfs
/// reports "ready" (background) and mounts (foreground) slightly before it does, and a signal sent in that window
/// kills it with the default action, leaving a dead mount behind. Tests that signal right after the start wait for
/// this first.
fn wait_for_signal_handlers(pid: i32) {
    const WANT: u64 = (1 << (libc::SIGHUP - 1)) | (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGUSR1 - 1)) | (1 << (libc::SIGTERM - 1));
    let caught = || {
        let t = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let hex = t.lines().find_map(|l| l.strip_prefix("SigCgt:"))?.trim();
        u64::from_str_radix(hex, 16).ok()
    };
    assert!(wait_cond(10, || caught().is_some_and(|c| c & WANT == WANT)), "xcheckfs {pid} never installed its signal handlers");
}

/// `fusermount3 -u` from outside. The kernel releases closed files asynchronously, so right after the last close the
/// plain unmount can still fail with EBUSY for a moment: retry.
#[track_caller]
fn fusermount_u(mp: &Path) {
    let ok = wait_cond(10, || {
        Command::new("fusermount3").arg("-u").arg(mp).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    });
    assert!(ok, "fusermount3 -u {} kept failing", mp.display());
}

/// ERROR lines of a log, except the one fuser prints when the kernel no longer wants the answer to a request (which
/// is what a lazy unmount, or a request the kernel gave up on, looks like).
fn error_lines(log: &str) -> Vec<&str> {
    log.lines().filter(|l| l.contains(" ERROR ") && !l.contains("Failed to send FUSE reply")).collect()
}

fn pid_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn kill(pid: i32, sig: i32) {
    // SAFETY: plain signal to a process of ours.
    unsafe { libc::kill(pid, sig) };
}

/// Makes this process the re-parenting target for orphaned daemons (once).
fn become_subreaper() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain prctl.
        let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        assert_eq!(rc, 0, "prctl(PR_SET_CHILD_SUBREAPER): {}", std::io::Error::last_os_error());
    });
}

fn decode_status(st: i32) -> i32 {
    if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { -libc::WTERMSIG(st) }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn both(&self) -> String {
        format!("exit {}\n--- stdout ---\n{}\n--- stderr ---\n{}", self.code, self.stdout, self.stderr)
    }
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or(Value::Null)
    }
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// One test's world: `p` and `s` (the trees), `m` and `m2` (mount points), and `x` for everything xcheckfs must not
/// see inside the trees: control socket, pid file, log file, quarantine, XDG directories.
struct Env {
    root: tempfile::TempDir,
    p: PathBuf,
    s: PathBuf,
    m: PathBuf,
    m2: PathBuf,
    x: PathBuf,
    sock: PathBuf,
    pid: PathBuf,
    log: PathBuf,
    watchdog: Option<Child>,
}

impl Env {
    fn new() -> Env {
        become_subreaper();
        let shm = Path::new("/dev/shm");
        let base = if shm.is_dir() && tempfile::tempdir_in(shm).is_ok() { shm.to_path_buf() } else { std::env::temp_dir() };
        // (short: the control socket path must fit sun_path)
        let root = tempfile::Builder::new().prefix("xcli-").tempdir_in(base).expect("temp dir");
        let r = root.path().canonicalize().unwrap();
        let (p, s_, m, m2, x) = (r.join("p"), r.join("s"), r.join("m"), r.join("m2"), r.join("x"));
        for d in [&p, &s_, &m, &m2, &x, &x.join("xdg"), &x.join("cfg")] {
            fs::create_dir_all(d).unwrap();
        }
        let mut e = Env { root, sock: x.join("c.sock"), pid: x.join("pid"), log: x.join("log"), p, s: s_, m, m2, x, watchdog: None };
        e.watchdog = e.spawn_watchdog();
        e
    }

    /// A separate `bash` (a hung test thread cannot block it): on timeout it kills the daemon from the pid file,
    /// lazily unmounts the mount points and kills this process. Closing its stdin (dropping `Env`) ends it quietly.
    fn spawn_watchdog(&self) -> Option<Child> {
        let secs = std::env::var("XCHECKFS_WATCHDOG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(WATCHDOG_SECS);
        let script = format!(
            "read -t {secs} _; rc=$?; if [ $rc -gt 128 ]; then \
               echo 'WATCHDOG: xcheckfs CLI test hung for {secs}s: killing the daemon and {me}' >&2; \
               [ -f '{pid}' ] && kill -9 $(cat '{pid}') 2>/dev/null; \
               for d in '{m}' '{m2}' '{p}'; do fusermount3 -u -z \"$d\" 2>/dev/null; done; kill -9 {me}; fi",
            me = std::process::id(),
            pid = s(&self.pid),
            m = s(&self.m),
            m2 = s(&self.m2),
            p = s(&self.p),
        );
        Command::new("bash").arg("-c").arg(script).stdin(Stdio::piped()).stdout(Stdio::null()).spawn().ok()
    }

    /// `xcheckfs` with a hermetic environment: no rules file from the user's config, no RUST_LOG, sockets under `x`.
    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env_remove("RUST_LOG")
            .env("XDG_RUNTIME_DIR", self.x.join("xdg"))
            .env("XDG_CONFIG_HOME", self.x.join("cfg"))
            .stdin(Stdio::null());
        c
    }

    /// Runs to completion (killed and reported as a failure after `secs`). Output goes through files, not pipes,
    /// so a daemon that inherited the descriptors can never keep us waiting.
    fn run<S: AsRef<str>>(&self, argv: &[S], secs: u64) -> Out {
        self.run_with(self.command(), argv, secs)
    }

    fn run_with<S: AsRef<str>>(&self, mut c: Command, argv: &[S], secs: u64) -> Out {
        let mut so = tempfile::tempfile().unwrap();
        let mut se = tempfile::tempfile().unwrap();
        c.args(argv.iter().map(|a| a.as_ref())).stdout(so.try_clone().unwrap()).stderr(se.try_clone().unwrap());
        let mut ch = c.spawn().expect("spawn xcheckfs");
        let end = Instant::now() + Duration::from_secs(secs);
        let status = loop {
            if let Some(st) = ch.try_wait().unwrap() {
                break Some(st);
            }
            if Instant::now() >= end {
                let _ = ch.kill();
                let _ = ch.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let rd = |f: &mut File| {
            let mut t = String::new();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_to_string(&mut t).unwrap();
            t
        };
        let out = Out { code: status.map(|st| st.code().unwrap_or(-1)).unwrap_or(-2), stdout: rd(&mut so), stderr: rd(&mut se) };
        assert!(status.is_some(), "xcheckfs {:?} did not finish within {secs}s\n{}", argv.iter().map(|a| a.as_ref()).collect::<Vec<_>>(), out.both());
        out
    }

    /// `mount FLAGS... MOUNTPOINT PRIMARY SECONDARY`, nothing added.
    fn mount_argv(&self, flags: &[&str], mp: &Path, pr: &Path, se: &Path) -> Vec<String> {
        let mut v = vec!["mount".to_string()];
        v.extend(flags.iter().map(|f| f.to_string()));
        v.extend([s(mp), s(pr), s(se)]);
        v
    }

    /// The usual flags of a background mount: pid file, log file and an explicit control socket.
    fn bg_flags(&self) -> Vec<String> {
        ["-b", "--pid-file", &s(&self.pid), "--log-file", &s(&self.log), "--control-socket", &s(&self.sock)].map(String::from).to_vec()
    }

    /// Foreground flags: just the control socket.
    fn fg_flags(&self) -> Vec<String> {
        ["--control-socket", &s(&self.sock)].map(String::from).to_vec()
    }

    fn start_bg(&self, extra: &[&str]) -> Daemon {
        self.start_bg_at(&self.m, extra)
    }

    /// `mount -b ...` at `mp`; returns once the command returned 0, having verified that the mount is up *at that
    /// moment*, with the daemon's pid from the pid file.
    fn start_bg_at(&self, mp: &Path, extra: &[&str]) -> Daemon {
        let mut flags = self.bg_flags();
        flags.extend(extra.iter().map(|f| f.to_string()));
        let flags: Vec<&str> = flags.iter().map(|f| f.as_str()).collect();
        let o = self.run(&self.mount_argv(&flags, mp, &self.p, &self.s), 30);
        assert_eq!(o.code, 0, "mount -b failed\n{}\nlog:\n{}", o.both(), self.log_text());
        let pid: i32 = fs::read_to_string(&self.pid).expect("pid file").trim().parse().expect("pid in pid file");
        let d = Daemon { pid, status: None };
        assert!(mounted(mp), "mount -b returned 0 but {} is not mounted\nlog:\n{}", mp.display(), self.log_text());
        wait_for_signal_handlers(pid);
        d
    }

    /// A foreground mount as a child process; returns once the mount is up.
    fn start_fg(&self, extra: &[&str]) -> Fg {
        let mut flags = self.fg_flags();
        flags.extend(extra.iter().map(|f| f.to_string()));
        let flags: Vec<&str> = flags.iter().map(|f| f.as_str()).collect();
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let (out, err) = (self.x.join(format!("fg{n}.out")), self.x.join(format!("fg{n}.err")));
        let child = self
            .command()
            .args(self.mount_argv(&flags, &self.m, &self.p, &self.s))
            .stdout(File::create(&out).unwrap())
            .stderr(File::create(&err).unwrap())
            .spawn()
            .expect("spawn xcheckfs");
        let mut fg = Fg { child, err, out, exited: None };
        let m = self.m.clone();
        let up = wait_cond(20, || mounted(&m) || fg.try_exit().is_some());
        assert!(up && mounted(&m), "foreground mount did not come up (exit {:?})\nstderr:\n{}", fg.exited, fg.stderr());
        wait_for_signal_handlers(fg.child.id() as i32);
        fg
    }

    fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// `ctl --socket SOCK WORDS...` -> (exit code, parsed JSON of stdout or Null).
    fn ctl(&self, words: &[&str]) -> (i32, Value) {
        let mut argv = vec!["ctl", "--socket"];
        let sock = s(&self.sock);
        argv.push(&sock);
        argv.extend(words);
        let o = self.run(&argv, 20);
        let v = o.json();
        (o.code, v)
    }

    fn status(&self) -> Value {
        let (c, v) = self.ctl(&["status"]);
        assert_eq!(c, 0, "ctl status: {v}");
        assert_eq!(v["ok"], true, "{v}");
        v
    }

    /// Polls `ctl status` until `f` accepts it.
    fn wait_status(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        let ok = wait_cond(10, || {
            last = self.status();
            f(&last)
        });
        assert!(ok, "status never showed: {what}; last status: {last}\nlog:\n{}", self.log_text());
        last
    }

    /// Waits for a frozen mismatch and returns its id.
    fn wait_pending(&self) -> u64 {
        let mut last = Value::Null;
        let id = wait_for(15, || {
            let (_, v) = self.ctl(&["pending"]);
            last = v.clone();
            v["pending"].get(0).and_then(|p| p["id"].as_u64())
        });
        id.unwrap_or_else(|| panic!("nothing became pending; last reply {last}\nlog:\n{}", self.log_text()))
    }

    /// `xcheckfs verify P S` must find the trees identical.
    #[track_caller]
    fn assert_trees_equal(&self) {
        let o = self.run(&["verify", &s(&self.p), &s(&self.s)], 30);
        assert_eq!(o.code, 0, "trees differ\n{}", o.both());
    }

    /// Writes the same file into both trees (before mounting).
    fn seed(&self, rel: &str, data: &[u8]) {
        for t in [&self.p, &self.s] {
            fs::write(t.join(rel), data).unwrap();
        }
    }

    /// Flips a byte of a secondary file behind the mount's back, keeping size and mtime, so the data is the only
    /// difference.
    fn corrupt_secondary(&self, rel: &str, at: u64) {
        let path = self.s.join(rel);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, at).unwrap();
        b[0] ^= 0xff;
        f.write_all_at(&b, at).unwrap();
        f.set_modified(mtime).unwrap();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // a daemon that outlived its guard (e.g. after a timeout): the pid file names it
        if let Ok(t) = fs::read_to_string(&self.pid)
            && let Ok(pid) = t.trim().parse::<i32>()
            && fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("xcheckfs"))
        {
            kill(pid, libc::SIGKILL);
        }
        for d in [&self.m, &self.m2, &self.p] {
            let _ = Command::new("fusermount3").arg("-u").arg("-z").arg(d).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        if let Some(mut w) = self.watchdog.take() {
            drop(w.stdin.take()); // EOF: the watchdog exits quietly
            let _ = w.wait();
        }
        let _ = &self.root;
    }
}

/// A daemon started with `mount -b`: not our child, but re-parented to us (subreaper), so it can be waited for.
struct Daemon {
    pid: i32,
    status: Option<i32>,
}

impl Daemon {
    fn signal(&self, sig: i32) {
        kill(self.pid, sig);
    }

    /// Waits for the daemon to exit; its exit code (negative: killed by that signal) or `None` on timeout.
    fn wait_exit(&mut self, secs: u64) -> Option<i32> {
        if self.status.is_none() {
            let pid = self.pid;
            self.status = wait_for(secs, || {
                let mut st = 0;
                // SAFETY: waitpid on a child of ours.
                let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
                (r == pid).then(|| decode_status(st))
            });
        }
        self.status
    }

    fn alive(&mut self) -> bool {
        self.wait_exit(0).is_none()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.status.is_none() {
            kill(self.pid, libc::SIGKILL);
            let mut st = 0;
            // SAFETY: waitpid on a child of ours (the zombie is reaped; the pid cannot have been reused).
            unsafe { libc::waitpid(self.pid, &mut st, 0) };
        }
    }
}

/// A foreground daemon (a plain child process).
struct Fg {
    child: Child,
    err: PathBuf,
    #[allow(dead_code)]
    out: PathBuf,
    exited: Option<i32>,
}

impl Fg {
    fn try_exit(&mut self) -> Option<i32> {
        if self.exited.is_none() {
            self.exited = self.child.try_wait().unwrap().map(|st| st.code().unwrap_or_else(|| -std::os::unix::process::ExitStatusExt::signal(&st).unwrap_or(0)));
        }
        self.exited
    }

    fn signal(&self, sig: i32) {
        kill(self.child.id() as i32, sig);
    }

    fn wait_exit(&mut self, secs: u64) -> Option<i32> {
        wait_for(secs, || self.try_exit())
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.err).unwrap_or_default()
    }
}

impl Drop for Fg {
    fn drop(&mut self) {
        if self.try_exit().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Is FUSE usable here? Checked once with a real probe mount through the binary.
fn fuse_env() -> Option<Env> {
    static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
    let r = PROBE.get_or_init(|| {
        let rw = OpenOptions::new().read(true).write(true).open("/dev/fuse");
        if let Err(e) = rw {
            return Err(format!("/dev/fuse is not usable: {e}"));
        }
        match Command::new("fusermount3").arg("-V").stdout(Stdio::null()).stderr(Stdio::null()).status() {
            Ok(st) if st.success() => {}
            other => return Err(format!("fusermount3 is not available ({other:?})")),
        }
        let e = Env::new();
        let flags = e.bg_flags();
        let flags: Vec<&str> = flags.iter().map(|f| f.as_str()).collect();
        let o = e.run(&e.mount_argv(&flags, &e.m, &e.p, &e.s), 30);
        if o.code != 0 {
            return Err(format!("a probe mount failed: {}", o.stderr.trim()));
        }
        let pid: i32 = fs::read_to_string(&e.pid).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        let mut d = Daemon { pid, status: None };
        let up = mounted(&e.m);
        wait_for_signal_handlers(pid);
        d.signal(libc::SIGTERM);
        d.wait_exit(10);
        if up { Ok(()) } else { Err("the probe mount did not show up in /proc/self/mountinfo".into()) }
    });
    match r {
        Ok(()) => Some(Env::new()),
        Err(why) => {
            assert!(std::env::var_os("XCHECKFS_CLI_REQUIRE_FUSE").is_none(), "FUSE required but unusable: {why}");
            eprintln!("SKIP: {why}");
            None
        }
    }
}

macro_rules! fuse_env {
    () => {
        match fuse_env() {
            Some(e) => e,
            None => return,
        }
    };
}

/// Reads FILE with `dd iflag=direct` as a separate process, for touching a mount that is going to freeze. O_DIRECT:
/// one READ request that reaches xcheckfs, no readahead (a failed readahead is silently retried by the kernel as a
/// synchronous read, which would hide an EIO the test expects). It must not be a thread of the
/// test process: a thread blocked in a FUSE request makes every later `posix_spawn` of this process hang in the
/// kernel (the vfork child never gets through `execve`), so the test could not even run `xcheckfs ctl` any more.
struct Reader {
    child: Child,
    out: PathBuf,
    err: PathBuf,
}

impl Reader {
    fn spawn(e: &Env, path: &Path) -> Reader {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let (out, err) = (e.x.join(format!("rd{n}.out")), e.x.join(format!("rd{n}.err")));
        let child = Command::new("dd")
            .arg(format!("if={}", path.display()))
            .args(["iflag=direct", "bs=16M", "status=none"])
            .stdin(Stdio::null())
            .stdout(File::create(&out).unwrap())
            .stderr(File::create(&err).unwrap())
            .spawn()
            .expect("spawn dd");
        Reader { child, out, err }
    }

    /// Still stuck in the file system after a moment?
    #[track_caller]
    fn assert_blocked(&mut self) {
        std::thread::sleep(Duration::from_millis(300));
        assert!(self.child.try_wait().unwrap().is_none(), "the operation was not blocked: {:?}", fs::read_to_string(&self.err));
    }

    /// The file's content, or the error message of `dd` (e.g. "Input/output error").
    #[track_caller]
    fn result(&mut self) -> Result<Vec<u8>, String> {
        let st = wait_for(15, || self.child.try_wait().unwrap()).expect("the blocked operation never returned");
        if st.success() { Ok(fs::read(&self.out).unwrap()) } else { Err(fs::read_to_string(&self.err).unwrap()) }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A set of ordinary file operations through the mount.
fn exercise(m: &Path) {
    fs::create_dir_all(m.join("d/sub")).unwrap();
    fs::write(m.join("d/a.txt"), b"hello").unwrap();
    let big = pattern(300_000);
    fs::write(m.join("d/sub/big"), &big).unwrap();
    OpenOptions::new().append(true).open(m.join("d/a.txt")).unwrap().write_all(b" world").unwrap();
    std::os::unix::fs::symlink("a.txt", m.join("d/ln")).unwrap();
    fs::hard_link(m.join("d/a.txt"), m.join("d/hl")).unwrap();
    fs::rename(m.join("d/sub/big"), m.join("d/big2")).unwrap();
    fs::set_permissions(m.join("d/a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    OpenOptions::new().write(true).open(m.join("d/big2")).unwrap().set_len(1000).unwrap();
    fs::create_dir(m.join("gone")).unwrap();
    fs::write(m.join("gone/f"), b"x").unwrap();
    fs::remove_file(m.join("gone/f")).unwrap();
    fs::remove_dir(m.join("gone")).unwrap();
    assert_eq!(fs::read(m.join("d/a.txt")).unwrap(), b"hello world");
    assert_eq!(fs::read(m.join("d/hl")).unwrap(), b"hello world");
    assert_eq!(fs::read_link(m.join("d/ln")).unwrap(), Path::new("a.txt"));
    assert_eq!(fs::read(m.join("d/big2")).unwrap(), &big[..1000]);
    assert_eq!(fs::metadata(m.join("d/a.txt")).unwrap().permissions().mode() & 0o7777, 0o640);
}

/// What `exercise` leaves behind must be identical in both trees.
#[track_caller]
fn assert_exercised(e: &Env) {
    for t in [&e.p, &e.s] {
        assert_eq!(fs::read(t.join("d/a.txt")).unwrap(), b"hello world", "{}", t.display());
        assert_eq!(fs::read(t.join("d/big2")).unwrap(), &pattern(300_000)[..1000]);
        assert_eq!(fs::read_link(t.join("d/ln")).unwrap(), Path::new("a.txt"));
        assert_eq!(fs::metadata(t.join("d/a.txt")).unwrap().ino(), fs::metadata(t.join("d/hl")).unwrap().ino(), "hard link");
        assert_eq!(fs::metadata(t.join("d/a.txt")).unwrap().permissions().mode() & 0o7777, 0o640);
        assert!(!t.join("gone").exists() && !t.join("d/sub/big").exists());
    }
    e.assert_trees_equal();
}

// ------------------------------------------------------------------------------------------------ 1. help / usage

#[test]
fn help_and_version() {
    let e = Env::new();
    let o = e.run(&["--version"], 10);
    assert_eq!(o.code, 0, "{}", o.both());
    assert_eq!(o.stdout.trim(), format!("xcheckfs {}", env!("CARGO_PKG_VERSION")));

    let o = e.run(&["--help"], 10);
    assert_eq!(o.code, 0, "{}", o.both());
    assert!(o.stdout.contains("Usage: xcheckfs"), "{}", o.stdout);
    for sub in ["mount", "verify", "ctl"] {
        assert!(o.stdout.lines().any(|l| l.trim_start().starts_with(sub)), "--help does not list {sub}:\n{}", o.stdout);
    }
    // `help` as a subcommand, and -h
    assert_eq!(e.run(&["help"], 10).stdout, o.stdout);
    let h = e.run(&["-h"], 10);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("Usage: xcheckfs"));
}

#[test]
fn subcommand_help() {
    let e = Env::new();
    let cases: [(&str, &[&str]); 3] = [
        ("mount", &["Usage: xcheckfs mount", "MOUNTPOINT", "PRIMARY", "SECONDARY", "--on-mismatch", "--quarantine", "--background", "--pid-file", "--control-socket", "--direct-io", "--log-file", "--ui"]),
        ("verify", &["Usage: xcheckfs verify", "--no-content", "--no-xattrs", "--no-mtime", "--max-reports", "--time-tolerance"]),
        ("ctl", &["Usage: xcheckfs ctl", "--socket", "status", "resolve", "detach"]),
    ];
    for (sub, needles) in cases {
        for form in [vec![sub, "--help"], vec!["help", sub]] {
            let o = e.run(&form, 10);
            assert_eq!(o.code, 0, "{form:?}\n{}", o.both());
            for n in needles {
                assert!(o.stdout.contains(n), "`xcheckfs {}` help lacks {n:?}:\n{}", form.join(" "), o.stdout);
            }
        }
    }
}

#[test]
fn usage_errors_exit_2() {
    let e = Env::new();
    let (m, p, sc) = (s(&e.m), s(&e.p), s(&e.s));
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![], "Usage"),
        (vec!["frobnicate"], "unrecognized subcommand"),
        (vec!["mount"], "required"),
        (vec!["mount", &m, &p], "required"),
        (vec!["mount", "--no-such-flag", &m, &p, &sc], "unexpected argument"),
        (vec!["mount", "-m", "bogus", &m, &p, &sc], "invalid value"),
        (vec!["mount", "-c", "bogus", &m, &p, &sc], "invalid value"),
        (vec!["mount", "-u", "bogus", &m, &p, &sc], "invalid value"),
        (vec!["mount", "-v", "-l", "info", &m, &p, &sc], "cannot be used with"),
        (vec!["mount", "--time-tolerance", "soon", &m, &p, &sc], "invalid duration"),
        (vec!["verify", &p], "required"),
        (vec!["verify", "--time-tolerance=-1s", &p, &sc], "negative duration"),
        (vec!["ctl"], "required"),
    ];
    for (argv, needle) in cases {
        let o = e.run(&argv, 10);
        assert_eq!(o.code, 2, "xcheckfs {argv:?}\n{}", o.both());
        assert!(o.stderr.contains(needle), "xcheckfs {argv:?}: expected {needle:?}\n{}", o.both());
        assert!(!mounted(&e.m));
    }
}

// ------------------------------------------------------------------------------------------------ 2. validation

/// The mount command must fail with exit 1 and `needle` on stderr, without mounting anything.
#[track_caller]
fn refused(e: &Env, argv: &[String], needle: &str) -> Out {
    let o = e.run(argv, 20);
    assert_eq!(o.code, 1, "expected a refusal: xcheckfs {argv:?}\n{}", o.both());
    assert!(o.stderr.starts_with("xcheckfs: "), "{}", o.both());
    assert!(o.stderr.contains(needle), "expected {needle:?} in the message of xcheckfs {argv:?}\n{}", o.both());
    assert!(o.stdout.is_empty(), "{}", o.both());
    for d in [&e.m, &e.m2, &e.p] {
        assert!(!mounted(d), "{} got mounted despite the refusal", d.display());
    }
    o
}

#[test]
fn refuses_primary_equal_to_secondary() {
    let e = Env::new();
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &e.p), "must not contain each other");
    // also through a symlink and a `..` spelling
    let link = e.x.join("plink");
    std::os::unix::fs::symlink(&e.p, &link).unwrap();
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &link), "must not contain each other");
    let dotdot = e.s.join("../p");
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &dotdot), "must not contain each other");
}

#[test]
fn refuses_secondary_inside_primary() {
    let e = Env::new();
    fs::create_dir(e.p.join("sec")).unwrap();
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &e.p.join("sec")), "must not contain each other");
}

#[test]
fn refuses_primary_inside_secondary() {
    let e = Env::new();
    fs::create_dir(e.s.join("pri")).unwrap();
    refused(&e, &e.mount_argv(&[], &e.m, &e.s.join("pri"), &e.s), "must not contain each other");
}

#[test]
fn refuses_mount_point_inside_the_secondary() {
    let e = Env::new();
    fs::create_dir(e.s.join("mnt")).unwrap();
    refused(&e, &e.mount_argv(&[], &e.s.join("mnt"), &e.p, &e.s), "mount point must not be inside the secondary");
}

#[test]
fn refuses_mount_point_inside_the_primary() {
    let e = Env::new();
    fs::create_dir(e.p.join("mnt")).unwrap();
    refused(&e, &e.mount_argv(&[], &e.p.join("mnt"), &e.p, &e.s), "mount point must not be inside the primary");
}

#[test]
fn refuses_auxiliary_files_inside_a_mirrored_tree() {
    let e = Env::new();
    let link = e.x.join("lnk");
    std::os::unix::fs::symlink(&e.p, &link).unwrap();
    let cases: Vec<(&str, PathBuf, &str)> = vec![
        ("--log-file", e.p.join("log"), "log file"),
        ("--log-file", e.m.join("log"), "log file"),
        ("--log-file", e.p.join("not/yet/there/log"), "log file"),
        ("--log-file", link.join("log"), "log file"),
        ("--pid-file", e.s.join("pid"), "pid file"),
        ("--pid-file", e.m.join("pid"), "pid file"),
        ("--control-socket", e.p.join("c.sock"), "control socket"),
        ("--control-socket", e.s.join("sub/../c.sock"), "control socket"),
        ("--quarantine", e.s.join("q"), "quarantine directory"),
        ("--quarantine", e.p.join("q"), "quarantine directory"),
    ];
    for (flag, path, what) in cases {
        let argv = e.mount_argv(&[flag, &s(&path)], &e.m, &e.p, &e.s);
        let o = refused(&e, &argv, &format!("{what} {}", path.display()));
        assert!(o.stderr.contains("inside a mirrored tree"), "{}", o.both());
        // nothing was created on the way
        assert!(!path.exists(), "{} was created", path.display());
        assert!(!e.p.join("not").exists());
    }
    // ... while the same files next to the trees are fine to *name* (the refusal above is about location only)
    let ok = e.mount_argv(&["--log-file", &s(&e.x.join("log")), "--pid-file", &s(&e.x.join("pid"))], &e.m, &e.p, &e.s.join("nonexistent"));
    refused(&e, &ok, "No such file or directory");
}

#[test]
fn refuses_default_control_socket_inside_a_tree() {
    if is_root() {
        eprintln!("SKIP: the default socket is under /run when running as root");
        return;
    }
    let e = Env::new();
    // XDG_RUNTIME_DIR points into the primary: the derived socket would be mirrored
    let mut c = e.command();
    c.env("XDG_RUNTIME_DIR", &e.p);
    let argv = e.mount_argv(&[], &e.m, &e.p, &e.s);
    let o = e.run_with(c, &argv, 20);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stderr.contains("default control socket") && o.stderr.contains("pass --control-socket"), "{}", o.both());
    assert!(!e.p.join("xcheckfs").exists());
}

#[test]
fn refuses_missing_and_non_directory_paths() {
    let e = Env::new();
    let gone = e.x.join("does-not-exist");
    refused(&e, &e.mount_argv(&[], &e.m, &gone, &e.s), &format!("primary {}: ", gone.display()));
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &gone), &format!("secondary {}: ", gone.display()));
    let o = refused(&e, &e.mount_argv(&[], &gone, &e.p, &e.s), &format!("mount point {}: ", gone.display()));
    assert!(o.stderr.contains("No such file or directory"), "{}", o.both());
    assert!(!gone.exists(), "the mount point must not be created");

    let file = e.x.join("a-file");
    fs::write(&file, b"x").unwrap();
    refused(&e, &e.mount_argv(&[], &e.m, &file, &e.s), "is not a directory");
    refused(&e, &e.mount_argv(&[], &e.m, &e.p, &file), "is not a directory");
    refused(&e, &e.mount_argv(&[], &file, &e.p, &e.s), "is not a directory");
}

#[test]
fn refuses_tui_in_the_background() {
    let e = Env::new();
    let argv = e.mount_argv(&["--ui", "tui", "--background", "--pid-file", &s(&e.pid)], &e.m, &e.p, &e.s);
    refused(&e, &argv, "--ui tui cannot be combined with --background");
    let argv = e.mount_argv(&["-u", "tui", "-b", "--control-socket", &s(&e.sock)], &e.m, &e.p, &e.s);
    refused(&e, &argv, "--ui tui cannot be combined with --background");
    assert!(!e.pid.exists() && !e.sock.exists(), "a refused mount must leave no files behind");
}

// ------------------------------------------------------------------------------------------------ 3. lifecycle

/// `mount -b`, work through the mount, then `sig`: the mount goes away, the daemon exits 0, the pid file and the
/// socket are removed and the log has the summary.
fn lifecycle(sig: i32, sig_name: &str) {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);

    // the pid file holds the live daemon
    let pid: i32 = fs::read_to_string(&e.pid).unwrap().trim().parse().unwrap();
    assert_eq!(pid, d.pid);
    assert!(pid_alive(pid) && d.alive());
    assert!(e.sock.exists(), "control socket missing");

    // ... which was detached from us: its own session, no controlling terminal
    // SAFETY: trivial syscall.
    assert_eq!(unsafe { libc::getsid(pid) }, pid, "the daemon is not a session leader");

    exercise(&e.m);
    let st = e.status();
    assert_eq!(st["state"], "running", "{st}");
    assert_eq!(st["mode"], "resync");
    assert_eq!(st["mismatches"], 0, "{st}");
    assert_eq!(st["pending"], 0);
    assert!(st["ops"].as_u64().unwrap() > 20, "{st}");
    assert_eq!(st["info"]["pid"], pid);
    assert_eq!(st["info"]["mountpoint"], s(&e.m));
    assert_eq!(st["info"]["primary"], s(&e.p));
    assert_eq!(st["info"]["secondary"], s(&e.s));
    assert_eq!(st["info"]["check"], "basic");
    // everything is in both trees already (writes are synchronous)
    assert_exercised(&e);

    d.signal(sig);
    assert!(wait_cond(10, || !mounted(&e.m)), "{sig_name}: the mount is still there\nlog:\n{}", e.log_text());
    assert_eq!(d.wait_exit(10), Some(0), "{sig_name}: daemon exit status\nlog:\n{}", e.log_text());
    assert!(!e.pid.exists(), "{sig_name}: the pid file was not removed");
    assert!(!e.sock.exists(), "{sig_name}: the control socket was not removed");
    let log = e.log_text();
    assert!(log.contains("mounted ") && log.contains("mirroring to"), "{log}");
    assert!(log.contains("unmounted: "), "{sig_name}: no summary line in the log:\n{log}");
    assert!(log.contains("0 mismatches"), "{log}");
    assert!(log.contains(&format!("signal {sig} received, unmounting")), "{log}");
    assert!(error_lines(&log).is_empty(), "{log}");
    assert_exercised(&e);
}

#[test]
fn background_lifecycle_sigterm() {
    lifecycle(libc::SIGTERM, "SIGTERM");
}

#[test]
fn background_lifecycle_sigint() {
    lifecycle(libc::SIGINT, "SIGINT");
}

#[test]
fn background_lifecycle_sighup() {
    lifecycle(libc::SIGHUP, "SIGHUP");
}

#[test]
fn external_fusermount_unmount_ends_the_daemon() {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);
    fs::write(e.m.join("f"), b"data").unwrap();
    fusermount_u(&e.m);
    assert!(!mounted(&e.m));
    assert_eq!(d.wait_exit(10), Some(0), "the daemon did not exit on its own\nlog:\n{}", e.log_text());
    assert!(!e.pid.exists() && !e.sock.exists());
    assert!(e.log_text().contains("unmounted: "), "{}", e.log_text());
    assert_eq!(fs::read(e.p.join("f")).unwrap(), b"data");
    assert_eq!(fs::read(e.s.join("f")).unwrap(), b"data");
}

/// The regression behind this file: with a busy mount the unmount used to be a silent no-op. A process that holds
/// a file open on the mount makes the plain unmount fail; the daemon must fall back to a lazy detach.
#[test]
fn sigterm_unmounts_a_busy_mount() {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);
    fs::write(e.m.join("busy"), b"busy").unwrap();
    let held = File::open(e.m.join("busy")).unwrap();
    d.signal(libc::SIGTERM);
    assert!(wait_cond(10, || !mounted(&e.m)), "a busy mount was not detached\nlog:\n{}", e.log_text());
    // the connection ends when the last reference goes away
    drop(held);
    assert_eq!(d.wait_exit(15), Some(0), "log:\n{}", e.log_text());
    assert!(!e.pid.exists());
    let log = e.log_text();
    assert!(log.contains("detaching lazily") && log.contains("unmounted: "), "{log}");
}

#[test]
fn sigusr1_logs_a_summary_and_keeps_running() {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);
    fs::write(e.m.join("f"), b"x").unwrap();
    d.signal(libc::SIGUSR1);
    let summary = |t: &str| t.lines().any(|l| l.contains(" ops in ") && !l.contains("unmounted"));
    assert!(wait_cond(10, || summary(&e.log_text())), "no summary after SIGUSR1:\n{}", e.log_text());
    assert!(d.alive() && mounted(&e.m), "SIGUSR1 must not stop the daemon");
    assert_eq!(fs::read(e.m.join("f")).unwrap(), b"x");
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0));
}

#[test]
fn background_mount_over_the_primary() {
    let e = fuse_env!();
    fs::write(e.p.join("before"), b"already there").unwrap();
    fs::write(e.s.join("before"), b"already there").unwrap();
    let mut d = e.start_bg_at(&e.p, &[]);
    assert!(e.log_text().contains("mounted over the primary"), "{}", e.log_text());
    // all access to the primary goes through xcheckfs now
    assert_eq!(fs::read(e.p.join("before")).unwrap(), b"already there");
    fs::write(e.p.join("new"), b"through the mount").unwrap();
    assert_eq!(fs::read(e.s.join("new")).unwrap(), b"through the mount", "the write was not mirrored");
    d.signal(libc::SIGTERM);
    assert!(wait_cond(10, || !mounted(&e.p)));
    assert_eq!(d.wait_exit(10), Some(0));
    assert_eq!(fs::read(e.p.join("new")).unwrap(), b"through the mount", "the primary lost the write");
    e.assert_trees_equal();
}

#[test]
fn background_failure_is_reported_by_the_parent() {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);
    // a second daemon on the same control socket must fail, saying so on the invoking terminal
    let mut flags = e.bg_flags();
    flags[2] = s(&e.x.join("pid2"));
    flags[4] = s(&e.x.join("log2"));
    let flags: Vec<&str> = flags.iter().map(|f| f.as_str()).collect();
    let o = e.run(&e.mount_argv(&flags, &e.m2, &e.p, &e.s), 30);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stderr.contains("in use by another xcheckfs"), "{}", o.both());
    assert!(!mounted(&e.m2));
    // the first one is unharmed
    assert_eq!(e.status()["state"], "running");
    assert!(d.alive() && mounted(&e.m));
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0));
}

// ------------------------------------------------------------------------------------------------ 4. mismatch

#[test]
fn mismatch_is_repaired_and_the_exit_code_is_3() {
    let e = fuse_env!();
    let good = pattern(100_000);
    e.seed("victim", &good);
    let mut d = e.start_bg(&NOCACHE);
    e.corrupt_secondary("victim", 5000);
    assert_ne!(fs::read(e.s.join("victim")).unwrap(), good);

    // the application gets the primary's data ...
    assert_eq!(fs::read(e.m.join("victim")).unwrap(), good);
    // ... the mismatch is counted and the secondary repaired
    let st = e.wait_status("a mismatch and a resync", |v| v["mismatches"].as_u64() >= Some(1) && v["resyncs"].as_u64() >= Some(1));
    assert_eq!(st["resync_failures"], 0, "{st}");
    assert!(wait_cond(5, || fs::read(e.s.join("victim")).is_ok_and(|b| b == good)), "the secondary was not repaired");
    let (c, v) = e.ctl(&["mismatches"]);
    assert_eq!(c, 0);
    let list = v["mismatches"].as_array().unwrap();
    assert!(!list.is_empty(), "{v}");
    assert!(list.iter().any(|m| m["path"] == "/victim"), "{v}");

    // reading again is quiet
    let before = e.status()["resyncs"].as_u64().unwrap();
    assert_eq!(fs::read(e.m.join("victim")).unwrap(), good);
    assert_eq!(e.status()["resyncs"].as_u64().unwrap(), before);

    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(3), "a run with mismatches must exit 3\nlog:\n{}", e.log_text());
    assert!(!mounted(&e.m) && !e.pid.exists());
    let log = e.log_text();
    assert!(log.lines().any(|l| l.contains("ERROR") && l.contains("MISMATCH #1")), "{log}");
    assert!(log.contains("secondary repaired from the primary"), "{log}");
    assert!(log.contains("unmounted: ") && !log.contains(" 0 mismatches"), "{log}");
    e.assert_trees_equal();
}

#[test]
fn mismatch_in_log_mode_is_not_repaired() {
    let e = fuse_env!();
    let good = pattern(50_000);
    e.seed("victim", &good);
    let mut d = e.start_bg(&[&NOCACHE[..], &["-m", "log"]].concat());
    e.corrupt_secondary("victim", 100);
    assert_eq!(fs::read(e.m.join("victim")).unwrap(), good);
    let st = e.wait_status("a mismatch", |v| v["mismatches"].as_u64() >= Some(1));
    assert_eq!(st["mode"], "log");
    assert_eq!(st["resyncs"], 0, "{st}");
    assert_ne!(fs::read(e.s.join("victim")).unwrap(), good, "log mode must not touch the secondary");
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(3));
}

// ------------------------------------------------------------------------------------------------ 5. quarantine

#[test]
fn quarantine_keeps_the_overwritten_secondary_version() {
    let e = fuse_env!();
    let good = pattern(60_000);
    e.seed("victim", &good);
    let q = e.x.join("quarantine");
    let mut d = e.start_bg(&[&NOCACHE[..], &["--quarantine", &s(&q)]].concat());
    assert!(q.is_dir(), "the quarantine directory is created at startup");
    e.corrupt_secondary("victim", 777);
    let bad = fs::read(e.s.join("victim")).unwrap();
    assert_ne!(bad, good);

    assert_eq!(fs::read(e.m.join("victim")).unwrap(), good);
    let st = e.wait_status("a quarantined object", |v| v["quarantined"].as_u64() >= Some(1));
    assert!(st["mismatches"].as_u64() >= Some(1) && st["resyncs"].as_u64() >= Some(1), "{st}");
    assert!(wait_cond(5, || fs::read(e.s.join("victim")).is_ok_and(|b| b == good)), "not repaired");

    let entries: Vec<PathBuf> = fs::read_dir(&q).unwrap().map(|x| x.unwrap().path()).collect();
    assert_eq!(entries.len(), 1, "expected one quarantine entry: {entries:?}");
    let dir = &entries[0];
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with("-victim"), "unexpected entry name {name}");
    assert_eq!(fs::read(dir.join("object")).unwrap(), bad, "the quarantined copy is the secondary's old version");
    let note = fs::read_to_string(dir.join("mismatch.txt")).unwrap();
    assert!(note.contains("/victim"), "{note}");

    assert!(e.log_text().contains("secondary version saved to"), "{}", e.log_text());
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(3));
    e.assert_trees_equal();
}

// ------------------------------------------------------------------------------------------------ 6. freeze

/// A freeze-mode mount with a corrupted secondary and a reader blocked on it.
struct Frozen {
    e: Env,
    d: Daemon,
    good: Vec<u8>,
    reader: Reader,
    id: u64,
}

fn frozen() -> Option<Frozen> {
    let e = fuse_env()?;
    let good = pattern(80_000);
    e.seed("victim", &good);
    let d = e.start_bg(&[&NOCACHE[..], &["-m", "freeze"]].concat());
    e.corrupt_secondary("victim", 4242);
    let mut reader = Reader::spawn(&e, &e.m.join("victim"));
    let id = e.wait_pending();
    reader.assert_blocked();
    let st = e.status();
    assert_eq!(st["state"], "frozen", "{st}");
    assert_eq!(st["mode"], "freeze");
    assert_eq!(st["pending"], 1, "{st}");
    let (_, v) = e.ctl(&["pending"]);
    assert_eq!(v["pending"][0]["path"], "/victim", "{v}");
    assert_eq!(v["pending"][0]["id"], id);
    Some(Frozen { e, d, good, reader, id })
}

macro_rules! frozen {
    () => {
        match frozen() {
            Some(f) => f,
            None => return,
        }
    };
}

#[test]
fn freeze_resolve_continue() {
    let mut f = frozen!();
    let (c, v) = f.e.ctl(&["resolve", &f.id.to_string(), "continue"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    assert_eq!(f.reader.result().unwrap(), f.good, "the blocked read continues with the primary's data");
    let st = f.e.wait_status("not frozen any more", |v| v["pending"] == 0 && v["state"] == "running");
    assert_eq!(st["resyncs"], 0, "continue does not repair: {st}");
    assert_ne!(fs::read(f.e.s.join("victim")).unwrap(), f.good, "continue leaves the secondary alone");
    // resolving twice is an error
    let (c, v) = f.e.ctl(&["resolve", &f.id.to_string(), "continue"]);
    assert_eq!((c, &v["ok"]), (1, &Value::Bool(false)), "{v}");
    // switching modes works: `mode log` stops freezing for good
    let (c, v) = f.e.ctl(&["mode", "log"]);
    assert_eq!((c, &v["mode"]), (0, &Value::from("log")), "{v}");
    assert_eq!(f.e.status()["mode"], "log");
    assert_eq!(fs::read(f.e.m.join("victim")).unwrap(), f.good);
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.d.wait_exit(10), Some(3));
}

#[test]
fn freeze_resolve_resync() {
    let mut f = frozen!();
    let (c, v) = f.e.ctl(&["resolve", &f.id.to_string(), "resync"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    assert_eq!(f.reader.result().unwrap(), f.good);
    let st = f.e.wait_status("a resync", |v| v["resyncs"].as_u64() >= Some(1) && v["pending"] == 0);
    assert_eq!(st["resync_failures"], 0, "{st}");
    assert!(wait_cond(5, || fs::read(f.e.s.join("victim")).is_ok_and(|b| b == f.good)), "the secondary was not repaired");
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.d.wait_exit(10), Some(3));
}

#[test]
fn freeze_resolve_fail_gives_eio() {
    let mut f = frozen!();
    let (c, v) = f.e.ctl(&["resolve", &f.id.to_string(), "fail"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    let err = f.reader.result().expect_err("the blocked read must fail");
    assert!(err.contains("Input/output error"), "expected EIO, got: {err}");
    f.e.wait_status("not frozen any more", |v| v["pending"] == 0);
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.d.wait_exit(10), Some(3));
}

#[test]
fn freeze_leaving_the_mode_releases_everything() {
    let mut f = frozen!();
    let (c, v) = f.e.ctl(&["mode", "log"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    assert_eq!(f.reader.result().unwrap(), f.good);
    let st = f.e.status();
    assert_eq!((st["mode"].as_str(), st["pending"].as_u64(), st["state"].as_str()), (Some("log"), Some(0), Some("running")), "{st}");
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.d.wait_exit(10), Some(3));
}

#[test]
fn freeze_then_detach_stops_mirroring() {
    let mut f = frozen!();
    let (c, v) = f.e.ctl(&["detach"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    assert_eq!(f.reader.result().unwrap(), f.good);
    let st = f.e.status();
    assert_eq!(st["state"], "detached", "{st}");
    assert_eq!(st["pending"], 0);
    // the application keeps working on the primary only
    fs::write(f.e.m.join("after"), b"primary only").unwrap();
    assert_eq!(fs::read(f.e.m.join("after")).unwrap(), b"primary only");
    assert_eq!(fs::read(f.e.p.join("after")).unwrap(), b"primary only");
    assert!(!f.e.s.join("after").exists(), "a detached secondary must not receive writes");
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.d.wait_exit(10), Some(3));
}

#[test]
fn sigterm_releases_frozen_operations() {
    let mut f = frozen!();
    f.d.signal(libc::SIGTERM);
    assert_eq!(f.reader.result().unwrap(), f.good, "the frozen read must be released by the shutdown");
    assert!(wait_cond(10, || !mounted(&f.e.m)), "log:\n{}", f.e.log_text());
    assert_eq!(f.d.wait_exit(10), Some(3), "log:\n{}", f.e.log_text());
    assert!(!f.e.pid.exists());
}

#[test]
fn ctl_detach_without_a_freeze() {
    let e = fuse_env!();
    let mut d = e.start_bg(&[]);
    fs::write(e.m.join("mirrored"), b"both").unwrap();
    assert_eq!(e.status()["state"], "running");
    let (c, v) = e.ctl(&["detach"]);
    assert_eq!((c, &v["ok"]), (0, &Value::Bool(true)), "{v}");
    assert_eq!(e.status()["state"], "detached");
    fs::write(e.m.join("lonely"), b"primary only").unwrap();
    assert!(e.p.join("lonely").exists() && !e.s.join("lonely").exists());
    assert_eq!(fs::read(e.s.join("mirrored")).unwrap(), b"both");
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0), "detaching is not a mismatch");
}

// ------------------------------------------------------------------------------------------------ 7. ctl

#[test]
fn ctl_commands_and_errors_against_a_daemon() {
    let e = fuse_env!();
    // a stale socket file from a crashed run is replaced
    drop(std::os::unix::net::UnixListener::bind(&e.sock).unwrap());
    assert!(e.sock.exists());
    let mut d = e.start_bg(&[]);
    exercise(&e.m);

    // no command means status
    let (c, v) = e.ctl(&[]);
    assert_eq!((c, &v["ok"], &v["state"]), (0, &Value::Bool(true), &Value::from("running")), "{v}");
    let (c, v) = e.ctl(&["status"]);
    assert_eq!(c, 0);
    for k in ["info", "state", "mode", "uptime_secs", "ops", "mismatches", "allowed", "repeats", "pending", "resyncs", "quarantined", "bytes_read", "bytes_written", "nodes", "open_files"] {
        assert!(v.get(k).is_some(), "status lacks {k}: {v}");
    }
    assert!(v["bytes_written"].as_u64().unwrap() >= 300_000, "{v}");

    let (c, v) = e.ctl(&["stats"]);
    assert_eq!(c, 0);
    let ops = v["ops"].as_array().unwrap();
    for want in ["write", "mkdir", "rename"] {
        assert!(ops.iter().any(|o| o["op"] == want && o["count"].as_u64() > Some(0)), "stats lack {want}: {v}");
    }
    assert_eq!(v["status"]["state"], "running");

    let (c, v) = e.ctl(&["mismatches", "5"]);
    assert_eq!((c, v["mismatches"].as_array().map(|a| a.len())), (0, Some(0)), "{v}");
    let (c, v) = e.ctl(&["pending"]);
    assert_eq!((c, v["pending"].as_array().map(|a| a.len())), (0, Some(0)), "{v}");
    let (c, v) = e.ctl(&["rules"]);
    assert_eq!(c, 0);
    assert_eq!(v["rules"].as_array().map(|a| a.len()), Some(0), "{v}");
    assert!(v["path"].as_str().is_some_and(|p| p.starts_with(&s(&e.x.join("cfg")))), "the rules path must follow XDG_CONFIG_HOME: {v}");

    // errors: ok:false on stdout, exit 1
    let bad: [(&[&str], &str); 6] = [
        (&["frobnicate"], "unknown command"),
        (&["status", "extra"], "unknown command"),
        (&["mode", "bogus"], "unknown mode"),
        (&["mode"], "unknown command"),
        (&["resolve", "99999", "continue"], "not pending"),
        (&["resolve", "1", "bogus"], "unknown action"),
    ];
    for (words, needle) in bad {
        let (c, v) = e.ctl(words);
        assert_eq!(c, 1, "{words:?}: {v}");
        assert_eq!(v["ok"], false, "{words:?}: {v}");
        assert!(v["error"].as_str().is_some_and(|m| m.contains(needle)), "{words:?}: expected {needle:?} in {v}");
    }
    let (c, v) = e.ctl(&["resolve", "abc", "continue"]);
    assert_eq!((c, v["ok"].as_bool()), (1, Some(false)), "{v}");

    // a failed command leaves the daemon alone
    assert_eq!(e.status()["state"], "running");
    // `mode` round trip, and the exit code stays 0 (no mismatch)
    let (c, v) = e.ctl(&["mode", "fail"]);
    assert_eq!((c, &v["mode"]), (0, &Value::from("fail")));
    assert_eq!(e.status()["mode"], "fail");
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0));
    assert!(!e.sock.exists());

    // after the unmount nobody answers any more
    let o = e.run(&["ctl", "--socket", &s(&e.sock), "status"], 10);
    assert_eq!(o.code, 1, "{}", o.both());
}

#[test]
fn ctl_without_a_daemon() {
    let e = Env::new();
    let missing = e.x.join("none.sock");
    let o = e.run(&["ctl", "--socket", &s(&missing), "status"], 10);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stdout.is_empty(), "{}", o.both());
    assert!(o.stderr.starts_with("xcheckfs: connect "), "{}", o.both());
    assert!(o.stderr.contains(&s(&missing)) && o.stderr.contains("No such file or directory"), "{}", o.both());

    // a socket file nobody listens on
    drop(std::os::unix::net::UnixListener::bind(&e.sock).unwrap());
    let o = e.run(&["ctl", "--socket", &s(&e.sock)], 10);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stderr.contains("connect") && o.stderr.contains("Connection refused"), "{}", o.both());

    // by mount point: the derived socket does not exist either
    let o = e.run(&["ctl", &s(&e.m), "status"], 10);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stderr.contains("connect") && o.stderr.contains("xcheckfs-") || o.stderr.contains("/xdg/xcheckfs/"), "{}", o.both());
    let o = e.run(&["ctl", &s(&e.m)], 10);
    assert_eq!(o.code, 1, "{}", o.both());
}

/// The one test that relies on the derived socket path: `$XDG_RUNTIME_DIR/xcheckfs/<name>-<hash>.sock`, found again
/// by `ctl MOUNTPOINT` from the same (lexical) spelling of the mount point.
#[test]
fn default_control_socket_derivation() {
    if is_root() {
        eprintln!("SKIP: running as root, the default socket lives in /run/xcheckfs");
        return;
    }
    let e = fuse_env!();
    let argv = ["mount", "-b", "--pid-file", &s(&e.pid), "--log-file", &s(&e.log), &s(&e.m), &s(&e.p), &s(&e.s)];
    let o = e.run(&argv, 30);
    assert_eq!(o.code, 0, "{}\nlog:\n{}", o.both(), e.log_text());
    let pid: i32 = fs::read_to_string(&e.pid).unwrap().trim().parse().unwrap();
    let mut d = Daemon { pid, status: None };
    wait_for_signal_handlers(pid);

    let dir = e.x.join("xdg/xcheckfs");
    let socks: Vec<PathBuf> = fs::read_dir(&dir).unwrap().map(|x| x.unwrap().path()).collect();
    assert_eq!(socks.len(), 1, "{socks:?}");
    let name = socks[0].file_name().unwrap().to_string_lossy().into_owned();
    let (stem, hash) = name.strip_suffix(".sock").expect(".sock").rsplit_once('-').expect("name-hash");
    assert_eq!(stem, "m");
    assert!(hash.len() == 16 && hash.chars().all(|c| c.is_ascii_hexdigit()), "{name}");
    assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&socks[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(s(&socks[0]).len() < 100, "default socket path too long for sun_path");

    for spelling in [s(&e.m), format!("{}/", s(&e.m)), s(&e.p.join("../m")), s(&e.m.join("."))] {
        let o = e.run(&["ctl", &spelling, "status"], 10);
        assert_eq!(o.code, 0, "ctl {spelling}\n{}", o.both());
        assert_eq!(o.json()["info"]["pid"], pid);
    }
    // without a command: status
    let o = e.run(&["ctl", &s(&e.m)], 10);
    assert_eq!(o.json()["state"], "running", "{}", o.both());
    // another mount point does not find it
    assert_eq!(e.run(&["ctl", &s(&e.m2), "status"], 10).code, 1);

    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0));
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0, "the socket must be removed at exit");
}

// ------------------------------------------------------------------------------------------------ 8. verify

fn populate(root: &Path) {
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    fs::write(root.join("top"), pattern(5000)).unwrap();
    fs::write(root.join("a/file2"), pattern(70_000)).unwrap();
    fs::write(root.join("a/b/file1"), b"small").unwrap();
    fs::set_permissions(root.join("a/file2"), fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink("a/file2", root.join("link")).unwrap();
    fs::hard_link(root.join("top"), root.join("hl")).unwrap();
}

fn verify_env() -> Env {
    let e = Env::new();
    populate(&e.p);
    populate(&e.s);
    e
}

fn verify(e: &Env, flags: &[&str]) -> Out {
    let mut argv = vec!["verify"];
    argv.extend(flags);
    let (p, sc) = (s(&e.p), s(&e.s));
    argv.push(&p);
    argv.push(&sc);
    e.run(&argv, 30)
}

#[test]
fn verify_identical_trees() {
    let e = verify_env();
    let o = verify(&e, &[]);
    assert_eq!(o.code, 0, "{}", o.both());
    assert!(o.stdout.trim_end().lines().count() == 1, "only the summary line is expected:\n{}", o.stdout);
    assert!(o.stdout.contains("0 differences, 0 errors"), "{}", o.stdout);
    assert!(o.stdout.contains("files") && o.stdout.contains("dirs"), "{}", o.stdout);
    // the other switches are accepted
    let o = verify(&e, &["--no-content", "--no-xattrs", "--no-mtime", "--no-dir-nlink", "--threads", "2", "--time-tolerance", "500ms", "--max-reports", "5"]);
    assert_eq!(o.code, 0, "{}", o.both());
}

#[test]
fn verify_reports_content_difference() {
    let e = verify_env();
    e.corrupt_secondary("a/file2", 30_000);
    let o = verify(&e, &[]);
    assert_eq!(o.code, 3, "{}", o.both());
    assert!(o.stdout.lines().any(|l| l.starts_with("/a/file2: content: primary=")), "{}", o.stdout);
    assert!(o.stdout.contains("1 differences, 0 errors"), "{}", o.stdout);
    // --no-content ignores it
    let o = verify(&e, &["--no-content"]);
    assert_eq!(o.code, 0, "{}", o.both());
}

#[test]
fn verify_reports_mode_difference() {
    let e = verify_env();
    fs::set_permissions(e.s.join("a/file2"), fs::Permissions::from_mode(0o600)).unwrap();
    let o = verify(&e, &[]);
    assert_eq!(o.code, 3, "{}", o.both());
    assert!(o.stdout.lines().any(|l| l.starts_with("/a/file2: attr mode: ") && l.contains("primary=") && l.contains("secondary=")), "{}", o.stdout);
    // none of the "no" switches hides a mode difference
    let o = verify(&e, &["--no-content", "--no-mtime", "--no-xattrs"]);
    assert_eq!(o.code, 3, "{}", o.both());
}

#[test]
fn verify_reports_missing_and_extra_names() {
    let e = verify_env();
    fs::remove_file(e.s.join("a/b/file1")).unwrap();
    fs::remove_dir_all(e.s.join("empty")).unwrap();
    fs::write(e.s.join("extra"), b"x").unwrap();
    fs::create_dir_all(e.s.join("extra_dir/deeper")).unwrap();
    let o = verify(&e, &[]);
    assert_eq!(o.code, 3, "{}", o.both());
    for want in ["/a/b/file1: only in primary", "/empty: only in primary", "/extra: only in secondary", "/extra_dir: only in secondary"] {
        assert!(o.stdout.lines().any(|l| l.starts_with(want)), "missing {want:?}:\n{}", o.stdout);
    }
    assert!(!o.stdout.contains("deeper"), "names present on one side only are not descended into:\n{}", o.stdout);
    assert!(o.stdout.contains("4 differences"), "{}", o.stdout);
}

#[test]
fn verify_mtime_switches() {
    let e = verify_env();
    let f = OpenOptions::new().write(true).open(e.s.join("a/file2")).unwrap();
    f.set_modified(fs::metadata(e.p.join("a/file2")).unwrap().modified().unwrap() + Duration::from_secs(3600)).unwrap();
    drop(f);
    let o = verify(&e, &[]);
    assert_eq!(o.code, 3, "{}", o.both());
    assert!(o.stdout.lines().any(|l| l.starts_with("/a/file2: attr mtime: ")), "{}", o.stdout);
    assert_eq!(verify(&e, &["--no-mtime"]).code, 0);
    assert_eq!(verify(&e, &["--time-tolerance", "7200s"]).code, 0);
    assert_eq!(verify(&e, &["--time-tolerance", "1s"]).code, 3);
}

#[test]
fn verify_reports_symlink_and_hard_link_differences() {
    let e = verify_env();
    fs::remove_file(e.s.join("link")).unwrap();
    std::os::unix::fs::symlink("elsewhere", e.s.join("link")).unwrap();
    fs::remove_file(e.s.join("hl")).unwrap();
    fs::copy(e.s.join("top"), e.s.join("hl")).unwrap();
    let o = verify(&e, &[]);
    assert_eq!(o.code, 3, "{}", o.both());
    assert!(o.stdout.lines().any(|l| l.starts_with("/link: symlink target: primary=a/file2 secondary=elsewhere")), "{}", o.stdout);
    assert!(o.stdout.contains("hardlink structure"), "{}", o.stdout);
}

#[test]
fn verify_max_reports_limits_the_listing_not_the_count() {
    let e = verify_env();
    for n in ["x1", "x2", "x3", "x4"] {
        fs::write(e.s.join(n), b"x").unwrap();
    }
    let o = verify(&e, &["--max-reports", "2"]);
    assert_eq!(o.code, 3, "{}", o.both());
    assert_eq!(o.stdout.lines().filter(|l| l.contains(": only in secondary")).count(), 2, "{}", o.stdout);
    assert!(o.stdout.contains("... 2 more differences not listed"), "{}", o.stdout);
    assert!(o.stdout.contains("4 differences"), "{}", o.stdout);
}

#[test]
fn verify_unreadable_entries_exit_2() {
    if is_root() {
        eprintln!("SKIP: root can read everything");
        return;
    }
    let e = verify_env();
    for t in [&e.p, &e.s] {
        fs::write(t.join("secret"), b"hidden").unwrap();
        fs::set_permissions(t.join("secret"), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let o = verify(&e, &[]);
    assert_eq!(o.code, 2, "{}", o.both());
    assert!(o.stderr.contains("error: ") && o.stderr.contains("secret"), "{}", o.both());
    assert!(o.stdout.contains("0 differences, 1 errors"), "{}", o.stdout);
    // without reading content there is nothing to complain about
    assert_eq!(verify(&e, &["--no-content"]).code, 0);
    // a real difference wins over the read error
    fs::write(e.s.join("extra"), b"x").unwrap();
    assert_eq!(verify(&e, &[]).code, 3);
}

#[test]
fn verify_fatal_errors_exit_1() {
    let e = Env::new();
    let gone = e.x.join("nope");
    let o = e.run(&["verify", &s(&e.p), &s(&gone)], 10);
    assert_eq!(o.code, 1, "{}", o.both());
    assert!(o.stderr.starts_with("xcheckfs: "), "{}", o.both());
    let o = e.run(&["verify", &s(&gone), &s(&e.s)], 10);
    assert_eq!(o.code, 1, "{}", o.both());
}

/// `verify` is meant to run before the first and after the last mount: trees that were worked on through a
/// mirror verify clean, and a daemon killed half way is easy to notice.
#[test]
fn verify_after_a_mirrored_session() {
    let e = fuse_env!();
    populate(&e.p);
    populate(&e.s);
    assert_eq!(verify(&e, &[]).code, 0);
    let mut d = e.start_bg(&[]);
    exercise(&e.m);
    fs::remove_file(e.m.join("top")).unwrap();
    d.signal(libc::SIGTERM);
    assert_eq!(d.wait_exit(10), Some(0));
    let o = verify(&e, &[]);
    assert_eq!(o.code, 0, "{}", o.both());
    assert!(!e.s.join("top").exists());
}

// ------------------------------------------------------------------------------------------------ 9. foreground

#[test]
fn foreground_log_ui_info_level() {
    let e = fuse_env!();
    let mut fg = e.start_fg(&["--ui", "log", "-l", "info", "--no-color"]);
    fs::write(e.m.join("f"), b"foreground").unwrap();
    assert!(fg.try_exit().is_none(), "a foreground mount keeps running");
    assert!(wait_cond(5, || fg.stderr().contains("mounted ")), "{}", fg.stderr());
    assert_eq!(e.status()["state"], "running");
    fg.signal(libc::SIGTERM);
    assert_eq!(fg.wait_exit(10), Some(0), "stderr:\n{}", fg.stderr());
    assert!(!mounted(&e.m));
    let err = fg.stderr();
    assert!(err.lines().any(|l| l.contains(" WARN ") && l.contains("mounted ") && l.contains("mirroring to")), "{err}");
    assert!(err.contains("signal 15 received, unmounting"), "{err}");
    assert!(err.lines().any(|l| l.contains(" INFO ") && l.contains("file system destroyed")), "-l info shows info lines:\n{err}");
    assert!(err.lines().any(|l| l.contains("unmounted: ") && l.contains("0 mismatches")), "{err}");
    assert!(error_lines(&err).is_empty(), "{err}");
    assert!(!err.contains('\u{1b}'), "no colors when stderr is not a terminal");
    assert_eq!(fs::read(e.s.join("f")).unwrap(), b"foreground");
    assert!(!e.sock.exists());
}

#[test]
fn foreground_default_level_hides_info_and_exits_on_sigint() {
    let e = fuse_env!();
    let mut fg = e.start_fg(&[]);
    fs::write(e.m.join("f"), b"x").unwrap();
    fg.signal(libc::SIGINT);
    assert_eq!(fg.wait_exit(10), Some(0), "stderr:\n{}", fg.stderr());
    assert!(!mounted(&e.m));
    let err = fg.stderr();
    assert!(err.contains("mounted ") && err.contains("unmounted: "), "{err}");
    assert!(!err.contains(" INFO "), "{err}");
}

#[test]
fn foreground_mismatch_is_logged_at_error_level() {
    let e = fuse_env!();
    let good = pattern(40_000);
    e.seed("victim", &good);
    let mut fg = e.start_fg(&[&NOCACHE[..], &["-l", "error", "-m", "log"]].concat());
    e.corrupt_secondary("victim", 1234);
    assert_eq!(fs::read(e.m.join("victim")).unwrap(), good);
    assert!(wait_cond(10, || fg.stderr().contains("MISMATCH #")), "no mismatch on stderr:\n{}", fg.stderr());
    let err = fg.stderr();
    let line = err.lines().find(|l| l.contains("MISMATCH #1")).unwrap_or_else(|| panic!("{err}"));
    assert!(line.contains(" ERROR "), "{line}");
    assert!(line.contains("/victim"), "the path is in the line: {line}");
    assert!(fg.try_exit().is_none());
    fg.signal(libc::SIGTERM);
    assert_eq!(fg.wait_exit(10), Some(3), "stderr:\n{}", fg.stderr());
    // -l error: nothing but errors
    let err = fg.stderr();
    assert!(!err.contains(" WARN ") && !err.contains(" INFO "), "{err}");
}

#[test]
fn foreground_verbose_flag_raises_the_level() {
    let e = fuse_env!();
    let mut fg = e.start_fg(&["-v"]);
    fg.signal(libc::SIGTERM);
    assert_eq!(fg.wait_exit(10), Some(0));
    assert!(fg.stderr().contains(" INFO "), "{}", fg.stderr());
}

#[test]
fn foreground_exits_by_itself_when_unmounted_externally() {
    let e = fuse_env!();
    let mut fg = e.start_fg(&[]);
    // (a mount that is not served yet would be a failed handshake, not an unmount)
    fs::write(e.m.join("f"), b"x").unwrap();
    fusermount_u(&e.m);
    assert_eq!(fg.wait_exit(10), Some(0), "stderr:\n{}", fg.stderr());
}

/// Regression test: the signal handlers used to be installed only after the mount existed (and, with
/// `--background`, after the parent was told "ready"), so a SIGTERM sent as soon as the mount showed up killed the
/// process with the default action and left a dead mount behind instead of unmounting.
#[test]
fn signal_right_after_the_mount_appears_still_unmounts() {
    let e = fuse_env!();
    let mut killed = 0;
    for _ in 0..40 {
        let mut fg = Fg {
            child: e
                .command()
                .args(e.mount_argv(&["--control-socket", &s(&e.sock)], &e.m, &e.p, &e.s))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
            err: e.x.join("race.err"),
            out: e.x.join("race.out"),
            exited: None,
        };
        // (spin: a poll interval of milliseconds would mostly miss the window)
        let end = Instant::now() + Duration::from_secs(20);
        while !mounted(&e.m) {
            assert!(Instant::now() < end, "the mount never appeared");
        }
        fg.signal(libc::SIGTERM);
        let code = fg.wait_exit(15);
        if code != Some(0) {
            killed += 1;
        }
        // clean up the leftover mount for the next round
        let _ = Command::new("fusermount3").arg("-u").arg("-z").arg(&e.m).stderr(Stdio::null()).status();
        assert!(wait_cond(5, || !mounted(&e.m)));
    }
    assert_eq!(killed, 0, "{killed} of 40 daemons died from the signal instead of unmounting");
}
