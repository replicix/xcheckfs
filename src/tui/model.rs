//! Pure data helpers behind the TUI: bounded buffers, the statistics sample
//! history (rates, windowed latencies), log entries and filters. Nothing in
//! here touches the terminal, so it is unit-testable.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::events::OpEvent;
use crate::stats::{HistSnapshot, OpKind, OpSnapshot, StatsSnapshot};
use crate::sys::errno_name;

/// Width of the "last N seconds" window for rates and latencies.
pub const WINDOW_SECS: f64 = 5.0;
/// Number of one-second samples kept (also the sparkline history).
pub const SAMPLE_CAP: usize = 180;
/// Only the newest samples keep their histogram buckets; older ones are
/// "thin" (counters only) to bound memory. Must exceed `WINDOW_SECS` plus
/// some slack for irregular sampling.
const FULL_SAMPLES: usize = 8;

/// A bounded FIFO: pushing beyond the capacity drops the oldest element.
pub struct Ring<T> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    pub fn new(cap: usize) -> Ring<T> {
        let cap = cap.max(1);
        Ring {
            buf: VecDeque::with_capacity(cap.min(4096)),
            cap,
        }
    }
    pub fn push(&mut self, v: T) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    /// Oldest to newest; the iterator is double-ended.
    pub fn iter(&self) -> std::collections::vec_deque::Iter<'_, T> {
        self.buf.iter()
    }
}

/// `(new - old) / dt` with counter resets and a zero interval tolerated.
pub fn rate(new: u64, old: u64, dt: f64) -> f64 {
    if dt <= 0.0 {
        0.0
    } else {
        new.saturating_sub(old) as f64 / dt
    }
}

/// Compact count: 999, 12.3k, 4.5M, 1.2G.
pub fn fmt_count(n: u64) -> String {
    let f = n as f64;
    if n < 10_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", f / 1e3)
    } else if n < 1_000_000_000 {
        format!("{:.2}M", f / 1e6)
    } else {
        format!("{:.2}G", f / 1e9)
    }
}

/// Compact rate: 0, 12.3, 456, 12.3k.
pub fn fmt_rate(r: f64) -> String {
    if r < 0.05 {
        "0".into()
    } else if r < 100.0 {
        format!("{r:.1}")
    } else if r < 10_000.0 {
        format!("{r:.0}")
    } else {
        fmt_count(r as u64)
    }
}

/// `1h02m03s`, `4m05s`, `6s`.
pub fn fmt_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Replaces control characters (which would corrupt the terminal) with
/// spaces / `?`.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' | '\n' | '\r' => ' ',
            c if c.is_control() => '?',
            c => c,
        })
        .collect()
}

/// ASCII-case-insensitive substring search; `needle_lower` must already be
/// lower case. An empty needle matches.
pub fn contains_ci(hay: &str, needle_lower: &str) -> bool {
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return true;
    }
    hay.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// `HH:MM:SS.mmm` in local time (UTC if the zone lookup fails).
pub fn fmt_clock(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (h, m, s) = local_hms(d.as_secs()).unwrap_or_else(|| utc_hms(d.as_secs()));
    format!("{h:02}:{m:02}:{s:02}.{:03}", d.subsec_millis())
}

pub fn utc_hms(secs: u64) -> (u32, u32, u32) {
    (
        (secs / 3600 % 24) as u32,
        (secs / 60 % 60) as u32,
        (secs % 60) as u32,
    )
}

fn local_hms(secs: u64) -> Option<(u32, u32, u32)> {
    #[allow(deprecated)] // libc's time_t note about musl < 1.2; Rust's musl is 64-bit time
    let t = secs as libc::time_t;
    // SAFETY: `tm` is plain old data and `localtime_r` only writes into it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        tm
    };
    Some((tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32))
}

/// One line of the rolling operations log.
#[derive(Clone, Debug)]
pub struct OpEntry {
    pub ts: String,
    pub op: &'static str,
    pub detail: String,
    pub errno: i32,
    pub sec_errno: Option<i32>,
    pub total_ns: u64,
    pub bytes: u64,
    pub mismatch: bool,
}

impl OpEntry {
    pub fn from_event(e: &OpEvent) -> OpEntry {
        OpEntry {
            ts: fmt_clock(e.time),
            op: e.op.name(),
            detail: sanitize(&e.detail),
            errno: e.errno,
            sec_errno: e.sec_errno,
            total_ns: e.total_ns,
            bytes: e.bytes,
            mismatch: e.mismatch,
        }
    }

    /// The secondary's errno when it differs from the primary's.
    pub fn sec_diff(&self) -> Option<i32> {
        self.sec_errno.filter(|&s| s != self.errno)
    }

