//! Secondary-fault episodes: an outage of the secondary as one finding.
//!
//! When the secondary stops working as a whole (its daemon restarts, its storage or network goes away, it fills up,
//! it is remounted read-only, or it fences xcheckfs's process after a lost lock: xcheckfs issues every client's
//! operations from one process), it fails nearly every operation that reaches it while the primary carries them out.
//! Reported one by one, that is a flood of mismatches: every failed mutation makes the trees diverge, every later
//! operation on what diverged adds knock-on mismatches, and in `resync` mode every one of them fires a repair into a
//! secondary that cannot take it.
//!
//! An *episode* is that outage as one finding. A *secondary fault* is a mismatch in which the secondary failed with an
//! errno that says the file system is unwell ([`is_fault`]) while the primary answered otherwise. A burst of them
//! opens an episode: at least `errors` faults within [`WINDOW`], on at least [`OBJECTS`] distinct objects (a defect of
//! one object repeats on that object, and deduplication handles it), making up at least [`SHARE`] percent of the
//! operations that ran on the secondary meanwhile (an outage fails most of them; a file system that fails one
//! operation in twenty has defects, which are reported and repaired one by one as always). The faults of the burst
//! are reported like any mismatch: the episode is not known yet. While it is open, faults and the knock-on mismatches
//! of what diverged in it are counted in it instead of being reported one by one: per operation, errno and kind, with
//! examples kept verbatim ([`Record`]). The episode itself is reported (error level, the UI, `ctl status`/`episodes`,
//! the summary and the exit status of the mount): every mismatch is either reported or counted in an episode, and an
//! episode fails a run like a mismatch does.
//!
//! **Knock-ons.** What a mutating fault (or knock-on) touched is *tainted* while the episode is open: the name of a
//! name-based operation (a create whose secondary half failed leaves the name missing there: a later lookup, unlink or
//! rename of it meets `ENOENT`) together with the directory (its listing, link count and times differ now), or the
//! object of any other operation. A mismatch about a tainted name, or about a tainted object other than a name in it,
//! is a knock-on. A read-only operation changes nothing, so its fault is counted but taints nothing. The taint ends
//! with the episode: from then on every mismatch is reported as usual, except that the ones counted in the episode
//! are in the deduplication table, so finding the same difference again is a repeat (as after any reported
//! mismatch). A difference that arises after the episode, on an object it touched, is therefore reported.
//!
//! **Repairs** are deferred while an episode is open (`Engine::resync`). The episode ends after `quiet` without a
//! fault, once the secondary answers a probe; the deferred repairs then run and are verified as usual.
//!
//! The no-mismatch path of an operation never gets here; a mismatch that is no fault costs one atomic load while no
//! episode is open.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::Serialize;

use crate::config::EpisodeConfig;
use crate::policy::{Mismatch, MismatchKind, systime};
use crate::stats::{OpKind, Stats};
use crate::sys::errno_name;

/// Whether the secondary failing with `e` (where the primary did not) is a fault of the file system as a whole rather
/// than an answer about the request: I/O errors and corruption (`EIO`, `EREMOTEIO`, `EUCLEAN`), a lost device
/// (`ENODEV`, `ENXIO`), a lost connection to the file system's servers or daemon (`ENOTCONN`, `ECONNRESET`,
/// `ECONNABORTED`, `ECONNREFUSED`, `ETIMEDOUT`, `ESHUTDOWN`, `EHOSTDOWN`, `EHOSTUNREACH`, `ENETDOWN`, `ENETUNREACH`),
/// handles invalidated by a server restart (`ESTALE`), no memory or space (`ENOMEM`, `ENOSPC`, `EDQUOT`), and a
/// read-only remount after an error (`EROFS`). Answers about the request (`ENOENT`, `EEXIST`, `EACCES`, `EINVAL`,
/// `EOPNOTSUPP`, ...) are not: they are what a defect of one object or one operation looks like. One such errno alone
/// is reported as always; only a burst of them opens an episode.
pub fn is_fault(e: i32) -> bool {
    matches!(
        e,
        libc::EIO
            | libc::EREMOTEIO
            | libc::EUCLEAN
            | libc::ENODEV
            | libc::ENXIO
            | libc::ENOTCONN
            | libc::ECONNRESET
            | libc::ECONNABORTED
            | libc::ECONNREFUSED
            | libc::ETIMEDOUT
            | libc::ESHUTDOWN
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::ESTALE
            | libc::ENOMEM
            | libc::ENOSPC
            | libc::EDQUOT
            | libc::EROFS
    )
}

