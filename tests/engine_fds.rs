//! Running out of file descriptors: an operation whose descriptors do not fit
//! under RLIMIT_NOFILE fails with EMFILE before either side runs it — never
//! on one side only, which would be a mismatch (and, for a mutation, a
//! divergence) that is no disagreement of the file systems.
//!
//! Its own test binary: it lowers the process's descriptor limit.

mod common;

use common::*;
use std::sync::atomic::Ordering::Relaxed;

fn open_now() -> u64 {
    std::fs::read_dir("/proc/self/fd").unwrap().count() as u64
}

#[test]
fn running_out_of_descriptors_refuses_operations_on_both_sides_alike() {
    let h = Harness::thorough();
    // Room for about a hundred cached nodes past the engine's margin (what
    // is open, two per FUSE thread, 256): two descriptors each.
    let limit = open_now() + 2 * 16 + 256 + 200;
    let rl = libc::rlimit { rlim_cur: limit, rlim_max: limit.max(xcheckfs::sys::nofile_limit()) };
    // SAFETY: valid pointer.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) }, 0);
    // The budget is taken when the engine starts: start one under the limit.
    let h2 = Harness::thorough();
    drop(h);

    let mut made = Vec::new();
    let refused = loop {
        match h2.try_mkdir(&format!("/d{}", made.len()), 0o755) {
            Ok(a) => made.push(a.id),
            Err(e) => break e,
        }
        assert!(made.len() < 10_000, "never refused");
    };
    assert_eq!(refused, libc::EMFILE);
    assert!(made.len() > 20, "{} directories before the refusal", made.len());
    h2.assert_no_mismatches();
    let next = format!("/d{}", made.len());
    assert!(!h2.p_path(&next).exists() && !h2.s_path(&next).exists(), "refused on both sides");
    assert!(h2.stats.fd_refusals.load(Relaxed) >= 1);

    // The kernel forgetting nodes frees their descriptors.
    for id in made.drain(..10) {
        h2.engine.forget(id, 1);
    }
    h2.try_mkdir(&next, 0o755).expect("room again");
    h2.assert_no_mismatches();
}
