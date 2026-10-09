//! Lock-free counters and latency histograms, cheap enough for the hot path.
//!
//! Everything is cumulative. Windowed values ("per second over the last
//! five seconds") are computed by the consumer from two snapshots.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

macro_rules! op_kinds {
    ($($v:ident => $n:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum OpKind { $($v),* }
        /// Serialized as the lowercase name, as used in rules and logs.
        impl Serialize for OpKind {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.name())
            }
        }
        impl OpKind {
            pub const ALL: &'static [OpKind] = &[$(OpKind::$v),*];
            pub fn name(&self) -> &'static str { match self { $(OpKind::$v => $n),* } }
            pub fn from_name(s: &str) -> Option<OpKind> {
                match s { $($n => Some(OpKind::$v),)* _ => None }
            }
        }
    };
}

op_kinds! {
    Lookup => "lookup", Forget => "forget", Getattr => "getattr", Setattr => "setattr",
    Readlink => "readlink", Mknod => "mknod", Mkdir => "mkdir", Unlink => "unlink",
    Rmdir => "rmdir", Symlink => "symlink", Rename => "rename", Link => "link",
    Open => "open", Read => "read", Write => "write", Flush => "flush",
    Release => "release", Fsync => "fsync", Opendir => "opendir", Readdir => "readdir",
    Releasedir => "releasedir", Fsyncdir => "fsyncdir", Statfs => "statfs",
    Setxattr => "setxattr", Getxattr => "getxattr", Listxattr => "listxattr",
    Removexattr => "removexattr", Access => "access", Create => "create",
    Getlk => "getlk", Setlk => "setlk", Fallocate => "fallocate", Lseek => "lseek",
    CopyFileRange => "copy_file_range", Ioctl => "ioctl", Poll => "poll", Bmap => "bmap",
}

pub const N_OPS: usize = OpKind::ALL.len();

/// Log-linear histogram: 8 sub-buckets per power of two (~12% resolution),
/// covering 1ns .. ~2^44ns (~4.9h).
const SUB: usize = 8;
const POW: usize = 45;
pub const HIST_BUCKETS: usize = SUB * POW;

pub struct Histogram {
    buckets: Box<[AtomicU64]>,
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: (0..HIST_BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            max: AtomicU64::new(0),
        }
    }
}

fn bucket_of(ns: u64) -> usize {
    if ns < SUB as u64 {
        return ns as usize;
    }
    let p = 63 - ns.leading_zeros() as usize; // >= 3
    let sub = ((ns >> (p - 3)) & (SUB as u64 - 1)) as usize;
    ((p - 2) * SUB + sub).min(HIST_BUCKETS - 1)
}

/// Lower bound of a bucket in ns.
pub fn bucket_low(i: usize) -> u64 {
    if i < SUB {
        return i as u64;
    }
    let p = i / SUB + 2;
    let sub = (i % SUB) as u64;
    (1u64 << p) | (sub << (p - 3))
}

