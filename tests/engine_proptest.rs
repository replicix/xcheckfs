//! Property-based tests of the engine (proptest): random sequences of file system operations are driven through
//! the engine over two temp trees, with and without a fault injected into the secondary.
//!
//! Properties
//!
//! 1. `no_false_positives` / `no_false_positives_concurrent`: healthy trees never produce a mismatch, at any check
//!    level, and end up identical. The primary's results do not depend on the check level.
//! 2. `benign_faults_are_invisible`: a fault that only delays the secondary changes nothing.
//! 3. `injected_faults_are_detected` (Log mode): after ONE fault on the secondary, whenever the trees differ at
//!    the end of a final "observation pass" at least one mismatch was reported. Additionally, at the op in which
//!    the fault fired (see `check_detection` for the exact rules): a failing/lying result is reported at once, a
//!    short write is reported at once, and from `thorough` on a mutation that diverged the trees is reported by the
//!    operation itself. In every case the primary's results are those of an engine without any fault.
//! 4. `lies_are_detected`: faults that falsify what the secondary returns (attributes, data, listings, link
//!    targets, xattrs) on every call are reported by the observation pass whenever the lie is effective.
//! 5. `resync_converges` (Resync mode): after a transient fault the observation pass, repeated until it is quiet,
//!    leaves identical trees, without failed repairs, without touching the primary, with the same primary results
//!    and the same primary tree as an engine without a fault.
//!
//! The same faults, every (method, effect) pair of the tables, also run on one fixed workload that uses every
//! operation (`canonical`, the `every_*` tests): random sequences meet the rarer faults rarely. The cases that the
//! properties found are plain `regression_*` tests (with a note about what they found).
//!
//! The oracle of a faulted run needs no model of the file system: the secondary is the only thing that is
//! broken, so the primary tree and the results the application gets are those of a run without the fault (a
//! "reference run" of the same operations), and whether the trees differ is read off the two directories
//! behind the engine's back.
//!
//! Case counts are small so that the file runs in well under a minute; `PROPTEST_CASES=2000 cargo test --release
//! --test engine_proptest` hunts harder. Failures are persisted in `tests/engine_proptest.proptest-regressions`
//! (commit it). Knobs for debugging: `XCHECKFS_TEST_LOG=warn cargo test --test engine_proptest NAME -- --nocapture`
//! shows the engine's log; `XCHECKFS_PT_ONLY=ShortWrite` restricts the `every_*` tests to the faults whose
//! description contains the text. The sequences of operations print readably, e.g. `Write { p: /d0/f1, off: 0,
//! len: 5, seed: 3 }` (paths are `/`, `/d0`, `/d1`, `/d0/d1` plus `f0`..`f7`, `d0`, `d1`).

mod common;

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Barrier;
use std::sync::atomic::Ordering::Relaxed;
use std::collections::BTreeMap;
use std::time::Duration;

use common::*;
use proptest::prelude::*;
use proptest::test_runner::{Config as PtConfig, FileFailurePersistence, TestCaseError};
use xcheckfs::backend::TimeSpec;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp, StatLie, Trigger};
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::engine::SetAttr;
use xcheckfs::sys::{FileKind, Ts};

const LEVELS: [CheckLevel; 3] = [CheckLevel::Basic, CheckLevel::Thorough, CheckLevel::Paranoid];

// =================================================================================================================
// The namespace: four directories, ten names. Collisions are the point, so there are few.
// =================================================================================================================

const DIRS: [&str; 4] = ["/", "/d0", "/d1", "/d0/d1"];
const NAMES: [&str; 10] = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "d0", "d1"];
/// Open-file slots per lane.
const SLOTS: usize = 4;

fn join(dir: &str, name: &str) -> String {
    if dir == "/" { format!("/{name}") } else { format!("{dir}/{name}") }
}

/// A path of the namespace; prints as the path.
#[derive(Clone, Copy, PartialEq, Eq)]
struct P {
    dir: u8,
    name: u8,
}

impl P {
    fn path(&self) -> String {
        join(DIRS[self.dir as usize], NAMES[self.name as usize])
    }
}

impl fmt::Debug for P {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path())
    }
}

#[derive(Clone, Copy, Debug)]
enum RFlag {
    Plain,
    NoReplace,
    Exchange,
}

#[derive(Clone, Copy, Debug)]
enum FMode {
    Alloc,
    KeepSize,
    PunchHole,
    ZeroRange,
}

