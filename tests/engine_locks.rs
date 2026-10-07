//! Mirrored POSIX record locks: conflicts between owners, queued blocking requests granted in FIFO order,
//! release on flush, lock mismatch detection, and concurrent lock traffic.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::backend::Lock;
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::policy::MismatchKind as K;

const A: u64 = 0xA1;
const B: u64 = 0xB2;
const C: u64 = 0xC3;
const D: u64 = 0xD4;

const R: i32 = libc::F_RDLCK;
const W: i32 = libc::F_WRLCK;
const U: i32 = libc::F_UNLCK;

/// Harness with a writable file `/lf`, opened once (the engine opens its own per-owner descriptors).
fn setup(level: CheckLevel, mode: MismatchMode) -> (Arc<Harness>, u64, Fh) {
    let h = Arc::new(Harness::new(level, mode));
    let f = h.write_file("/lf", &pattern(1, 4096));
    let fh = h.open("/lf", libc::O_RDWR);
    (h, f.id, fh)
}

fn nothing_yet(rx: &Receiver<Result<(), i32>>) {
    match rx.recv_timeout(Duration::from_millis(60)) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("expected the request to still be queued, got {other:?}"),
    }
}

fn granted(rx: &Receiver<Result<(), i32>>) {
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).expect("lock request was not answered"), Ok(()));
}

#[test]
fn conflicting_locks_between_two_owners() {
    for level in [CheckLevel::Basic, CheckLevel::Thorough, CheckLevel::Paranoid] {
        let (h, ino, _fh) = setup(level, MismatchMode::Log);
        assert_eq!(h.setlk(ino, A, W, 0, 100), Ok(()));
        // overlapping write/write and read/write conflict
        assert_eq!(h.setlk(ino, B, W, 50, 100), Err(libc::EAGAIN));
        assert_eq!(h.setlk(ino, B, R, 99, 1), Err(libc::EAGAIN));
        // adjacent range is fine
        assert_eq!(h.setlk(ino, B, W, 100, 50), Ok(()));
        // the owner itself never conflicts with its own locks (upgrade, downgrade, extend)
        assert_eq!(h.setlk(ino, A, R, 0, 100), Ok(()));
        assert_eq!(h.setlk(ino, A, W, 0, 100), Ok(()));
        // shared read locks coexist, a writer is kept out
        assert_eq!(h.setlk(ino, A, R, 200, 100), Ok(()));
        assert_eq!(h.setlk(ino, B, R, 250, 100), Ok(()));
        assert_eq!(h.setlk(ino, C, W, 260, 10), Err(libc::EAGAIN));
        assert_eq!(h.setlk(ino, C, R, 260, 10), Ok(()));
        // to-EOF locks (len 0)
        assert_eq!(h.setlk(ino, D, W, 1000, 0), Ok(()));
        assert_eq!(h.setlk(ino, A, W, 1_000_000, 1), Err(libc::EAGAIN));
        // unlocking a range of a lock splits it
        assert_eq!(h.setlk(ino, A, U, 40, 20), Ok(()));
        assert_eq!(h.setlk(ino, B, W, 45, 10), Ok(()));
        assert_eq!(h.setlk(ino, B, W, 30, 10), Err(libc::EAGAIN));
        // unlock everything, then the former conflict is gone
        assert_eq!(h.setlk(ino, A, U, 0, 0), Ok(()));
        assert_eq!(h.setlk(ino, C, W, 0, 40), Ok(()));
        // invalid type
        assert_eq!(h.setlk(ino, C, 42, 0, 1), Err(libc::EINVAL));
        h.assert_no_mismatches();
    }
}

#[test]
fn getlk_reports_the_blocking_lock() {
    let (h, ino, _fh) = setup(CheckLevel::Thorough, MismatchMode::Log);
    h.setlk(ino, A, W, 10, 20).unwrap();
    // B asks about an overlapping range: A's lock comes back
    let l = h.getlk(ino, B, W, 0, 15).unwrap();
    assert_eq!((l.typ, l.start, l.len), (W, 10, 20));
    // a read lock of A conflicts only with write requests
    h.setlk(ino, A, R, 100, 10).unwrap();
    let l = h.getlk(ino, B, R, 100, 10).unwrap();
    assert_eq!(l.typ, U, "read vs read: no conflict");
    let l = h.getlk(ino, B, W, 100, 10).unwrap();
    assert_eq!((l.typ, l.start, l.len), (R, 100, 10));
    // the owner's own lock is not a conflict
    assert_eq!(h.getlk(ino, A, W, 10, 20).unwrap().typ, U);
    // free range
    let l = h.getlk(ino, B, W, 500, 5).unwrap();
    assert_eq!((l.typ, l.start, l.len), (U, 500, 5));
    h.assert_no_mismatches();
}

