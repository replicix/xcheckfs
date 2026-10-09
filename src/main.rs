use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::Level;

use xcheckfs::backend::posix::PosixBackend;
use xcheckfs::config::{CheckLevel, DirectIo, EngineConfig, MismatchMode, Serialization};
use xcheckfs::control;
use xcheckfs::engine::Engine;
use xcheckfs::events::EventSink;
use xcheckfs::fusefs::XcheckFs;
use xcheckfs::policy::{Policy, RulesFile};
use xcheckfs::stats::Stats;
use xcheckfs::{daemon, logging, sys, tui};

// The release binaries are static musl builds, whose allocator scales
// poorly across the FUSE worker threads.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Mirror a trusted file system and an experimental one in lockstep and
/// report every disagreement.
#[derive(Parser)]
#[command(name = "xcheckfs", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount PRIMARY at MOUNTPOINT, mirroring every operation to SECONDARY.
    Mount(Box<MountArgs>),
    /// Compare two directory trees offline (run before the first mount).
    Verify(VerifyArgs),
    /// Talk to a running mount: status, mismatches, resolve frozen ones.
    Ctl(CtlArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Ui {
    /// Plain log lines on stderr.
    Log,
    /// Interactive dashboard.
    Tui,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn level(self) -> Level {
        match self {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Trace => Level::TRACE,
        }
    }
}

#[derive(Args)]
struct MountArgs {
    /// Where to mount (may be PRIMARY itself to route all access through xcheckfs).
    mountpoint: PathBuf,
    /// The trusted, authoritative directory. Applications always get its results.
    primary: PathBuf,
    /// The directory on the file system under test.
    secondary: PathBuf,

    /// Foreground user interface.
    #[arg(short, long, value_enum, default_value_t = Ui::Log)]
    ui: Ui,
    /// Log level (mismatches are logged at error level).
    #[arg(short = 'l', long, value_enum, default_value_t = LogLevel::Warn)]
    log_level: LogLevel,
    /// Increase verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count, conflicts_with = "log_level")]
    verbose: u8,
    /// Write logs to this file instead of stderr/syslog.
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Disable colored log output.
    #[arg(long)]
    no_color: bool,

    /// How much checking to do per operation.
    #[arg(short, long, value_enum, default_value_t = CheckLevel::Basic)]
    check: CheckLevel,
    /// What to do on a mismatch.
    #[arg(short = 'm', long, value_enum, default_value_t = MismatchMode::Resync)]
    on_mismatch: MismatchMode,
    /// Before resync overwrites or removes the secondary's version of an
    /// object, copy it into a new directory here (outside both trees).
    /// Without it, resync only reports what it repaired.
    #[arg(long)]
    quarantine: Option<PathBuf>,
    /// Bytes saved per quarantined object (larger content is truncated).
    #[arg(long, default_value_t = 64 << 20)]
    quarantine_cap: u64,
    /// Repairs of one object within ten minutes before resync gives up on it
    /// (it then stays diverged and is used on the primary only).
    #[arg(long, default_value_t = 5)]
    resync_limit: u32,
    /// Allow-rules file [default: /etc/xcheckfs/rules.toml as root, else
    /// $XDG_CONFIG_HOME/xcheckfs/rules.toml].
    #[arg(long)]
    rules: Option<PathBuf>,
    /// Operations kept in the TUI's scrollable log.
    #[arg(long, default_value_t = 10_000)]
    history: usize,

    /// Run in the background (logs go to syslog unless --log-file is given).
    #[arg(short, long)]
    background: bool,
    /// Write the daemon's pid here.
    #[arg(long)]
    pid_file: Option<PathBuf>,
    /// Control socket path [default: derived from MOUNTPOINT under
    /// /run/xcheckfs or $XDG_RUNTIME_DIR/xcheckfs].
    #[arg(long)]
    control_socket: Option<PathBuf>,

    /// FUSE worker threads [default: twice the number of CPUs, at least 16,
    /// at most 64].
    #[arg(long)]
    threads: Option<usize>,
    /// Serve FUSE over io_uring (Linux 6.14+, fuse module parameter
    /// enable_uring=Y) instead of reading /dev/fuse; falls back to /dev/fuse
    /// with a warning where the kernel does not offer it. Less CPU per
    /// operation, but a call that stalls in a backend (a journal commit)
    /// holds up the other requests of its CPU.
    #[arg(long)]
    io_uring: bool,
    /// Requests in flight per CPU queue with --io-uring. A blocking lock
    /// wait holds an entry, and at most depth - 1 of them may wait at once
    /// on one queue: past that, F_SETLKW is answered ENOLCK.
    #[arg(long, default_value_t = 64)]
    io_uring_depth: u32,
    /// Run the secondary half of each operation after the primary instead of
    /// concurrently.
    #[arg(long)]
    sequential: bool,
    /// Tolerance for timestamp comparisons, e.g. 1s, 500ms, 2s.
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    time_tolerance: Duration,
    /// Do not compare link counts of directories.
    #[arg(long)]
    no_dir_nlink: bool,
    /// Do not probe the file systems at mount time. The probe (a few
    /// operations in a scratch directory at the root of each tree, removed
    /// again) finds where they legitimately differ, which xcheckfs then
    /// adapts to instead of reporting it, and which optional operations only
    /// one of them supports.
    #[arg(long)]
    no_probe: bool,
    /// Do not switch to the caller's credentials for mutations (root only).
    #[arg(long)]
    no_creds: bool,
    /// Let the kernel handle fcntl locks locally instead of mirroring them.
    #[arg(long)]
    no_lock_mirroring: bool,
    /// Kernel attribute cache timeout in seconds (0 = every stat is checked).
    #[arg(long, default_value_t = 1.0)]
    attr_timeout: f64,
    /// Kernel dentry cache timeout in seconds.
    #[arg(long, default_value_t = 1.0)]
    entry_timeout: f64,
    /// Kernel direct-I/O path for: `auto` files opened with O_DIRECT (with
    /// parallel direct writes), `all` every file (every read and write
    /// reaches xcheckfs), `off` none. `--direct-io` alone means `all`.
    #[arg(long, value_enum, num_args = 0..=1, default_value_t = DirectIo::Auto, default_missing_value = "all")]
    direct_io: DirectIo,
    /// How strictly operations on one object are serialized: `relaxed` lets
    /// in-place reads and writes on disjoint byte ranges of a file run
    /// concurrently on both file systems (anything changing the size stays
    /// exclusive); `strict` serializes all writes per file.
    #[arg(long, value_enum, default_value_t = Serialization::Relaxed)]
    serialize: Serialization,
    /// Lock stripes (objects whose ids hash to one stripe serialize each
    /// other's exclusive operations).
    #[arg(long, default_value_t = 65536)]
    lock_stripes: usize,
    /// Allow other users to access the mount [default: on when root].
    #[arg(long, overrides_with = "no_allow_other")]
    allow_other: bool,
    #[arg(long, hide = true)]
    no_allow_other: bool,
    /// Extra mount options (comma separated), e.g. -o suid,dev.
    #[arg(short = 'o', value_delimiter = ',')]
    options: Vec<String>,
}

#[derive(Args)]
struct VerifyArgs {
    primary: PathBuf,
    secondary: PathBuf,
    /// Do not compare file contents.
    #[arg(long)]
    no_content: bool,
    /// Do not compare extended attributes.
    #[arg(long)]
    no_xattrs: bool,
    /// Do not compare modification times.
    #[arg(long)]
    no_mtime: bool,
    /// Do not compare link counts of directories.
    #[arg(long)]
    no_dir_nlink: bool,
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    time_tolerance: Duration,
    /// Stop listing differences after this many (counting continues).
    #[arg(long, default_value_t = 200)]
    max_reports: usize,
    #[arg(long)]
    threads: Option<usize>,
}

#[derive(Args)]
struct CtlArgs {
    /// Control socket path (instead of deriving it from MOUNTPOINT).
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Mount point of the running xcheckfs.
    #[arg(required_unless_present = "socket")]
    mountpoint: Option<PathBuf>,
    /// status | stats | mismatches [N] | pending | resolve ID ACTION |
    /// mode MODE | detach | rules. ACTION: continue, retry, resync, fail,
    /// detach, allow, allow-path.
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1e-3)
    } else if let Some(n) = s.strip_suffix("us") {
        (n, 1e-6)
    } else if let Some(n) = s.strip_suffix("ns") {
        (n, 1e-9)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.trim().parse().map_err(|_| format!("invalid duration {s:?}"))?;
    if v < 0.0 {
        return Err("negative duration".into());
    }
    Ok(Duration::from_secs_f64(v * mult))
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Mount(a) => cmd_mount(*a),
        Cmd::Verify(a) => cmd_verify(a),
        Cmd::Ctl(a) => cmd_ctl(a),
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("xcheckfs: {e:#}");
            std::process::exit(1);
        }
    }
}

