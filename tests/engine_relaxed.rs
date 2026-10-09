//! Relaxed serialization (`Serialization::Relaxed`, the default): in-place data operations on disjoint byte ranges
//! of one file run concurrently on both file systems. These tests prove that
//!
//! 1. the secondary really sees the concurrency (and only where it is allowed): `FaultBackend`'s in-flight recorder
//!    plus `Effect::Gate` hold calls inside the backends so that "two calls in flight at once" is observed, not
//!    inferred (`concurrency_*`, `serializes_*`);
//! 2. there are no false positives: gates force every interleaving of the halves of two disjoint writers with a
//!    racing getattr / lookup / read / fsync, at every check level (`interleavings_*`, `blocking_*`, `eof_*`,
//!    `copy_file_range_*`, `punch_hole_*`, ...), and the existing stress tests run with both serializations;
//! 3. there are no false negatives: faults on one of two concurrent writers are detected (`detects_*`).
//!
//! Nothing here sleeps to synchronise. Gates are event-driven; the only sleeps are the short grace periods of the
//! negative checks ("this call must NOT have reached the file system": a bounded wait that can only make an
//! assertion weaker, never make it fail), and one pause that separates timestamps in time.
//!
//! Method of the interleaving tests: every half-call (primary/secondary x writer A/writer B/observer) is held in a
//! gate of its own, all are in flight together, and the gates are opened one by one in a chosen order, each
//! waiting for the released call to finish. File times are the thing that differs between the halves: the file's
//! mtime is set 1000 s into the past first, so a stat that sees one side before and the other after a write
//! differs by 1000 s, far beyond any tolerance. Only the engine's racy-stat rule can keep such a stat quiet.

mod common;

use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use common::*;
use xcheckfs::backend::fault::{obj_of, Effect, Fault, FaultBackend, FaultId, FaultOp, Gate, ObjId, StatLie};
use xcheckfs::backend::TimeSpec;
use xcheckfs::config::{CheckLevel, MismatchMode, Serialization};
use xcheckfs::engine::{SetAttr, ROOT_ID};
use xcheckfs::policy::MismatchKind as K;
use xcheckfs::stats::OpKind;

const LEVELS: [CheckLevel; 3] = [CheckLevel::Basic, CheckLevel::Thorough, CheckLevel::Paranoid];
const SERS: [Serialization; 2] = [Serialization::Strict, Serialization::Relaxed];
const WAIT: Duration = Duration::from_secs(20);
/// How long a call that must NOT happen gets to happen.
const GRACE: Duration = Duration::from_millis(150);

// =================================================================================================================
// Rig
// =================================================================================================================

/// A harness over two trees with the in-flight recorders on, and the gates that were added (opened when the rig
/// goes away, so that a failing assertion cannot leave threads blocked).
struct Rig {
    h: Arc<Harness>,
    gates: parking_lot::Mutex<Vec<Arc<Gate>>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.open_all();
    }
}

fn rig_with(level: CheckLevel, ser: Serialization, tweak: impl FnOnce(&mut xcheckfs::config::EngineConfig) + 'static) -> Rig {
    if let Ok(f) = std::env::var("XCHECKFS_TEST_LOG") {
        let _ = tracing_subscriber::fmt().with_env_filter(f).with_test_writer().try_init();
    }
    let h = Harness::builder()
        .level(level)
        .mode(MismatchMode::Log)
        .config(move |c| {
            c.serialize = ser;
            tweak(c);
        })
        // (the tolerance can be tiny: the roots must start out with equal times, wherever the trees live)
        .build_with(|p, s| {
            let t = std::time::SystemTime::now();
            for r in [p, s] {
                std::fs::File::open(r).unwrap().set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t)).unwrap();
            }
        });
    h.fault.record(true);
    h.pfault.record(true);
    Rig { h: Arc::new(h), gates: parking_lot::Mutex::new(Vec::new()) }
}

fn rig(level: CheckLevel, ser: Serialization) -> Rig {
    rig_with(level, ser, |_| {})
}

impl Rig {
    /// A gate on every `op` call on `path` of the secondary / primary.
    fn sgate(&self, op: FaultOp, path: &str) -> Arc<Gate> {
        self.gate_on(&self.h.fault, op, path, None)
    }
    fn pgate(&self, op: FaultOp, path: &str) -> Arc<Gate> {
        self.gate_on(&self.h.pfault, op, path, None)
    }
    fn gate_on(&self, be: &FaultBackend, op: FaultOp, path: &str, nth: Option<u64>) -> Arc<Gate> {
        let g = Gate::new();
        let mut f = Fault::new(op, Effect::Gate(g.clone())).path(path);
        if let Some(n) = nth {
            f = f.nth(n);
        }
        be.add(f);
        self.gates.lock().push(g.clone());
        g
    }
    fn open_all(&self) {
        for g in self.gates.lock().iter() {
            g.open();
        }
    }
    #[track_caller]
    fn assert_no_gate_timeouts(&self) {
        for g in self.gates.lock().iter() {
            assert!(!g.timed_out(), "a call gave up waiting for a gate: {g:?}");
        }
    }
    /// Every gate open, nothing timed out, zero mismatches, identical trees.
    #[track_caller]
    fn finish(&self) {
        self.open_all();
        self.assert_no_gate_timeouts();
        self.h.assert_no_mismatches();
        self.h.assert_trees_equal();
    }
    fn sobj(&self, path: &str) -> ObjId {
        obj_of(&self.h.s_path(path))
    }
    fn pobj(&self, path: &str) -> ObjId {
        obj_of(&self.h.p_path(path))
    }
    /// Largest number of `op` calls on `path` in flight at once, per side.
    fn smax(&self, op: FaultOp, path: &str) -> u32 {
        self.h.fault.max_inflight(op, self.sobj(path))
    }
    fn pmax(&self, op: FaultOp, path: &str) -> u32 {
        self.h.pfault.max_inflight(op, self.pobj(path))
    }
    fn file(&self, path: &str, size: usize) -> Fh {
        self.h.write_file(path, &vec![0u8; size]);
        self.h.open(path, libc::O_RDWR)
    }
    fn stat(&self, name: &'static str) -> u64 {
        let s = &self.h.stats;
        match name {
            "concurrent" => s.concurrent_data_ops.load(Relaxed),
            "waits" => s.range_waits.load(Relaxed),
            "skipped" => s.attr_time_skipped.load(Relaxed),
            _ => unreachable!(),
        }
    }
    fn spawn_write(&self, f: Fh, off: u64, len: usize, seed: u64) -> JoinHandle<()> {
        let h = self.h.clone();
        std::thread::spawn(move || h.pwrite(f, off, &pattern(seed, len)))
    }
    fn spawn_read(&self, f: Fh, off: u64, len: usize) -> JoinHandle<Vec<u8>> {
        let h = self.h.clone();
        std::thread::spawn(move || h.pread(f, off, len))
    }
}

#[track_caller]
fn arrived(g: &Gate, n: u64) {
    assert!(g.wait_arrived(n, WAIT), "{n} call(s) never reached the gate: {g:?}");
}

#[track_caller]
fn done(g: &Gate, n: u64) {
    assert!(g.wait_done(n, WAIT), "{n} call(s) never finished at the gate: {g:?}");
}

/// A negative check: nothing more reaches the gate within the grace period.
#[track_caller]
fn stays_at(g: &Gate, n: u64) {
    assert!(!g.wait_arrived(n + 1, GRACE), "a call that must wait for the others reached the file system: {g:?}");
    assert_eq!(g.arrived(), n);
}

fn join<T>(t: JoinHandle<T>) -> T {
    t.join().expect("worker thread panicked")
}

// =================================================================================================================
// 1. The secondary really sees the concurrency
// =================================================================================================================

/// N threads, N disjoint in-place pwrites to one file, each held inside both file systems. Relaxed: all N are in
/// flight at once on both sides; strict: one after the other.
fn writers_in_flight(level: CheckLevel, ser: Serialization, n: u64) -> (u32, u32, u64) {
    let r = rig(level, ser);
    let files: Vec<Fh> = {
        r.h.write_file("/f", &vec![0u8; 1 << 20]);
        (0..n).map(|_| r.h.open("/f", libc::O_RDWR)).collect()
    };
    let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
    let ts: Vec<_> = files.iter().enumerate().map(|(i, f)| r.spawn_write(*f, i as u64 * 8192, 4096, 10 + i as u64)).collect();
    arrived(&gs, 1);
    arrived(&gp, 1);
    match ser {
        Serialization::Relaxed => {
            arrived(&gs, n);
            arrived(&gp, n);
        }
        Serialization::Strict => {
            // the others wait for the object: they never get to the file systems while the first is held
            if n > 1 {
                stays_at(&gs, 1);
                stays_at(&gp, 1);
            }
        }
    }
    let (smax, pmax) = (r.smax(FaultOp::Pwrite, "/f"), r.pmax(FaultOp::Pwrite, "/f"));
    r.open_all();
    for t in ts {
        join(t);
    }
    let concurrent = r.stat("concurrent");
    for f in files {
        r.h.close(f);
    }
    r.finish();
    // (a held write ran through the file systems exactly once per side)
    assert_eq!(gs.arrived(), n);
    assert_eq!(gp.arrived(), n);
    eprintln!("{level:?} {ser:?} {n} writers: secondary max in flight {smax}, primary {pmax}, concurrent_data_ops {concurrent}");
    (smax, pmax, concurrent)
}