/// The time the faults of a burst must fall within.
pub const WINDOW: Duration = Duration::from_secs(10);
/// Distinct objects the faults of a burst must be on.
pub const OBJECTS: usize = 4;
/// The share (percent) of the operations that ran on the secondary that the faults of a burst must make up.
pub const SHARE: u64 = 25;
/// Mismatches kept verbatim per episode: the first [`FIRST_RECORDS`], then the first of each kind (cause, operation,
/// kind, field) until there are this many.
pub const RECORDS: usize = 100;
pub const FIRST_RECORDS: usize = 20;
/// Taints kept at most per table (names, objects).
const TAINT_CAP: usize = 1 << 18;
/// Episodes kept for `ctl episodes` (the counters cover all of them).
const KEEP: usize = 64;
/// How often an open episode logs its progress.
const PROGRESS_EVERY: Duration = Duration::from_secs(10);

/// One fault of the burst that opened an episode (reported as a mismatch before the episode was known).
#[derive(Clone, Debug, Serialize)]
pub struct BurstFault {
    #[serde(with = "systime")]
    pub time: SystemTime,
    pub op: OpKind,
    pub path: String,
    pub errno: &'static str,
}

/// A mismatch counted in an episode, kept verbatim.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
    /// `fault` (the secondary failed) or `knock-on` (about something that diverged in the episode).
    pub cause: &'static str,
    pub mismatch: Mismatch,
}

type RecordKey = (&'static str, OpKind, MismatchKind, Option<String>);

#[derive(Clone, Debug, Serialize)]
pub struct Episode {
    pub id: u64,
    /// The first fault of the burst that opened it.
    #[serde(with = "systime")]
    pub started: SystemTime,
    #[serde(with = "systime")]
    pub opened: SystemTime,
    /// `None` while it is open.
    #[serde(serialize_with = "systime_opt")]
    pub ended: Option<SystemTime>,
    /// `quiet` (no fault for the quiet period, and the secondary answered) or `detached`.
    pub end_reason: Option<&'static str>,
    /// The faults that opened it, oldest first: they were reported as mismatches.
    pub burst: Vec<BurstFault>,
    /// Mismatches counted in it: `faults` + `knock_ons`.
    pub mismatches: u64,
    pub faults: u64,
    pub knock_ons: u64,
    pub by_op: BTreeMap<&'static str, u64>,
    /// Of the faults.
    pub by_errno: BTreeMap<&'static str, u64>,
    pub by_kind: BTreeMap<&'static str, u64>,
    /// Distinct objects (directories included) and names it tainted.
    pub objects: u64,
    pub names: u64,
    /// Taints not recorded (the tables were full): knock-ons on those were reported one by one.
    pub untracked: u64,
    /// Repairs deferred while it was open, and those that did not fit in the queue (left for the next mismatch on the
    /// object).
    pub repairs_deferred: u64,
    pub repairs_dropped: u64,
    /// Examples of the mismatches counted in it ([`RECORDS`]).
    pub records: Vec<Record>,
    #[serde(skip)]
    recorded: HashSet<RecordKey>,
}

fn systime_opt<S: serde::Serializer>(t: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => systime::serialize(t, s),
        None => s.serialize_none(),
    }
}

impl Episode {
    pub fn is_open(&self) -> bool {
        self.ended.is_none()
    }

    /// One line: what failed, how often, since when.
    pub fn describe(&self) -> String {
        let top = |m: &BTreeMap<&'static str, u64>| {
            let mut v: Vec<(&&str, &u64)> = m.iter().collect();
            v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            v.iter().take(6).map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ")
        };
        let secs = self.ended.unwrap_or_else(SystemTime::now).duration_since(self.started).unwrap_or_default().as_secs();
        let mut s = format!(
            "{} after {secs}s; opened by {} faults reported as mismatches (first: {}), then counted {} mismatches \
             ({} faults, {} knock-ons) on {} objects and {} names",
            if self.is_open() { "open" } else { "ended" },
            self.burst.len(),
            self.burst.first().map(|f| format!("{} {} {}", f.op.name(), f.path, f.errno)).unwrap_or_default(),
            self.mismatches,
            self.faults,
            self.knock_ons,
            self.objects,
            self.names,
        );
        for (what, m) in [("errnos", &self.by_errno), ("ops", &self.by_op), ("kinds", &self.by_kind)] {
            if !m.is_empty() {
                s += &format!("; {what}: {}", top(m));
            }
        }
        if self.repairs_deferred > 0 || self.repairs_dropped > 0 {
            s += &format!("; repairs deferred {}, not deferred (queue full) {}", self.repairs_deferred, self.repairs_dropped);
        }
        if self.untracked > 0 {
            s += &format!("; {} objects not tracked (their knock-ons were reported)", self.untracked);
        }
        s
    }
}