impl FMode {
    fn flags(self) -> i32 {
        match self {
            FMode::Alloc => 0,
            FMode::KeepSize => libc::FALLOC_FL_KEEP_SIZE,
            FMode::PunchHole => libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            FMode::ZeroRange => libc::FALLOC_FL_ZERO_RANGE,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum OMode {
    Read,
    ReadWrite,
    Append,
    Truncate,
}

impl OMode {
    fn flags(self) -> i32 {
        match self {
            OMode::Read => libc::O_RDONLY,
            OMode::ReadWrite => libc::O_RDWR,
            OMode::Append => libc::O_WRONLY | libc::O_APPEND,
            OMode::Truncate => libc::O_RDWR | libc::O_TRUNC,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Mt {
    Now,
    /// 2001-09-09 plus this many days.
    At(u8),
}

#[derive(Clone, Copy, Debug)]
enum XFlag {
    Any,
    Create,
    Replace,
}

const XNAMES: [&str; 3] = ["user.a", "user.b", "user.c"];

/// One file system operation. Path based operations behave like the kernel does for the corresponding system
/// call (lookup of the components, open, the call, close); the `Open`/`Fd*` ones work on handles that stay open
/// across other operations, also across an unlink of the name. Failing operations are fine: both file systems
/// must fail the same way.
#[derive(Clone, Debug)]
enum Op {
    // ---- file content
    Create { p: P, excl: bool },
    Write { p: P, off: u16, len: u16, seed: u8 },
    Append { p: P, len: u16, seed: u8 },
    Read { p: P, off: u16, len: u16 },
    Setattr { p: P, mode: Option<u16>, size: Option<u16>, mtime: Option<Mt> },
    Fallocate { p: P, mode: FMode, off: u16, len: u16 },
    CopyRange { src: P, dst: P, off_in: u16, off_out: u16, len: u16 },
    Fsync { p: P, data: bool },
    // ---- namespace
    Getattr { p: P },
    Mkdir { p: P },
    Rmdir { p: P },
    Unlink { p: P },
    Rename { from: P, to: P, flag: RFlag },
    Link { from: P, to: P },
    Symlink { p: P, target: u8 },
    Readlink { p: P },
    Readdir { dir: u8 },
    // ---- xattrs
    Setxattr { p: P, name: u8, len: u8, seed: u8, flag: XFlag },
    Getxattr { p: P, name: u8 },
    Listxattr { p: P },
    Removexattr { p: P, name: u8 },
    // ---- open handles
    Open { slot: u8, p: P, mode: OMode },
    Close { slot: u8 },
    FdWrite { slot: u8, off: u16, len: u16, seed: u8 },
    FdRead { slot: u8, off: u16, len: u16 },
    FdTruncate { slot: u8, size: u16 },
    FdCopy { src: u8, dst: u8, off_in: u16, off_out: u16, len: u16 },
}

const SYMLINK_TARGETS: [&str; 6] = ["f0", "f1", "d0", "../f2", "/d0/f1", "dangling"];
/// Modes for `Setattr`. Those without search permission lock the applicant out of a directory: fine, both file
/// systems must agree on that, too (the tests clean up after themselves, see `Env`).
const MODES: [u16; 8] = [0o755, 0o755, 0o700, 0o750, 0o555, 0o644, 0o600, 0o444];

/// Deterministic, never-zero data.
fn data(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| ((seed as usize * 131 + i * 17 + (i >> 5) * 5) % 251 + 1) as u8).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Strategies

fn name_idx() -> impl Strategy<Value = u8> {
    prop_oneof![6 => 0u8..3, 3 => 3u8..8, 1 => 8u8..10]
}

fn dir_idx() -> impl Strategy<Value = u8> {
    prop_oneof![5 => Just(0u8), 3 => Just(1u8), 3 => Just(2u8), 1 => Just(3u8)]
}

fn any_p() -> impl Strategy<Value = P> {
    (dir_idx(), name_idx()).prop_map(|(dir, name)| P { dir, name })
}

/// Names that are good directory names.
fn dir_p() -> impl Strategy<Value = P> {
    (0u8..3, prop_oneof![4 => 8u8..10, 1 => 0u8..8]).prop_map(|(dir, name)| P { dir, name })
}

fn offset() -> impl Strategy<Value = u16> {
    prop_oneof![3 => Just(0u16), 5 => 0u16..300, 2 => 0u16..2500]
}

fn length() -> impl Strategy<Value = u16> {
    prop_oneof![8 => 1u16..200, 2 => 200u16..2000]
}

fn slot() -> impl Strategy<Value = u8> {
    0u8..SLOTS as u8
}

fn mode_s() -> impl Strategy<Value = u16> {
    prop::sample::select(MODES.to_vec())
}

fn data_ops() -> BoxedStrategy<Op> {
    prop_oneof![
        6 => (any_p(), any::<bool>()).prop_map(|(p, excl)| Op::Create { p, excl }),
        10 => (any_p(), offset(), length(), 0u8..4).prop_map(|(p, off, len, seed)| Op::Write { p, off, len, seed }),
        3 => (any_p(), length(), 0u8..4).prop_map(|(p, len, seed)| Op::Append { p, len, seed }),
        5 => (any_p(), offset(), length()).prop_map(|(p, off, len)| Op::Read { p, off, len }),
        4 => (
            any_p(),
            prop::option::of(mode_s()),
            prop::option::of(prop_oneof![Just(0u16), 0u16..3000]),
            prop::option::of(prop_oneof![Just(Mt::Now), (0u8..4).prop_map(Mt::At)]),
        )
            .prop_map(|(p, mode, size, mtime)| Op::Setattr { p, mode, size, mtime }),
        2 => (
            any_p(),
            prop_oneof![Just(FMode::Alloc), Just(FMode::KeepSize), Just(FMode::PunchHole), Just(FMode::ZeroRange)],
            offset(),
            length(),
        )
            .prop_map(|(p, mode, off, len)| Op::Fallocate { p, mode, off, len }),
        3 => (any_p(), any_p(), offset(), offset(), length())
            .prop_map(|(src, dst, off_in, off_out, len)| Op::CopyRange { src, dst, off_in, off_out, len }),
        1 => (any_p(), any::<bool>()).prop_map(|(p, data)| Op::Fsync { p, data }),
    ]
    .boxed()
}

fn namespace_ops() -> BoxedStrategy<Op> {
    prop_oneof![
        3 => any_p().prop_map(|p| Op::Getattr { p }),
        4 => dir_p().prop_map(|p| Op::Mkdir { p }),
        3 => prop_oneof![3 => dir_p(), 1 => any_p()].prop_map(|p| Op::Rmdir { p }),
        5 => any_p().prop_map(|p| Op::Unlink { p }),
        6 => (
            any_p(),
            any_p(),
            prop_oneof![8 => Just(RFlag::Plain), 1 => Just(RFlag::NoReplace), 1 => Just(RFlag::Exchange)],
        )
            .prop_map(|(from, to, flag)| Op::Rename { from, to, flag }),
        4 => (any_p(), any_p()).prop_map(|(from, to)| Op::Link { from, to }),
        3 => (any_p(), 0u8..SYMLINK_TARGETS.len() as u8).prop_map(|(p, target)| Op::Symlink { p, target }),
        2 => any_p().prop_map(|p| Op::Readlink { p }),
        3 => (0u8..DIRS.len() as u8).prop_map(|dir| Op::Readdir { dir }),
    ]
    .boxed()
}

fn xattr_ops() -> BoxedStrategy<Op> {
    let xname = || 0u8..XNAMES.len() as u8;
    prop_oneof![
        4 => (
            any_p(),
            xname(),
            prop_oneof![Just(0u8), 1u8..64],
            0u8..3,
            prop_oneof![6 => Just(XFlag::Any), 1 => Just(XFlag::Create), 1 => Just(XFlag::Replace)],
        )
            .prop_map(|(p, name, len, seed, flag)| Op::Setxattr { p, name, len, seed, flag }),
        3 => (any_p(), xname()).prop_map(|(p, name)| Op::Getxattr { p, name }),
        2 => any_p().prop_map(|p| Op::Listxattr { p }),
        2 => (any_p(), xname()).prop_map(|(p, name)| Op::Removexattr { p, name }),
    ]
    .boxed()
}

fn handle_ops() -> BoxedStrategy<Op> {
    prop_oneof![
        4 => (
            slot(),
            any_p(),
            prop_oneof![
                Just(OMode::Read),
                Just(OMode::ReadWrite),
                Just(OMode::Append),
                Just(OMode::Truncate)
            ],
        )
            .prop_map(|(slot, p, mode)| Op::Open { slot, p, mode }),
        2 => slot().prop_map(|slot| Op::Close { slot }),
        4 => (slot(), offset(), length(), 0u8..4).prop_map(|(slot, off, len, seed)| Op::FdWrite { slot, off, len, seed }),
        3 => (slot(), offset(), length()).prop_map(|(slot, off, len)| Op::FdRead { slot, off, len }),
        1 => (slot(), 0u16..3000).prop_map(|(slot, size)| Op::FdTruncate { slot, size }),
        1 => (slot(), slot(), offset(), offset(), length())
            .prop_map(|(src, dst, off_in, off_out, len)| Op::FdCopy { src, dst, off_in, off_out, len }),
    ]
    .boxed()
}

fn op_s() -> BoxedStrategy<Op> {
    prop_oneof![6 => data_ops(), 6 => namespace_ops(), 2 => xattr_ops(), 4 => handle_ops()].boxed()
}

fn ops_s() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_s(), 1..60)
}

fn lanes_s() -> impl Strategy<Value = Vec<Vec<Op>>> {
    prop::collection::vec(prop::collection::vec(op_s(), 1..25), 2..5)
}

fn level_s() -> impl Strategy<Value = CheckLevel> {
    prop_oneof![Just(CheckLevel::Basic), Just(CheckLevel::Thorough), Just(CheckLevel::Paranoid)]
}

// =================================================================================================================
// Execution
// =================================================================================================================

/// The outcome of one operation as seen by the application: a deterministic text (never contains times or inode
/// numbers) and the number of bytes written / copied.
struct Out {
    text: String,
    bytes: usize,
}

fn e(errno: i32) -> String {
    xcheckfs::sys::errno_name(errno).to_string()
}

fn st_text(st: &xcheckfs::sys::Stat) -> String {
    let size = if st.mode & libc::S_IFMT == libc::S_IFDIR { 0 } else { st.size };
    format!("{:o} size={size} nlink={}", st.mode, st.nlink)
}

fn hash(v: &[u8]) -> String {
    format!("{}:{:016x}", v.len(), xxhash_rust::xxh3::xxh3_64(v))
}

/// Executes operations through the harness like a process would. Never panics on an unexpected errno.
struct Runner<'a> {
    h: &'a Harness,
    slots: [Option<Fh>; SLOTS],
}

impl<'a> Runner<'a> {
    fn new(h: &'a Harness) -> Runner<'a> {
        Runner { h, slots: [None; SLOTS] }
    }

    fn close(&self, f: Fh) -> String {
        let a = self.h.engine.flush(&self.h.ctx, f.ino, f.fh, f.fh);
        let _ = self.h.engine.release(&self.h.ctx, f.ino, f.fh);
        match a {
            Ok(()) => "ok".into(),
            Err(x) => format!("flush {}", e(x)),
        }
    }

    /// Closes everything that is still open.
    fn finish(&mut self) {
        for s in 0..SLOTS {
            if let Some(f) = self.slots[s].take() {
                self.close(f);
            }
        }
    }

    fn with_file(&self, path: &str, flags: i32, body: impl FnOnce(Fh) -> (String, usize)) -> Out {
        match self.h.try_open(path, flags) {
            Err(x) => Out { text: format!("open {}", e(x)), bytes: 0 },
            Ok(f) => {
                let (t, bytes) = body(f);
                let c = self.close(f);
                Out { text: format!("{t}; close {c}"), bytes }
            }
        }
    }

    fn text(t: String) -> Out {
        Out { text: t, bytes: 0 }
    }

    fn res<T: fmt::Debug>(r: Result<T, i32>) -> Out {
        Runner::text(match r {
            Ok(v) => format!("ok {v:?}"),
            Err(x) => e(x),
        })
    }

    fn slot(&self, s: u8) -> Option<Fh> {
        self.slots[s as usize % SLOTS]
    }

    fn exec(&mut self, op: &Op) -> Out {
        let h = self.h;
        let ctx = &h.ctx;
        match op {
            Op::Create { p, excl } => match h.try_create(&p.path(), 0o644, if *excl { libc::O_EXCL } else { 0 }) {
                Ok(f) => Runner::text(format!("created; close {}", self.close(f))),
                Err(x) => Runner::text(e(x)),
            },
            Op::Write { p, off, len, seed } => self.with_file(&p.path(), libc::O_WRONLY, |f| {
                match h.try_pwrite(f, *off as u64, &data(*seed, *len as usize)) {
                    Ok(n) => (format!("wrote {n}"), n),
                    Err(x) => (e(x), 0),
                }
            }),
            Op::Append { p, len, seed } => self.with_file(&p.path(), libc::O_WRONLY | libc::O_APPEND, |f| {
                match h.try_pwrite(f, 0, &data(*seed, *len as usize)) {
                    Ok(n) => (format!("appended {n}"), n),
                    Err(x) => (e(x), 0),
                }
            }),
            Op::Read { p, off, len } => self.with_file(&p.path(), libc::O_RDONLY, |f| {
                match h.try_pread(f, *off as u64, *len as usize) {
                    Ok(v) => (format!("read {}", hash(&v)), 0),
                    Err(x) => (e(x), 0),
                }
            }),
            Op::Setattr { p, mode, size, mtime } => {
                let sa = SetAttr {
                    mode: mode.map(u32::from),
                    size: size.map(u64::from),
                    mtime: mtime.map(|m| match m {
                        Mt::Now => TimeSpec::Now,
                        Mt::At(d) => TimeSpec::Set(Ts { sec: 1_000_000_000 + d as i64 * 86_400, nsec: 0 }),
                    }),
                    ..Default::default()
                };
                match h.setattr(&p.path(), sa) {
                    Ok(a) => Runner::text(st_text(&a.st)),
                    Err(x) => Runner::text(e(x)),
                }
            }
            Op::Fallocate { p, mode, off, len } => self.with_file(&p.path(), libc::O_RDWR, |f| {
                (format!("{:?}", h.engine.fallocate(ctx, f.ino, f.fh, *off as u64, *len as u64, mode.flags()).map_err(e)), 0)
            }),
            Op::CopyRange { src, dst, off_in, off_out, len } => {
                match (h.try_open(&src.path(), libc::O_RDONLY), h.try_open(&dst.path(), libc::O_RDWR)) {
                    (Ok(a), Ok(b)) => {
                        let r = h.engine.copy_file_range(ctx, a.fh, *off_in as u64, b.fh, *off_out as u64, *len as u64, 0);
                        let (ca, cb) = (self.close(a), self.close(b));
                        match r {
                            Ok(n) => Out { text: format!("copied {n}; close {ca} {cb}"), bytes: n as usize },
                            Err(x) => Runner::text(format!("{}; close {ca} {cb}", e(x))),
                        }
                    }
                    (a, b) => {
                        let mut t = String::new();
                        for (what, r) in [("src", a), ("dst", b)] {
                            match r {
                                Ok(f) => t += &format!("{what} opened (close {}) ", self.close(f)),
                                Err(x) => t += &format!("{what} {} ", e(x)),
                            }
                        }
                        Runner::text(t)
                    }
                }
            }
            Op::Fsync { p, data } => {
                self.with_file(&p.path(), libc::O_RDONLY, |f| (format!("{:?}", h.engine.fsync(ctx, f.ino, f.fh, *data).map_err(e)), 0))
            }
            Op::Getattr { p } => Runner::text(match h.try_lookup(&p.path()) {
                Err(x) => e(x),
                Ok(a) => match h.engine.getattr(ctx, a.id) {
                    Ok(a) => st_text(&a.st),
                    Err(x) => e(x),
                },
            }),
            Op::Mkdir { p } => Runner::res(h.try_mkdir(&p.path(), 0o755).map(|a| st_text(&a.st))),
            Op::Rmdir { p } => Runner::res(h.try_rmdir(&p.path())),
            Op::Unlink { p } => Runner::res(h.try_unlink(&p.path())),
            Op::Rename { from, to, flag } => {
                let fl = match flag {
                    RFlag::Plain => 0,
                    RFlag::NoReplace => libc::RENAME_NOREPLACE,
                    RFlag::Exchange => libc::RENAME_EXCHANGE,
                };
                Runner::res(h.try_rename_flags(&from.path(), &to.path(), fl))
            }
            Op::Link { from, to } => Runner::res(h.try_link(&from.path(), &to.path()).map(|a| st_text(&a.st))),
            Op::Symlink { p, target } => {
                Runner::res(h.try_symlink(SYMLINK_TARGETS[*target as usize % SYMLINK_TARGETS.len()], &p.path()).map(|a| st_text(&a.st)))
            }
            Op::Readlink { p } => Runner::res(h.readlink(&p.path()).map(|v| String::from_utf8_lossy(&v).into_owned())),
            Op::Readdir { dir } => Runner::res(list_dir(h, DIRS[*dir as usize % DIRS.len()])),
            Op::Setxattr { p, name, len, seed, flag } => {
                let fl = match flag {
                    XFlag::Any => 0,
                    XFlag::Create => libc::XATTR_CREATE,
                    XFlag::Replace => libc::XATTR_REPLACE,
                };
                let name = XNAMES[*name as usize % XNAMES.len()];
                Runner::res(h.try_lookup(&p.path()).and_then(|a| {
                    h.engine.setxattr(ctx, a.id, std::ffi::OsStr::new(name), &data(*seed, *len as usize), fl)
                }))
            }
            Op::Getxattr { p, name } => {
                Runner::res(h.getxattr(&p.path(), XNAMES[*name as usize % XNAMES.len()]).map(|v| hash(&v)))
            }
            Op::Listxattr { p } => Runner::res(h.user_xattrs(&p.path())),
            Op::Removexattr { p, name } => Runner::res(h.removexattr(&p.path(), XNAMES[*name as usize % XNAMES.len()])),
            Op::Open { slot, p, mode } => {
                let s = *slot as usize % SLOTS;
                let mut t = String::new();
                if let Some(old) = self.slots[s].take() {
                    t += &format!("closed old: {}; ", self.close(old));
                }
                match h.try_open(&p.path(), mode.flags()) {
                    Ok(f) => {
                        self.slots[s] = Some(f);
                        t += "opened";
                    }
                    Err(x) => t += &e(x),
                }
                Runner::text(t)
            }
            Op::Close { slot } => match self.slots[*slot as usize % SLOTS].take() {
                Some(f) => Runner::text(format!("closed {}", self.close(f))),
                None => Runner::text("not open".into()),
            },
            Op::FdWrite { slot, off, len, seed } => match self.slot(*slot) {
                None => Runner::text("not open".into()),
                Some(f) => match h.try_pwrite(f, *off as u64, &data(*seed, *len as usize)) {
                    Ok(n) => Out { text: format!("wrote {n}"), bytes: n },
                    Err(x) => Runner::text(e(x)),
                },
            },
            Op::FdRead { slot, off, len } => match self.slot(*slot) {
                None => Runner::text("not open".into()),
                Some(f) => Runner::res(h.try_pread(f, *off as u64, *len as usize).map(|v| hash(&v))),
            },
            Op::FdTruncate { slot, size } => match self.slot(*slot) {
                None => Runner::text("not open".into()),
                Some(f) => Runner::res(
                    h.engine
                        .setattr(ctx, f.ino, SetAttr { size: Some(*size as u64), ..Default::default() }, Some(f.fh))
                        .map(|a| st_text(&a.st)),
                ),
            },
            Op::FdCopy { src, dst, off_in, off_out, len } => match (self.slot(*src), self.slot(*dst)) {
                (Some(a), Some(b)) => match h.engine.copy_file_range(ctx, a.fh, *off_in as u64, b.fh, *off_out as u64, *len as u64, 0) {
                    Ok(n) => Out { text: format!("copied {n}"), bytes: n as usize },
                    Err(x) => Runner::text(e(x)),
                },
                _ => Runner::text("not open".into()),
            },
        }
    }
}

/// `readdir` through the engine without panicking.
fn list_dir(h: &Harness, path: &str) -> Result<Vec<String>, i32> {
    let a = h.try_lookup(path)?;
    let fh = h.engine.opendir(&h.ctx, a.id)?;
    let mut names = Vec::new();
    let mut off = 0;
    let mut res = Ok(());
    loop {
        let mut n = 0;
        let r = h.engine.readdir(&h.ctx, a.id, fh, off, &mut |_, next, _, name| {
            names.push(String::from_utf8_lossy(name).into_owned());
            off = next;
            n += 1;
            false
        });
        if let Err(x) = r {
            res = Err(x);
            break;
        }
        if n == 0 {
            break;
        }
    }
    let _ = h.engine.releasedir(&h.ctx, a.id, fh);
    res?;
    names.retain(|n| n != "." && n != "..");
    names.sort();
    Ok(names)
}

/// A harness that gives back the permissions of its trees before they are removed (a `chmod` of the model may
/// have taken away the right to enter or change a directory).
struct Env(Harness);

impl std::ops::Deref for Env {
    type Target = Harness;
    fn deref(&self) -> &Harness {
        &self.0
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        fn open_up(dir: &Path) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        open_up(&e.path());
                    }
                }
            }
        }
        open_up(self.0.p_root());
        open_up(self.0.s_root());
    }
}