    /// A mismatch, or differing results (e.g. allowed by a rule). An errno
    /// both sides agree on is a normal result.
    pub fn is_notable(&self) -> bool {
        self.mismatch || self.sec_diff().is_some()
    }
}

/// One line of the (tracing) log pane.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub ts: String,
    pub level: tracing::Level,
    pub message: String,
}

/// Operations-log filter: optional "errors (mismatches) only" plus a
/// case-insensitive substring over op name, detail and result names.
#[derive(Clone, Debug, Default)]
pub struct LogFilter {
    pub errors_only: bool,
    /// Lower-cased.
    pub text: String,
}

impl LogFilter {
    pub fn is_active(&self) -> bool {
        self.errors_only || !self.text.is_empty()
    }

    pub fn matches(&self, e: &OpEntry) -> bool {
        if self.errors_only && !e.is_notable() {
            return false;
        }
        if self.text.is_empty() {
            return true;
        }
        let t = &self.text;
        contains_ci(e.op, t)
            || contains_ci(&e.detail, t)
            || contains_ci(errno_name(e.errno), t)
            || e.sec_errno.is_some_and(|s| contains_ci(errno_name(s), t))
    }
}

/// Latency summary of one histogram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lat {
    pub n: u64,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
    /// The window had no samples: these are the since-start values.
    pub stale: bool,
}

impl Lat {
    pub fn of(h: &HistSnapshot) -> Lat {
        Lat {
            n: h.count,
            p50: h.percentile(50.0),
            p99: h.percentile(99.0),
            max: h.max,
            stale: false,
        }
    }

    /// Windowed when `old` is given, cumulative otherwise. An empty window
    /// (no operations of this kind lately) falls back to the since-start
    /// values, marked stale.
    fn windowed(new: &HistSnapshot, old: Option<&HistSnapshot>) -> Lat {
        match old {
            Some(o) => match Lat::of(&new.delta(o)) {
                l if l.n == 0 && new.count > 0 => Lat { stale: true, ..Lat::of(new) },
                l => l,
            },
            None => Lat::of(new),
        }
    }
}

/// `sec / pri` p50 latency ratio; `None` when either side has no samples.
pub fn latency_ratio(sec: &Lat, pri: &Lat) -> Option<f64> {
    (sec.n > 0 && pri.n > 0 && pri.p50 > 0).then(|| sec.p50 as f64 / pri.p50 as f64)
}

/// One row of the per-operation statistics table.
#[derive(Clone, Debug)]
pub struct OpRow {
    pub kind: OpKind,
    pub count: u64,
    pub rate: f64,
    pub mismatches: u64,
    pub bytes: u64,
    pub bps: f64,
    pub total: Lat,
    pub pri: Lat,
    pub sec: Lat,
}

impl OpRow {
    pub fn ratio(&self) -> Option<f64> {
        latency_ratio(&self.sec, &self.pri)
    }
}

/// A statistics snapshot taken by the TUI.
pub struct Sample {
    pub at: Instant,
    pub snap: StatsSnapshot,
    /// Still carries the histogram buckets (see `FULL_SAMPLES`).
    full: bool,
}

/// The newest sample and the baseline it is compared against.
pub struct Window<'a> {
    pub new: &'a StatsSnapshot,
    /// `None` until there are two samples: everything counts as new.
    pub old: Option<&'a StatsSnapshot>,
    pub dt: f64,
}

impl Window<'_> {
    pub fn rate(&self, f: impl Fn(&StatsSnapshot) -> u64) -> f64 {
        rate(f(self.new), self.old.map_or(0, f), self.dt)
    }
}

/// Ring of one-second snapshots plus the per-second throughput series
/// derived from consecutive ones.
pub struct History {
    samples: VecDeque<Sample>,
    pub ops_series: Ring<u64>,
    pub read_series: Ring<u64>,
    pub write_series: Ring<u64>,
}

impl Default for History {
    fn default() -> Self {
        History {
            samples: VecDeque::new(),
            ops_series: Ring::new(SAMPLE_CAP),
            read_series: Ring::new(SAMPLE_CAP),
            write_series: Ring::new(SAMPLE_CAP),
        }
    }
}

/// Drops the histogram buckets (the bulk of a snapshot's size).
fn drop_buckets(o: &mut OpSnapshot) {
    o.total.buckets = Vec::new();
    o.primary.buckets = Vec::new();
    o.secondary.buckets = Vec::new();
}