#[test]
fn concurrency_two_disjoint_writers() {
    for level in LEVELS {
        let (s, p, c) = writers_in_flight(level, Serialization::Relaxed, 2);
        assert_eq!((s, p), (2, 2), "{level:?}");
        assert_eq!(c, 1, "{level:?}: concurrent_data_ops");
        let (s, p, c) = writers_in_flight(level, Serialization::Strict, 2);
        assert_eq!((s, p, c), (1, 1, 0), "{level:?}");
    }
}

#[test]
fn concurrency_many_disjoint_writers() {
    for level in LEVELS {
        let (s, p, c) = writers_in_flight(level, Serialization::Relaxed, 8);
        assert_eq!((s, p), (8, 8), "{level:?}");
        assert_eq!(c, 7, "{level:?}: concurrent_data_ops");
        let (s, p, c) = writers_in_flight(level, Serialization::Strict, 8);
        assert_eq!((s, p, c), (1, 1, 0), "{level:?}");
    }
}

/// A read and a write on disjoint ranges are in the file systems together (relaxed) or not (strict); two reads are
/// together in both modes. `read_first`: which of the two is held before the other starts.
#[test]
fn concurrency_read_and_write_disjoint() {
    for ser in SERS {
        for read_first in [false, true] {
            for level in LEVELS {
                let r = rig(level, ser);
                let f1 = r.file("/f", 1 << 20);
                let f2 = r.h.open("/f", libc::O_RDWR);
                let (gw, gr) = (r.sgate(FaultOp::Pwrite, "/f"), r.sgate(FaultOp::Pread, "/f"));
                let (pw, pr) = (r.pgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pread, "/f"));
                if read_first {
                    let first = r.spawn_read(f1, 100_000, 4096);
                    arrived(&gr, 1);
                    arrived(&pr, 1);
                    let second = r.spawn_write(f2, 0, 4096, 1);
                    match ser {
                        Serialization::Relaxed => {
                            arrived(&gw, 1);
                            arrived(&pw, 1);
                        }
                        Serialization::Strict => stays_at(&gw, 0),
                    }
                    r.open_all();
                    join(second);
                    join(first);
                } else {
                    let t = r.spawn_write(f2, 0, 4096, 1);
                    arrived(&gw, 1);
                    arrived(&pw, 1);
                    let rd = r.spawn_read(f1, 100_000, 4096);
                    match ser {
                        Serialization::Relaxed => {
                            arrived(&gr, 1);
                            arrived(&pr, 1);
                        }
                        Serialization::Strict => stays_at(&gr, 0),
                    }
                    r.open_all();
                    join(t);
                    assert_eq!(join(rd), vec![0u8; 4096]);
                }
                r.h.close(f1);
                r.h.close(f2);
                r.finish();
            }
        }
    }
}

#[test]
fn concurrency_two_readers_overlap_in_both_modes() {
    for ser in SERS {
        let r = rig(CheckLevel::Basic, ser);
        let f1 = r.file("/f", 1 << 16);
        let f2 = r.h.open("/f", libc::O_RDONLY);
        let gr = r.sgate(FaultOp::Pread, "/f");
        let a = r.spawn_read(f1, 0, 4096);
        arrived(&gr, 1);
        // the very same range, too: readers share it
        let b = r.spawn_read(f2, 0, 4096);
        arrived(&gr, 2);
        assert_eq!(r.smax(FaultOp::Pread, "/f"), 2, "{ser:?}");
        r.open_all();
        join(a);
        join(b);
        r.h.close(f1);
        r.h.close(f2);
        r.finish();
    }
}

#[test]
fn concurrency_counts_in_stats() {
    let r = rig(CheckLevel::Basic, Serialization::Relaxed);
    let f1 = r.file("/f", 1 << 16);
    let f2 = r.h.open("/f", libc::O_RDWR);
    let g = r.sgate(FaultOp::Pwrite, "/f");
    let a = r.spawn_write(f1, 0, 100, 1);
    arrived(&g, 1);
    let b = r.spawn_write(f2, 100, 100, 2);
    arrived(&g, 2);
    // an overlapping one waits for a range (and is counted when it did)
    let c = r.spawn_write(f2, 50, 100, 3);
    stays_at(&g, 2);
    r.open_all();
    for t in [a, b, c] {
        join(t);
    }
    assert!(r.stat("concurrent") >= 1);
    assert!(r.stat("waits") >= 1, "the overlapping write waited for its range");
    r.h.close(f1);
    r.h.close(f2);
    r.finish();
}

// ---------------------------------------------------------------------------------------------------------------
// What must NOT overlap: max in flight stays 1

/// Writer A is held inside both file systems. `b` starts a second operation whose backend call is `b_op`; that
/// call must not reach the file systems while A is held, and must complete after A is released.
fn must_wait_for_in_place_write(name: &str, b_op: FaultOp, b: impl Fn(&Rig, Fh, Fh) -> JoinHandle<()>, b_flags: i32) {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fb = r.h.open("/f", b_flags);
        let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let gb = (b_op != FaultOp::Pwrite).then(|| (r.sgate(b_op, "/f"), r.pgate(b_op, "/f")));
        let t = r.spawn_write(fa, 0, 4096, 1);
        arrived(&gs, 1);
        arrived(&gp, 1);
        let tb = b(&r, fa, fb);
        match &gb {
            Some((s, p)) => {
                stays_at(s, 0);
                stays_at(p, 0);
            }
            None => {
                stays_at(&gs, 1);
                stays_at(&gp, 1);
            }
        }
        assert_eq!(r.smax(FaultOp::Pwrite, "/f"), 1, "{name} {level:?}");
        assert_eq!(r.pmax(FaultOp::Pwrite, "/f"), 1, "{name} {level:?}");
        r.open_all();
        join(t);
        join(tb);
        assert_eq!(r.smax(FaultOp::Pwrite, "/f"), 1, "{name} {level:?}: pwrite ran concurrently");
        if let Some((s, p)) = &gb {
            assert_eq!((s.arrived(), p.arrived()), (1, 1), "{name} {level:?}: the waiting call ran after the release");
        }
        r.h.close(fa);
        r.h.close(fb);
        r.finish();
    }
}

#[test]
fn serializes_overlapping_writes() {
    must_wait_for_in_place_write("overlapping", FaultOp::Pwrite, |r, _, fb| r.spawn_write(fb, 2048, 4096, 2), libc::O_RDWR);
}

#[test]
fn serializes_extending_write_after_in_place_write() {
    // the file is 64 KiB: this one grows it
    must_wait_for_in_place_write("extending", FaultOp::Pwrite, |r, _, fb| r.spawn_write(fb, (1 << 16) - 100, 4096, 2), libc::O_RDWR);
}

#[test]
fn serializes_append_write_after_in_place_write() {
    must_wait_for_in_place_write("append", FaultOp::Pwrite, |r, _, fb| r.spawn_write(fb, 40_000, 100, 2), libc::O_RDWR | libc::O_APPEND);
}

#[test]
fn serializes_truncate_after_in_place_write() {
    must_wait_for_in_place_write(
        "truncate",
        FaultOp::Truncate,
        |r, _, fb| {
            let h = r.h.clone();
            std::thread::spawn(move || {
                let a = h.lookup("/f");
                h.engine.setattr(&h.ctx, a.id, SetAttr { size: Some(1 << 15), ..Default::default() }, Some(fb.fh)).expect("truncate");
            })
        },
        libc::O_RDWR,
    );
}

#[test]
fn serializes_fallocate_past_eof_after_in_place_write() {
    // mode 0 beyond the end grows the file
    must_wait_for_in_place_write(
        "fallocate",
        FaultOp::Fallocate,
        |r, _, fb| {
            let h = r.h.clone();
            std::thread::spawn(move || h.engine.fallocate(&h.ctx, fb.ino, fb.fh, (1 << 16) - 10, 4096, 0).expect("fallocate"))
        },
        libc::O_RDWR,
    );
}

#[test]
fn serializes_zero_range_past_eof_after_in_place_write() {
    must_wait_for_in_place_write(
        "zero-range",
        FaultOp::Fallocate,
        |r, _, fb| {
            let h = r.h.clone();
            std::thread::spawn(move || {
                // (tmpfs may refuse; the engine must serialise it either way)
                let _ = h.engine.fallocate(&h.ctx, fb.ino, fb.fh, 60_000, 20_000, libc::FALLOC_FL_ZERO_RANGE);
            })
        },
        libc::O_RDWR,
    );
}

#[test]
fn serializes_in_place_write_after_extending_write() {
    // the other way round: an extending write is held, an in-place write must not reach the file systems
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fb = r.h.open("/f", libc::O_RDWR);
        let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let a = r.spawn_write(fa, (1 << 16) - 100, 4096, 1); // grows the file
        arrived(&gs, 1);
        arrived(&gp, 1);
        let b = r.spawn_write(fb, 0, 4096, 2);
        stays_at(&gs, 1);
        stays_at(&gp, 1);
        assert_eq!((r.smax(FaultOp::Pwrite, "/f"), r.pmax(FaultOp::Pwrite, "/f")), (1, 1));
        r.open_all();
        join(a);
        join(b);
        assert_eq!((r.smax(FaultOp::Pwrite, "/f"), r.pmax(FaultOp::Pwrite, "/f")), (1, 1));
        r.h.close(fa);
        r.h.close(fb);
        r.finish();
    }
}

