//! Byte-range locks and in-flight bookkeeping for data operations
//! (`--serialize relaxed`).
//!
//! In relaxed mode, operations that change neither a file's size nor its
//! metadata (in-place writes, reads, `fallocate` inside the file) hold the
//! object's stripe lock *shared* and additionally a byte-range lock. Writes
//! to disjoint ranges commute on every POSIX file system, so they may run
//! concurrently (and reach both file systems concurrently) while overlapping
//! ranges keep one order. Anything that changes the size holds the stripe
//! exclusively, as in strict mode.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};

use parking_lot::{Condvar, Mutex};

/// A table of held byte ranges with first-come-first-served fairness: a
/// request waits for every *earlier* conflicting request, held or waiting, so
/// a stream of readers cannot starve a writer.
#[derive(Default)]
pub struct RangeLocks {
    st: Mutex<RangeState>,
    cv: Condvar,
}

#[derive(Default)]
struct RangeState {
    next: u64,
    /// (ticket, start, end, exclusive, granted)
    reqs: Vec<(u64, u64, u64, bool, bool)>,
}

fn conflicts(a: (u64, u64, bool), b: (u64, u64, bool)) -> bool {
    a.0 < b.1 && b.0 < a.1 && (a.2 || b.2)
}

impl RangeLocks {
    /// Locks `[start, end)`; returns the guard and whether it had to wait.
    pub fn lock(&self, start: u64, end: u64, exclusive: bool) -> (RangeGuard<'_>, bool) {
        self.lock_many(&[(start, end, exclusive)])
    }

    /// Locks several ranges of this table as one request: it is granted when none of them conflicts with an
    /// earlier request, and holds all of them. (One request per operation matters: an operation that took
    /// its ranges one after the other could wait for a queued request that itself waits for a range the
    /// operation already holds.)
    pub fn lock_many(&self, ranges: &[(u64, u64, bool)]) -> (RangeGuard<'_>, bool) {
        let mut st = self.st.lock();
        let ticket = st.next;
        st.next += 1;
        for &(start, end, exclusive) in ranges {
            st.reqs.push((ticket, start, end.max(start + 1), exclusive, false));
        }
        let mut waited = false;
        loop {
            let blocked = st.reqs.iter().any(|r| {
                r.0 < ticket && ranges.iter().any(|&(s, e, x)| conflicts((r.1, r.2, r.3), (s, e.max(s + 1), x)))
            });
            if !blocked {
                break;
            }
            waited = true;
            self.cv.wait(&mut st);
        }
        for r in st.reqs.iter_mut().filter(|r| r.0 == ticket) {
            r.4 = true;
        }
        (RangeGuard { locks: self, ticket }, waited)
    }
}

pub struct RangeGuard<'a> {
    locks: &'a RangeLocks,
    ticket: u64,
}

impl Drop for RangeGuard<'_> {
    fn drop(&mut self) {
        let mut st = self.locks.st.lock();
        st.reqs.retain(|r| r.0 != self.ticket);
        self.locks.cv.notify_all();
    }
}

/// Data operations in flight on one object, for the racy-stat rule: a stat
/// that overlapped an in-place data operation may see one file system's
/// timestamps updated and the other's not yet.
#[derive(Default)]
pub struct DataInFlight {
    count: AtomicU32,
    seq: AtomicU64,
}

/// What a stat saw before it ran.
#[derive(Clone, Copy, Debug)]
pub struct DataSnap {
    count: u32,
    seq: u64,
}

impl DataInFlight {
    pub fn enter(&self) -> DataOpGuard<'_> {
        let before = self.count.fetch_add(1, SeqCst);
        self.seq.fetch_add(1, SeqCst);
        DataOpGuard { d: self, concurrent: before > 0 }
    }

    pub fn snap(&self) -> DataSnap {
        // `enter` bumps `count` and then `seq`: reading them in the opposite order means that an operation that
        // enters in between is seen either as in flight (`count`) or as a changed `seq`. (Reading `count` first
        // would miss an operation that enters between the two loads and is gone again by the end of the stat.)
        let seq = self.seq.load(SeqCst);
        DataSnap { count: self.count.load(SeqCst), seq }
    }

    /// Whether a data operation overlapped the interval since `snap`.
    pub fn overlapped(&self, snap: DataSnap) -> bool {
        snap.count > 0 || self.count.load(SeqCst) > 0 || self.seq.load(SeqCst) != snap.seq
    }
}

pub struct DataOpGuard<'a> {
    d: &'a DataInFlight,
    /// Another data operation on the object was in flight at entry.
    pub concurrent: bool,
}

impl Drop for DataOpGuard<'_> {
    fn drop(&mut self) {
        self.d.count.fetch_sub(1, SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn disjoint_ranges_do_not_block_overlapping_do() {
        let l = Arc::new(RangeLocks::default());
        let (_a, w) = l.lock(0, 100, true);
        assert!(!w);
        let (_b, w) = l.lock(100, 200, true);
        assert!(!w, "disjoint");
        let (_c, w) = l.lock(300, 400, false);
        assert!(!w);
        let (_d, w) = l.lock(300, 400, false);
        assert!(!w, "shared/shared");
        let l2 = l.clone();
        let t = std::thread::spawn(move || l2.lock(50, 60, false).1);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished(), "overlapping a held exclusive range must wait");
        drop(_a);
        assert!(t.join().unwrap());
    }

    #[test]
    fn waiting_writer_is_not_starved_by_later_readers() {
        let l = Arc::new(RangeLocks::default());
        let (r1, _) = l.lock(0, 10, false);
        let l2 = l.clone();
        let w = std::thread::spawn(move || {
            let _g = l2.lock(0, 10, true);
        });
        std::thread::sleep(Duration::from_millis(30));
        let l3 = l.clone();
        let r2 = std::thread::spawn(move || {
            let _g = l3.lock(0, 10, false);
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!r2.is_finished(), "a later reader queues behind the waiting writer");
        drop(r1);
        w.join().unwrap();
        r2.join().unwrap();
    }

    /// An operation with two ranges (copy_file_range inside one file) is one request: with the ranges taken one
    /// by one, this interleaving would deadlock (the writer queues behind the first range and the second range
    /// queues behind the writer).
    #[test]
    fn several_ranges_are_one_request() {
        let l = Arc::new(RangeLocks::default());
        let (first, _) = l.lock(0, 10, false); // somebody holds the source range of the copy
        let l2 = l.clone();
        let writer = std::thread::spawn(move || {
            let _g = l2.lock(5, 25, true); // queues behind it
        });
        std::thread::sleep(Duration::from_millis(30));
        let l3 = l.clone();
        // the copy asks for both its ranges as one request, after the writer
        let copy = std::thread::spawn(move || {
            let _g = l3.lock_many(&[(0, 10, false), (20, 30, true)]);
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!writer.is_finished() && !copy.is_finished());
        drop(first);
        writer.join().unwrap();
        copy.join().unwrap();
    }

    #[test]
    fn data_snap_detects_overlap() {
        let d = DataInFlight::default();
        let s = d.snap();
        assert!(!d.overlapped(s));
        {
            let _g = d.enter();
            assert!(d.overlapped(s));
        }
        assert!(d.overlapped(s), "an op that started and ended since the snapshot");
        assert!(!d.overlapped(d.snap()));
    }
}