/// A fresh harness with `/d0` and `/d1` in place. `parallel`: the two halves of an operation run concurrently
/// (the default of the engine; the sequential variant is cheaper for the tests that run hundreds of harnesses).
fn harness(level: CheckLevel, mode: MismatchMode, parallel: bool) -> Env {
    // `XCHECKFS_TEST_LOG=warn cargo test --test engine_proptest -- --nocapture` shows the engine's log
    if let Ok(f) = std::env::var("XCHECKFS_TEST_LOG") {
        let _ = tracing_subscriber::fmt().with_env_filter(f).with_test_writer().try_init();
    }
    let h = Harness::builder().level(level).mode(mode).config(move |c| c.parallel = parallel).build();
    h.mkdir("/d0");
    h.mkdir("/d1");
    Env(h)
}

fn run_ops(h: &Harness, ops: &[Op]) -> Vec<String> {
    let mut r = Runner::new(h);
    let outs = ops.iter().map(|op| r.exec(op).text).collect();
    r.finish();
    outs
}

/// Mismatches reported so far, repeats of already reported ones included.
fn found(h: &Harness) -> u64 {
    h.stats.mismatches.load(Relaxed) + h.stats.repeats.load(Relaxed)
}

/// The observation pass: looks at everything the file system has to show, through the engine, the way a backup
/// program would: every name of the namespace (also the ones that should not exist) is looked up and stat'ed, files
/// are read completely, directories listed, links read, extended attributes listed and read.
fn observe(h: &Harness) {
    /// Looks at one object; returns its type.
    fn object(h: &Harness, path: &str) -> Option<u32> {
        let a = h.try_lookup(path).ok()?;
        let _ = h.engine.getattr(&h.ctx, a.id);
        let kind = a.st.mode & libc::S_IFMT;
        match kind {
            libc::S_IFREG => {
                if let Ok(f) = h.try_open(path, libc::O_RDONLY) {
                    let mut off = 0u64;
                    while let Ok(d) = h.try_pread(f, off, 128 << 10) {
                        if d.is_empty() {
                            break;
                        }
                        off += d.len() as u64;
                    }
                    let _ = h.engine.flush(&h.ctx, f.ino, f.fh, f.fh);
                    let _ = h.engine.release(&h.ctx, f.ino, f.fh);
                }
            }
            libc::S_IFLNK => {
                let _ = h.readlink(path);
            }
            _ => {}
        }
        if let Ok(names) = h.listxattr(path) {
            for n in names {
                let _ = h.getxattr(path, &n);
            }
        }
        Some(kind)
    }
    /// Lists a directory and looks at everything in it (a directory can have been renamed to a name that is not a
    /// directory of the model, and still holds objects).
    fn walk(h: &Harness, dir: &str, depth: usize) {
        for n in list_dir(h, dir).unwrap_or_default() {
            let path = join(dir, &n);
            if object(h, &path) == Some(libc::S_IFDIR) && depth < 8 {
                walk(h, &path, depth + 1);
            }
        }
    }
    object(h, "/");
    walk(h, "/", 0);
    // and the names of the model that might exist on one side only
    for d in DIRS {
        for n in NAMES {
            object(h, &join(d, n));
        }
    }
}