#[test]
fn serializes_concurrent_appends() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1000);
        r.h.close(fa);
        let (f1, f2) = (r.h.open("/f", libc::O_WRONLY | libc::O_APPEND), r.h.open("/f", libc::O_WRONLY | libc::O_APPEND));
        let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let a = r.spawn_write(f1, 0, 100, 1);
        arrived(&gs, 1);
        arrived(&gp, 1);
        let b = r.spawn_write(f2, 0, 100, 2);
        stays_at(&gs, 1);
        stays_at(&gp, 1);
        r.open_all();
        join(a);
        join(b);
        assert_eq!((r.smax(FaultOp::Pwrite, "/f"), r.pmax(FaultOp::Pwrite, "/f")), (1, 1), "{level:?}");
        assert_eq!(r.h.read_file("/f").len(), 1200);
        r.h.close(f1);
        r.h.close(f2);
        r.finish();
    }
}

/// The ranged operations that keep the file's size also overlap with in-place writes: fallocate inside the file,
/// fallocate KEEP_SIZE even past the end, PUNCH_HOLE.
#[test]
fn concurrency_ranged_fallocate_overlaps_in_place_write() {
    let cases: [(&str, i32, u64, u64); 4] = [
        ("inside the file", 0, 32768, 4096),
        ("keep size past eof", libc::FALLOC_FL_KEEP_SIZE, 100_000, 4096),
        ("punch hole", libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, 16384, 8192),
        ("zero range keep size", libc::FALLOC_FL_ZERO_RANGE | libc::FALLOC_FL_KEEP_SIZE, 16384, 8192),
    ];
    for (name, mode, off, len) in cases {
        for ser in SERS {
            let r = rig(CheckLevel::Thorough, ser);
            let fa = r.file("/f", 1 << 16);
            let fb = r.h.open("/f", libc::O_RDWR);
            let gw = r.sgate(FaultOp::Pwrite, "/f");
            let gf = r.sgate(FaultOp::Fallocate, "/f");
            let a = r.spawn_write(fa, 0, 4096, 1);
            arrived(&gw, 1);
            let h = r.h.clone();
            let b = std::thread::spawn(move || {
                let _ = h.engine.fallocate(&h.ctx, fb.ino, fb.fh, off, len, mode);
            });
            if ser == Serialization::Relaxed {
                arrived(&gf, 1);
            } else {
                stays_at(&gf, 0);
            }
            eprintln!("{name} {ser:?}: fallocate in flight together with the write: {}", gf.arrived() == 1);
            r.open_all();
            join(a);
            join(b);
            r.h.close(fa);
            r.h.close(fb);
            r.finish();
        }
    }
}

// =================================================================================================================
// 2. No false positives: forced interleavings
// =================================================================================================================

/// One half-call that is held in a gate: P/S x writer A / writer B / observer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    PA,
    SA,
    PB,
    SB,
    PO,
    SO,
}

