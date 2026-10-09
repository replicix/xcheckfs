//! Descriptor accounting: an operation that could run out of descriptors
//! halfway fails before it touches either file system.
//!
//! xcheckfs keeps two descriptors per cached inode and per open handle (one
//! per side), so a large cached tree can exhaust `RLIMIT_NOFILE`. Running
//! out in the middle of an operation is the worst way to find out: one side
//! executes it and the other fails with `EMFILE`, a mismatch that is no
//! disagreement of the file systems — and, for a mutation, a divergence
//! that needs a repair. So every long-lived descriptor is counted
//! ([`CountedFd`]), and every operation first reserves the most
//! descriptors it can create ([`FdBudget::reserve`]); when that does not
//! fit under the limit, the operation fails with `EMFILE` on both sides
//! alike, before either runs it. Short-lived descriptors (an operation's
//! own temporary opens) are within its reservation; the rest of the
//! process's (stdio, log, control socket, the FUSE device clones) within a
//! margin set at mount.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};

/// Counted descriptors alive in the process.
static HELD: AtomicI64 = AtomicI64::new(0);

/// A descriptor xcheckfs keeps beyond one operation (a node's, an open
/// handle's, a lock owner's), counted against [`FdBudget`] while it lives.
#[derive(Debug)]
pub struct CountedFd(OwnedFd);

impl CountedFd {
    pub fn new(fd: OwnedFd) -> CountedFd {
        HELD.fetch_add(1, Relaxed);
        CountedFd(fd)
    }
}

impl Drop for CountedFd {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Relaxed);
    }
}

impl AsFd for CountedFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl std::os::fd::AsRawFd for CountedFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.0)
    }
}

/// Counted descriptors alive (for `ctl status`).
pub fn held() -> i64 {
    HELD.load(Relaxed)
}

/// What the descriptors operations may hold add up to at most.
pub struct FdBudget {
    /// Descriptors the counting does not cover.
    margin: i64,
    /// Reads the soft limit (a stub in tests).
    limit: fn() -> u64,
    /// The soft limit less `margin`, as last read.
    budget: AtomicI64,
    /// Descriptors operations in flight have reserved.
    reserved: AtomicI64,
}

/// Descriptors an operation reserved; returned when it ends.
pub struct Reservation<'a>(&'a AtomicI64, i64);

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Relaxed);
    }
}

impl FdBudget {
    /// The soft `RLIMIT_NOFILE` less `margin` descriptors the counting does
    /// not cover.
    pub fn new(margin: i64) -> FdBudget {
        let limit = crate::sys::nofile_limit;
        FdBudget { margin, limit, budget: AtomicI64::new(Self::from_limit(limit, margin)), reserved: AtomicI64::new(0) }
    }

    fn from_limit(limit: fn() -> u64, margin: i64) -> i64 {
        (limit().min(i64::MAX as u64) as i64).saturating_sub(margin).max(0)
    }

    /// Reserves `n` descriptors, or `None` when they do not fit. The
    /// reservation is taken before it is checked, so concurrent callers can
    /// only fail each other spuriously, never both pass. A refusal reads
    /// the limit again first: it may have been changed (`prlimit`) since.
    pub fn reserve(&self, n: i64) -> Option<Reservation<'_>> {
        if n == 0 {
            return Some(Reservation(&self.reserved, 0));
        }
        let reserved = self.reserved.fetch_add(n, Relaxed) + n;
        if HELD.load(Relaxed) + reserved > self.budget.load(Relaxed) {
            let budget = Self::from_limit(self.limit, self.margin);
            self.budget.store(budget, Relaxed);
            if HELD.load(Relaxed) + reserved > budget {
                self.reserved.fetch_sub(n, Relaxed);
                return None;
            }
        }
        Some(Reservation(&self.reserved, n))
    }

    pub fn budget(&self) -> i64 {
        self.budget.load(Relaxed)
    }
}

/// Descriptors open in this process now (`/proc/self/fd`), for the margin
/// set at mount. Not for the hot path: it reads a directory.
pub fn open_now() -> i64 {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // One limit per test: the tests run in parallel.
    static LIMIT: AtomicI64 = AtomicI64::new(0);
    static CHANGED: AtomicI64 = AtomicI64::new(0);

    fn stub() -> u64 {
        LIMIT.load(Relaxed) as u64
    }

    fn changed() -> u64 {
        CHANGED.load(Relaxed) as u64
    }

    #[test]
    fn a_reservation_that_does_not_fit_is_refused_and_returns_nothing() {
        LIMIT.store(held() + 10, Relaxed);
        let b = FdBudget { margin: 0, limit: stub, budget: AtomicI64::new(held() + 10), reserved: AtomicI64::new(0) };
        let r1 = b.reserve(6).expect("fits");
        assert!(b.reserve(6).is_none(), "6 + 6 > 10");
        let r2 = b.reserve(4).expect("6 + 4 fits");
        drop((r1, r2));
        let f = CountedFd::new(std::fs::File::open("/dev/null").unwrap().into());
        assert!(b.reserve(10).is_none(), "a held descriptor counts");
        drop(f);
        assert!(b.reserve(10).is_some());
    }

    #[test]
    fn a_limit_changed_after_mount_is_seen_at_the_next_refusal() {
        // Far above the descriptors the parallel test holds.
        let base = held() + 1_000_000;
        CHANGED.store(base, Relaxed);
        let b = FdBudget { margin: 0, limit: changed, budget: AtomicI64::new(base), reserved: AtomicI64::new(0) };
        CHANGED.store(base + 1_000_000, Relaxed);
        let r = b.reserve(base + 500_000).expect("raised by prlimit");
        assert_eq!(b.budget(), base + 1_000_000);
        drop(r);
        CHANGED.store(base / 2, Relaxed);
        let r = b.reserve(base).expect("fits the stale budget: not re-read");
        assert_eq!(b.budget(), base + 1_000_000, "a fitting reservation reads nothing");
        assert!(b.reserve(base + 1_000_000).is_none(), "refused by the stale budget too");
        assert_eq!(b.budget(), base / 2, "the refusal read the lowered limit");
        drop(r);
    }
}