// =================================================================================================================
// Property 1: no false positives
// =================================================================================================================

#[track_caller]
fn first_difference(a: &[String], b: &[String], ops: &[Op], what: &str) -> Result<(), TestCaseError> {
    if let Some(i) = a.iter().zip(b).position(|(x, y)| x != y) {
        return Err(TestCaseError::fail(format!(
            "{what}: the results differ at op #{i} {:?}:\n  {}\n  {}",
            ops[i], a[i], b[i]
        )));
    }
    prop_assert_eq!(a.len(), b.len());
    Ok(())
}

fn assert_healthy(h: &Harness, level: CheckLevel) -> Result<(), TestCaseError> {
    prop_assert_eq!(found(h), 0, "{:?}: mismatches:\n  {}", level, h.describe_mismatches());
    let d = h.tree_diff();
    prop_assert!(d.is_empty(), "{:?}: trees differ:\n  {}", level, d.join("\n  "));
    prop_assert_eq!(h.stats.secondary_skipped.load(Relaxed), 0, "{:?}: the secondary was skipped", level);
    Ok(())
}

fn check_healthy(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut first: Option<Vec<String>> = None;
    for level in LEVELS {
        let h = harness(level, MismatchMode::Log, true);
        let outs = run_ops(&h, ops);
        observe(&h);
        assert_healthy(&h, level)?;
        match &first {
            None => first = Some(outs),
            // what the application sees does not depend on how much the engine checks
            Some(f) => first_difference(f, &outs, ops, &format!("{level:?} vs Basic"))?,
        }
    }
    Ok(())
}

fn check_healthy_concurrent(lanes: &[Vec<Op>], level: CheckLevel) -> Result<(), TestCaseError> {
    let h = harness(level, MismatchMode::Log, true);
    let barrier = Barrier::new(lanes.len());
    std::thread::scope(|s| {
        for lane in lanes {
            let (h, barrier) = (&h, &barrier);
            s.spawn(move || {
                let mut r = Runner::new(h);
                barrier.wait();
                for op in lane {
                    r.exec(op);
                }
                r.finish();
            });
        }
    });
    observe(&h);
    assert_healthy(&h, level)
}

// =================================================================================================================
// Faults
// =================================================================================================================

/// One fault on the secondary.
#[derive(Clone, Debug)]
struct Inj {
    op: FaultOp,
    effect: Effect,
    trigger: Trigger,
}

impl Inj {
    fn fault(&self) -> Fault {
        Fault::new(self.op, self.effect.clone()).trigger(self.trigger)
    }
}

/// Faults that change what the secondary does: calls that fail, calls that are not carried out, writes that are
/// lost, short or corrupted. These can leave the trees different.
fn mutating_table() -> Vec<(FaultOp, Effect)> {
    use FaultOp as F;
    let mut v = Vec::new();
    for op in [
        F::Lookup, F::StatAt, F::Stat, F::Open, F::Create, F::Pread, F::Pwrite, F::Flush, F::Fsync, F::Opendir, F::Readdir,
        F::Mkdir, F::Unlink, F::Rmdir, F::Symlink, F::Rename, F::Link, F::Chmod, F::Truncate, F::Utimens, F::Readlink,
        F::Setxattr, F::Getxattr, F::Listxattr, F::Removexattr, F::Fallocate, F::CopyFileRange,
    ] {
        v.push((op, Effect::Errno(libc::EIO)));
        v.push((op, Effect::AppliedThenErrno(libc::EIO)));
    }
    for op in [
        F::Unlink, F::Rmdir, F::Symlink, F::Rename, F::Link, F::Chmod, F::Truncate, F::Utimens, F::Mkdir, F::Setxattr,
        F::Removexattr, F::Fallocate, F::Pwrite, F::CopyFileRange, F::Flush, F::Fsync,
    ] {
        v.push((op, Effect::Skip));
    }
    v.push((F::Pwrite, Effect::DropWrite));
    for offset in [0, 3, 17] {
        v.push((F::Pwrite, Effect::CorruptWrite { offset }));
    }
    for k in [0, 1, 7] {
        v.push((F::Pwrite, Effect::ShortWrite(k)));
        v.push((F::CopyFileRange, Effect::ShortWrite(k)));
    }
    // a phantom entry in one listing, a missing one (see also the lies, which falsify every call)
    v.push((F::Readdir, Effect::AddEntry(b"zz".to_vec())));
    v.push((F::Readdir, Effect::DropEntry(b"f0".to_vec())));
    v
}

fn mutating_s(triggers: BoxedStrategy<Trigger>) -> impl Strategy<Value = Inj> {
    (prop::sample::select(mutating_table()), triggers).prop_map(|((op, effect), trigger)| Inj { op, effect, trigger })
}

/// Any trigger. `Once` first: that is what a failing case shrinks to.
fn any_trigger() -> BoxedStrategy<Trigger> {
    prop_oneof![
        3 => Just(Trigger::Once),
        3 => (2u64..6).prop_map(Trigger::Nth),
        1 => (1u64..4).prop_map(Trigger::After),
        1 => (2u64..4).prop_map(Trigger::Every),
        2 => Just(Trigger::Always),
    ]
    .boxed()
}

/// Triggers that fire exactly once (a transient fault).
fn transient_trigger() -> BoxedStrategy<Trigger> {
    prop_oneof![3 => Just(Trigger::Once), 2 => (2u64..8).prop_map(Trigger::Nth)].boxed()
}

/// Faults that falsify what the secondary reports without changing it. They fire on every call, so that the
/// observation pass is guaranteed to meet them.
fn lie_table() -> Vec<(FaultOp, Effect)> {
    use FaultOp as F;
    let mut v = Vec::new();
    let stat_lies = [
        StatLie::Size(999_999),
        StatLie::Mode(0o111),
        StatLie::Nlink(77),
        StatLie::Uid(4242),
        StatLie::Gid(4242),
        StatLie::MtimeShift(5),
        StatLie::MtimeShift(-86_400),
        StatLie::Type(libc::S_IFLNK),
        StatLie::Type(libc::S_IFREG),
    ];
    for op in [F::Stat, F::Lookup] {
        for l in &stat_lies {
            v.push((op, Effect::Stat(l.clone())));
        }
    }
    v.push((F::Readlink, Effect::ReadlinkTarget(b"LIE".to_vec())));
    v.push((F::Getxattr, Effect::XattrValue(b"LIE".to_vec())));
    v.push((F::Listxattr, Effect::XattrListAdd(b"user.zz".to_vec())));
    for n in ["user.a", "user.b", "user.c"] {
        v.push((F::Listxattr, Effect::XattrListDrop(n.as_bytes().to_vec())));
    }
    for n in ["f0", "f1", "d0"] {
        v.push((F::Readdir, Effect::DropEntry(n.as_bytes().to_vec())));
        v.push((F::Readdir, Effect::ChangeEntryKind(n.as_bytes().to_vec(), FileKind::Symlink)));
        v.push((F::Readdir, Effect::ChangeEntryKind(n.as_bytes().to_vec(), FileKind::Dir)));
    }
    v.push((F::Readdir, Effect::AddEntry(b"zz".to_vec())));
    for offset in [0, 5] {
        v.push((F::Pread, Effect::CorruptRead { offset }));
    }
    for k in [0, 3] {
        v.push((F::Pread, Effect::ShortRead(k)));
    }
    v
}

/// What the primary tree holds (read behind the engine's back).
struct Fact {
    root: bool,
    kind: u32,
    perm: u32,
    size: u64,
    nlink: u64,
    target: Option<Vec<u8>>,
    xattrs: BTreeMap<String, Vec<u8>>,
    /// (name, type) of the entries of a directory
    entries: Vec<(String, u32)>,
}

