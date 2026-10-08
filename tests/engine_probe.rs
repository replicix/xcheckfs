//! The mount-time probe. The secondary is made to behave like another kind of file system with fault effects that
//! are in place before the engine starts, so the probe sees them: where the behavior is one POSIX allows, the
//! engine adapts (no mismatch; without the probe the difference is reported); a neighboring behavior that is a
//! defect is still reported; differences in supported operations are listed; the probe leaves no trace.

mod common;

use std::os::unix::fs::MetadataExt;
use std::sync::atomic::Ordering::Relaxed;

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::config::CheckLevel;
use xcheckfs::policy::MismatchKind as K;

const OLD: i64 = 1_000_000;

fn rig(probe: bool, f: Fault) -> Harness {
    Harness::builder().level(CheckLevel::Thorough).secondary_fault(f).config(move |c| c.probe = probe).build()
}

fn aligned(h: &Harness) -> u64 {
    h.stats.aligned_mtimes.load(Relaxed)
}

fn adapted(h: &Harness, what: &str) -> bool {
    h.engine.adaptations().iter().any(|a| a.contains(what))
}

/// A directory moved to another parent: one file system stamps its mtime (f2fs does), the other does not.
#[test]
fn directory_moved_to_another_parent() {
    for probe in [true, false] {
        let h = rig(probe, Fault::new(FaultOp::Rename, Effect::StampMtime));
        if probe && !adapted(&h, "moved to another parent") {
            eprintln!("SKIP: the primary stamps moved directories too");
            return;
        }
        h.mkdir_p("/a/x");
        h.mkdir_p("/b");
        h.set_mtime("/a/x", OLD).unwrap();
        h.rename("/a/x", "/a/y"); // (the same parent: no file system stamps it)
        h.rename("/a/y", "/b/x");
        h.getattr("/b/x");
        if probe {
            h.assert_no_mismatches();
            h.assert_trees_equal();
            assert_eq!(aligned(&h), 1);
        } else {
            h.expect_mismatch(K::Attr, Some("mtime"));
        }
    }
}

/// `RENAME_EXCHANGE` of directories in different parents: xfs and f2fs stamp both.
#[test]
fn directories_exchanged_between_parents() {
    for probe in [true, false] {
        let h = rig(probe, Fault::new(FaultOp::Rename, Effect::StampMtime));
        if probe && !adapted(&h, "RENAME_EXCHANGE") {
            eprintln!("SKIP: the primary stamps exchanged directories too");
            return;
        }
        h.mkdir_p("/p/a");
        h.mkdir_p("/q/b");
        h.write_file("/q/f", b"file");
        h.set_mtime("/p/a", OLD).unwrap();
        h.set_mtime("/q/b", OLD).unwrap();
        h.set_mtime("/q/f", OLD).unwrap();
        h.try_rename_flags("/p/a", "/q/b", libc::RENAME_EXCHANGE).unwrap();
        h.getattr("/p/a");
        h.getattr("/q/b");
        if probe {
            // a directory exchanged with a file: only the directory's time is the file systems' choice
            h.try_rename_flags("/q/b", "/q/f", libc::RENAME_EXCHANGE).unwrap();
            h.getattr("/q/f");
            h.assert_no_mismatches();
            h.assert_trees_equal();
            assert_eq!(aligned(&h), 2);
        } else {
            h.expect_mismatch(K::Attr, Some("mtime"));
        }
    }
}

/// A truncate to the current size: ext4, f2fs and tmpfs stamp mtime, xfs and btrfs do not. A truncate that changes
/// the size must stamp it everywhere: a file system that does not is still reported.
#[test]
fn truncate_to_the_current_size() {
    for probe in [true, false] {
        let h = rig(probe, Fault::new(FaultOp::Truncate, Effect::RestoreTimes));
        if probe && !adapted(&h, "truncated by path") {
            eprintln!("SKIP: the primary does not stamp a truncate to the same size either");
            return;
        }
        h.write_file("/f", &[7u8; 8192]);
        h.set_mtime("/f", OLD).unwrap();
        h.truncate("/f", 8192).unwrap();
        h.getattr("/f");
        if probe {
            h.assert_no_mismatches();
            assert_eq!(aligned(&h), 1);
            h.set_mtime("/f", OLD).unwrap();
            h.mark();
            h.truncate("/f", 4096).unwrap();
            h.expect_mismatch(K::Attr, Some("mtime"));
        } else {
            h.expect_mismatch(K::Attr, Some("mtime"));
        }
    }
}