#[test]
fn blocking_requests_are_queued_and_granted_in_fifo_order() {
    for level in [CheckLevel::Basic, CheckLevel::Paranoid] {
        let (h, ino, _fh) = setup(level, MismatchMode::Log);
        h.setlk(ino, A, W, 0, 10).unwrap();
        let rb = h.setlkw(ino, B, W, 0, 10);
        let rc = h.setlkw(ino, C, W, 0, 10);
        let rd = h.setlkw(ino, D, W, 0, 10);
        for rx in [&rb, &rc, &rd] {
            nothing_yet(rx);
        }
        assert_eq!(h.stats.lock_waiters.load(SeqCst), 3);

        h.setlk(ino, A, U, 0, 10).unwrap();
        granted(&rb);
        nothing_yet(&rc);
        nothing_yet(&rd);
        assert_eq!(h.stats.lock_waiters.load(SeqCst), 2);
        // B really holds it now, on both file systems
        assert_eq!(h.getlk(ino, D, W, 0, 10).unwrap().typ, W);

        h.setlk(ino, B, U, 0, 10).unwrap();
        granted(&rc);
        nothing_yet(&rd);
        h.setlk(ino, C, U, 0, 10).unwrap();
        granted(&rd);
        assert_eq!(h.stats.lock_waiters.load(SeqCst), 0);
        h.assert_no_mismatches();
    }
}

#[test]
fn shared_waiters_are_granted_together_and_a_writer_keeps_waiting() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    let rb = h.setlkw(ino, B, R, 0, 10);
    let rc = h.setlkw(ino, C, R, 0, 10);
    let rd = h.setlkw(ino, D, W, 0, 10);
    h.setlk(ino, A, U, 0, 10).unwrap();
    granted(&rb);
    granted(&rc);
    nothing_yet(&rd);
    h.setlk(ino, B, U, 0, 10).unwrap();
    nothing_yet(&rd);
    h.setlk(ino, C, U, 0, 10).unwrap();
    granted(&rd);
    h.assert_no_mismatches();
}

#[test]
fn partial_unlock_wakes_a_waiter_only_when_the_conflict_is_gone() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 100).unwrap();
    let rb = h.setlkw(ino, B, W, 50, 10);
    h.setlk(ino, A, U, 0, 40).unwrap();
    nothing_yet(&rb);
    h.setlk(ino, A, U, 40, 30).unwrap(); // frees [40,70)
    granted(&rb);
    h.assert_no_mismatches();
}

#[test]
fn flush_releases_the_owners_locks_and_wakes_waiters() {
    let (h, ino, fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    h.setlk(ino, A, R, 100, 10).unwrap();
    let rb = h.setlkw(ino, B, W, 0, 10);
    nothing_yet(&rb);
    // closing a descriptor of ANOTHER owner changes nothing
    h.engine.flush(&h.ctx, ino, fh.fh, C).unwrap();
    nothing_yet(&rb);
    assert_eq!(h.getlk(ino, D, W, 0, 10).unwrap().typ, W);
    // closing A's descriptor releases everything A held (POSIX semantics) and grants B
    h.engine.flush(&h.ctx, ino, fh.fh, A).unwrap();
    granted(&rb);
    assert_eq!(h.getlk(ino, D, W, 100, 10).unwrap().typ, U, "A's second lock is gone too");
    assert_eq!(h.getlk(ino, D, W, 0, 10).unwrap().typ, W, "B holds it now");
    h.assert_no_mismatches();
}

#[test]
fn flush_of_a_waiting_owner_cancels_its_request_with_eintr() {
    let (h, ino, fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    let rb = h.setlkw(ino, B, W, 0, 10);
    let rc = h.setlkw(ino, C, W, 0, 10);
    h.engine.flush(&h.ctx, ino, fh.fh, B).unwrap();
    assert_eq!(rb.recv_timeout(Duration::from_secs(5)).unwrap(), Err(libc::EINTR));
    nothing_yet(&rc);
    assert_eq!(h.stats.lock_waiters.load(SeqCst), 1);
    h.engine.flush(&h.ctx, ino, fh.fh, A).unwrap();
    granted(&rc);
    h.assert_no_mismatches();
}

#[test]
fn unlock_by_an_owner_without_locks_is_a_noop() {
    let (h, ino, _fh) = setup(CheckLevel::Thorough, MismatchMode::Log);
    assert_eq!(h.setlk(ino, A, U, 0, 0), Ok(()));
    h.assert_no_mismatches();
}

// ----------------------------------------------------------------------------- lock mismatch detection

#[test]
fn lock_always_ok_on_the_secondary_is_a_lock_mismatch() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.fault.inject(FaultOp::Setlk, Effect::LockAlwaysOk);
    h.setlk(ino, A, W, 0, 10).unwrap();
    h.assert_no_mismatches();
    // B conflicts on the primary, while the secondary (which never took A's lock) grants it
    assert_eq!(h.setlk(ino, B, W, 0, 10), Err(libc::EAGAIN), "the primary's answer is returned");
    let m = h.expect_mismatch(K::Lock, Some("setlk"));
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("conflict", "OK"));
}