fn facts(root: &Path) -> Vec<Fact> {
    fn one(path: &Path, is_root: bool) -> Option<Fact> {
        let md = std::fs::symlink_metadata(path).ok()?;
        let kind = md.mode() & libc::S_IFMT;
        let mut f = Fact {
            root: is_root,
            kind,
            perm: md.mode() & 0o7777,
            size: md.size(),
            nlink: md.nlink(),
            target: None,
            xattrs: BTreeMap::new(),
            entries: Vec::new(),
        };
        if kind == libc::S_IFLNK {
            f.target = std::fs::read_link(path).ok().map(|t| t.as_os_str().as_encoded_bytes().to_vec());
        } else {
            for n in raw_user_xattrs(path).unwrap_or_default() {
                f.xattrs.insert(n.clone(), raw_getxattr(path, &n).unwrap_or_default());
            }
        }
        if kind == libc::S_IFDIR {
            for ent in std::fs::read_dir(path).into_iter().flatten().flatten() {
                let k = std::fs::symlink_metadata(ent.path()).map(|m| m.mode() & libc::S_IFMT).unwrap_or(0);
                f.entries.push((ent.file_name().to_string_lossy().into_owned(), k));
            }
        }
        Some(f)
    }
    fn rec(path: &Path, out: &mut Vec<Fact>) {
        let Ok(rd) = std::fs::read_dir(path) else { return };
        for ent in rd.flatten() {
            out.extend(one(&ent.path(), false));
            if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                rec(&ent.path(), out);
            }
        }
    }
    let mut out: Vec<Fact> = one(root, true).into_iter().collect();
    rec(root, &mut out);
    out
}

fn kind_bits(k: FileKind) -> u32 {
    match k {
        FileKind::Regular => libc::S_IFREG,
        FileKind::Dir => libc::S_IFDIR,
        FileKind::Symlink => libc::S_IFLNK,
        FileKind::Fifo => libc::S_IFIFO,
        FileKind::Socket => libc::S_IFSOCK,
        FileKind::CharDev => libc::S_IFCHR,
        FileKind::BlockDev => libc::S_IFBLK,
    }
}

/// Whether a lie that fires on every call to `op` changes anything the observation pass looks at, given what the
/// tree holds. (Looking up the root never happens, `getattr` of it does.)
fn lie_effective(op: FaultOp, effect: &Effect, facts: &[Fact]) -> bool {
    let objects = || facts.iter().filter(|f| op != FaultOp::Lookup || !f.root);
    match effect {
        Effect::Stat(l) => objects().any(|f| match l {
            StatLie::Size(s) => f.kind != libc::S_IFDIR && f.size != *s,
            StatLie::Mode(m) => f.perm != *m,
            StatLie::Nlink(n) => f.nlink != *n,
            StatLie::Uid(_) | StatLie::Gid(_) | StatLie::MtimeShift(_) => true,
            StatLie::Type(t) => f.kind != (*t & libc::S_IFMT),
            StatLie::CtimeShift(_) | StatLie::Custom(_) => false,
        }),
        Effect::ReadlinkTarget(t) => facts.iter().any(|f| f.target.as_ref().is_some_and(|x| x != t)),
        Effect::XattrValue(v) => facts.iter().any(|f| f.xattrs.values().any(|x| x != v)),
        Effect::XattrListAdd(_) => true,
        Effect::XattrListDrop(n) => facts.iter().any(|f| f.xattrs.contains_key(&String::from_utf8_lossy(n).into_owned())),
        Effect::DropEntry(n) => {
            let n = String::from_utf8_lossy(n).into_owned();
            facts.iter().any(|f| f.entries.iter().any(|(x, _)| *x == n))
        }
        Effect::AddEntry(_) => true,
        Effect::ChangeEntryKind(n, k) => {
            let n = String::from_utf8_lossy(n).into_owned();
            facts.iter().any(|f| f.entries.iter().any(|(x, t)| *x == n && *t != kind_bits(*k)))
        }
        Effect::CorruptRead { offset } => facts.iter().any(|f| f.kind == libc::S_IFREG && f.size > *offset as u64),
        Effect::ShortRead(k) => facts.iter().any(|f| f.kind == libc::S_IFREG && f.size > *k as u64),
        _ => false,
    }
}

// =================================================================================================================
// Property 3: injected faults are detected
// =================================================================================================================

/// The backend calls (on the secondary) that are made on behalf of an operation and whose result is compared with
/// the primary's right there, at the level `level`. A failure injected into one of them is reported by the
/// operation itself.
///
fn direct(op: &Op, level: CheckLevel) -> Vec<FaultOp> {
    use FaultOp as F;
    let paranoid = level == CheckLevel::Paranoid;
    let mut v = vec![F::Lookup];
    // paranoid: a written file that is closed is read completely on both sides, and the listing of the parent of an
    // entry that was created, removed or renamed is compared
    let written_file = [F::Open, F::Pread];
    let listing = [F::Opendir, F::Readdir];
    match op {
        Op::Create { .. } => v.extend([F::Create, F::Flush]),
        Op::Write { .. } | Op::Append { .. } => v.extend([F::Open, F::Pwrite, F::Flush]),
        Op::Read { .. } => v.extend([F::Open, F::Pread, F::Flush]),
        Op::Setattr { .. } => v.extend([F::Chmod, F::Truncate, F::Utimens, F::Stat]),
        Op::Fallocate { .. } => v.extend([F::Open, F::Fallocate, F::Flush]),
        Op::CopyRange { .. } => v.extend([F::Open, F::CopyFileRange, F::Flush]),
        Op::Fsync { .. } => v.extend([F::Open, F::Fsync, F::Flush]),
        Op::Getattr { .. } => v.push(F::Stat),
        Op::Mkdir { .. } => v.push(F::Mkdir),
        Op::Rmdir { .. } => v.push(F::Rmdir),
        Op::Unlink { .. } => v.push(F::Unlink),
        Op::Rename { .. } => v.push(F::Rename),
        Op::Link { .. } => v.push(F::Link),
        Op::Symlink { .. } => v.push(F::Symlink),
        Op::Readlink { .. } => v.push(F::Readlink),
        Op::Readdir { .. } => v.extend(listing),
        Op::Setxattr { .. } => v.push(F::Setxattr),
        Op::Getxattr { .. } => v.push(F::Getxattr),
        Op::Listxattr { .. } => v.push(F::Listxattr),
        Op::Removexattr { .. } => v.push(F::Removexattr),
        Op::Open { .. } => v.push(F::Open),
        Op::Close { .. } => v.push(F::Flush),
        Op::FdWrite { .. } => v.push(F::Pwrite),
        Op::FdRead { .. } => v.push(F::Pread),
        Op::FdTruncate { .. } => v.extend([F::Truncate, F::Stat]),
        Op::FdCopy { .. } => v.push(F::CopyFileRange),
    }
    // thorough: every mutation is read back, and a read-back that fails on one side only is a difference
    if level >= CheckLevel::Thorough {
        match op {
            Op::Write { .. } | Op::Append { .. } | Op::FdWrite { .. } => v.extend([F::Open, F::Pread, F::Stat]),
            Op::Unlink { .. } | Op::Rmdir { .. } => v.extend([F::StatAt, F::Stat]),
            // (the identities before and after a rename are taken with `stat_at(..).ok()`: a name that cannot be
            // stat'ed counts as absent, which the comparison of before and after notices only if it matters)
            Op::Rename { .. } => v.push(F::Stat),
            Op::Setxattr { .. } | Op::Removexattr { .. } => v.push(F::Getxattr),
            Op::Symlink { .. } => v.push(F::Readlink),
            Op::Open { .. } => v.push(F::Stat),
            Op::Fallocate { .. } => v.extend([F::Stat, F::Open, F::Pread]),
            Op::CopyRange { .. } | Op::FdCopy { .. } => v.extend([F::Open, F::Pread]),
            _ => {}
        }
    }
    if paranoid {
        if matches!(
            op,
            Op::Create { .. }
                | Op::Write { .. }
                | Op::Append { .. }
                | Op::Fallocate { .. }
                | Op::CopyRange { .. }
                | Op::Close { .. }
        ) {
            v.extend(written_file);
        }
        if matches!(
            op,
            Op::Create { .. } | Op::Mkdir { .. } | Op::Rmdir { .. } | Op::Unlink { .. } | Op::Rename { .. } | Op::Link { .. } | Op::Symlink { .. }
        ) {
            v.extend(listing);
        }
    }
    v
}