/// The result of [`Episodes::observe`].
pub enum Seen {
    /// Not an episode's business: report it.
    No,
    /// Counted in the open episode.
    Counted,
    /// A fault that opened this episode: report it, and the episode.
    Opened(Box<Episode>),
}

/// One fault of the current burst.
struct Fault {
    at: Instant,
    /// The object: the name for a name-based mismatch, else the node.
    key: u64,
    /// Operations that ran on the secondary so far.
    ops: u64,
    ino: u64,
    entry: bool,
    /// The operation may have changed the object (it taints it).
    mutating: bool,
    info: BurstFault,
}

#[derive(Default)]
struct State {
    burst: VecDeque<Fault>,
    /// Newest last; the last one is open while `Episodes::open` is set.
    list: VecDeque<Episode>,
    next_id: u64,
    last_fault: Option<Instant>,
    /// What the open episode tainted: names (paths) and objects (node ids). Cleared when it ends.
    names: HashSet<String>,
    objects: HashSet<u64>,
    progress: Option<(Instant, u64)>,
}

impl State {
    fn current(&mut self) -> Option<&mut Episode> {
        self.list.back_mut().filter(|e| e.is_open())
    }

    /// Whether a mismatch that is no fault is a knock-on of the open episode.
    fn tainted(&self, m: &Mismatch) -> bool {
        if m.entry { self.names.contains(&m.path) } else { self.objects.contains(&m.ino) || self.names.contains(&m.path) }
    }

    /// Taints what a mutating mismatch counted in the open episode touched: the object, and for a name-based one the
    /// name too (`ino` is then the directory).
    fn taint(&mut self, ino: u64, entry: bool, path: &str) {
        let (mut objects, mut names, mut untracked) = (0, 0, 0);
        if self.objects.len() >= TAINT_CAP && !self.objects.contains(&ino) {
            untracked += 1;
        } else if self.objects.insert(ino) {
            objects += 1;
        }
        if entry && !path.is_empty() {
            if self.names.len() >= TAINT_CAP && !self.names.contains(path) {
                untracked += 1;
            } else if self.names.insert(path.to_string()) {
                names += 1;
            }
        }
        if let Some(e) = self.current() {
            e.objects += objects;
            e.names += names;
            e.untracked += untracked;
        }
    }
}

pub struct Episodes {
    cfg: EpisodeConfig,
    /// An episode is open (read for every mismatch and every repair).
    open: AtomicBool,
    st: Mutex<State>,
}

fn object_key(m: &Mismatch) -> u64 {
    if m.entry { xxhash_rust::xxh3::xxh3_64(m.path.as_bytes()) } else { m.ino }
}

impl Episodes {
    pub fn new(cfg: EpisodeConfig) -> Episodes {
        Episodes { cfg, open: AtomicBool::new(false), st: Mutex::new(State::default()) }
    }

    pub fn config(&self) -> &EpisodeConfig {
        &self.cfg
    }

    #[inline]
    pub fn is_open(&self) -> bool {
        self.open.load(Relaxed)
    }

    /// Whether `m` may be an episode's business: a fault, or anything while an episode is open.
    #[inline]
    pub fn concerns(&self, m: &Mismatch) -> bool {
        self.cfg.errors > 0 && (m.fault.is_some() || self.is_open())
    }