fn perms<T: Copy>(v: &[T]) -> Vec<Vec<T>> {
    if v.len() <= 1 {
        return vec![v.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..v.len() {
        let mut rest = v.to_vec();
        let x = rest.remove(i);
        for mut p in perms(&rest) {
            p.insert(0, x);
            out.push(p);
        }
    }
    out
}

/// What races with the two writers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Obs {
    Getattr,
    Lookup,
    ReadDisjoint,
    Fsync,
    Open,
}

impl Obs {
    /// The backend call whose halves are held.
    fn op(self) -> FaultOp {
        match self {
            Obs::Getattr => FaultOp::Stat,
            Obs::Lookup => FaultOp::Lookup,
            Obs::ReadDisjoint => FaultOp::Pread,
            Obs::Fsync => FaultOp::Fsync,
            Obs::Open => FaultOp::Open,
        }
    }
    /// Whether the racing operation compares times (and so is skipped by the racy-stat rule).
    fn stats(self) -> bool {
        matches!(self, Obs::Getattr | Obs::Lookup)
    }
}

const OLD: i64 = 1000;
const PRE_SLEEP: Duration = Duration::from_millis(30);

/// One file of the interleaving tests.
struct Scn {
    name: String,
    path: String,
    ino: u64,
    fa: Fh,
    fb: Fh,
    fo: Fh,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Creates the files of a worker: 64 KiB of zeros, mtime 1000 s ago, a getattr to set the baselines, three
/// handles.
fn make_scn(h: &Harness, i: usize) -> Scn {
    let name = format!("s{i}");
    let path = format!("/{name}");
    h.write_file(&path, &vec![0u8; 1 << 16]);
    h.set_mtime(&path, now_secs() - OLD).unwrap();
    let a = h.getattr(&path);
    Scn {
        name,
        ino: a.id,
        fa: h.open(&path, libc::O_RDWR),
        fb: h.open(&path, libc::O_RDWR),
        fo: h.open(&path, libc::O_RDWR),
        path,
    }
}

/// The two halves of one operation, each held in a gate (armed for the `n`-th matching call on `path`).
struct Held {
    p: Arc<Gate>,
    s: Arc<Gate>,
    ids: [FaultId; 2],
}

impl Held {
    fn new(h: &Harness, op: FaultOp, path: &str, n: u64) -> Held {
        let (p, s) = (Gate::new(), Gate::new());
        let ids = [
            h.pfault.add(Fault::new(op, Effect::Gate(p.clone())).path(path).nth(n)),
            h.fault.add(Fault::new(op, Effect::Gate(s.clone())).path(path).nth(n)),
        ];
        Held { p, s, ids }
    }
    fn disarm(&self, h: &Harness) {
        h.pfault.remove(self.ids[0]);
        h.fault.remove(self.ids[1]);
    }
    #[track_caller]
    fn all_arrived(&self) {
        arrived(&self.p, 1);
        arrived(&self.s, 1);
    }
}

fn spawn_observer(h: &Arc<Harness>, sc: &Scn, obs: Obs) -> JoinHandle<()> {
    let (h, ino, fo, name) = (h.clone(), sc.ino, sc.fo, sc.name.clone());
    std::thread::spawn(move || match obs {
        Obs::Getattr => {
            h.engine.getattr(&h.ctx, ino).unwrap();
        }
        Obs::Lookup => {
            let a = h.engine.lookup(&h.ctx, ROOT_ID, OsStr::new(&name)).unwrap();
            h.engine.forget(a.id, 1);
        }
        Obs::ReadDisjoint => {
            h.pread(fo, 32768, 4096);
        }
        Obs::Fsync => h.engine.fsync(&h.ctx, fo.ino, fo.fh, false).unwrap(),
        Obs::Open => {
            let fh = h.engine.open(&h.ctx, ino, libc::O_RDONLY).unwrap();
            h.engine.release(&h.ctx, ino, fh).unwrap();
        }
    })
}

/// Runs one interleaving: writers A ([0, 4096)) and B ([8192, 12288)) and the observer are launched one after the
/// other (`launch`: the observer's position among them: 0 first, 1 between, 2 last; A always before B), each is
/// held with both of its halves in flight, and then the six halves are released in `order`, each waiting for the
/// released call to finish.
#[track_caller]
fn run_scn(h: &Arc<Harness>, sc: &Scn, obs: Obs, order: &[Ev], launch: usize, level: CheckLevel) {
    let ctx = h.ctx;
    let path = sc.path.as_str();
    let (wa, wb) = (Held::new(h, FaultOp::Pwrite, path, 1), Held::new(h, FaultOp::Pwrite, path, 2));
    let mut guard = OpenOnDrop(vec![wa.p.clone(), wa.s.clone(), wb.p.clone(), wb.s.clone()]);
    let skipped0 = h.stats.attr_time_skipped.load(Relaxed);
    let concurrent0 = h.stats.concurrent_data_ops.load(Relaxed);
    let before = h.mismatches().len();
    let tag = format!("{level:?} {obs:?} launch {launch} order {order:?}");

    #[derive(Clone, Copy, PartialEq)]
    enum Step {
        A,
        B,
        O,
    }
    let steps = match launch {
        0 => [Step::O, Step::A, Step::B],
        1 => [Step::A, Step::O, Step::B],
        _ => [Step::A, Step::B, Step::O],
    };
    let mut threads = Vec::new();
    let mut wo: Option<Held> = None;
    for st in steps {
        match st {
            Step::A => {
                let (h2, f) = (h.clone(), sc.fa);
                threads.push(std::thread::spawn(move || h2.pwrite(f, 0, &pattern(1, 4096))));
                wa.all_arrived();
            }
            Step::B => {
                let (h2, f) = (h.clone(), sc.fb);
                threads.push(std::thread::spawn(move || h2.pwrite(f, 8192, &pattern(2, 4096))));
                wb.all_arrived();
            }
            Step::O => {
                // armed only now, with no other call of its kind running: the observer's is the first
                let o = Held::new(h, obs.op(), path, 1);
                threads.push(spawn_observer(h, sc, obs));
                o.all_arrived();
                guard.0.extend([o.p.clone(), o.s.clone()]);
                wo = Some(o);
            }
        }
    }
    let wo = wo.unwrap();
    let gate = |e: &Ev| match e {
        Ev::PA => &wa.p,
        Ev::SA => &wa.s,
        Ev::PB => &wb.p,
        Ev::SB => &wb.s,
        Ev::PO => &wo.p,
        Ev::SO => &wo.s,
    };
    for e in order {
        let g = gate(e);
        g.open();
        done(g, 1);
    }
    for t in threads {
        join(t);
    }
    for w in [&wa, &wb, &wo] {
        w.disarm(h);
    }
    let new: Vec<_> = h.mismatches().into_iter().skip(before).map(|m| m.summary()).collect();
    assert!(new.is_empty(), "{tag}: mismatches:\n  {}", new.join("\n  "));
    let skipped = h.stats.attr_time_skipped.load(Relaxed) - skipped0;
    assert_eq!(skipped, obs.stats() as u64, "{tag}: skipped time comparisons");
    assert!(h.stats.concurrent_data_ops.load(Relaxed) > concurrent0, "{tag}: the writers did not overlap");

    // a quiet getattr afterwards compares normally
    let before = h.mismatches().len();
    let skipped0 = h.stats.attr_time_skipped.load(Relaxed);
    h.engine.getattr(&ctx, sc.ino).unwrap();
    assert_eq!(h.mismatches().len(), before, "{tag}: quiet getattr after the race");
    assert_eq!(h.stats.attr_time_skipped.load(Relaxed), skipped0, "{tag}: a quiet getattr must not skip");
}

struct OpenOnDrop(Vec<Arc<Gate>>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        for g in &self.0 {
            g.open();
        }
    }
}

/// All `orders` x launch positions for one observer at one level, spread over worker threads (each with an engine of
/// its own and its own files).
fn interleavings(level: CheckLevel, obs: Obs, orders: Vec<Vec<Ev>>, workers: usize) {
    let jobs: Vec<(usize, Vec<Ev>)> = orders.into_iter().enumerate().collect();
    let n = jobs.len();
    let done = Arc::new(AtomicU64::new(0));
    std::thread::scope(|sc| {
        for w in 0..workers {
            let jobs = &jobs;
            let done = done.clone();
            sc.spawn(move || {
                let mine: Vec<&(usize, Vec<Ev>)> = jobs.iter().skip(w).step_by(workers).collect();
                if mine.is_empty() {
                    return;
                }
                let r = rig_with(level, Serialization::Relaxed, |c| c.time_tolerance = Duration::from_millis(20));
                let files: Vec<Scn> = (0..mine.len()).map(|i| make_scn(&r.h, i)).collect();
                // the files' ctime baselines are older than the tolerance when the first write comes
                std::thread::sleep(PRE_SLEEP);
                for (i, (idx, order)) in mine.iter().enumerate() {
                    run_scn(&r.h, &files[i], obs, order, idx % 3, level);
                    done.fetch_add(1, Relaxed);
                }
                r.finish();
            });
        }
    });
    assert_eq!(done.load(Relaxed) as usize, n);
}

fn all_orders() -> Vec<Vec<Ev>> {
    perms(&[Ev::PA, Ev::SA, Ev::PB, Ev::SB, Ev::PO, Ev::SO])
}

/// Every ordering of the six halves, at every level, for the observers that compare times.
fn each_level(obs: Obs, every: usize) {
    for level in LEVELS {
        let orders: Vec<_> = all_orders().into_iter().step_by(every).collect();
        interleavings(level, obs, orders, 12);
    }
}

#[test]
fn interleavings_getattr() {
    each_level(Obs::Getattr, 1);
}

#[test]
fn interleavings_lookup() {
    each_level(Obs::Lookup, 1);
}

#[test]
fn interleavings_read_of_a_disjoint_range() {
    each_level(Obs::ReadDisjoint, 3);
}

#[test]
fn interleavings_fsync() {
    each_level(Obs::Fsync, 3);
}

#[test]
fn interleavings_open_and_release() {
    each_level(Obs::Open, 3);
}

// ---------------------------------------------------------------------------------------------------------------
// Operations that must wait for a writer (and writers that must wait for them)

/// P half of the held write has run, the S half has not: the two file systems differ until the second is released.
struct HalfWrite {
    t: JoinHandle<()>,
}

impl Rig {
    /// Starts a write of `len` bytes at `off`, holds the secondary's half, lets the primary's run.
    fn half_write(&self, f: Fh, off: u64, len: usize, seed: u64) -> HalfWrite {
        let (sg, pg) = (self.sgate(FaultOp::Pwrite, "/f"), self.pgate(FaultOp::Pwrite, "/f"));
        let t = self.spawn_write(f, off, len, seed);
        arrived(&sg, 1);
        arrived(&pg, 1);
        pg.open();
        done(&pg, 1);
        HalfWrite { t }
    }
    /// Releases everything and waits for the write.
    fn finish_half(&self, w: HalfWrite) {
        self.open_all();
        join(w.t);
    }
}

#[test]
fn blocking_overlapping_read_waits_for_the_write() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fo = r.h.open("/f", libc::O_RDWR);
        let w = r.half_write(fa, 0, 8192, 1);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        let t = r.spawn_read(fo, 4096, 8192);
        // without the range lock this read would see the data on the primary and zeros on the secondary
        stays_at(&rs, 0);
        stays_at(&rp, 0);
        r.finish_half(w);
        r.open_all();
        let got = join(t);
        let want = pattern(1, 8192);
        assert_eq!(&got[..4096], &want[4096..], "{level:?}");
        assert_eq!(&got[4096..], &[0u8; 4096][..], "{level:?}");
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

#[test]
fn blocking_overlapping_write_waits_for_the_read() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fo = r.h.open("/f", libc::O_RDWR);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        let t = r.spawn_read(fo, 0, 8192);
        arrived(&rs, 1);
        arrived(&rp, 1);
        let (ws, wp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let w = r.spawn_write(fa, 4096, 8192, 3);
        stays_at(&ws, 0);
        stays_at(&wp, 0);
        // (the thorough read-back of the write must not be held by the gate that holds the first read)
        r.open_all();
        assert_eq!(join(t), vec![0u8; 8192], "the read ran before the write");
        join(w);
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

#[test]
fn blocking_paranoid_release_waits_for_in_place_writes() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fo = r.h.open("/f", libc::O_RDWR);
        r.h.pwrite(fo, 40_000, &pattern(9, 100)); // fo has written: its release compares the whole content (paranoid)
        let w = r.half_write(fa, 0, 4096, 1);
        let h = r.h.clone();
        let closer = std::thread::spawn(move || h.close(fo));
        if level == CheckLevel::Paranoid {
            // the content comparison needs the whole file stable: it must wait for the write
            std::thread::sleep(GRACE);
            assert!(!closer.is_finished(), "paranoid release compared the file while a write was half done");
        }
        r.finish_half(w);
        join(closer);
        r.h.close(fa);
        r.finish();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Reads at and after EOF vs extending writes

#[test]
fn eof_read_at_eof_serializes_with_an_extending_write() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 4096);
        let fo = r.h.open("/f", libc::O_RDWR);
        // a: extending write held half way
        let w = r.half_write(fa, 4096, 4096, 7);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        let (ss, sp) = (r.sgate(FaultOp::Stat, "/f"), r.pgate(FaultOp::Stat, "/f"));
        // reads at EOF, after EOF, spanning EOF, and a getattr: all must wait
        let reads = [r.spawn_read(fo, 4096, 4096), r.spawn_read(fo, 8192, 4096), r.spawn_read(fo, 2048, 4096)];
        let h = r.h.clone();
        let ga = std::thread::spawn(move || h.engine.getattr(&h.ctx, fo.ino).unwrap().st.size);
        stays_at(&rs, 0);
        stays_at(&rp, 0);
        stays_at(&ss, 0);
        stays_at(&sp, 0);
        r.finish_half(w);
        r.open_all();
        let [a, b, c] = reads;
        let (a, b, c) = (join(a), join(b), join(c));
        let want = pattern(7, 4096);
        assert_eq!(a, want, "{level:?}: read at the old EOF");
        assert!(b.is_empty(), "{level:?}: read after the new EOF");
        assert_eq!(c.len(), 4096 + 2048 - 2048);
        assert_eq!(&c[2048..], &want[..2048]);
        assert_eq!(join(ga), 8192);
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

#[test]
fn eof_extending_write_waits_for_a_read_at_eof() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 4096);
        let fo = r.h.open("/f", libc::O_RDWR);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        let t = r.spawn_read(fo, 4096, 4096);
        arrived(&rs, 1);
        arrived(&rp, 1);
        let (ws, wp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let w = r.spawn_write(fa, 4096, 4096, 7);
        stays_at(&ws, 0);
        stays_at(&wp, 0);
        r.open_all();
        assert!(join(t).is_empty(), "{level:?}: the read at EOF saw the write");
        join(w);
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

/// A read that spans EOF and an in-place write elsewhere in the file are independent.
#[test]
fn eof_spanning_read_is_concurrent_with_a_disjoint_write() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 4096);
        let fo = r.h.open("/f", libc::O_RDWR);
        let (ws, wp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let w = r.spawn_write(fa, 0, 1024, 7);
        arrived(&ws, 1);
        arrived(&wp, 1);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        let t = r.spawn_read(fo, 2048, 4096);
        arrived(&rs, 1);
        arrived(&rp, 1);
        r.open_all();
        join(w);
        assert_eq!(join(t).len(), 2048);
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// copy_file_range racing writes on both files

#[test]
fn copy_file_range_races_writes_on_both_files() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        r.h.write_file("/x", &pattern(5, 1 << 16));
        r.h.write_file("/y", &vec![0u8; 1 << 16]);
        let (fx, fy) = (r.h.open("/x", libc::O_RDWR), r.h.open("/y", libc::O_RDWR));
        let (fx2, fy2) = (r.h.open("/x", libc::O_RDWR), r.h.open("/y", libc::O_RDWR));
        let want_y = pattern(5, 1 << 16)[..8192].to_vec();
        // (the copy runs on the primary first and then on the secondary: only the secondary's half is held)
        let cs = r.sgate(FaultOp::CopyFileRange, "/y");
        let h = r.h.clone();
        let cfr = std::thread::spawn(move || h.engine.copy_file_range(&h.ctx, fx.fh, 0, fy.fh, 0, 8192, 0).unwrap());
        arrived(&cs, 1);
        let (xs, ys) = (r.sgate(FaultOp::Pwrite, "/x"), r.sgate(FaultOp::Pwrite, "/y"));
        let (xrs, yrs) = (r.sgate(FaultOp::Pread, "/x"), r.sgate(FaultOp::Pread, "/y"));
        // allowed alongside: writes to either file outside the copied ranges, a read of the source (shared)
        let ok = [
            r.h.clone(),
            r.h.clone(),
        ];
        let t1 = { let h = ok[0].clone(); std::thread::spawn(move || h.pwrite(fx2, 16384, &pattern(11, 4096))) };
        let t2 = { let h = ok[1].clone(); std::thread::spawn(move || h.pwrite(fy2, 16384, &pattern(12, 4096))) };
        let t3 = r.spawn_read(fx2, 0, 100);
        arrived(&xs, 1);
        arrived(&ys, 1);
        arrived(&xrs, 1);
        assert_eq!((r.smax(FaultOp::Pwrite, "/x"), r.smax(FaultOp::Pwrite, "/y")), (1, 1));
        // not allowed: a write into the source range, a write into or a read of the destination range
        let t4 = { let h = r.h.clone(); std::thread::spawn(move || h.pwrite(fx2, 4096, &pattern(13, 100))) };
        let t5 = { let h = r.h.clone(); std::thread::spawn(move || h.pwrite(fy2, 100, &pattern(14, 100))) };
        let t6 = r.spawn_read(fy2, 0, 100);
        stays_at(&xs, 1);
        stays_at(&ys, 1);
        stays_at(&yrs, 0);
        stays_at(&cs, 1);
        r.open_all();
        assert_eq!(join(cfr), 8192);
        for t in [t1, t2, t4, t5] {
            join(t);
        }
        join(t3);
        // the read of the destination ran after the copy (the copy came first): the source data
        assert_eq!(join(t6), want_y[..100].to_vec(), "{level:?}");
        // the waiting write into the source ran after the copy: the copy has the old source data
        let y = r.h.read_file("/y");
        assert_eq!(&y[8192..16384], &[0u8; 8192][..]);
        assert_eq!(&y[200..8192], &want_y[200..], "{level:?}");
        for f in [fx, fy, fx2, fy2] {
            r.h.close(f);
        }
        r.finish();
        let _ = (xrs, yrs);
    }
}