impl History {
    pub fn push(&mut self, mut snap: StatsSnapshot, at: Instant) {
        // Operations that never ran have all-zero buckets; skip storing them.
        snap.ops
            .iter_mut()
            .filter(|(_, o)| o.count == 0)
            .for_each(|(_, o)| drop_buckets(o));
        if let Some(prev) = self.samples.back() {
            let dt = at.saturating_duration_since(prev.at).as_secs_f64();
            let r = |f: fn(&StatsSnapshot) -> u64| rate(f(&snap), f(&prev.snap), dt).round() as u64;
            self.ops_series.push(r(StatsSnapshot::total_ops));
            self.read_series.push(r(|s| s.bytes_read));
            self.write_series.push(r(|s| s.bytes_written));
        }
        self.samples.push_back(Sample { at, snap, full: true });
        if self.samples.len() > SAMPLE_CAP {
            self.samples.pop_front();
        }
        let n = self.samples.len();
        if n > FULL_SAMPLES {
            let s = &mut self.samples[n - 1 - FULL_SAMPLES];
            if s.full {
                s.full = false;
                s.snap.ops.iter_mut().for_each(|(_, o)| drop_buckets(o));
            }
        }
    }

    pub fn latest(&self) -> Option<&StatsSnapshot> {
        self.samples.back().map(|s| &s.snap)
    }

    /// The newest sample against the one that is at least `secs` older (or
    /// the oldest usable one while the history is still short).
    pub fn window(&self, secs: f64) -> Option<Window<'_>> {
        let newest = self.samples.back()?;
        let mut pick: Option<&Sample> = None;
        for s in self.samples.iter().rev().skip(1).filter(|s| s.full) {
            pick = Some(s);
            if newest.at.saturating_duration_since(s.at).as_secs_f64() >= secs {
                break;
            }
        }
        let dt = match pick {
            Some(p) => newest.at.saturating_duration_since(p.at).as_secs_f64(),
            None => newest.snap.uptime_secs,
        };
        Some(Window {
            new: &newest.snap,
            old: pick.map(|p| &p.snap),
            dt,
        })
    }
}