#[test]
fn lock_always_conflict_on_the_secondary_is_a_lock_mismatch() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.fault.inject(FaultOp::Setlk, Effect::LockAlwaysConflict);
    assert_eq!(h.setlk(ino, A, W, 0, 10), Ok(()));
    let m = h.expect_mismatch(K::Lock, Some("setlk"));
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "conflict"));
}

#[test]
fn lock_errno_on_the_secondary_is_a_lock_mismatch() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.fault.inject(FaultOp::Setlk, Effect::Errno(libc::ENOLCK));
    assert_eq!(h.setlk(ino, A, W, 0, 10), Ok(()));
    let m = h.expect_mismatch(K::Lock, Some("setlk"));
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "ENOLCK"));
}

#[test]
fn lock_mismatch_in_fail_mode_returns_eio() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Fail);
    h.fault.inject(FaultOp::Setlk, Effect::LockAlwaysConflict);
    assert_eq!(h.setlk(ino, A, W, 0, 10), Err(libc::EIO));
}

#[test]
fn getlk_faults_are_lock_mismatches() {
    // secondary claims nothing is locked
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    h.fault.inject(FaultOp::Getlk, Effect::GetlkFree);
    let l = h.getlk(ino, B, W, 0, 10).unwrap();
    assert_eq!(l.typ, W, "primary's answer");
    h.expect_mismatch(K::Lock, Some("getlk"));

    // secondary reports a different conflicting range / type
    for lie in [Lock { typ: W, start: 5, len: 5, pid: 0 }, Lock { typ: R, start: 0, len: 10, pid: 0 }, Lock { typ: W, start: 0, len: 11, pid: 0 }] {
        let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
        h.setlk(ino, A, W, 0, 10).unwrap();
        h.fault.inject(FaultOp::Getlk, Effect::GetlkResult(lie));
        h.getlk(ino, B, W, 0, 10).unwrap();
        h.expect_mismatch(K::Lock, Some("getlk"));
    }

    // identical answers are fine
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    h.fault.inject(FaultOp::Getlk, Effect::GetlkResult(Lock { typ: W, start: 0, len: 10, pid: 77 }));
    h.getlk(ino, B, W, 0, 10).unwrap();
    h.assert_no_mismatches(); // (the pid is not compared: it is not meaningful across owners)
}

/// A secondary that fails to wake a queued waiter's retry: the retry of a queued request is compared like any
/// other attempt.
#[test]
fn mismatch_while_granting_a_queued_waiter() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    h.setlk(ino, A, W, 0, 10).unwrap();
    let rb = h.setlkw(ino, B, W, 0, 10);
    nothing_yet(&rb);
    // from now on the secondary refuses B's retry
    h.fault.add(Fault::new(FaultOp::Setlk, Effect::LockAlwaysConflict));
    h.setlk(ino, A, U, 0, 10).unwrap();
    granted(&rb); // the primary granted it
    h.expect_mismatch(K::Lock, Some("setlk"));
}

// --------------------------------------------------------------------------------------- fuzz / stress