    /// Counts `m` in the open episode when it is a fault or a knock-on, or notes a fault in the current burst (which
    /// may open an episode).
    pub fn observe(&self, m: &Mismatch, stats: &Stats) -> Seen {
        let mut st = self.st.lock();
        let open = st.current().is_some();
        let cause = match (m.fault, open) {
            (Some(e), false) => {
                return match self.burst(&mut st, m, e, stats) {
                    Some(ep) => Seen::Opened(Box::new(ep)),
                    None => Seen::No,
                };
            }
            (Some(_), true) => {
                st.last_fault = Some(Instant::now());
                "fault"
            }
            (None, true) if st.tainted(m) => "knock-on",
            (None, _) => return Seen::No,
        };
        // (what a read-only operation found changed nothing; a knock-on spreads: a rename of a missing name, say,
        // makes the target directory differ too)
        if !m.retryable {
            st.taint(m.ino, m.entry, &m.path);
        }
        let Some(ep) = st.current() else { return Seen::No };
        ep.mismatches += 1;
        if m.fault.is_some() { ep.faults += 1 } else { ep.knock_ons += 1 }
        *ep.by_op.entry(m.op.name()).or_default() += 1;
        *ep.by_kind.entry(m.kind.name()).or_default() += 1;
        if let Some(e) = m.fault {
            *ep.by_errno.entry(errno_name(e)).or_default() += 1;
        }
        let key = (cause, m.op, m.kind, m.field.clone());
        if ep.records.len() < FIRST_RECORDS || (ep.records.len() < RECORDS && !ep.recorded.contains(&key)) {
            ep.records.push(Record { cause, mismatch: m.clone() });
        }
        ep.recorded.insert(key);
        stats.episode_mismatches.fetch_add(1, Relaxed);
        Seen::Counted
    }

    /// Adds a fault to the burst; opens an episode when the burst qualifies.
    fn burst(&self, st: &mut State, m: &Mismatch, errno: i32, stats: &Stats) -> Option<Episode> {
        // (fewer faults than objects could never be on enough objects; the command line refuses such a number)
        let errors = (self.cfg.errors as usize).max(OBJECTS);
        let now = Instant::now();
        let ran = stats.ops_done().saturating_sub(stats.secondary_skipped.load(Relaxed));
        st.burst.push_back(Fault {
            at: now,
            key: object_key(m),
            ops: ran,
            ino: m.ino,
            entry: m.entry,
            mutating: !m.retryable,
            info: BurstFault { time: m.time, op: m.op, path: m.path.clone(), errno: errno_name(errno) },
        });
        while st.burst.len() > errors || st.burst.front().is_some_and(|f| now.duration_since(f.at) > WINDOW) {
            st.burst.pop_front();
        }
        if st.burst.len() < errors {
            return None;
        }
        let mut keys: Vec<u64> = st.burst.iter().map(|f| f.key).collect();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() < OBJECTS {
            return None;
        }
        // (the operations of the faults themselves may not be counted yet: they are still running)
        let n = st.burst.len() as u64;
        let between = ran.saturating_sub(st.burst.front().map_or(ran, |f| f.ops)).max(n);
        if n * 100 < SHARE * between {
            return None;
        }
        st.next_id += 1;
        let id = st.next_id;
        let burst: Vec<Fault> = st.burst.drain(..).collect();
        let ep = Episode {
            id,
            started: burst.first().map_or(m.time, |f| f.info.time),
            opened: SystemTime::now(),
            ended: None,
            end_reason: None,
            burst: burst.iter().map(|f| f.info.clone()).collect(),
            mismatches: 0,
            faults: 0,
            knock_ons: 0,
            by_op: BTreeMap::new(),
            by_errno: BTreeMap::new(),
            by_kind: BTreeMap::new(),
            objects: 0,
            names: 0,
            untracked: 0,
            repairs_deferred: 0,
            repairs_dropped: 0,
            records: Vec::new(),
            recorded: HashSet::new(),
        };
        if st.list.len() == KEEP {
            st.list.pop_front();
        }
        st.list.push_back(ep);
        for f in burst.iter().filter(|f| f.mutating) {
            st.taint(f.ino, f.entry, &f.info.path);
        }
        st.last_fault = Some(now);
        st.progress = Some((now, 0));
        self.open.store(true, Relaxed);
        stats.episodes.fetch_add(1, Relaxed);
        st.list.back().cloned()
    }

    /// Ends the open episode after the quiet period, once `answers()` (a probe of the secondary, called without any
    /// lock held) says the secondary works again. Returns the episode that ended.
    pub fn tick(&self, answers: impl FnOnce() -> bool) -> Option<Episode> {
        if !self.is_open() {
            return None;
        }
        let last = self.st.lock().last_fault?;
        if last.elapsed() < self.cfg.quiet || !answers() {
            return None;
        }
        let mut st = self.st.lock();
        if st.last_fault != Some(last) {
            return None; // a fault meanwhile
        }
        self.end(&mut st, "quiet")
    }