/// Builds the statistics table: operations that ran at least once, most
/// frequent first. Rates are always windowed; latencies are windowed only
/// when `windowed` is set (otherwise cumulative).
pub fn compute_rows(w: &Window<'_>, windowed: bool) -> Vec<OpRow> {
    let mut rows: Vec<OpRow> = w
        .new
        .ops
        .iter()
        .filter(|(_, o)| o.count > 0)
        .map(|(k, o)| {
            let old = w.old.map(|s| s.op(*k));
            let hist = |sel: fn(&OpSnapshot) -> &HistSnapshot| {
                Lat::windowed(sel(o), if windowed { old.map(sel) } else { None })
            };
            OpRow {
                kind: *k,
                count: o.count,
                rate: rate(o.count, old.map_or(0, |x| x.count), w.dt),
                mismatches: o.mismatches,
                bytes: o.bytes,
                bps: rate(o.bytes, old.map_or(0, |x| x.bytes), w.dt),
                total: hist(|x| &x.total),
                pri: hist(|x| &x.primary),
                sec: hist(|x| &x.secondary),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.kind.name().cmp(b.kind.name())));
    rows
}

/// Scroll position of a pane. For top-anchored panes `off` is the first
/// visible row; for bottom-anchored ones (logs) it is the number of rows
/// hidden below the view. The renderer clamps it to the content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Scroll {
    pub off: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

impl Scroll {
    pub fn nav(&mut self, nav: Nav, page: usize, bottom_anchored: bool) {
        let page = page.max(1);
        // Movement toward older rows for bottom-anchored panes, toward the
        // start for top-anchored ones, is "backward".
        let (back, step) = match nav {
            Nav::Up => (true, 1),
            Nav::PageUp => (true, page),
            Nav::Down => (false, 1),
            Nav::PageDown => (false, page),
            Nav::Top => {
                self.off = if bottom_anchored { usize::MAX } else { 0 };
                return;
            }
            Nav::Bottom => {
                self.off = if bottom_anchored { 0 } else { usize::MAX };
                return;
            }
        };
        self.off = if back == bottom_anchored {
            self.off.saturating_add(step)
        } else {
            self.off.saturating_sub(step)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Stats;
    use std::sync::atomic::Ordering::Relaxed;
    use std::time::Duration;

    #[test]
    fn ring_is_bounded() {
        let mut r = Ring::new(3);
        for i in 0..5 {
            r.push(i);
        }
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
        assert_eq!(r.iter().next_back(), Some(&4));
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn rates() {
        assert_eq!(rate(150, 50, 5.0), 20.0);
        assert_eq!(rate(10, 50, 5.0), 0.0, "counter reset");
        assert_eq!(rate(10, 0, 0.0), 0.0);
        assert_eq!(fmt_rate(0.0), "0");
        assert_eq!(fmt_rate(12.34), "12.3");
        assert_eq!(fmt_count(12_345), "12.3k");
        assert_eq!(fmt_duration(3723), "1h02m03s");
    }

    fn sample_stats(reads: u64, read_ns: u64) -> StatsSnapshot {
        let s = Stats::default();
        let o = s.op(OpKind::Read);
        for _ in 0..reads {
            o.count.fetch_add(1, Relaxed);
            o.bytes.fetch_add(4096, Relaxed);
            o.total.record(read_ns);
            o.primary.record(read_ns / 2);
            o.secondary.record(read_ns / 2 * 5);
        }
        s.bytes_read.fetch_add(reads * 4096, Relaxed);
        s.snapshot()
    }

    #[test]
    fn history_window_and_rows() {
        let t0 = Instant::now();
        let mut h = History::default();
        // 10 s of history, 100 reads per second, slow reads at first.
        for i in 0..=10u64 {
            let ns = if i < 5 { 1_000_000 } else { 100_000 };
            // Cumulative counts: build them by hand for the window maths.
            let mut snap = sample_stats(100 * i, ns);
            snap.uptime_secs = i as f64;
            h.push(snap, t0 + Duration::from_secs(i));
        }
        let w = h.window(5.0).unwrap();
        assert!((w.dt - 5.0).abs() < 1e-9);
        assert!((w.rate(|s| s.bytes_read) - 100.0 * 4096.0).abs() < 1.0);
        let rows = compute_rows(&w, true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, OpKind::Read);
        assert!((rows[0].rate - 100.0).abs() < 1e-6);
        // Secondary samples were recorded 5x slower than the primary's.
        let r = rows[0].ratio().unwrap();
        assert!((4.0..6.5).contains(&r), "ratio {r}");
        assert_eq!(compute_rows(&w, false)[0].count, 1000);
        // Series has one point per interval; old samples were thinned.
        assert_eq!(h.read_series.len(), 10);
        assert_eq!(h.samples.iter().filter(|s| s.full).count(), FULL_SAMPLES);
    }

    #[test]
    fn single_sample_window_uses_uptime() {
        let mut h = History::default();
        let mut snap = sample_stats(50, 1000);
        snap.uptime_secs = 10.0;
        h.push(snap, Instant::now());
        let w = h.window(5.0).unwrap();
        assert!(w.old.is_none());
        assert!((w.rate(StatsSnapshot::total_ops) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn filter() {
        let e = |op, detail: &str, errno, sec, mismatch| OpEntry {
            ts: String::new(),
            op,
            detail: detail.into(),
            errno,
            sec_errno: sec,
            total_ns: 0,
            bytes: 0,
            mismatch,
        };
        let ok = e("read", "/a/Readme.md", 0, Some(0), false);
        let err = e("lookup", "/a/missing", libc::ENOENT, Some(libc::ENOENT), false);
        let diff = e("open", "/b", 0, Some(libc::EACCES), false);
        let mm = e("write", "/c", 0, Some(0), true);
        let mut f = LogFilter::default();
        assert!(!f.is_active());
        assert!([&ok, &err, &diff, &mm].iter().all(|x| f.matches(x)));
        f.errors_only = true;
        assert!(!f.matches(&ok) && !f.matches(&err));
        assert!([&diff, &mm].iter().all(|x| f.matches(x)));
        f.errors_only = false;
        f.text = "readme".into();
        assert!(f.matches(&ok) && !f.matches(&err));
        f.text = "enoent".into();
        assert!(f.matches(&err) && !f.matches(&ok));
        f.text = "eacces".into();
        assert!(f.matches(&diff));
        f.text = "write".into();
        assert!(f.matches(&mm));
    }

    #[test]
    fn text_helpers() {
        assert!(contains_ci("Hello World", "o w"));
        assert!(!contains_ci("abc", "abcd"));
        assert!(contains_ci("abc", ""));
        assert_eq!(sanitize("a\tb\n\x1b[31mc"), "a b ?[31mc");
        assert_eq!(utc_hms(3723), (1, 2, 3));
        assert_eq!(fmt_clock(UNIX_EPOCH + Duration::from_millis(1234)).len(), 12);
    }

    #[test]
    fn scrolling() {
        let mut s = Scroll::default();
        s.nav(Nav::Up, 10, true);
        s.nav(Nav::PageUp, 10, true);
        assert_eq!(s.off, 11);
        s.nav(Nav::Down, 10, true);
        assert_eq!(s.off, 10);
        s.nav(Nav::Bottom, 10, true);
        assert_eq!(s.off, 0);
        s.nav(Nav::Down, 10, true);
        assert_eq!(s.off, 0);
        s.nav(Nav::Top, 10, true);
        assert_eq!(s.off, usize::MAX);
        let mut t = Scroll::default();
        t.nav(Nav::Down, 5, false);
        t.nav(Nav::PageDown, 5, false);
        assert_eq!(t.off, 6);
        t.nav(Nav::Up, 5, false);
        assert_eq!(t.off, 5);
        t.nav(Nav::Top, 5, false);
        assert_eq!(t.off, 0);
    }
}
