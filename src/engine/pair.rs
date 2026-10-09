//! Runs an operation's two halves at once: the secondary on a helper thread
//! of the calling thread, the primary on the caller.
//!
//! Each thread that issues operations (a FUSE worker, a test thread) gets its
//! own helper, created on first use and parked on a condition variable
//! between operations. So a secondary half never queues behind another
//! operation's, and an idle helper costs nothing. (It was a rayon pool: every
//! hand-off woke a worker, and idle workers spin through every other worker's
//! queue before they sleep. Under a metadata-heavy load that was most of the
//! process's CPU — 465 CPU-seconds in 36 s, against 86 with the halves run
//! one after the other — and cut a 4k random-write rate to a quarter.)

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct Slot {
    job: Option<Job>,
    done: bool,
    panic: Option<Box<dyn Any + Send>>,
    exit: bool,
}

#[derive(Default)]
struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
}

impl Shared {
    fn submit(&self, job: Job) {
        let mut slot = self.slot.lock().unwrap();
        slot.job = Some(job);
        slot.done = false;
        self.cv.notify_all();
    }

    /// Blocks until the submitted job has run; its panic, if it panicked.
    fn wait(&self) -> Option<Box<dyn Any + Send>> {
        let mut slot = self.slot.lock().unwrap();
        while !slot.done {
            slot = self.cv.wait(slot).unwrap();
        }
        slot.panic.take()
    }

    fn serve(&self) {
        loop {
            let job = {
                let mut slot = self.slot.lock().unwrap();
                loop {
                    if let Some(job) = slot.job.take() {
                        break job;
                    }
                    if slot.exit {
                        return;
                    }
                    slot = self.cv.wait(slot).unwrap();
                }
            };
            let panic = catch_unwind(AssertUnwindSafe(job)).err();
            let mut slot = self.slot.lock().unwrap();
            slot.panic = panic;
            slot.done = true;
            self.cv.notify_all();
        }
    }
}

struct Helper {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Helper {
    fn spawn() -> Option<Helper> {
        let shared = Arc::new(Shared::default());
        let serving = shared.clone();
        let name = format!(
            "{}-sec",
            std::thread::current().name().unwrap_or("xcheckfs")
        );
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || serving.serve())
            .ok()?;
        Some(Helper {
            shared,
            thread: Some(thread),
        })
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        self.shared.slot.lock().unwrap().exit = true;
        self.shared.cv.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

thread_local! {
    static HELPER: RefCell<Option<Helper>> = const { RefCell::new(None) };
    /// This thread's helper is running a half of an operation that is still
    /// in progress here: a nested `join` runs its halves in turn.
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

/// Waits for the helper's job when dropped, so the job cannot outlive the
/// frame it borrows from even if the primary half unwinds.
struct Pending<'a> {
    shared: &'a Shared,
    waited: bool,
}

impl Pending<'_> {
    fn wait(mut self) -> Option<Box<dyn Any + Send>> {
        self.waited = true;
        let panic = self.shared.wait();
        BUSY.with(|b| b.set(false));
        panic
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.waited {
            let _ = self.shared.wait();
            BUSY.with(|b| b.set(false));
        }
    }
}

/// Runs `secondary` on this thread's helper and `primary` here, and returns
/// both results once both have finished. A panic in either half is resumed
/// here, after both have finished. Runs the halves in turn when no helper
/// can be had (the thread could not be spawned, or a `join` is nested).
pub(crate) fn join<A, B: Send>(
    primary: impl FnOnce() -> A,
    secondary: impl FnOnce() -> B + Send,
) -> (A, B) {
    let shared = if BUSY.with(|b| b.get()) {
        None
    } else {
        HELPER.with(|h| {
            let mut h = h.borrow_mut();
            if h.is_none() {
                *h = Helper::spawn();
            }
            h.as_ref().map(|h| h.shared.clone())
        })
    };
    let Some(shared) = shared else {
        let a = primary();
        return (a, secondary());
    };
    let mut out: Option<B> = None;
    let slot = &mut out;
    let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || *slot = Some(secondary()));
    // SAFETY: the job borrows from this frame (`out`, and whatever
    // `secondary` captured). `Pending` waits for the helper to finish it
    // before this frame can be left, by return or by unwinding, and the
    // helper drops the job before it reports it done.
    let job: Job = unsafe { std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Job>(job) };
    BUSY.with(|b| b.set(true));
    shared.submit(job);
    let pending = Pending {
        shared: &shared,
        waited: false,
    };
    let a = primary();
    if let Some(panic) = pending.wait() {
        resume_unwind(panic);
    }
    (a, out.expect("the helper ran the secondary half"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn both_halves_run_and_overlap() {
        let t0 = Instant::now();
        let (a, b) = join(
            || {
                std::thread::sleep(Duration::from_millis(100));
                1
            },
            || {
                std::thread::sleep(Duration::from_millis(100));
                std::thread::current().name().map(str::to_owned)
            },
        );
        assert_eq!(a, 1);
        assert!(
            b.unwrap().ends_with("-sec"),
            "the secondary half ran on the helper"
        );
        assert!(
            t0.elapsed() < Duration::from_millis(190),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn borrows_from_the_caller_and_reuses_the_helper() {
        let data = [1, 2, 3];
        for i in 0..1000 {
            let (a, b) = join(|| data.len() + i, || data.iter().sum::<i32>());
            assert_eq!((a, b), (3 + i, 6));
        }
    }

    #[test]
    fn a_nested_join_runs_in_turn() {
        let (a, (b, c)) = join(|| 1, || join(|| 2, || 3));
        assert_eq!((a, b, c), (1, 2, 3));
        let ((a, b), c) = join(|| join(|| 1, || 2), || 3);
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn a_panicking_half_is_resumed_after_both_finished() {
        let finished = std::sync::atomic::AtomicBool::new(false);
        let r = catch_unwind(AssertUnwindSafe(|| {
            join(
                || panic!("primary"),
                || {
                    std::thread::sleep(Duration::from_millis(50));
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                },
            )
        }));
        assert!(r.is_err());
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "the secondary finished before the unwind"
        );
        let r = catch_unwind(AssertUnwindSafe(|| {
            join(|| 1, || -> i32 { panic!("secondary") })
        }));
        assert!(r.is_err());
        assert_eq!(
            join(|| 1, || 2),
            (1, 2),
            "the helper survives a panicking job"
        );
    }
}