/// Runs `ops` with `inj` on the secondary in Log mode, then the observation pass.
///
/// Rules, applied to every operation `i` in which the fault fired (`hit`):
///
/// * R1, at every level: if the fault makes a call fail (`Errno`, `AppliedThenErrno`) that is in `direct(op)`,
///   the operation itself reported a mismatch (the secondary's errno differs from the primary's).
/// * R2, at every level: a short `pwrite` (`ShortWrite(k)`) of more than `k` bytes is reported by the operation
///   (the secondary reports another length). R2b: a short `copy_file_range` is not (the engine completes it).
/// * R6, at every level: a phantom directory entry in a listing is reported by the operation that compared it.
/// * R3, from `thorough` on: if the trees were identical before the operation and differ after it, the operation
///   itself reported a mismatch (every mutation is read back: written data, attributes, names, identities,
///   xattrs, ranges). In `basic` mode a lost or corrupted write is only found by what looks at it later (R4).
///
/// R4, at the end, at every level: if the trees differ after the observation pass, at least one mismatch was
/// reported at some point. (Faults that do not change the secondary, but falsify what it says, cannot make the
/// trees differ; they have their own property.)
///
/// R5: the application's results are those of an engine without a fault.
fn check_detection(ops: &[Op], inj: &Inj, level: CheckLevel, par: bool, reference: &Reference) -> Result<(), TestCaseError> {
    let h = harness(level, MismatchMode::Log, par);
    let id = h.inject(inj.fault());
    let mut r = Runner::new(&h);
    let mut outs = Vec::new();
    let mut equal_before = true;
    for (i, op) in ops.iter().enumerate() {
        let (hits0, found0) = (h.fault.hits(id), found(&h));
        let out = r.exec(op);
        let hits1 = h.fault.hits(id);
        let detected = found(&h) > found0;
        // (until the fault has fired the trees are the same, see `no_false_positives`)
        let equal_after = hits1 == 0 || h.tree_diff().is_empty();
        if hits1 > hits0 {
            let call_made_here = direct(op, level).contains(&inj.op);
            match inj.effect {
                Effect::Errno(_) | Effect::AppliedThenErrno(_) if call_made_here => {
                    prop_assert!(
                        detected,
                        "R1: op #{i} {op:?}: the secondary's {:?} failed with {:?} and nothing was reported ({:?})",
                        inj.op,
                        inj.effect,
                        level
                    );
                }
                Effect::ShortWrite(k) if call_made_here && inj.op == FaultOp::Pwrite && out.bytes > k => {
                    prop_assert!(detected, "R2: op #{i} {op:?}: a short write ({k} of {} bytes) was not reported", out.bytes);
                }
                // R2b: the secondary may copy less than asked (file systems differ in how much copy_file_range copies
                // at once); the engine continues until it has copied what the primary did
                Effect::ShortWrite(k) if inj.op == FaultOp::CopyFileRange && k >= 1 => {
                    prop_assert!(
                        !detected && equal_after,
                        "R2b: op #{i} {op:?}: a short copy_file_range ({k} bytes) must be completed by the engine, \
                         not reported (reported: {detected}, trees equal: {equal_after})"
                    );
                }
                // R6: a listing with a phantom entry is never the primary's (a listing is compared by `readdir`, and
                // after every namespace operation in paranoid mode)
                Effect::AddEntry(_) => {
                    prop_assert!(detected, "R6: op #{i} {op:?}: a phantom directory entry was not reported ({level:?})");
                }
                _ => {}
            }
            if equal_before && !equal_after && level >= CheckLevel::Thorough {
                prop_assert!(
                    detected,
                    "R3: op #{i} {op:?} made the trees differ ({:?}) and the {:?} engine did not notice:\n  {}",
                    inj.effect,
                    level,
                    h.tree_diff().join("\n  ")
                );
            }
        }
        equal_before = equal_after;
        outs.push(out.text);
    }
    r.finish();
    observe(&h);
    let d = h.tree_diff();
    if !d.is_empty() {
        prop_assert!(
            found(&h) > 0,
            "R4: the trees differ after the observation pass and nothing was reported ({:?}):\n  {}",
            level,
            d.join("\n  ")
        );
    }
    first_difference(&reference.outs, &outs, ops, "R5: reference run vs faulted run")
}

/// The results and the trees of the same operations without any fault.
struct Reference {
    outs: Vec<String>,
    env: Env,
}

fn reference_run(ops: &[Op], level: CheckLevel, mode: MismatchMode, par: bool) -> Reference {
    let env = harness(level, mode, par);
    let outs = run_ops(&env, ops);
    observe(&env);
    Reference { outs, env }
}

// =================================================================================================================
// Property 4: lies
// =================================================================================================================

fn check_lie(ops: &[Op], op: FaultOp, effect: &Effect, level: CheckLevel, par: bool, reference: &Reference) -> Result<(), TestCaseError> {
    let h = harness(level, MismatchMode::Log, par);
    h.inject(Fault::new(op, effect.clone()));
    let outs = run_ops(&h, ops);
    observe(&h);
    let d = h.tree_diff();
    prop_assert!(d.is_empty(), "a lie must not change the trees:\n  {}", d.join("\n  "));
    if lie_effective(op, effect, &facts(h.p_root())) {
        prop_assert!(found(&h) > 0, "{op:?} {effect:?} is effective on this tree ({level:?}) but nothing was reported");
    }
    first_difference(&reference.outs, &outs, ops, "reference run vs run with a lying secondary")
}

// =================================================================================================================
// Property 2: benign faults
// =================================================================================================================

fn check_benign(ops: &[Op], inj: &Inj, level: CheckLevel, par: bool, reference: &Reference) -> Result<(), TestCaseError> {
    let h = harness(level, MismatchMode::Log, par);
    h.inject(inj.fault());
    let outs = run_ops(&h, ops);
    observe(&h);
    assert_healthy(&h, level)?;
    first_difference(&reference.outs, &outs, ops, "reference run vs run with a slow secondary")
}

// =================================================================================================================
// Property 5: resync converges
// =================================================================================================================

/// A snapshot of everything about a tree that a repair must not change on the primary: inode, mode, owner, link
/// count, size, mtime and ctime to the nanosecond, content, link target, extended attributes.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
        for ent in rd.flatten() {
            let r = rel.join(ent.file_name());
            let full = root.join(&r);
            // (entries of a directory without search permission cannot be looked at)
            let Ok(md) = std::fs::symlink_metadata(&full) else { continue };
            let mut d = format!(
                "ino={} mode={:o} uid={} gid={} nlink={} size={} mtime={}.{:09} ctime={}.{:09}",
                md.ino(),
                md.mode(),
                md.uid(),
                md.gid(),
                md.nlink(),
                md.size(),
                md.mtime(),
                md.mtime_nsec(),
                md.ctime(),
                md.ctime_nsec()
            );
            let ft = md.file_type();
            if ft.is_symlink() {
                d += &format!(" -> {:?}", std::fs::read_link(&full).ok());
            } else if ft.is_file() {
                d += &format!(" xxh3={:016x}", std::fs::read(&full).ok().map(|c| xxhash_rust::xxh3::xxh3_64(&c)).unwrap_or(0));
            }
            if !ft.is_symlink() {
                for n in raw_listxattr(&full).unwrap_or_default() {
                    d += &format!(" {n}={:?}", raw_getxattr(&full, &n).ok());
                }
            }
            out.insert(format!("/{}", r.display()), d);
            if ft.is_dir() {
                walk(root, &r, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out);
    out
}

fn repair_counters(h: &Harness) -> [u64; 5] {
    let s = &h.stats;
    [
        s.mismatches.load(Relaxed),
        s.repeats.load(Relaxed),
        s.resyncs.load(Relaxed),
        s.resync_failures.load(Relaxed),
        s.resync_giveups.load(Relaxed),
    ]
}

fn check_resync(ops: &[Op], inj: &Inj, level: CheckLevel, par: bool, reference: &Reference) -> Result<(), TestCaseError> {
    let h = harness(level, MismatchMode::Resync, par);
    h.inject(inj.fault());
    let outs = run_ops(&h, ops);
    h.clear_faults();
    // From here on only reads reach the primary: the repairs must not touch it either.
    let before = snapshot(h.p_root());
    let mut quiet = false;
    for _ in 0..8 {
        let c0 = repair_counters(&h);
        observe(&h);
        if repair_counters(&h) == c0 {
            quiet = true;
            break;
        }
    }
    let [_, _, _, failures, giveups] = repair_counters(&h);
    prop_assert!(quiet, "the observation pass kept finding differences:\n  {}\n{:?}", h.describe_mismatches(), h.tree_diff());
    prop_assert_eq!((failures, giveups), (0, 0), "a repair failed or was given up:\n  {}", h.describe_mismatches());
    let d = h.tree_diff();
    prop_assert!(d.is_empty(), "the trees still differ after the repairs ({:?}):\n  {}\n  {}", level, d.join("\n  "), h.describe_mismatches());
    prop_assert!(snapshot(h.p_root()) == before, "a repair changed the primary");
    first_difference(&reference.outs, &outs, ops, "reference run vs faulted run")?;
    // (the reference may have been made a while ago: times set to "now" differ by that much)
    let opts = TreeOpts { mtime: Some(Duration::from_secs(600)), ..TreeOpts::default() };
    let d = tree_diff(h.p_root(), reference.env.p_root(), &opts);
    prop_assert!(d.is_empty(), "the primary differs from the primary of a run without a fault:\n  {}", d.join("\n  "));
    Ok(())
}

// =================================================================================================================
// The tests
// =================================================================================================================

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn config(default_cases: u32) -> PtConfig {
    PtConfig {
        cases: cases(default_cases),
        max_shrink_iters: 3000,
        max_shrink_time: 180_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource("proptest-regressions"))),
        ..PtConfig::default()
    }
}

/// Operations that make the secondary call `op`, to be mixed into a sequence so that a fault on `op` has something to
/// act on (a uniformly random sequence meets most of the 75 faults of the tables rarely).
fn focus_for(op: FaultOp) -> BoxedStrategy<Op> {
    use FaultOp as F;
    match op {
        F::Setxattr | F::Getxattr | F::Listxattr | F::Removexattr => xattr_ops(),
        F::Mkdir | F::Rmdir | F::Unlink | F::Rename | F::Link | F::Symlink | F::Readlink | F::Readdir | F::Opendir
        | F::Lookup | F::StatAt | F::Stat => namespace_ops(),
        _ => prop_oneof![data_ops(), handle_ops()].boxed(),
    }
}