impl Histogram {
    pub fn record(&self, ns: u64) {
        self.buckets[bucket_of(ns)].fetch_add(1, Relaxed);
        self.count.fetch_add(1, Relaxed);
        self.sum.fetch_add(ns, Relaxed);
        self.max.fetch_max(ns, Relaxed);
    }
    pub fn snapshot(&self) -> HistSnapshot {
        HistSnapshot {
            buckets: self.buckets.iter().map(|b| b.load(Relaxed)).collect(),
            count: self.count.load(Relaxed),
            sum: self.sum.load(Relaxed),
            max: self.max.load(Relaxed),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HistSnapshot {
    #[serde(skip)]
    pub buckets: Vec<u64>,
    pub count: u64,
    pub sum: u64,
    /// All-time maximum (not windowed).
    pub max: u64,
}

impl HistSnapshot {
    /// Value at percentile `p` (0..=100), in ns.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 || self.buckets.is_empty() {
            return 0;
        }
        let target = ((p / 100.0) * self.count as f64).ceil().max(1.0) as u64;
        let mut acc = 0;
        for (i, &c) in self.buckets.iter().enumerate() {
            acc += c;
            if acc >= target {
                return bucket_low(i);
            }
        }
        self.max
    }
    pub fn mean(&self) -> u64 {
        self.sum.checked_div(self.count).unwrap_or(0)
    }
    /// `self - older`, for windowed statistics.
    pub fn delta(&self, older: &HistSnapshot) -> HistSnapshot {
        HistSnapshot {
            buckets: self
                .buckets
                .iter()
                .zip(older.buckets.iter().chain(std::iter::repeat(&0)))
                .map(|(a, b)| a.saturating_sub(*b))
                .collect(),
            count: self.count.saturating_sub(older.count),
            sum: self.sum.saturating_sub(older.sum),
            max: self.max,
        }
    }
}

#[derive(Default)]
pub struct OpStats {
    pub count: AtomicU64,
    /// Operations where the primary returned an error (not a mismatch).
    pub errors: AtomicU64,
    pub mismatches: AtomicU64,
    pub bytes: AtomicU64,
    pub total: Histogram,
    pub primary: Histogram,
    pub secondary: Histogram,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct OpSnapshot {
    pub count: u64,
    pub errors: u64,
    pub mismatches: u64,
    pub bytes: u64,
    pub total: HistSnapshot,
    pub primary: HistSnapshot,
    pub secondary: HistSnapshot,
}

pub struct Stats {
    pub started: Instant,
    pub ops: Vec<OpStats>,
    /// Mismatches reported (not allowed by a rule).
    pub mismatches: AtomicU64,
    /// Mismatches that matched an allow rule.
    pub allowed: AtomicU64,
    /// Repeats of an already reported mismatch on the same object.
    pub repeats: AtomicU64,
    /// Operations where the secondary half was skipped (detached, or the
    /// object does not exist on the secondary any more).
    pub secondary_skipped: AtomicU64,
    /// Thorough/paranoid verifications performed.
    pub verifications: AtomicU64,
    /// Data operations that ran while another data operation on the same
    /// object was in flight (relaxed serialization at work).
    pub concurrent_data_ops: AtomicU64,
    /// Data operations that waited for an overlapping byte range.
    pub range_waits: AtomicU64,
    /// Operations refused with `EMFILE` before either side ran them: their
    /// descriptors did not fit under the limit (`engine::fds`).
    pub fd_refusals: AtomicU64,
    /// What the counted descriptors may add up to (`engine::fds`).
    pub fd_budget: AtomicU64,
    /// Stats whose mtime/ctime comparison was skipped because an in-place
    /// data operation on the object overlapped them.
    pub attr_time_skipped: AtomicU64,
    /// Objects repaired from the primary (resync), verified afterwards.
    pub resyncs: AtomicU64,
    /// Secondary mtimes set to the primary's after an operation that stamps them on one file system only (a
    /// difference POSIX allows, found by the mount-time probe).
    pub aligned_mtimes: AtomicU64,
    /// Repairs whose verification failed: the object is left diverged.
    pub resync_failures: AtomicU64,
    /// Repairs skipped because the object exceeded its repair budget.
    pub resync_giveups: AtomicU64,
    /// Secondary objects saved to the quarantine directory.
    pub quarantined: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    /// Op events dropped because the UI could not keep up.
    pub events_dropped: AtomicU64,
    pub nodes: AtomicU64,
    pub open_files: AtomicU64,
    pub open_dirs: AtomicU64,
    pub lock_waiters: AtomicU64,
    pub detached: AtomicBool,
    pub inflight: InFlight,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            started: Instant::now(),
            ops: (0..N_OPS).map(|_| OpStats::default()).collect(),
            mismatches: AtomicU64::new(0),
            allowed: AtomicU64::new(0),
            repeats: AtomicU64::new(0),
            secondary_skipped: AtomicU64::new(0),
            verifications: AtomicU64::new(0),
            concurrent_data_ops: AtomicU64::new(0),
            range_waits: AtomicU64::new(0),
            fd_refusals: AtomicU64::new(0),
            fd_budget: AtomicU64::new(0),
            attr_time_skipped: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
            aligned_mtimes: AtomicU64::new(0),
            resync_failures: AtomicU64::new(0),
            resync_giveups: AtomicU64::new(0),
            quarantined: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
            nodes: AtomicU64::new(0),
            open_files: AtomicU64::new(0),
            open_dirs: AtomicU64::new(0),
            lock_waiters: AtomicU64::new(0),
            detached: AtomicBool::new(false),
            inflight: InFlight::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatsSnapshot {
    #[serde(skip)]
    pub at: Option<Instant>,
    pub uptime_secs: f64,
    pub ops: Vec<(OpKind, OpSnapshot)>,
    pub mismatches: u64,
    pub allowed: u64,
    pub repeats: u64,
    pub secondary_skipped: u64,
    pub verifications: u64,
    pub concurrent_data_ops: u64,
    pub range_waits: u64,
    pub fd_refusals: u64,
    pub fd_budget: u64,
    pub attr_time_skipped: u64,
    pub resyncs: u64,
    pub aligned_mtimes: u64,
    pub resync_failures: u64,
    pub resync_giveups: u64,
    pub quarantined: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub events_dropped: u64,
    pub nodes: u64,
    pub open_files: u64,
    pub open_dirs: u64,
    pub lock_waiters: u64,
    pub detached: bool,
}

impl StatsSnapshot {
    pub fn op(&self, k: OpKind) -> &OpSnapshot {
        &self.ops[k as usize].1
    }
    pub fn total_ops(&self) -> u64 {
        self.ops.iter().map(|(_, o)| o.count).sum()
    }
}

impl Stats {
    pub fn op(&self, k: OpKind) -> &OpStats {
        &self.ops[k as usize]
    }
    pub fn snapshot(&self) -> StatsSnapshot {
        let l = |a: &AtomicU64| a.load(Relaxed);
        StatsSnapshot {
            at: Some(Instant::now()),
            uptime_secs: self.started.elapsed().as_secs_f64(),
            ops: OpKind::ALL
                .iter()
                .map(|&k| {
                    let o = self.op(k);
                    (
                        k,
                        OpSnapshot {
                            count: l(&o.count),
                            errors: l(&o.errors),
                            mismatches: l(&o.mismatches),
                            bytes: l(&o.bytes),
                            total: o.total.snapshot(),
                            primary: o.primary.snapshot(),
                            secondary: o.secondary.snapshot(),
                        },
                    )
                })
                .collect(),
            mismatches: l(&self.mismatches),
            allowed: l(&self.allowed),
            repeats: l(&self.repeats),
            secondary_skipped: l(&self.secondary_skipped),
            verifications: l(&self.verifications),
            concurrent_data_ops: l(&self.concurrent_data_ops),
            range_waits: l(&self.range_waits),
            fd_refusals: l(&self.fd_refusals),
            fd_budget: l(&self.fd_budget),
            attr_time_skipped: l(&self.attr_time_skipped),
            resyncs: l(&self.resyncs),
            aligned_mtimes: l(&self.aligned_mtimes),
            resync_failures: l(&self.resync_failures),
            resync_giveups: l(&self.resync_giveups),
            quarantined: l(&self.quarantined),
            bytes_read: l(&self.bytes_read),
            bytes_written: l(&self.bytes_written),
            events_dropped: l(&self.events_dropped),
            nodes: l(&self.nodes),
            open_files: l(&self.open_files),
            open_dirs: l(&self.open_dirs),
            lock_waiters: l(&self.lock_waiters),
            detached: self.detached.load(Relaxed),
        }
    }
}

/// Operations currently being executed, for the "in flight" pane: a stuck
/// operation on the experimental file system shows up here first.
#[derive(Default)]
pub struct InFlight {
    shards: [Mutex<Vec<InFlightOp>>; 16],
    next: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct InFlightOp {
    pub id: u64,
    pub op: OpKind,
    pub ino: u64,
    pub detail: String,
    pub started: Instant,
}

impl InFlight {
    pub fn begin(&self, op: OpKind, ino: u64, detail: String) -> u64 {
        let id = self.next.fetch_add(1, Relaxed);
        self.shards[(id % 16) as usize].lock().push(InFlightOp {
            id,
            op,
            ino,
            detail,
            started: Instant::now(),
        });
        id
    }
    pub fn end(&self, id: u64) {
        let mut s = self.shards[(id % 16) as usize].lock();
        if let Some(i) = s.iter().position(|o| o.id == id) {
            s.swap_remove(i);
        }
    }
    /// All in-flight operations, oldest first.
    pub fn snapshot(&self) -> Vec<InFlightOp> {
        let mut v: Vec<InFlightOp> = self.shards.iter().flat_map(|s| s.lock().clone()).collect();
        v.sort_by_key(|o| o.started);
        v
    }
}

pub fn fmt_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1e3)
    } else if ns < 1_000_000_000 {
        format!("{:.1}ms", ns as f64 / 1e6)
    } else {
        format!("{:.2}s", ns as f64 / 1e9)
    }
}

pub fn fmt_bytes(b: f64) -> String {
    const U: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0}{}", U[i]) } else { format!("{v:.1}{}", U[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_monotonic_and_cover() {
        let mut last = 0;
        for i in 0..HIST_BUCKETS {
            let lo = bucket_low(i);
            assert!(i == 0 || lo > last, "bucket {i}");
            last = lo;
            assert_eq!(bucket_of(lo), i, "bucket_of(bucket_low({i}))");
        }
        assert_eq!(bucket_of(u64::MAX), HIST_BUCKETS - 1);
    }

    #[test]
    fn percentiles() {
        let h = Histogram::default();
        for i in 1..=1000u64 {
            h.record(i * 1000);
        }
        let s = h.snapshot();
        let p50 = s.percentile(50.0);
        assert!((440_000..=520_000).contains(&p50), "p50={p50}");
        assert!(s.percentile(100.0) <= 1_000_000);
        assert_eq!(s.max, 1_000_000);
    }
}
