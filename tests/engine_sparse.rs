//! Huge sparse files: whole-file comparisons (paranoid mode, resync and its
//! verification) must cost what the data costs, not the logical size.

mod common;

use std::time::{Duration, Instant};

use common::*;
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::policy::MismatchKind;

const TIB: u64 = 1 << 40;

fn sparse_file(h: &Harness, path: &str) {
    let f = h.create(path);
    h.truncate(path, TIB).unwrap();
    h.pwrite(f, TIB / 2, b"middle");
    h.pwrite(f, TIB - 4, b"tail");
    h.close(f); // paranoid: the whole-file comparison runs here
}

#[test]
fn paranoid_close_of_a_terabyte_sparse_file_is_fast() {
    let h = Harness::new(CheckLevel::Paranoid, MismatchMode::Log);
    let t0 = Instant::now();
    sparse_file(&h, "/big");
    assert!(t0.elapsed() < Duration::from_secs(20), "took {:?}", t0.elapsed());
    h.assert_no_mismatches();
}

#[test]
fn sparse_file_corruption_is_found_and_repaired_quickly() {
    let h = Harness::new(CheckLevel::Paranoid, MismatchMode::Resync);
    sparse_file(&h, "/big");
    // damage the data on the secondary behind the engine's back
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(h.s_path("/big")).unwrap();
        f.write_all_at(b"X", TIB / 2).unwrap();
    }
    let t0 = Instant::now();
    let f = h.open("/big", libc::O_RDWR);
    h.pwrite(f, 0, b"head"); // marks the handle written: compared at close
    h.close(f);
    assert!(t0.elapsed() < Duration::from_secs(20), "took {:?}", t0.elapsed());
    h.expect_mismatch(MismatchKind::Content, None);
    assert!(h.stats.resyncs.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    assert_eq!(h.stats.resync_failures.load(std::sync::atomic::Ordering::Relaxed), 0);
    let mut buf = [0u8; 6];
    {
        use std::os::unix::fs::FileExt;
        std::fs::File::open(h.s_path("/big")).unwrap().read_exact_at(&mut buf, TIB / 2).unwrap();
    }
    assert_eq!(&buf, b"middle", "the secondary was repaired");
}