/// A sequence in which half of the operations are those that fault `op` acts on.
fn focused_ops(op: FaultOp) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(prop_oneof![1 => focus_for(op), 1 => op_s()], 1..60)
}

fn fault_and_ops(triggers: BoxedStrategy<Trigger>) -> impl Strategy<Value = (Inj, Vec<Op>)> {
    mutating_s(triggers).prop_flat_map(|inj| {
        let ops = focused_ops(inj.op);
        (Just(inj), ops)
    })
}

fn lie_and_ops() -> impl Strategy<Value = ((FaultOp, Effect), Vec<Op>)> {
    prop::sample::select(lie_table()).prop_flat_map(|lie| {
        let ops = focused_ops(lie.0);
        (Just(lie), ops)
    })
}

proptest! {
    #![proptest_config(config(32))]

    /// Healthy trees: no mismatches at any level, identical trees, results independent of the level.
    #[test]
    fn no_false_positives(ops in ops_s()) {
        check_healthy(&ops)?;
    }

    /// Faults that only delay the secondary change nothing.
    #[test]
    fn benign_faults_are_invisible(
        ops in ops_s(),
        level in level_s(),
        par in any::<bool>(),
        op in prop::sample::select(vec![
            FaultOp::Lookup, FaultOp::Stat, FaultOp::Open, FaultOp::Create, FaultOp::Pread, FaultOp::Pwrite,
            FaultOp::Mkdir, FaultOp::Unlink, FaultOp::Rename, FaultOp::Readdir, FaultOp::Setxattr,
        ]),
        ms in 1u64..4,
        trigger in prop_oneof![Just(Trigger::Once), (2u64..6).prop_map(Trigger::Nth), (3u64..8).prop_map(Trigger::Every)],
    ) {
        let r = reference_run(&ops, level, MismatchMode::Log, par);
        check_benign(&ops, &Inj { op, effect: Effect::Delay(Duration::from_millis(ms)), trigger }, level, par, &r)?;
    }

    /// After one fault, a difference between the trees is always reported (see `check_detection`).
    #[test]
    fn injected_faults_are_detected(
        (inj, ops) in fault_and_ops(any_trigger()),
        level in level_s(),
        par in any::<bool>(),
    ) {
        let r = reference_run(&ops, level, MismatchMode::Log, par);
        check_detection(&ops, &inj, level, par, &r)?;
    }

    /// Lies about attributes, data, listings, link targets and xattrs on every call are reported.
    #[test]
    fn lies_are_detected(
        (lie, ops) in lie_and_ops(),
        level in level_s(),
        par in any::<bool>(),
    ) {
        let r = reference_run(&ops, level, MismatchMode::Log, par);
        check_lie(&ops, lie.0, &lie.1, level, par, &r)?;
    }

    /// After a transient fault the secondary is repaired from the primary, which stays what it was.
    #[test]
    fn resync_converges(
        (inj, ops) in fault_and_ops(transient_trigger()),
        level in level_s(),
        par in any::<bool>(),
    ) {
        let r = reference_run(&ops, level, MismatchMode::Resync, par);
        check_resync(&ops, &inj, level, par, &r)?;
    }
}

proptest! {
    // Concurrent runs are not reproducible: no persistence, a short shrink.
    #![proptest_config(PtConfig {
        cases: cases(24),
        max_shrink_iters: 200,
        failure_persistence: None,
        ..PtConfig::default()
    })]

    /// 2-4 threads, each with its own sequence and its own open files, on one engine.
    #[test]
    fn no_false_positives_concurrent(lanes in lanes_s(), level in level_s()) {
        check_healthy_concurrent(&lanes, level)?;
    }
}

// =================================================================================================================
// Every fault of the tables on a fixed workload
// =================================================================================================================
//
// Random sequences meet the rarer faults rarely. These tests run every (method, effect) pair of the tables, at
// every level, on one workload that uses every operation (several times, so that "the n-th call" triggers fire).

fn pp(dir: u8, name: u8) -> P {
    P { dir, name }
}

/// Uses every operation kind at least twice, incl. open files that outlive their name and renames over files.
fn canonical() -> Vec<Op> {
    let (f0, f1, f2, f3, f4, f5, f6) = (pp(0, 0), pp(1, 1), pp(2, 2), pp(0, 3), pp(0, 4), pp(0, 5), pp(2, 6));
    let nested = pp(1, 9); // /d0/d1
    vec![
        Op::Create { p: f0, excl: true },
        Op::Write { p: f0, off: 0, len: 300, seed: 1 },
        Op::Write { p: f0, off: 200, len: 300, seed: 2 },
        Op::Append { p: f0, len: 50, seed: 3 },
        Op::Append { p: f0, len: 20, seed: 4 },
        Op::Read { p: f0, off: 100, len: 200 },
        Op::Read { p: f0, off: 0, len: 900 },
        Op::Setattr { p: f0, mode: Some(0o600), size: Some(400), mtime: Some(Mt::At(1)) },
        Op::Setattr { p: f0, mode: Some(0o644), size: Some(700), mtime: None },
        Op::Setattr { p: pp(0, 8), mode: None, size: None, mtime: Some(Mt::At(2)) },
        Op::Getattr { p: f0 },
        Op::Mkdir { p: nested },
        Op::Mkdir { p: pp(2, 8) },
        Op::Create { p: f1, excl: false },
        Op::Write { p: f1, off: 0, len: 100, seed: 5 },
        Op::Symlink { p: f5, target: 0 },
        Op::Symlink { p: pp(0, 7), target: 5 },
        Op::Readlink { p: f5 },
        Op::Readlink { p: pp(0, 7) },
        Op::Link { from: f0, to: f2 },
        Op::Link { from: f0, to: pp(3, 1) },
        Op::Getattr { p: f2 },
        Op::Setxattr { p: f0, name: 0, len: 20, seed: 1, flag: XFlag::Any },
        Op::Setxattr { p: f0, name: 1, len: 5, seed: 2, flag: XFlag::Create },
        Op::Setxattr { p: nested, name: 2, len: 7, seed: 3, flag: XFlag::Any },
        Op::Getxattr { p: f0, name: 0 },
        Op::Getxattr { p: f0, name: 1 },
        Op::Listxattr { p: f0 },
        Op::Listxattr { p: nested },
        Op::Fallocate { p: f0, mode: FMode::Alloc, off: 0, len: 1000 },
        Op::Fallocate { p: f0, mode: FMode::PunchHole, off: 100, len: 200 },
        Op::Fallocate { p: f0, mode: FMode::ZeroRange, off: 50, len: 100 },
        Op::Fallocate { p: f0, mode: FMode::KeepSize, off: 0, len: 2000 },
        Op::Removexattr { p: f0, name: 0 },
        Op::Removexattr { p: f0, name: 1 },
        Op::Create { p: f3, excl: true },
        Op::CopyRange { src: f0, dst: f3, off_in: 10, off_out: 20, len: 150 },
        Op::CopyRange { src: f0, dst: f3, off_in: 0, off_out: 0, len: 300 },
        Op::Fsync { p: f0, data: false },
        Op::Fsync { p: f0, data: true },
        Op::Open { slot: 0, p: f0, mode: OMode::ReadWrite },
        Op::FdWrite { slot: 0, off: 10, len: 60, seed: 6 },
        Op::FdWrite { slot: 0, off: 500, len: 60, seed: 7 },
        Op::FdRead { slot: 0, off: 0, len: 100 },
        Op::FdRead { slot: 0, off: 50, len: 100 },
        Op::FdTruncate { slot: 0, size: 250 },
        Op::FdTruncate { slot: 0, size: 600 },
        Op::Open { slot: 1, p: f3, mode: OMode::ReadWrite },
        Op::FdCopy { src: 0, dst: 1, off_in: 5, off_out: 5, len: 100 },
        Op::FdCopy { src: 0, dst: 1, off_in: 0, off_out: 0, len: 50 },
        Op::Rename { from: f3, to: f4, flag: RFlag::Plain },
        Op::Rename { from: f0, to: f4, flag: RFlag::Exchange },
        Op::Rename { from: f4, to: f6, flag: RFlag::NoReplace },
        Op::Rename { from: f1, to: f6, flag: RFlag::Plain },
        Op::Open { slot: 2, p: f6, mode: OMode::Truncate },
        Op::FdWrite { slot: 2, off: 0, len: 80, seed: 8 },
        Op::Unlink { p: f6 },
        Op::FdWrite { slot: 2, off: 80, len: 80, seed: 9 },
        Op::Close { slot: 2 },
        Op::Close { slot: 1 },
        Op::Readdir { dir: 0 },
        Op::Readdir { dir: 1 },
        Op::Readdir { dir: 2 },
        Op::Getattr { p: pp(0, 8) },
        Op::Unlink { p: f5 },
        Op::Unlink { p: pp(3, 1) },
        Op::Rmdir { p: nested },
        Op::Rename { from: pp(0, 8), to: pp(2, 9), flag: RFlag::Plain },
        Op::Rmdir { p: pp(2, 9) },
        Op::Setxattr { p: f2, name: 0, len: 9, seed: 4, flag: XFlag::Any },
        Op::Setxattr { p: f2, name: 2, len: 3, seed: 5, flag: XFlag::Any },
        Op::Setxattr { p: pp(2, 6), name: 1, len: 4, seed: 6, flag: XFlag::Any },
        Op::Read { p: f4, off: 0, len: 900 },
        Op::Read { p: f2, off: 0, len: 900 },
        Op::Getattr { p: f4 },
        Op::Readdir { dir: 0 },
        Op::Readdir { dir: 2 },
    ]
}