/// A copy that extends its destination takes the destination exclusively; its source must still be protected
/// from in-place writes (which hold only the source's stripe shared and their own range): otherwise the two halves
/// of the copy can read different source data.
#[test]
fn extending_copy_file_range_keeps_writers_out_of_its_source() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let old = pattern(5, 1 << 16);
        r.h.write_file("/x", &old);
        r.h.write_file("/y", b"");
        let (fx, fy, fx2) = (r.h.open("/x", libc::O_RDWR), r.h.open("/y", libc::O_RDWR), r.h.open("/x", libc::O_RDWR));
        let cs = r.sgate(FaultOp::CopyFileRange, "/y");
        let h = r.h.clone();
        let cfr = std::thread::spawn(move || h.engine.copy_file_range(&h.ctx, fx.fh, 0, fy.fh, 0, 8192, 0).unwrap());
        arrived(&cs, 1);
        let xs = r.sgate(FaultOp::Pwrite, "/x");
        // outside the source range: runs; inside it: waits for the copy
        let t1 = r.spawn_write(fx2, 16384, 4096, 11);
        arrived(&xs, 1);
        let t2 = r.spawn_write(fx2, 4096, 100, 12);
        stays_at(&xs, 1);
        r.open_all();
        assert_eq!(join(cfr), 8192);
        join(t1);
        join(t2);
        assert_eq!(r.h.read_file("/y"), old[..8192].to_vec(), "{level:?}: the copy has the source data from before the write");
        for f in [fx, fy, fx2] {
            r.h.close(f);
        }
        r.finish();
    }
}

/// The two ranges of one file: non-overlapping ones are ranged, overlapping ones take the file exclusively.
#[test]
fn copy_file_range_within_one_file() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        r.h.write_file("/f", &pattern(5, 1 << 16));
        let (f1, f2) = (r.h.open("/f", libc::O_RDWR), r.h.open("/f", libc::O_RDWR));
        let cs = r.sgate(FaultOp::CopyFileRange, "/f");
        let h = r.h.clone();
        let cfr = std::thread::spawn(move || h.engine.copy_file_range(&h.ctx, f1.fh, 0, f1.fh, 8192, 4096, 0).unwrap());
        arrived(&cs, 1);
        let (ws, wp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let far = r.spawn_write(f2, 30_000, 100, 1);
        arrived(&ws, 1);
        arrived(&wp, 1);
        let near = r.spawn_write(f2, 9000, 100, 2); // inside the destination
        stays_at(&ws, 1);
        r.open_all();
        assert_eq!(join(cfr), 4096);
        join(far);
        join(near);
        r.h.close(f1);
        r.h.close(f2);
        r.finish();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// fallocate PUNCH_HOLE / KEEP_SIZE racing reads

#[test]
fn punch_hole_races_reads() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        r.h.write_file("/f", &pattern(5, 1 << 16));
        let (f1, f2) = (r.h.open("/f", libc::O_RDWR), r.h.open("/f", libc::O_RDWR));
        let (fs_, fp) = (r.sgate(FaultOp::Fallocate, "/f"), r.pgate(FaultOp::Fallocate, "/f"));
        let mode = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        let h = r.h.clone();
        let punch = std::thread::spawn(move || h.engine.fallocate(&h.ctx, f1.ino, f1.fh, 8192, 8192, mode).unwrap());
        arrived(&fs_, 1);
        arrived(&fp, 1);
        fp.open(); // the primary has the hole, the secondary not yet
        done(&fp, 1);
        let (rs, rp) = (r.sgate(FaultOp::Pread, "/f"), r.pgate(FaultOp::Pread, "/f"));
        // a read elsewhere is concurrent and sees the same data on both
        let far = r.spawn_read(f2, 32768, 4096);
        arrived(&rs, 1);
        arrived(&rp, 1);
        // a read of the hole is not
        let near = r.spawn_read(f2, 12288, 100);
        stays_at(&rs, 1);
        stays_at(&rp, 1);
        // a read spanning the end of the hole is not either
        let span = r.spawn_read(f2, 16000, 1000);
        stays_at(&rs, 1);
        fs_.open();
        r.open_all();
        join(punch);
        let want = pattern(5, 1 << 16);
        assert_eq!(join(far), want[32768..32768 + 4096].to_vec());
        assert_eq!(join(near), vec![0u8; 100], "{level:?}");
        let s = join(span);
        assert_eq!(&s[..384], &[0u8; 384][..]);
        assert_eq!(&s[384..], &want[16384..16384 + 616]);
        r.h.close(f1);
        r.h.close(f2);
        r.finish();
    }
}

/// lseek(SEEK_DATA) looks at which parts of the file hold data: a write into a hole changes the answer (even
/// to an error: ENXIO), so it must not run between the halves of a write.
#[test]
fn seek_data_races_a_write_into_a_hole() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        r.h.write_file("/f", b"");
        r.h.truncate("/f", 1 << 20).unwrap(); // one big hole
        let (fa, fo) = (r.h.open("/f", libc::O_RDWR), r.h.open("/f", libc::O_RDWR));
        let w = r.half_write(fa, 524_288, 4096, 1);
        let (ls, lp) = (r.sgate(FaultOp::Lseek, "/f"), r.pgate(FaultOp::Lseek, "/f"));
        let h = r.h.clone();
        let seek = std::thread::spawn(move || h.engine.lseek(&h.ctx, fo.ino, fo.fh, 0, libc::SEEK_DATA));
        // the primary has data after 0, the secondary has none: an lseek in between would report ENXIO vs an offset
        stays_at(&ls, 0);
        stays_at(&lp, 0);
        r.finish_half(w);
        r.open_all();
        let off = join(seek).expect("seek");
        assert!(off > 0 && off <= 524_288, "{level:?}: data starts at {off}");
        r.h.close(fa);
        r.h.close(fo);
        r.finish();
    }
}