    /// Ends the open episode (the secondary was detached).
    pub fn end_detached(&self) -> Option<Episode> {
        let mut st = self.st.lock();
        self.end(&mut st, "detached")
    }

    fn end(&self, st: &mut State, reason: &'static str) -> Option<Episode> {
        let ep = st.current()?;
        ep.ended = Some(SystemTime::now());
        ep.end_reason = Some(reason);
        let ep = ep.clone();
        st.burst.clear();
        st.names = HashSet::new();
        st.objects = HashSet::new();
        self.open.store(false, Relaxed);
        Some(ep)
    }

    /// The open episode's progress, at most every ten seconds and only when it counted more.
    pub fn progress(&self) -> Option<Episode> {
        let mut st = self.st.lock();
        let (at, n) = st.progress?;
        let ep = st.list.back().filter(|e| e.is_open())?;
        if at.elapsed() < PROGRESS_EVERY || ep.mismatches == n {
            return None;
        }
        let ep = ep.clone();
        st.progress = Some((Instant::now(), ep.mismatches));
        Some(ep)
    }

    /// A repair was deferred (`dropped`: it did not fit in the queue) while the current episode was open.
    pub fn deferred(&self, dropped: bool) {
        let mut st = self.st.lock();
        if let Some(ep) = st.current() {
            if dropped { ep.repairs_dropped += 1 } else { ep.repairs_deferred += 1 }
        }
    }

    /// The open episode's id and the mismatches counted in it so far (for the TUI's header, every tick).
    pub fn brief(&self) -> Option<(u64, u64)> {
        if !self.is_open() {
            return None;
        }
        self.st.lock().list.back().filter(|e| e.is_open()).map(|e| (e.id, e.mismatches))
    }

    /// The open episode.
    pub fn current(&self) -> Option<Episode> {
        self.st.lock().current().map(|e| e.clone())
    }