/// `XCHECKFS_PT_ONLY=ShortWrite cargo test --test engine_proptest canonical` restricts the canonical tests to the
/// faults whose description contains the text (for debugging one fault with `XCHECKFS_TEST_LOG`).
fn selected(op: FaultOp, effect: &Effect) -> bool {
    std::env::var("XCHECKFS_PT_ONLY").map(|f| format!("{op:?} {effect:?}").contains(&f)).unwrap_or(true)
}

#[test]
fn canonical_workload_is_healthy() {
    check_healthy(&canonical()).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn every_mutating_fault_is_detected_on_the_canonical_workload() {
    let ops = canonical();
    let mut failures = Vec::new();
    for level in LEVELS {
        let reference = reference_run(&ops, level, MismatchMode::Log, false);
        for (op, effect) in mutating_table().into_iter().filter(|(o, e)| selected(*o, e)) {
            // (later calls: the verification steps of the higher levels make calls of their own)
            let mut triggers = vec![Trigger::Once];
            match level {
                CheckLevel::Basic => {}
                CheckLevel::Thorough => triggers.extend([Trigger::Nth(2), Trigger::Always]),
                CheckLevel::Paranoid => triggers.extend([Trigger::Nth(2), Trigger::Nth(3)]),
            }
            for trigger in triggers {
                let inj = Inj { op, effect: effect.clone(), trigger };
                if let Err(e) = check_detection(&ops, &inj, level, false, &reference) {
                    failures.push(format!("{inj:?} at {level:?}: {e}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} faults:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn every_lie_is_detected_on_the_canonical_workload() {
    let ops = canonical();
    let mut failures = Vec::new();
    // (thorough adds nothing for lies)
    for level in [CheckLevel::Basic, CheckLevel::Paranoid] {
        let reference = reference_run(&ops, level, MismatchMode::Log, false);
        for (op, effect) in lie_table().into_iter().filter(|(o, e)| selected(*o, e)) {
            if let Err(e) = check_lie(&ops, op, &effect, level, false, &reference) {
                failures.push(format!("{op:?} {effect:?} at {level:?}: {e}"));
            }
        }
    }
    assert!(failures.is_empty(), "{} lies:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn every_transient_fault_is_repaired_on_the_canonical_workload() {
    let ops = canonical();
    let mut failures = Vec::new();
    for level in LEVELS {
        let reference = reference_run(&ops, level, MismatchMode::Resync, false);
        for (op, effect) in mutating_table().into_iter().filter(|(o, e)| selected(*o, e)) {
            let mut triggers = vec![Trigger::Once];
            if level == CheckLevel::Basic {
                triggers.push(Trigger::Nth(2));
            }
            for trigger in triggers {
                let inj = Inj { op, effect: effect.clone(), trigger };
                if let Err(e) = check_resync(&ops, &inj, level, false, &reference) {
                    failures.push(format!("{inj:?} at {level:?}: {e}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} faults:\n{}", failures.len(), failures.join("\n"));
}

/// Regression: a `RENAME_EXCHANGE` that the secondary did not carry out, with hard-linked files, was repaired name
/// by name, the first one in place: the content of the other names of its inode was overwritten and the repair
/// failed ("still differs"). A name whose inode has other names on the secondary only is replaced instead.
#[test]
fn regression_failed_exchange_of_hard_linked_files_is_repaired() {
    let (f0, f4) = (pp(0, 0), pp(0, 4));
    let setup = |linked: P| {
        vec![
            Op::Create { p: f0, excl: true },
            Op::Write { p: f0, off: 0, len: 300, seed: 1 },
            Op::Create { p: f4, excl: true },
            Op::Link { from: linked, to: pp(2, 2) },
            Op::Rename { from: f0, to: f4, flag: RFlag::Exchange },
        ]
    };
    for linked in [f0, f4] {
        for level in LEVELS {
            let inj = Inj { op: FaultOp::Rename, effect: Effect::Errno(libc::EIO), trigger: Trigger::Once };
            check_resync(&setup(linked), &inj, level, true, &reference_run(&setup(linked), level, MismatchMode::Resync, true)).unwrap_or_else(|e| panic!("{linked:?} {level:?}: {e}"));
        }
    }
}

// =================================================================================================================
// Regressions: the cases the properties found, as plain tests (the shrunk sequences are in these)
// =================================================================================================================

/// The repair of a read-only file (mode 0444) failed with EACCES when the engine does not run as root: the
/// secondary was opened for writing. (Found by `resync_converges`: `Create /d0/f1; Setattr /d0/f1 mode 0555`
/// with the secondary's chmod reporting EIO after it was done.)
#[test]
fn regression_resync_repairs_read_only_files() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::builder().mode(MismatchMode::Resync).build();
    h.write_file("/ro", &pattern(1, 3000));
    h.chmod("/ro", 0o444);
    // behind the engine's back: the secondary's copy is damaged
    let s = h.s_path("/ro");
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&s, pattern(2, 3000)).unwrap();
    std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert_eq!(h.read_file("/ro"), pattern(1, 3000));
    assert!(h.stats.resyncs.load(Relaxed) > 0, "the damage was not repaired:\n  {}", h.describe_mismatches());
    assert_eq!(h.stats.resync_failures.load(Relaxed), 0, "{}", h.describe_mismatches());
    h.assert_trees_equal();
    assert_eq!(std::fs::metadata(&s).unwrap().permissions().mode() & 0o7777, 0o444, "the mode was not restored");
}

/// Listing a directory needs read permission, not search permission (found by `resync_converges`: `Setattr /d0
/// mode 0644` with the chmod failing on the secondary: the repair could not list the directory).
#[test]
fn regression_a_directory_can_be_listed_without_search_permission() {
    let h = Harness::basic();
    h.mkdir("/d");
    h.write_file("/d/f", b"x");
    h.chmod("/d", 0o600); // rw-: no search permission
    if !is_root() {
        assert_eq!(list_dir(&h, "/d").unwrap(), vec!["f"]);
        h.chmod("/d", 0o300); // -wx: no read permission
        assert_eq!(list_dir(&h, "/d").unwrap_err(), libc::EACCES);
    }
    h.chmod("/d", 0o755);
    h.assert_no_mismatches();
    h.assert_trees_equal();
}

/// Read-backs and whole-file / whole-listing comparisons that fail on the secondary only are a difference. They
/// were skipped silently. (Found by `injected_faults_are_detected`: the rules R1 for `Open`/`Pread`/`Opendir`/
/// `Readdir`/`Stat`.)
#[test]
fn regression_a_secondary_that_cannot_be_read_back_is_reported() {
    use xcheckfs::policy::MismatchKind as K;
    // paranoid: closing a written file reads it completely on both sides
    for op in [FaultOp::Open, FaultOp::Pread] {
        let h = Harness::paranoid();
        let f = h.create("/f");
        h.pwrite(f, 0, &pattern(1, 100));
        h.inject(Fault::new(op, Effect::Errno(libc::EIO)).once());
        h.mark();
        h.close(f);
        h.expect_mismatch(K::Result, None);
    }
    // paranoid: after a namespace change the parent's listing is compared
    for op in [FaultOp::Opendir, FaultOp::Readdir] {
        let h = Harness::paranoid();
        h.inject(Fault::new(op, Effect::Errno(libc::EIO)).once());
        h.mkdir("/d");
        h.expect_mismatch(K::Result, None);
    }
    // the stat that ends a setattr
    let h = Harness::basic();
    h.mkdir("/d");
    h.write_file("/d/f", b"x");
    h.lookup("/d/f");
    h.inject(Fault::new(FaultOp::Stat, Effect::Errno(libc::EIO)).once());
    h.chmod("/d/f", 0o600);
    h.expect_mismatch(K::Result, None);
    // thorough: the stat of an object after unlink / rename / fallocate, the read-back of copy_file_range
    for (what, act) in [
        ("unlink", Box::new(|h: &Harness| h.unlink("/d/g")) as Box<dyn Fn(&Harness)>),
        ("rename", Box::new(|h: &Harness| h.rename("/d/f", "/d/g"))),
        ("fallocate", Box::new(|h: &Harness| {
            let f = h.open("/d/f", libc::O_RDWR);
            h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 5000, 0).unwrap();
        })),
        ("copy_file_range", Box::new(|h: &Harness| {
            let (a, b) = (h.open("/d/f", libc::O_RDONLY), h.open("/d/g", libc::O_RDWR));
            h.engine.copy_file_range(&h.ctx, a.fh, 0, b.fh, 0, 100, 0).unwrap();
        })),
    ] {
        // (in a directory: resolving a name in the root would `getattr` it, which is a `Stat`, too)
        let h = Harness::thorough();
        h.mkdir("/d");
        h.write_file("/d/f", &pattern(1, 200));
        h.write_file("/d/g", &pattern(2, 200));
        h.lookup("/d/f");
        h.lookup("/d/g");
        let fault = if what == "copy_file_range" { FaultOp::Pread } else { FaultOp::Stat };
        h.inject(Fault::new(fault, Effect::Errno(libc::EIO)).once());
        h.mark();
        act(&h);
        h.expect_mismatch(K::Result, None);
    }
}