/// A write by an unprivileged user clears the set-id bits of the file: the mode changes by an in-place write.
#[test]
fn setuid_bit_cleared_by_a_write_is_not_seen_half_way() {
    if is_root() {
        eprintln!("SKIP: root keeps the set-id bits on write");
        return;
    }
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        r.h.chmod("/f", 0o4755);
        let w = r.half_write(fa, 0, 4096, 1);
        // the primary's file has lost the set-uid bit, the secondary's has not yet: nobody may look in between
        let (ss, sp) = (r.sgate(FaultOp::Stat, "/f"), r.pgate(FaultOp::Stat, "/f"));
        let h = r.h.clone();
        let ga = std::thread::spawn(move || h.engine.getattr(&h.ctx, fa.ino).unwrap().st.mode & 0o7777);
        stays_at(&ss, 0);
        stays_at(&sp, 0);
        r.finish_half(w);
        assert_eq!(join(ga), 0o755, "{level:?}");
        r.h.close(fa);
        r.finish();
    }
}

// ---------------------------------------------------------------------------------------------------------------
// unlink / rename of a file that is being written through an open handle

#[test]
fn unlink_and_rename_wait_for_writers_and_writers_survive_them() {
    for level in LEVELS {
        for rename in [false, true] {
            let r = rig(level, Serialization::Relaxed);
            let fa = r.file("/f", 1 << 16);
            let w = r.half_write(fa, 0, 4096, 1);
            let op = if rename { FaultOp::Rename } else { FaultOp::Unlink };
            let (us, up) = (r.sgate(op, "/f"), r.pgate(op, "/f"));
            let h = r.h.clone();
            let t = std::thread::spawn(move || if rename { h.rename("/f", "/g") } else { h.unlink("/f") });
            stays_at(&us, 0);
            stays_at(&up, 0);
            r.finish_half(w);
            join(t);
            // the handle outlives the name
            r.h.pwrite(fa, 8192, &pattern(2, 4096));
            let back = r.h.pread(fa, 0, 12288);
            assert_eq!(&back[..4096], &pattern(1, 4096)[..]);
            assert_eq!(&back[8192..], &pattern(2, 4096)[..]);
            r.h.close(fa);
            r.finish();
        }
    }
}

#[test]
fn writers_wait_for_unlink_and_rename() {
    for level in LEVELS {
        for rename in [false, true] {
            let r = rig(level, Serialization::Relaxed);
            let fa = r.file("/f", 1 << 16);
            let op = if rename { FaultOp::Rename } else { FaultOp::Unlink };
            let (us, up) = (r.sgate(op, "/f"), r.pgate(op, "/f"));
            let h = r.h.clone();
            let t = std::thread::spawn(move || if rename { h.rename("/f", "/g") } else { h.unlink("/f") });
            arrived(&us, 1);
            arrived(&up, 1);
            let (ws, wp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
            let w = r.spawn_write(fa, 0, 4096, 1);
            stays_at(&ws, 0);
            stays_at(&wp, 0);
            r.open_all();
            join(t);
            join(w);
            assert_eq!(r.h.pread(fa, 0, 4096), pattern(1, 4096));
            r.h.close(fa);
            r.finish();
        }
    }
}

// =================================================================================================================
// 3. No false negatives: faults on one of two concurrent writers
// =================================================================================================================

/// Two concurrent disjoint writers, A at [0, 4096) and B at [16384, 20480), both held inside both file systems and
/// released together; `effect` hits the secondary's first pwrite, which is A's. Returns the rig.
fn faulty_writers(level: CheckLevel, effect: Effect) -> (Rig, Fh, Fh) {
    let r = rig(level, Serialization::Relaxed);
    let fa = r.file("/f", 1 << 16);
    let fb = r.h.open("/f", libc::O_RDWR);
    r.h.fault.add(Fault::new(FaultOp::Pwrite, effect).path("/f").nth(1));
    let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
    let a = r.spawn_write(fa, 0, 4096, 1);
    arrived(&gs, 1);
    let b = r.spawn_write(fb, 16384, 4096, 2);
    arrived(&gs, 2);
    arrived(&gp, 2);
    assert_eq!(r.smax(FaultOp::Pwrite, "/f"), 2, "the writers must be concurrent in the secondary");
    r.open_all();
    join(a);
    join(b);
    (r, fa, fb)
}

/// The lookups of a file that are expected to differ: which `K` the operation itself reports, at which level.
fn detects_one_sided_write(effect: Effect, op_kind_basic: Option<K>, op_kind_thorough: K) {
    for level in LEVELS {
        let (r, fa, fb) = faulty_writers(level, effect.clone());
        let during = r.h.new_mismatches();
        let want = if level == CheckLevel::Basic { op_kind_basic } else { Some(op_kind_thorough) };
        match want {
            Some(k) => {
                let m = during.iter().find(|m| m.kind == k && m.op == OpKind::Write);
                assert!(m.is_some(), "{effect:?} {level:?}: the write itself must report a {k:?} mismatch, got:\n  {}", r.h.describe_mismatches());
                assert_eq!(during.len(), 1, "{effect:?} {level:?}: only the faulty write is reported:\n  {}", r.h.describe_mismatches());
            }
            None => assert!(during.is_empty(), "{effect:?} basic: nothing is visible at the write:\n  {}", r.h.describe_mismatches()),
        }
        // the observation pass: B's range is fine, A's is not
        r.h.mark();
        r.h.pread(fb, 16384, 4096);
        r.h.assert_no_mismatches();
        r.h.pread(fa, 0, 4096);
        let data = r.h.find_mismatch(K::Data, None);
        let len = r.h.find_mismatch(K::Length, None);
        assert!(data.is_some() || len.is_some(), "{effect:?} {level:?}: a read of the damaged range sees it:\n  {}", r.h.describe_mismatches());
        // and the trees really differ
        assert!(!r.h.tree_diff().is_empty(), "{effect:?} {level:?}: no damage at all?");
        // paranoid: the close compares the content
        r.h.mark();
        r.h.close(fa);
        r.h.close(fb);
        if level == CheckLevel::Paranoid {
            r.h.expect_mismatch(K::Content, None);
        }
        r.open_all();
        r.assert_no_gate_timeouts();
    }
}

#[test]
fn detects_dropped_write_of_a_concurrent_writer() {
    detects_one_sided_write(Effect::DropWrite, None, K::Verify);
}

#[test]
fn detects_corrupted_write_of_a_concurrent_writer() {
    detects_one_sided_write(Effect::CorruptWrite { offset: 100 }, None, K::Verify);
}

#[test]
fn detects_short_write_of_a_concurrent_writer() {
    // a short count is visible at once, at every level
    detects_one_sided_write(Effect::ShortWrite(1000), Some(K::Length), K::Length);
}

#[test]
fn detects_write_at_the_wrong_offset_of_a_concurrent_writer() {
    detects_one_sided_write(Effect::ShiftOffset(7), None, K::Verify);
    detects_one_sided_write(Effect::ShiftOffset(4096 * 3), None, K::Verify);
}

/// A file system that does not update mtime on write: thorough reports it at the write (even with racing stats,
/// which cannot compare times); basic notices at the first quiet getattr after the burst.
#[test]
fn detects_missing_mtime_update_thorough_at_the_write() {
    for level in [CheckLevel::Thorough, CheckLevel::Paranoid] {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fb = r.h.open("/f", libc::O_RDWR);
        r.h.set_mtime("/f", now_secs() - OLD).unwrap();
        r.h.fault.add(Fault::new(FaultOp::Pwrite, Effect::RestoreTimes).path("/f"));
        let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let a = r.spawn_write(fa, 0, 4096, 1);
        let b = r.spawn_write(fb, 16384, 4096, 2);
        arrived(&gs, 2);
        arrived(&gp, 2);
        // racing stats cannot compare times ...
        let skipped0 = r.stat("skipped");
        r.h.engine.getattr(&r.h.ctx, fa.ino).unwrap();
        assert_eq!(r.stat("skipped"), skipped0 + 1);
        assert!(r.h.new_mismatches().is_empty());
        r.open_all();
        join(a);
        join(b);
        // ... the writes check the times themselves
        let m = r.h.expect_mismatch_on(K::Verify, Some("write mtime"), OpKind::Write, "/f");
        eprintln!("{level:?}: {}", m.summary());
        r.h.close(fa);
        r.h.close(fb);
    }
}

#[test]
fn detects_missing_mtime_update_basic_at_the_first_quiet_getattr() {
    let r = rig(CheckLevel::Basic, Serialization::Relaxed);
    let fa = r.file("/f", 1 << 16);
    let fb = r.h.open("/f", libc::O_RDWR);
    r.h.set_mtime("/f", now_secs() - OLD).unwrap();
    r.h.engine.getattr(&r.h.ctx, fa.ino).unwrap(); // baseline
    r.h.fault.add(Fault::new(FaultOp::Pwrite, Effect::RestoreTimes).path("/f"));
    let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
    let a = r.spawn_write(fa, 0, 4096, 1);
    let b = r.spawn_write(fb, 16384, 4096, 2);
    arrived(&gs, 2);
    arrived(&gp, 2);
    // a burst of stats while the writes are in flight: time comparisons are skipped (and counted) ...
    let skipped0 = r.stat("skipped");
    for _ in 0..5 {
        r.h.engine.getattr(&r.h.ctx, fa.ino).unwrap();
    }
    assert_eq!(r.stat("skipped") - skipped0, 5);
    r.open_all();
    join(a);
    join(b);
    r.h.assert_no_mismatches();
    // ... the first quiet one compares: the secondary's mtime did not move
    let skipped1 = r.stat("skipped");
    r.h.engine.getattr(&r.h.ctx, fa.ino).unwrap();
    assert_eq!(r.stat("skipped"), skipped1);
    let m = r.h.find_mismatch(K::Attr, None).unwrap_or_else(|| panic!("no mismatch: {}", r.h.describe_mismatches()));
    assert!(matches!(m.field.as_deref(), Some("mtime") | Some("ctime")), "{}", m.summary());
    eprintln!("quiet getattr: {}", m.summary());
    r.h.close(fa);
    r.h.close(fb);
}

/// The size is never skipped: every getattr and lookup that races the writers reports a lying secondary.
#[test]
fn detects_lying_size_in_every_racing_stat() {
    for level in LEVELS {
        let r = rig(level, Serialization::Relaxed);
        let fa = r.file("/f", 1 << 16);
        let fb = r.h.open("/f", libc::O_RDWR);
        let (gs, gp) = (r.sgate(FaultOp::Pwrite, "/f"), r.pgate(FaultOp::Pwrite, "/f"));
        let a = r.spawn_write(fa, 0, 4096, 1);
        let b = r.spawn_write(fb, 16384, 4096, 2);
        arrived(&gs, 2);
        arrived(&gp, 2);
        r.h.mark();
        r.h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Size(12345))).path("/f").every(1));
        r.h.fault.add(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Size(777))).path("/f"));
        const N: u64 = 6;
        let skipped0 = r.stat("skipped");
        let reports0 = r.h.stats.mismatches.load(Relaxed) + r.h.stats.repeats.load(Relaxed);
        for _ in 0..N {
            r.h.engine.getattr(&r.h.ctx, fa.ino).unwrap();
            let a = r.h.engine.lookup(&r.h.ctx, ROOT_ID, OsStr::new("f")).unwrap();
            r.h.engine.forget(a.id, 1);
        }
        assert_eq!(r.stat("skipped") - skipped0, 2 * N, "{level:?}: every one of them raced the writers");
        // (the policy keeps the first of identical reports and counts the others as repeats)
        let reports = (r.h.stats.mismatches.load(Relaxed) + r.h.stats.repeats.load(Relaxed)) - reports0;
        assert_eq!(reports, 2 * N, "{level:?}: one report per racing getattr and lookup: {}", r.h.describe_mismatches());
        let new = r.h.new_mismatches();
        assert_eq!(new.len(), 2, "{level:?}: {}", r.h.describe_mismatches());
        for (op, lie) in [(OpKind::Getattr, "12345"), (OpKind::Lookup, "777")] {
            let m = new.iter().find(|m| m.op == op).unwrap_or_else(|| panic!("{op:?}: {}", r.h.describe_mismatches()));
            assert_eq!((m.kind, m.field.as_deref(), m.secondary.as_str()), (K::Attr, Some("size"), lie));
        }
        r.open_all();
        join(a);
        join(b);
        r.h.close(fa);
        r.h.close(fb);
    }
}