/// A hole punched where there is no data: btrfs and ZFS do not stamp mtime, the others do. A hole punched into
/// data must stamp it everywhere.
#[test]
fn hole_punched_into_a_hole() {
    for probe in [true, false] {
        let h = rig(probe, Fault::new(FaultOp::Fallocate, Effect::RestoreTimes));
        if probe && !adapted(&h, "hole is punched") {
            eprintln!("SKIP: the primary does not stamp a hole punched into a hole either (or cannot punch holes)");
            return;
        }
        let f = h.create("/f");
        h.truncate("/f", 1 << 20).unwrap();
        h.pwrite(f, 0, &[1u8; 8192]);
        h.set_mtime("/f", OLD).unwrap();
        let punch = libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE;
        h.engine.fallocate(&h.ctx, f.ino, f.fh, 1 << 16, 1 << 16, punch).unwrap();
        h.getattr("/f");
        if probe {
            h.assert_no_mismatches();
            assert_eq!(aligned(&h), 1);
            h.set_mtime("/f", OLD).unwrap();
            h.mark();
            h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 4096, punch).unwrap();
            h.expect_mismatch(K::Attr, Some("mtime"));
        } else {
            h.expect_mismatch(K::Attr, Some("mtime"));
        }
        h.close(f);
    }
}

/// Two file systems of the same kind: nothing to adapt to, nothing aligned.
#[test]
fn same_file_systems_need_no_adaptation() {
    let h = Harness::builder().level(CheckLevel::Thorough).build();
    if test_bases().0 == test_bases().1 {
        assert!(h.engine.adaptations().is_empty(), "{:?}", h.engine.adaptations());
        assert!(h.engine.capability_gaps().is_empty(), "{:?}", h.engine.capability_gaps());
    }
    h.mkdir_p("/a/x");
    h.mkdir_p("/b");
    h.rename("/a/x", "/b/x");
    h.write_file("/f", &[7u8; 100]);
    h.truncate("/f", 100).unwrap();
    assert_eq!(aligned(&h), 0);
    h.assert_no_mismatches();
}

/// An operation only the primary supports is listed with the allow rule that accepts it.
#[test]
fn missing_fallocate_modes_are_listed() {
    let h = rig(true, Fault::new(FaultOp::Fallocate, Effect::Errno(libc::EOPNOTSUPP)));
    let gaps = h.engine.capability_gaps().join("\n");
    assert!(gaps.contains("FALLOC_FL_PUNCH_HOLE"), "{gaps}");
    assert!(gaps.contains("secondary = \"EOPNOTSUPP\""), "{gaps}");
}

/// The probe's scratch directory is gone and the roots keep their times; without write access nothing is learned
/// and nothing breaks.
#[test]
fn the_probe_leaves_no_trace() {
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(OLD as u64);
    let h = Harness::builder().build_with(|p, s| {
        for r in [p, s] {
            std::fs::write(r.join("keep"), b"x").unwrap();
            std::fs::File::open(r).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        }
    });
    for r in [h.p_root(), h.s_root()] {
        let names: Vec<_> = std::fs::read_dir(r).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["keep"], "{}", r.display());
        assert_eq!(std::fs::metadata(r).unwrap().mtime(), OLD, "{}", r.display());
    }
    h.assert_no_mismatches();

    let h = Harness::builder().secondary_fault(Fault::new(FaultOp::Mkdir, Effect::Errno(libc::EROFS))).build();
    assert!(h.engine.capability_gaps().is_empty());
    h.write_file("/f", b"works");
    h.assert_no_mismatches();
}