    /// The last `n` episodes, newest first.
    pub fn list(&self, n: usize) -> Vec<Episode> {
        self.st.lock().list.iter().rev().take(n).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EpisodeConfig {
        EpisodeConfig { errors: 4, quiet: Duration::ZERO, ..EpisodeConfig::default() }
    }

    fn mm(ino: u64, path: &str, entry: bool, fault: Option<i32>) -> Mismatch {
        Mismatch {
            id: 0,
            time: SystemTime::now(),
            op: if entry { OpKind::Create } else { OpKind::Write },
            kind: MismatchKind::Result,
            ino,
            path: path.into(),
            field: None,
            primary: "OK".into(),
            secondary: if fault.is_some() { "EIO".into() } else { "ENOENT".into() },
            detail: String::new(),
            retryable: false,
            resyncable: true,
            fault,
            entry,
        }
    }

    fn fault(ino: u64) -> Mismatch {
        mm(ino, &format!("/f{ino}"), false, Some(libc::EIO))
    }

    fn opened(s: Seen) -> bool {
        matches!(s, Seen::Opened(_))
    }

    #[test]
    fn faults() {
        for e in [libc::EIO, libc::ENOSPC, libc::EROFS, libc::ENOTCONN, libc::ESTALE, libc::EUCLEAN, libc::ENETUNREACH] {
            assert!(is_fault(e));
        }
        for e in [0, libc::ENOENT, libc::EEXIST, libc::EACCES, libc::EOPNOTSUPP, libc::EINVAL, libc::EMFILE] {
            assert!(!is_fault(e));
        }
    }

    #[test]
    fn a_burst_needs_several_objects_and_most_operations() {
        let stats = Stats::default();
        let ep = Episodes::new(cfg());
        // one object failing again and again
        for _ in 0..10 {
            assert!(!opened(ep.observe(&fault(7), &stats)));
        }
        // faults on many objects, but among many operations that worked
        for ino in 10..20 {
            stats.op(OpKind::Read).count.fetch_add(10, Relaxed);
            assert!(!opened(ep.observe(&fault(ino), &stats)));
        }
        assert!(!ep.is_open());
        // an outage: nothing works any more (the last fault above, among few operations, already counts)
        let mut opener = None;
        for ino in 20..24 {
            match ep.observe(&fault(ino), &stats) {
                Seen::Opened(e) => opener = Some((ino, e)),
                Seen::Counted => assert!(opener.is_some()),
                Seen::No => assert!(opener.is_none()),
            }
        }
        let (at, e) = opener.expect("an episode");
        assert!(ep.is_open() && e.is_open());
        assert_eq!(e.burst.len(), 4);
        assert_eq!(e.burst.last().unwrap().path, format!("/f{at}"));
        assert_eq!(stats.episodes.load(Relaxed), 1);
        // from now on faults are counted
        assert!(matches!(ep.observe(&fault(99), &stats), Seen::Counted));
        assert_eq!(ep.current().unwrap().faults, 23 - at + 1);
    }

    #[test]
    fn knock_ons_follow_what_diverged_while_the_episode_is_open() {
        let stats = Stats::default();
        let ep = Episodes::new(cfg());
        // creates in directory 5 fail on the secondary
        for i in 0..4 {
            ep.observe(&mm(5, &format!("/d/n{i}"), true, Some(libc::EIO)), &stats);
        }
        assert!(ep.is_open());
        // a name whose create failed is missing on the secondary: a knock-on
        assert!(matches!(ep.observe(&mm(5, "/d/n0", true, None), &stats), Seen::Counted));
        // another name of the directory is not
        assert!(matches!(ep.observe(&mm(5, "/d/other", true, None), &stats), Seen::No));
        // the directory itself (its listing, link count, times) is
        let mut listing = mm(5, "/d", false, None);
        listing.kind = MismatchKind::Readdir;
        listing.retryable = true;
        assert!(matches!(ep.observe(&listing, &stats), Seen::Counted));
        // a read-only fault is counted, but taints nothing
        let mut read = fault(42);
        read.retryable = true;
        assert!(matches!(ep.observe(&read, &stats), Seen::Counted));
        assert!(matches!(ep.observe(&mm(42, "/f42", false, None), &stats), Seen::No));
        let e = ep.current().unwrap();
        assert_eq!((e.faults, e.knock_ons, e.objects, e.names), (1, 2, 1, 4));
        // once the secondary works again for the quiet period, the episode ends, and so does the taint
        assert!(ep.tick(|| false).is_none(), "the secondary does not answer yet");
        let e = ep.tick(|| true).expect("ended");
        assert_eq!(e.end_reason, Some("quiet"));
        assert!(matches!(ep.observe(&mm(5, "/d/n1", true, None), &stats), Seen::No));
        assert!(matches!(ep.observe(&listing, &stats), Seen::No));
        assert_eq!(stats.episode_mismatches.load(Relaxed), 3);
    }

    /// Knock-ons are never counted in an episode that is gone from the list (only the open one counts anything).
    #[test]
    fn old_episodes_count_nothing() {
        let stats = Stats::default();
        let ep = Episodes::new(cfg());
        for round in 0..(KEEP as u64 + 6) {
            for i in 0..4 {
                ep.observe(&mm(5, &format!("/d/r{round}-{i}"), true, Some(libc::EIO)), &stats);
            }
            assert!(ep.is_open(), "round {round}");
            ep.tick(|| true).expect("ended");
        }
        assert_eq!(stats.episodes.load(Relaxed), KEEP as u64 + 6);
        assert_eq!(ep.list(usize::MAX).len(), KEEP);
        // what the first episode touched is an ordinary mismatch now: reported, not lost
        assert!(matches!(ep.observe(&mm(5, "/d/r0-0", true, None), &stats), Seen::No));
        assert_eq!(stats.episode_mismatches.load(Relaxed), 0);
    }

    #[test]
    fn records_keep_examples_of_every_kind() {
        let stats = Stats::default();
        let ep = Episodes::new(cfg());
        for ino in 0..200 {
            ep.observe(&fault(ino), &stats);
        }
        let mut late = fault(500);
        late.op = OpKind::Rename;
        ep.observe(&late, &stats);
        let e = ep.current().unwrap();
        assert_eq!((e.records.len(), e.faults, e.mismatches), (FIRST_RECORDS + 1, 197, 197));
        assert_eq!(e.records.last().unwrap().mismatch.op, OpKind::Rename);
        assert_eq!(e.by_errno.get("EIO"), Some(&197));
    }
}