#[test]
fn random_lock_traffic_single_thread() {
    for level in [CheckLevel::Basic, CheckLevel::Thorough] {
        let (h, ino, fh) = setup(level, MismatchMode::Log);
        let mut rng = Rng::new(77);
        let owners = [A, B, C];
        let mut granted_cnt = 0;
        let mut refused = 0;
        for _ in 0..6000 {
            let o = *rng.pick(&owners);
            let start = rng.below(60);
            let len = if rng.below(10) == 0 { 0 } else { 1 + rng.below(30) };
            match rng.below(5) {
                0 => {
                    let _ = h.getlk(ino, o, *rng.pick(&[R, W]), start, len);
                }
                1 => {
                    let _ = h.setlk(ino, o, U, start, len);
                }
                2 => {
                    if rng.below(40) == 0 {
                        h.engine.flush(&h.ctx, ino, fh.fh, o).ok();
                    }
                }
                _ => match h.setlk(ino, o, *rng.pick(&[R, W]), start, len) {
                    Ok(()) => granted_cnt += 1,
                    Err(libc::EAGAIN) => refused += 1,
                    Err(e) => panic!("unexpected errno {e}"),
                },
            }
        }
        assert!(granted_cnt > 200 && refused > 200, "granted {granted_cnt} refused {refused}");
        h.assert_no_mismatches();
    }
}

/// Mutual exclusion through blocking locks across many threads: nobody may be inside the critical section together,
/// every request is eventually granted (no lost wake-up), and both file systems stay in agreement.
#[test]
fn blocking_lock_mutual_exclusion_across_threads() {
    let (h, ino, _fh) = setup(CheckLevel::Basic, MismatchMode::Log);
    let inside = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let (tx, done) = std::sync::mpsc::channel();
    let threads = 8;
    let rounds = 40;
    for t in 0..threads {
        let (h, inside, total, tx) = (h.clone(), inside.clone(), total.clone(), tx.clone());
        std::thread::spawn(move || {
            let owner = 0x1000 + t as u64;
            for _ in 0..rounds {
                let rx = h.setlkw(ino, owner, W, 0, 10);
                assert_eq!(rx.recv_timeout(Duration::from_secs(30)).expect("lost wake-up"), Ok(()));
                assert_eq!(inside.fetch_add(1, SeqCst), 0, "two owners inside the critical section");
                total.fetch_add(1, SeqCst);
                std::thread::yield_now();
                inside.fetch_sub(1, SeqCst);
                h.setlk(ino, owner, U, 0, 10).unwrap();
            }
            tx.send(()).unwrap();
        });
    }
    for _ in 0..threads {
        done.recv_timeout(Duration::from_secs(60)).expect("lock workers did not finish (deadlock?)");
    }
    assert_eq!(total.load(SeqCst), threads * rounds);
    assert_eq!(h.stats.lock_waiters.load(SeqCst), 0);
    h.assert_no_mismatches();
}

/// Lock traffic racing with data and namespace operations on the same file (lock state and stripe locks must not
/// deadlock or disagree).
#[test]
fn lock_traffic_concurrent_with_io() {
    let (h, ino, _fh) = setup(CheckLevel::Thorough, MismatchMode::Log);
    let (tx, done) = std::sync::mpsc::channel();
    for t in 0..4 {
        let (h, tx) = (h.clone(), tx.clone());
        std::thread::spawn(move || {
            let mut rng = Rng::new(900 + t);
            let owner = 0x2000 + t;
            for _ in 0..800 {
                let (s, l) = (rng.below(50), 1 + rng.below(20));
                match rng.below(4) {
                    0 => {
                        let _ = h.setlk(ino, owner, *rng.pick(&[R, W]), s, l);
                    }
                    1 => {
                        let _ = h.setlk(ino, owner, U, s, l);
                    }
                    2 => {
                        let _ = h.getlk(ino, owner, W, s, l);
                    }
                    _ => {
                        let f = h.open("/lf", libc::O_RDWR);
                        h.pwrite(f, rng.below(4000), &rng.blob(100));
                        h.pread(f, 0, 200);
                        h.close(f);
                    }
                }
            }
            tx.send(()).unwrap();
        });
    }
    for _ in 0..4 {
        done.recv_timeout(Duration::from_secs(60)).expect("deadlock?");
    }
    h.assert_no_mismatches();
    h.assert_trees_equal();
}