/// `inner` is inside (or equal to) `outer`.
fn within(inner: &Path, outer: &Path) -> bool {
    inner.starts_with(outer)
}

/// Resolves a path that may not exist yet (canonical parent + file name).
fn resolve_future(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(d), Some(f)) => resolve_future(if d.as_os_str().is_empty() { Path::new(".") } else { d }).join(f),
        _ => std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()),
    }
}

fn default_rules_path() -> Option<PathBuf> {
    if sys::is_root() {
        return Some(PathBuf::from("/etc/xcheckfs/rules.toml"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("xcheckfs/rules.toml"))
}

fn mount_options(a: &MountArgs, primary: &Path) -> Vec<fuser::MountOption> {
    use fuser::MountOption as M;
    let mut v = vec![
        M::FSName(format!("xcheckfs:{}", primary.display())),
        M::Subtype("xcheckfs".into()),
        // The kernel checks permissions against the attributes we return
        // (the primary's), with the caller's full credentials.
        M::DefaultPermissions,
    ];
    for o in &a.options {
        v.push(match o.as_str() {
            "dev" => M::Dev,
            "nodev" => M::NoDev,
            "suid" => M::Suid,
            "nosuid" => M::NoSuid,
            "ro" => M::RO,
            "rw" => M::RW,
            "exec" => M::Exec,
            "noexec" => M::NoExec,
            "atime" => M::Atime,
            "noatime" => M::NoAtime,
            "sync" => M::Sync,
            "async" => M::Async,
            "dirsync" => M::DirSync,
            "auto_unmount" => M::AutoUnmount,
            other => M::CUSTOM(other.to_string()),
        });
    }
    v
}

enum Wake {
    Signal(i32),
    Ended,
}

fn cmd_mount(a: MountArgs) -> anyhow::Result<i32> {
    // ---- validation, before anything irreversible happens
    let primary = a.primary.canonicalize().with_context(|| format!("primary {}", a.primary.display()))?;
    let secondary = a.secondary.canonicalize().with_context(|| format!("secondary {}", a.secondary.display()))?;
    let mountpoint = a.mountpoint.canonicalize().with_context(|| format!("mount point {}", a.mountpoint.display()))?;
    for (n, p) in [("primary", &primary), ("secondary", &secondary), ("mount point", &mountpoint)] {
        if !p.is_dir() {
            bail!("{n} {} is not a directory", p.display());
        }
    }
    if within(&primary, &secondary) || within(&secondary, &primary) {
        bail!(
            "primary {} and secondary {} must not contain each other",
            primary.display(),
            secondary.display()
        );
    }
    if mountpoint != secondary && within(&mountpoint, &secondary) {
        bail!("the mount point must not be inside the secondary tree");
    }
    if mountpoint != primary && within(&mountpoint, &primary) {
        bail!("the mount point must not be inside the primary tree (it would appear as an extra directory)");
    }
    let trees = [&primary, &secondary, &mountpoint];
    let inside_trees = |p: &Path| {
        let r = resolve_future(p);
        trees.iter().any(|t| within(&r, t))
    };
    for (what, p) in [
        ("log file", &a.log_file),
        ("pid file", &a.pid_file),
        ("control socket", &a.control_socket),
        ("quarantine directory", &a.quarantine),
    ] {
        if let Some(p) = p
            && inside_trees(p) {
                bail!(
                    "{what} {} is inside a mirrored tree: writing it would bypass xcheckfs (divergence) or deadlock while frozen",
                    p.display()
                );
            }
    }
    if a.ui == Ui::Tui && a.background {
        bail!("--ui tui cannot be combined with --background");
    }
    if let Some(q) = &a.quarantine {
        std::fs::create_dir_all(q).with_context(|| format!("quarantine directory {}", q.display()))?;
    }
    let rules_path = a.rules.clone().or_else(default_rules_path);
    let rules = match &rules_path {
        Some(p) => RulesFile::load(p)?.allow,
        None => Vec::new(),
    };
    let persist = match &rules_path {
        Some(p) if inside_trees(p) => {
            eprintln!(
                "xcheckfs: warning: rules file {} is inside a mirrored tree; new rules will not be saved (use --rules)",
                p.display()
            );
            false
        }
        Some(_) => true,
        None => false,
    };
    let socket_path = a.control_socket.clone().unwrap_or_else(|| control::default_socket(&a.mountpoint));
    if a.control_socket.is_none() && inside_trees(&socket_path) {
        bail!("default control socket {} is inside a mirrored tree; pass --control-socket", socket_path.display());
    }

    // ---- process setup: the kernel already masked the mode; don't mask twice
    // SAFETY: trivial syscall.
    unsafe { libc::umask(0) };
    let nofile = sys::raise_nofile_limit();

    // Open the backends now: this works even when mounting over PRIMARY.
    let pb = PosixBackend::open("primary", &primary).with_context(|| format!("open {}", primary.display()))?;
    let sb = PosixBackend::open("secondary", &secondary).with_context(|| format!("open {}", secondary.display()))?;

    let mut notifier = if a.background { Some(daemon::daemonize()?) } else { None };
    let res = run_mount(&a, primary, secondary, mountpoint, pb, sb, rules, rules_path, persist, socket_path, nofile, &mut notifier);
    if let (Err(e), Some(n)) = (&res, notifier.as_mut()) {
        // The waiting parent prints the error.
        n.failed(&format!("{e:#}"));
        std::process::exit(1);
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn run_mount(
    a: &MountArgs,
    primary: PathBuf,
    secondary: PathBuf,
    mountpoint: PathBuf,
    pb: PosixBackend,
    sb: PosixBackend,
    rules: Vec<xcheckfs::policy::Rule>,
    rules_path: Option<PathBuf>,
    persist: bool,
    socket_path: PathBuf,
    nofile: u64,
    notifier: &mut Option<daemon::Notifier>,
) -> anyhow::Result<i32> {
    let level = match a.verbose {
        0 => a.log_level.level(),
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };
    let stats = Arc::new(Stats::default());
    let (sink, rx) = if a.ui == Ui::Tui {
        let (s, r) = EventSink::channel(65_536);
        (s, Some(r))
    } else {
        (EventSink::disabled(), None)
    };
    match (&a.log_file, a.background, &rx) {
        (Some(f), _, _) => logging::init_file(f, level)?,
        (None, true, _) => logging::init_syslog(level),
        (None, false, Some(_)) => logging::init_tui(level, sink_sender(&sink)),
        (None, false, None) => {
            use std::io::IsTerminal;
            logging::init_stderr(level, !a.no_color && std::io::stderr().is_terminal())
        }
    }
    // Signals are caught from here on: one arriving while the mount is being
    // set up is handled (by unmounting) right after it is up, instead of
    // killing the process and leaving a dead mount behind.
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<Wake>();
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1};
        let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP, SIGUSR1])?;
        let (tx, stats) = (wake_tx.clone(), stats.clone());
        std::thread::Builder::new().name("xcheckfs-signals".into()).spawn(move || {
            for s in signals.forever() {
                if s == SIGUSR1 {
                    tracing::warn!("{}", control::summary(&stats));
                } else if tx.send(Wake::Signal(s)).is_err() {
                    return;
                }
            }
        })?;
    }
    if nofile < 65_536 {
        tracing::warn!("RLIMIT_NOFILE is only {nofile}; xcheckfs needs two descriptors per cached inode");
    }

    // Workers mostly wait in two file systems' syscalls: more than one per
    // CPU. Each holds a request buffer of about 1 MiB.
    let threads = a
        .threads
        .unwrap_or_else(|| (2 * std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)).clamp(16, 64));
    let policy = Arc::new(Policy::new(a.on_mismatch, rules, rules_path.clone(), persist, sink.clone(), stats.clone())?);
    let cfg = EngineConfig {
        check: a.check,
        time_tolerance: a.time_tolerance,
        dir_nlink: !a.no_dir_nlink,
        probe: !a.no_probe,
        parallel: !a.sequential,
        creds: !a.no_creds,
        mirror_locks: !a.no_lock_mirroring,
        op_events: a.ui == Ui::Tui,
        attr_ttl: Duration::from_secs_f64(a.attr_timeout.max(0.0)),
        entry_ttl: Duration::from_secs_f64(a.entry_timeout.max(0.0)),
        direct_io: a.direct_io,
        serialize: a.serialize,
        lock_stripes: a.lock_stripes.clamp(16, 1 << 22),
        fuse_threads: threads,
        quarantine: a.quarantine.clone(),
        quarantine_cap: a.quarantine_cap,
        resync_limit: a.resync_limit.max(1),
    };
    let engine = Arc::new(Engine::new(cfg.clone(), Arc::new(pb), Arc::new(sb), policy.clone(), stats.clone(), sink)?);
    Engine::spawn_lock_watchdog(&engine);

    let info = serde_json::json!({
        "mountpoint": mountpoint.display().to_string(),
        "primary": primary.display().to_string(),
        "secondary": secondary.display().to_string(),
        "check": a.check.name(),
        "pid": std::process::id(),
        "adaptations": engine.adaptations(),
        "capability_gaps": engine.capability_gaps(),
    });
    let _ctl = control::serve(&socket_path, Arc::new(control::Shared { stats: stats.clone(), policy: policy.clone(), info }))?;

    let allow_other = if a.no_allow_other { false } else { a.allow_other || sys::is_root() };
    let mut fcfg = fuser::Config::default();
    fcfg.mount_options = mount_options(a, &primary);
    fcfg.acl = if allow_other { fuser::SessionACL::All } else { fuser::SessionACL::Owner };
    fcfg.n_threads = Some(threads.max(1));
    fcfg.clone_fd = threads > 1;
    fcfg.io_uring = a.io_uring;
    fcfg.io_uring_queue_depth = a.io_uring_depth.max(1);
    // Both sides are local file systems: every request but the ones that wait
    // (locks, fsync, fallocate, copies) is served on the ring thread.
    fcfg.io_uring_dispatch = fuser::RingDispatch::AllButWaits;

    let session = fuser::Session::new(XcheckFs { engine: engine.clone() }, &mountpoint, &fcfg)
        .with_context(|| format!("mount on {}", mountpoint.display()))?;
    let bg = session.spawn()?;
    tracing::warn!(
        "mounted {} on {}, mirroring to {}, check={} on-mismatch={}{}",
        primary.display(),
        mountpoint.display(),
        secondary.display(),
        a.check.name(),
        a.on_mismatch.name(),
        if mountpoint == primary { " [mounted over the primary: all access goes through xcheckfs]" } else { "" }
    );
    // Only now: a failed start must not clobber another daemon's pid file.
    if let Some(p) = &a.pid_file
        && let Err(e) = std::fs::write(p, format!("{}\n", std::process::id()))
    {
        tracing::error!("pid file {}: {e}", p.display());
    }
    if let Some(n) = notifier.as_mut() {
        n.ready();
    }

    let ended = Arc::new(AtomicBool::new(false));
    {
        let (tx, ended) = (wake_tx.clone(), ended.clone());
        std::thread::Builder::new().name("xcheckfs-session".into()).spawn(move || {
            if let Err(e) = bg.join() {
                tracing::error!("FUSE session ended with an error: {e}");
            }
            ended.store(true, Ordering::SeqCst);
            let _ = tx.send(Wake::Ended);
        })?;
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(rx) = rx {
        {
            let (shutdown, ended) = (shutdown.clone(), ended.clone());
            let wake_rx = wake_rx.clone();
            std::thread::spawn(move || {
                if wake_rx.recv().is_ok() || ended.load(Ordering::SeqCst) {
                    shutdown.store(true, Ordering::SeqCst);
                }
            });
        }
        let ctx = tui::TuiContext {
            stats: stats.clone(),
            policy: policy.clone(),
            events: rx,
            history: a.history,
            info: tui::MountInfo {
                mountpoint: mountpoint.clone(),
                primary: primary.clone(),
                secondary: secondary.clone(),
                check: a.check,
                engine: cfg,
                rules_path,
                control_socket: Some(socket_path.clone()),
            },
            shutdown: shutdown.clone(),
        };
        if let Err(e) = tui::run(ctx) {
            eprintln!("xcheckfs: TUI failed: {e:#}");
        }
    } else {
        match wake_rx.recv() {
            Ok(Wake::Signal(s)) => tracing::warn!("signal {s} received, unmounting"),
            Ok(Wake::Ended) | Err(_) => {}
        }
    }

    // A frozen operation would keep the mount busy forever: release pending
    // ones and make sure nothing freezes again while unmounting.
    if policy.mode() == MismatchMode::Freeze {
        policy.set_mode(MismatchMode::Log);
    }
    for p in policy.pending() {
        policy.resolve(p.mismatch.id, xcheckfs::policy::Action::Continue);
    }
    if !ended.load(Ordering::SeqCst) {
        unmount(&mountpoint);
        // Wait for the session to finish draining.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ended.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let summary = control::summary(&stats);
    tracing::warn!("unmounted: {summary}");
    if a.ui == Ui::Tui {
        eprintln!("xcheckfs: {summary}");
    }
    if let Some(p) = &a.pid_file {
        let _ = std::fs::remove_file(p);
    }
    let mismatches = stats.mismatches.load(Ordering::Relaxed);
    Ok(if mismatches > 0 { 3 } else { 0 })
}

fn sink_sender(sink: &EventSink) -> crossbeam_channel::Sender<xcheckfs::events::UiEvent> {
    sink.sender().expect("enabled sink")
}

/// Unmounts, falling back to a lazy detach when the mount is busy. (fuser's
/// own unmount handle cannot be used once the session runs in the
/// background: the mount moves into the background session.)
///
/// The kernel releases closed files asynchronously, so a mount that was busy
/// a moment ago is retried a few times before detaching lazily.
fn unmount(mp: &Path) {
    let Ok(c) = std::ffi::CString::new(mp.as_os_str().as_encoded_bytes()) else { return };
    let fusermount = |args: &[&str]| {
        std::process::Command::new("fusermount3")
            .args(args)
            .arg(mp)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // SAFETY: valid C string.
    let plain = || if sys::is_root() { unsafe { libc::umount2(c.as_ptr(), 0) == 0 } } else { fusermount(&["-u"]) };
    for attempt in 0..6 {
        if plain() {
            return;
        }
        if attempt < 5 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    tracing::warn!("unmount failed (busy); detaching lazily");
    if sys::is_root() {
        // SAFETY: valid C string.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    } else {
        fusermount(&["-u", "-z"]);
    }
}

fn cmd_verify(a: VerifyArgs) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    let opts = xcheckfs::verify::VerifyOptions {
        content: !a.no_content,
        xattrs: !a.no_xattrs,
        mtime: !a.no_mtime,
        time_tolerance: a.time_tolerance,
        dir_nlink: !a.no_dir_nlink,
        threads: a.threads.unwrap_or(0),
        max_reports: a.max_reports,
        one_file_system: true,
    };
    let tty = std::io::stderr().is_terminal();
    let progress = |r: &xcheckfs::verify::VerifyReport| {
        eprint!(
            "\r{} files, {} dirs, {} compared, {} differences   ",
            r.files,
            r.dirs,
            xcheckfs::stats::fmt_bytes(r.bytes_compared as f64),
            r.total_differences
        );
    };
    let r = xcheckfs::verify::verify(&a.primary, &a.secondary, &opts, if tty { Some(&progress) } else { None })?;
    if tty {
        eprintln!();
    }
    for d in &r.differences {
        println!("{}: {}: primary={} secondary={}", d.path, d.what, d.primary, d.secondary);
    }
    for e in &r.errors {
        eprintln!("error: {e}");
    }
    if r.total_differences as usize > r.differences.len() {
        println!("... {} more differences not listed", r.total_differences as usize - r.differences.len());
    }
    println!(
        "{} files, {} dirs, {} compared: {} differences, {} errors",
        r.files,
        r.dirs,
        xcheckfs::stats::fmt_bytes(r.bytes_compared as f64),
        r.total_differences,
        r.errors.len()
    );
    Ok(if r.total_differences > 0 { 3 } else if !r.errors.is_empty() { 2 } else { 0 })
}

fn cmd_ctl(a: CtlArgs) -> anyhow::Result<i32> {
    let mut words = a.command.clone();
    let path = match (&a.socket, &a.mountpoint) {
        (Some(s), first) => {
            // With --socket there is no mount point: the first word is the command.
            if let Some(f) = first {
                words.insert(0, f.to_string_lossy().into_owned());
            }
            s.clone()
        }
        (None, Some(m)) => control::default_socket(m),
        (None, None) => bail!("give a mount point or --socket"),
    };
    let cmd = if words.is_empty() { "status".to_string() } else { words.join(" ") };
    let v = control::request(&path, &cmd)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(if v.get("ok").and_then(|x| x.as_bool()) == Some(true) { 0 } else { 1 })
}