// =================================================================================================================
// Stress: a hot set of files, every data operation, slow halves
// =================================================================================================================

/// `threads` threads on three shared 256 KiB files: in-place writes (disjoint and overlapping), reads, getattr,
/// lookup, fsync, fallocate (ranged and not), punch hole, lseek, copy_file_range between the files and inside
/// them, now and then a truncate or append that takes the file exclusively. Delays on both sides widen every
/// window. Healthy trees: zero mismatches, identical trees.
fn relaxed_stress(level: CheckLevel, ser: Serialization, stripes: usize, threads: usize, iters: usize) {
    let r = rig_with(level, ser, move |c| c.lock_stripes = stripes);
    let h = r.h.clone();
    for op in [FaultOp::Pwrite, FaultOp::Pread, FaultOp::Stat, FaultOp::Fallocate, FaultOp::CopyFileRange, FaultOp::Lseek, FaultOp::Fsync, FaultOp::Lookup] {
        h.fault.add(Fault::new(op, Effect::Delay(Duration::from_micros(400))).every(3));
        h.pfault.add(Fault::new(op, Effect::Delay(Duration::from_micros(250))).every(4));
    }
    const SIZE: usize = 256 << 10;
    let names = ["/a", "/b", "/c"];
    for n in names {
        h.write_file(n, &pattern(7, SIZE));
    }
    let ops_done = Arc::new(AtomicU64::new(0));
    std::thread::scope(|sc| {
        for t in 0..threads {
            let (h, ops_done) = (h.clone(), ops_done.clone());
            sc.spawn(move || {
                let mut rng = Rng::new(900 + t as u64);
                let fhs: Vec<Fh> = names.iter().map(|n| h.open(n, libc::O_RDWR)).collect();
                for i in 0..iters {
                    let k = rng.below(3) as usize;
                    let (f, name) = (fhs[k], names[k]);
                    let off = rng.below(SIZE as u64 - 20_000);
                    let len = 1 + rng.below(16_000) as usize;
                    match rng.below(100) {
                        0..=29 => h.pwrite(f, off, &pattern(rng.next(), len)),
                        30..=44 => {
                            h.pread(f, off, len);
                        }
                        45..=52 => {
                            h.engine.getattr(&h.ctx, f.ino).unwrap();
                        }
                        53..=58 => {
                            let a = h.lookup(name);
                            h.engine.forget(a.id, 1);
                        }
                        59..=62 => h.engine.fsync(&h.ctx, f.ino, f.fh, i % 2 == 0).unwrap(),
                        63..=68 => {
                            let mode = [0, libc::FALLOC_FL_KEEP_SIZE, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, libc::FALLOC_FL_ZERO_RANGE][rng.below(4) as usize];
                            let _ = h.engine.fallocate(&h.ctx, f.ino, f.fh, off, len as u64, mode);
                        }
                        69..=73 => {
                            let _ = h.engine.lseek(&h.ctx, f.ino, f.fh, off as i64, [libc::SEEK_DATA, libc::SEEK_HOLE, libc::SEEK_END][rng.below(3) as usize]);
                        }
                        74..=83 => {
                            let k2 = rng.below(3) as usize;
                            let off2 = rng.below(SIZE as u64 - 20_000);
                            let _ = h.engine.copy_file_range(&h.ctx, f.fh, off, fhs[k2].fh, off2, len as u64, 0);
                        }
                        84..=86 => {
                            // grows (or at least may grow) the file: exclusive
                            h.pwrite(f, SIZE as u64 - 100 + rng.below(3000), &pattern(rng.next(), len.min(5000)));
                        }
                        87..=88 => h.append(name, &pattern(rng.next(), 1 + rng.below(500) as usize)),
                        89 => {
                            let _ = h.truncate(name, SIZE as u64);
                        }
                        _ => {
                            let big = h.pread(f, 0, 70_000);
                            assert!(big.len() <= 70_000);
                        }
                    }
                    ops_done.fetch_add(1, Relaxed);
                }
                for f in fhs {
                    h.close(f);
                }
            });
        }
    });
    assert_eq!(ops_done.load(Relaxed) as usize, threads * iters);
    if ser == Serialization::Relaxed {
        assert!(h.stats.concurrent_data_ops.load(Relaxed) > 0, "no data operations overlapped");
    } else {
        assert_eq!(h.stats.concurrent_data_ops.load(Relaxed), 0);
    }
    eprintln!(
        "{level:?} {ser:?} stripes {stripes}: concurrent_data_ops {} range_waits {} attr_time_skipped {}; secondary max in flight: pwrite {} pread {}",
        h.stats.concurrent_data_ops.load(Relaxed),
        h.stats.range_waits.load(Relaxed),
        h.stats.attr_time_skipped.load(Relaxed),
        h.fault.max_inflight_any(FaultOp::Pwrite),
        h.fault.max_inflight_any(FaultOp::Pread)
    );
    r.finish();
}

#[test]
fn stress_basic() {
    for ser in SERS {
        relaxed_stress(CheckLevel::Basic, ser, 65536, 12, 500 * soak());
    }
}

#[test]
fn stress_thorough() {
    for ser in SERS {
        relaxed_stress(CheckLevel::Thorough, ser, 65536, 12, 400 * soak());
    }
}

#[test]
fn stress_paranoid() {
    for ser in SERS {
        relaxed_stress(CheckLevel::Paranoid, ser, 65536, 8, 300 * soak());
    }
}

/// Sixteen stripes: unrelated files share stripes, range waits and stripe waits meet every which way.
#[test]
fn stress_few_stripes() {
    for ser in SERS {
        relaxed_stress(CheckLevel::Thorough, ser, 16, 12, 300 * soak());
    }
}

/// Ranges of one operation are taken as one request: copy_file_range inside a file (source shared, destination
/// exclusive) against writers on overlapping ranges used to be able to deadlock on the byte-range queue. The
/// workload must finish; a deadlock shows up as the timeout.
#[test]
fn copy_file_range_inside_one_file_does_not_deadlock_with_writers() {
    for level in [CheckLevel::Basic, CheckLevel::Thorough] {
        let r = rig(level, Serialization::Relaxed);
        const SIZE: u64 = 256 << 10;
        r.h.write_file("/f", &pattern(3, SIZE as usize));
        let (tx, rx) = std::sync::mpsc::channel();
        let threads = 10;
        for t in 0..threads {
            let (h, tx) = (r.h.clone(), tx.clone());
            std::thread::spawn(move || {
                let f = h.open("/f", libc::O_RDWR);
                let mut rng = Rng::new(40 + t);
                for _ in 0..400 * soak() {
                    let len = 1 + rng.below(30_000);
                    let a = rng.below(SIZE - len);
                    if t % 2 == 0 {
                        // (a destination that overlaps the source is refused by the kernel and serialized by the engine)
                        let b = rng.below(SIZE - len);
                        let _ = h.engine.copy_file_range(&h.ctx, f.fh, a, f.fh, b, len, 0);
                    } else if rng.below(3) == 0 {
                        h.pread(f, a, len as usize);
                    } else {
                        h.pwrite(f, a, &pattern(rng.next(), len as usize));
                    }
                }
                h.close(f);
                tx.send(()).unwrap();
            });
        }
        for i in 0..threads {
            rx.recv_timeout(Duration::from_secs(120)).unwrap_or_else(|_| panic!("deadlock: {i} of {threads} workers finished"));
        }
        r.finish();
    }
}

/// The timestamps of a healthy file system lag the wall clock by up to a tick: the "write mtime" check must not
/// report them however small `--time-tolerance` is.
#[test]
fn write_mtime_check_has_a_floor_under_the_tolerance() {
    for level in [CheckLevel::Thorough, CheckLevel::Paranoid] {
        let r = rig_with(level, Serialization::Relaxed, |c| c.time_tolerance = Duration::from_nanos(1));
        let f = r.file("/f", 1 << 16);
        for i in 0..400 {
            r.h.pwrite(f, (i % 10) * 4096, &pattern(i, 100));
        }
        r.h.close(f);
        // (only the times are allowed to differ by more than a nanosecond between two file systems)
        let v: Vec<_> = r.h.new_mismatches().into_iter().filter(|m| m.field.as_deref() == Some("write mtime")).collect();
        assert!(v.is_empty(), "{level:?}: {}", r.h.describe_mismatches());
    }
}

// =================================================================================================================
// The instrumentation itself
// =================================================================================================================

/// A `FaultBackend` over a plain directory, to try the recorder and the gates without an engine.
fn raw_backend() -> (tempfile::TempDir, Arc<FaultBackend>, std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::AsFd;
    use xcheckfs::backend::posix::PosixBackend;
    use xcheckfs::backend::Backend;
    let dir = tmp_in(&fast_base());
    let fb = FaultBackend::new(Arc::new(PosixBackend::open("raw", dir.path()).unwrap()));
    let root = fb.root().unwrap();
    let mk = |name: &std::ffi::CStr| {
        let fd = fb.create(root.as_fd(), name, libc::O_RDWR, 0o644).unwrap();
        fb.pwrite(fd.as_fd(), &[1u8; 100], 0).unwrap();
        fd
    };
    let a = mk(c"a");
    let b = mk(c"b");
    (dir, fb, a, b)
}

#[test]
fn recorder_counts_per_method_and_object_and_gates_hold_calls() {
    use std::os::fd::AsFd;
    use xcheckfs::backend::Backend;
    let (dir, fb, a, b) = raw_backend();
    let (oa, ob) = (obj_of(&dir.path().join("a")), obj_of(&dir.path().join("b")));
    fb.record(true);
    let g = Gate::new();
    fb.add(Fault::new(FaultOp::Pwrite, Effect::Gate(g.clone())));
    let (fb2, a2) = (fb.clone(), a.try_clone().unwrap());
    let (fb3, a3) = (fb.clone(), a.try_clone().unwrap());
    let (fb4, b4) = (fb.clone(), b.try_clone().unwrap());
    let ts: Vec<_> = [(fb2, a2), (fb3, a3), (fb4, b4)]
        .into_iter()
        .map(|(f, fd)| std::thread::spawn(move || f.pwrite(fd.as_fd(), b"x", 0).unwrap()))
        .collect();
    assert!(g.wait_arrived(3, WAIT));
    assert_eq!(fb.inflight(FaultOp::Pwrite, oa), 2);
    assert_eq!(fb.inflight(FaultOp::Pwrite, ob), 1);
    g.open();
    for t in ts {
        join(t);
    }
    assert!(g.wait_done(3, WAIT));
    assert_eq!((fb.inflight(FaultOp::Pwrite, oa), fb.max_inflight(FaultOp::Pwrite, oa)), (0, 2));
    assert_eq!(fb.max_inflight(FaultOp::Pwrite, ob), 1);
    assert_eq!(fb.max_inflight_any(FaultOp::Pwrite), 3);
    assert_eq!(fb.recorded_calls(FaultOp::Pwrite, oa), 2);
    assert_eq!(fb.max_inflight(FaultOp::Pread, oa), 0, "other methods are counted apart");
    fb.reset_inflight();
    assert_eq!(fb.max_inflight(FaultOp::Pwrite, oa), 0);
    // off: nothing recorded
    fb.record(false);
    fb.pwrite(a.as_fd(), b"y", 0).unwrap();
    assert_eq!(fb.recorded_calls(FaultOp::Pwrite, oa), 0);
}

#[test]
fn a_closed_gate_times_out_instead_of_hanging() {
    use std::os::fd::AsFd;
    use xcheckfs::backend::Backend;
    let (_dir, fb, a, _b) = raw_backend();
    let g = Gate::with_timeout(Duration::from_millis(100));
    fb.add(Fault::new(FaultOp::Pwrite, Effect::Gate(g.clone())).once());
    assert_eq!(fb.pwrite(a.as_fd(), b"x", 0).unwrap(), 1, "the call goes through after the timeout");
    assert!(g.timed_out());
    assert_eq!((g.arrived(), g.done()), (1, 1));
}

#[test]
fn shift_offset_and_restore_times_effects() {
    use std::os::fd::AsFd;
    use xcheckfs::backend::Backend;
    let (_dir, fb, a, _b) = raw_backend();
    fb.add(Fault::new(FaultOp::Pwrite, Effect::ShiftOffset(10)).once());
    fb.pwrite(a.as_fd(), b"ZZ", 0).unwrap();
    let mut buf = [0u8; 16];
    fb.pread(a.as_fd(), &mut buf, 0).unwrap();
    assert_eq!(&buf[..2], &[1, 1], "not written where it was asked to");
    assert_eq!(&buf[10..12], b"ZZ");
    // times: put the mtime in the past, write, and see it stay there (while the data changed)
    let old = xcheckfs::sys::Ts { sec: now_secs() - 5000, nsec: 0 };
    fb.utimens(a.as_fd(), xcheckfs::sys::FileKind::Regular, Some(a.as_fd()), TimeSpec::Set(old), TimeSpec::Set(old)).unwrap();
    fb.add(Fault::new(FaultOp::Pwrite, Effect::RestoreTimes).once());
    fb.pwrite(a.as_fd(), b"Q", 5).unwrap();
    assert_eq!(fb.stat(a.as_fd()).unwrap().mtime, old);
    fb.pread(a.as_fd(), &mut buf, 0).unwrap();
    assert_eq!(buf[5], b'Q');
    fb.pwrite(a.as_fd(), b"R", 5).unwrap();
    assert!(fb.stat(a.as_fd()).unwrap().mtime > old, "a write without the effect stamps the time");
}

/// An fsync changes nothing either side compares, and can take seconds (an ext4 `fsyncdir` under directory
/// churn took up to 5 s): it must not hold the object, or a create in the directory waits for it, and every
/// lookup there queues behind the create.
#[test]
fn fsync_and_fsyncdir_do_not_hold_the_object() {
    for dir in [false, true] {
        let r = rig(CheckLevel::Thorough, Serialization::Relaxed);
        r.h.mkdir("/d");
        r.h.write_file("/d/f", b"x");
        let (path, ino) = if dir { ("/d", r.h.lookup("/d").id) } else { ("/d/f", r.h.lookup("/d/f").id) };
        let fh = if dir { r.h.engine.opendir(&r.h.ctx, ino).unwrap() } else { r.h.open(path, libc::O_RDWR).fh };
        let g = r.sgate(FaultOp::Fsync, path);
        let h = r.h.clone();
        let sync = std::thread::spawn(move || {
            if dir { h.engine.fsyncdir(&h.ctx, ino, fh, false) } else { h.engine.fsync(&h.ctx, ino, fh, false) }
        });
        assert!(g.wait_arrived(1, WAIT), "{path}: the fsync reaches the secondary");
        // While it is held there: a create in the directory, a change of the object itself.
        let h = r.h.clone();
        let other = std::thread::spawn(move || {
            h.mkdir("/d/sub");
            h.chmod(path, 0o700);
        });
        let deadline = std::time::Instant::now() + WAIT;
        while !other.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(other.is_finished(), "{path}: a create and a chmod waited for the fsync");
        other.join().unwrap();
        g.open();
        sync.join().unwrap().expect("fsync");
        r.finish();
    }
}
