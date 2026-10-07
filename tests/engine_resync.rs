//! Resync tests (`MismatchMode::Resync`, the default): after a mismatch the engine reports it, returns the
//! primary's result and repairs the secondary from the primary.
//!
//! Every repair test follows one pattern: create a divergence of the secondary (behind the engine's back with
//! plain `std::fs` calls on `h.s_path`, pre-seeded with `build_with`, or by a fault on the secondary backend), run
//! the operations that notice it, then assert
//!
//! 1. the mismatch was reported (and the application got the primary's result),
//! 2. `stats.resyncs` advanced, no repair failed,
//! 3. both trees are identical afterwards (checked independently of the engine, see `assert_trees_equal`),
//! 4. comparisons RESUME: the same divergence again is reported again (the policy counts it in `repeats`, the
//!    mismatch is not a new history entry) and repaired again, and once the divergence is gone nothing is reported.
//!
//! The primary must never be touched by a repair: `primary_is_never_modified_by_repairs` pins that down.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::policy::{Action, MismatchKind as K};
use xcheckfs::stats::OpKind;

fn data(seed: u64, n: usize) -> Vec<u8> {
    pattern(seed, n)
}

fn rs() -> HarnessBuilder {
    // `XCHECKFS_TEST_LOG=warn cargo test --test engine_resync -- --nocapture` shows the engine's repair log
    if let Ok(f) = std::env::var("XCHECKFS_TEST_LOG") {
        let _ = tracing_subscriber::fmt().with_env_filter(f).with_test_writer().try_init();
    }
    Harness::builder().mode(MismatchMode::Resync)
}

fn xattr_ok(dir: &Path) -> bool {
    let ok = xattrs_supported(dir);
    if !ok {
        eprintln!("SKIP: user xattrs not supported in {}", dir.display());
    }
    ok
}

/// Runs `f` on both roots (identical seeding).
fn both(p: &Path, s: &Path, f: impl Fn(&Path)) {
    f(p);
    f(s);
}

fn write(root: &Path, rel: &str, content: &[u8]) {
    let p = root.join(rel);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

/// utimensat without following symlinks.
fn raw_utimes(path: &Path, atime: i64, mtime: i64) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let ts = [libc::timespec { tv_sec: atime, tv_nsec: 0 }, libc::timespec { tv_sec: mtime, tv_nsec: 0 }];
    // SAFETY: valid C string and timespec array.
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    assert_eq!(r, 0, "utimensat {}: {}", path.display(), std::io::Error::last_os_error());
}

/// Flips every bit of the byte at `off` of a file, in place (same inode).
fn flip_byte(path: &Path, off: u64) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let mut b = [0u8];
    f.read_exact_at(&mut b, off).unwrap();
    b[0] ^= 0xff;
    f.write_all_at(&b, off).unwrap();
}

/// Counters that the tests compare before and after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cnt {
    mismatches: u64,
    repeats: u64,
    resyncs: u64,
    failures: u64,
    giveups: u64,
    quarantined: u64,
    skipped: u64,
}

fn cnt(h: &Harness) -> Cnt {
    Cnt {
        mismatches: h.stats.mismatches.load(Relaxed),
        repeats: h.stats.repeats.load(Relaxed),
        resyncs: h.stats.resyncs.load(Relaxed),
        failures: h.stats.resync_failures.load(Relaxed),
        giveups: h.stats.resync_giveups.load(Relaxed),
        quarantined: h.stats.quarantined.load(Relaxed),
        skipped: h.stats.secondary_skipped.load(Relaxed),
    }
}

/// Nothing was reported, repaired or given up since `before` (the secondary may still be skipped).
#[track_caller]
fn assert_quiet(h: &Harness, before: Cnt) {
    let now = cnt(h);
    assert_eq!(
        (now.mismatches, now.repeats, now.resyncs, now.failures, now.giveups),
        (before.mismatches, before.repeats, before.resyncs, before.failures, before.giveups),
        "mismatches / repairs after the divergence was repaired:\n  {}",
        h.describe_mismatches()
    );
}

/// The first mismatch of a kind was reported and repaired successfully.
#[track_caller]
fn assert_repaired_once(h: &Harness, before: Cnt) -> Cnt {
    let now = cnt(h);
    assert!(now.mismatches > before.mismatches, "no new mismatch: {}", h.describe_mismatches());
    assert!(now.resyncs > before.resyncs, "no repair ran: {}", h.describe_mismatches());
    assert_eq!(now.failures, before.failures, "a repair failed: {}", h.describe_mismatches());
    assert_eq!(now.giveups, before.giveups);
    now
}

/// The same divergence again: not a new history entry (the policy de-duplicates), but counted as a repeat and
/// repaired again.
#[track_caller]
fn assert_repeated_and_repaired(h: &Harness, before: Cnt) -> Cnt {
    let now = cnt(h);
    assert_eq!(now.mismatches, before.mismatches, "a repeat must not be a new mismatch: {}", h.describe_mismatches());
    assert!(now.repeats > before.repeats, "the repeat was not noticed ({:?} vs {:?})", before, now);
    assert!(now.resyncs > before.resyncs, "the repeat was not repaired ({:?} vs {:?})", before, now);
    assert_eq!(now.failures, before.failures, "a repair failed");
    now
}

/// The standard object test: `seed` creates identical trees, `diverge` damages the secondary root (called twice),
/// `observe` is the application's view that notices it. Checks the four points of the module documentation.
#[track_caller]
fn case(
    seed: impl Fn(&Path),
    diverge: impl Fn(&Path),
    observe: impl Fn(&Harness),
    kind: K,
    field: Option<&str>,
) -> Harness {
    let h = rs().build_with(|p, s| {
        both(p, s, &seed);
        diverge(s);
    });
    // round 1: reported, repaired, trees equal
    let c0 = cnt(&h);
    observe(&h);
    h.expect_mismatch(kind, field);
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    // comparisons resume: the same damage again is noticed again and repaired again
    diverge(h.s_root());
    observe(&h);
    let c2 = assert_repeated_and_repaired(&h, c1);
    h.assert_trees_equal();
    // healthy: nothing to report
    let before = cnt(&h);
    observe(&h);
    assert_quiet(&h, before);
    assert_eq!(c2.failures, 0);
    h.assert_trees_equal();
    h
}

fn read_through(h: &Harness, path: &str) -> Vec<u8> {
    h.read_file(path)
}

// =================================================================================================================
// 1. Object repairs
// =================================================================================================================

#[test]
fn object_corrupted_content_is_repaired_on_read() {
    let content = data(1, 300_000);
    let h = case(
        |r| write(r, "f", &data(1, 300_000)),
        |s| {
            flip_byte(&s.join("f"), 5);
            flip_byte(&s.join("f"), 150_000);
            flip_byte(&s.join("f"), 299_999);
        },
        |h| assert_eq!(read_through(h, "/f"), content, "the application always gets the primary's data"),
        K::Data,
        None,
    );
    assert_eq!(std::fs::read(h.s_path("/f")).unwrap(), content);
}

#[test]
fn object_wrong_size_is_repaired_on_lookup() {
    // too long, and too short (a repair must fix the size in both directions)
    for (name, new_len) in [("longer", 20_000usize), ("shorter", 700), ("empty", 0)] {
        let h = case(
            |r| write(r, "f", &data(2, 5000)),
            |s| {
                let mut v = data(2, 5000);
                v.resize(new_len, 0xAB);
                std::fs::write(s.join("f"), v).unwrap();
            },
            |h| assert_eq!(h.lookup("/f").st.size, 5000),
            K::Attr,
            Some("size"),
        );
        assert_eq!(std::fs::read(h.s_path("/f")).unwrap(), data(2, 5000), "{name}");
    }
}

#[test]
fn object_wrong_mode_is_repaired() {
    let h = case(
        |r| {
            write(r, "f", b"x");
            std::fs::set_permissions(r.join("f"), std::fs::Permissions::from_mode(0o640)).unwrap();
            std::fs::create_dir(r.join("d")).unwrap();
            std::fs::set_permissions(r.join("d"), std::fs::Permissions::from_mode(0o750)).unwrap();
        },
        |s| {
            std::fs::set_permissions(s.join("f"), std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(s.join("d"), std::fs::Permissions::from_mode(0o777)).unwrap();
        },
        |h| {
            assert_eq!(h.lookup("/f").st.mode & 0o7777, 0o640);
            assert_eq!(h.lookup("/d").st.mode & 0o7777, 0o750);
        },
        K::Attr,
        Some("mode"),
    );
    assert_eq!(std::fs::metadata(h.s_path("/f")).unwrap().mode() & 0o7777, 0o640);
    assert_eq!(std::fs::metadata(h.s_path("/d")).unwrap().mode() & 0o7777, 0o750);
}

#[test]
fn object_wrong_mtime_is_repaired() {
    let h = case(
        |r| {
            write(r, "f", b"x");
            raw_utimes(&r.join("f"), 1_500_000_000, 1_500_000_000);
        },
        |s| raw_utimes(&s.join("f"), 1_400_000_000, 1_400_000_000),
        |h| assert_eq!(h.lookup("/f").st.mtime.sec, 1_500_000_000),
        K::Attr,
        Some("mtime"),
    );
    assert_eq!(std::fs::metadata(h.s_path("/f")).unwrap().mtime(), 1_500_000_000);
}

#[test]
fn object_wrong_group_is_repaired() {
    // chgrp to another group of the current user is allowed without privileges
    let me = unsafe { libc::getegid() };
    let mut groups = [0 as libc::gid_t; 64];
    // SAFETY: valid buffer.
    let n = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
    let Some(other) = groups[..n.max(0) as usize].iter().copied().find(|&g| g != me) else {
        eprintln!("SKIP: the user has no second group");
        return;
    };
    let h = case(
        |r| write(r, "f", b"x"),
        |s| std::os::unix::fs::chown(s.join("f"), None, Some(other)).unwrap(),
        |h| assert_eq!(h.lookup("/f").st.gid, me),
        K::Attr,
        Some("gid"),
    );
    assert_eq!(std::fs::metadata(h.s_path("/f")).unwrap().gid(), me);
}

#[test]
fn object_wrong_uid_reported_by_the_secondary_is_repaired() {
    // An unprivileged test cannot give files to other users: the secondary claims a wrong uid once (the engine's
    // own comparison sees the lie, the repair's verification does not).
    let h = rs().build_with(|p, s| both(p, s, |r| write(r, "f", b"x")));
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Uid(4242))).path("/f").once());
    let c0 = cnt(&h);
    h.lookup("/f");
    h.expect_mismatch(K::Attr, Some("uid"));
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    // comparisons resume: the same lie again is a repeat, repaired again
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Uid(4242))).path("/f").once());
    h.lookup("/f");
    assert_repeated_and_repaired(&h, c1);
    h.clear_faults();
    let before = cnt(&h);
    h.lookup("/f");
    assert_quiet(&h, before);
    h.assert_trees_equal();
}

#[test]
fn object_xattr_differences_are_repaired() {
    let probe = tmp_in(&fast_base());
    if !xattr_ok(probe.path()) {
        return;
    }
    // changed value
    case(
        |r| {
            write(r, "f", b"x");
            raw_setxattr(&r.join("f"), "user.k", b"value-primary").unwrap();
        },
        |s| raw_setxattr(&s.join("f"), "user.k", b"value-WRONG").unwrap(),
        |h| assert_eq!(h.getxattr("/f", "user.k").unwrap(), b"value-primary"),
        K::Xattr,
        Some("user.k"),
    );
    // extra xattr on the secondary (seen in the list)
    case(
        |r| {
            write(r, "f", b"x");
            raw_setxattr(&r.join("f"), "user.a", b"1").unwrap();
        },
        |s| raw_setxattr(&s.join("f"), "user.extra", b"zzz").unwrap(),
        |h| assert_eq!(h.listxattr("/f").unwrap(), vec!["user.a".to_string()]),
        K::Xattr,
        Some("list"),
    );
    // missing xattr on the secondary: getxattr succeeds on the primary only (a Result mismatch)
    let h = case(
        |r| {
            write(r, "f", b"x");
            raw_setxattr(&r.join("f"), "user.a", b"1").unwrap();
            raw_setxattr(&r.join("f"), "user.b", b"2").unwrap();
        },
        |s| {
            // `lremovexattr` is not wrapped by the helpers: set the value to a different one then remove through the std path
            let c = std::ffi::CString::new(s.join("f").to_str().unwrap()).unwrap();
            let n = std::ffi::CString::new("user.b").unwrap();
            // SAFETY: valid C strings.
            assert_eq!(unsafe { libc::lremovexattr(c.as_ptr(), n.as_ptr()) }, 0);
        },
        |h| assert_eq!(h.getxattr("/f", "user.b").unwrap(), b"2"),
        K::Result,
        None,
    );
    assert_eq!(raw_listxattr(&h.s_path("/f")).unwrap(), vec!["user.a", "user.b"]);
    // xattrs of a directory and of a symlink are repaired as well (the latter: lookup-visible only through list)
    case(
        |r| {
            std::fs::create_dir(r.join("d")).unwrap();
            raw_setxattr(&r.join("d"), "user.k", b"v").unwrap();
        },
        |s| raw_setxattr(&s.join("d"), "user.k", b"other").unwrap(),
        |h| assert_eq!(h.getxattr("/d", "user.k").unwrap(), b"v"),
        K::Xattr,
        Some("user.k"),
    );
}

#[test]
fn object_replaced_symlink_is_found_by_lookup() {
    // The engine holds the old symlink's descriptor, the name now leads to a different inode with another
    // target: the name-based check (identity) finds it, the entry is replaced.
    let h = rs().build_with(|p, s| {
        both(p, s, |r| symlink("right-target", r.join("l")).unwrap());
    });
    h.lookup("/l");
    h.assert_no_mismatches();
    for round in 0..2 {
        std::fs::remove_file(h.s_path("/l")).unwrap();
        symlink("wrong-target", h.s_path("/l")).unwrap();
        let c0 = cnt(&h);
        h.lookup("/l");
        if round == 0 {
            h.expect_mismatch(K::Identity, None);
            assert_repaired_once(&h, c0);
        } else {
            assert_repeated_and_repaired(&h, c0);
        }
        assert_eq!(std::fs::read_link(h.s_path("/l")).unwrap(), Path::new("right-target"));
        h.assert_trees_equal();
        let before = cnt(&h);
        assert_eq!(h.readlink("/l").unwrap(), b"right-target");
        h.lookup("/l");
        assert_quiet(&h, before);
    }
}

#[test]
fn object_wrong_symlink_target_seen_by_readlink() {
    // The engine holds the symlink's descriptor: a changed target can only be a different inode... except on file
    // systems that allow rewriting it; here the object repair is exercised through a pre-seeded difference.
    let h = rs().build_with(|p, s| {
        symlink("t1", p.join("l")).unwrap();
        symlink("t2", s.join("l")).unwrap();
    });
    let c0 = cnt(&h);
    assert_eq!(h.readlink("/l").unwrap(), b"t1");
    h.expect_mismatch(K::Readlink, None);
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    assert_eq!(std::fs::read_link(h.s_path("/l")).unwrap(), Path::new("t1"));
    let before = cnt(&h);
    assert_eq!(h.readlink("/l").unwrap(), b"t1");
    assert_quiet(&h, before);
    assert_eq!(c1.failures, 0);
    // the node follows the new secondary object: a later fault on it is seen again
    h.inject(Fault::new(FaultOp::Readlink, Effect::ReadlinkTarget(b"lie".to_vec())).once());
    assert_eq!(h.readlink("/l").unwrap(), b"t1");
    assert_repeated_and_repaired(&h, c1);
    h.assert_trees_equal();
}

#[test]
fn object_type_differs_primary_file_secondary_directory() {
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        write(p, "f", b"primary file");
        write(s, "f/inner/x", b"secondary subtree");
    });
    let c0 = cnt(&h);
    h.lookup("/f");
    h.expect_mismatch(K::Attr, Some("type"));
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    assert_eq!(std::fs::read(h.s_path("/f")).unwrap(), b"primary file");
    // the node follows: operations on it compare again and are clean
    let before = cnt(&h);
    assert_eq!(h.read_file("/f"), b"primary file");
    assert_quiet(&h, before);
    // and comparisons resume: corrupt the repaired file
    flip_byte(&h.s_path("/f"), 2);
    assert_eq!(h.read_file("/f"), b"primary file");
    assert!(cnt(&h).resyncs > c1.resyncs);
    h.assert_trees_equal();
}

#[test]
fn object_type_differs_primary_directory_secondary_file() {
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        std::fs::create_dir(p.join("d")).unwrap();
        write(p, "d/child", b"c");
        write(p, "d/sub/deep", b"deep");
        write(s, "d", b"secondary file");
    });
    let c0 = cnt(&h);
    h.lookup("/d");
    h.expect_mismatch(K::Attr, Some("type"));
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    assert_eq!(std::fs::read(h.s_path("/d/child")).unwrap(), b"c");
    let before = cnt(&h);
    assert_eq!(h.readdir("/d"), vec!["child".to_string(), "sub".to_string()]);
    assert_eq!(h.read_file("/d/child"), b"c");
    assert_eq!(h.read_file("/d/sub/deep"), b"deep");
    assert_quiet(&h, before);
    // comparisons resume
    flip_byte(&h.s_path("/d/child"), 0);
    assert_eq!(h.read_file("/d/child"), b"c");
    assert!(cnt(&h).resyncs > c1.resyncs);
    h.assert_trees_equal();
}

// =================================================================================================================
// 2. Entry repairs
// =================================================================================================================

fn mkfifo(p: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    // SAFETY: valid C string.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0, "mkfifo");
}

fn ino(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).unwrap().ino()
}

fn nlink(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).unwrap().nlink()
}

#[test]
fn entry_missing_file_is_copied_and_the_node_reconnects() {
    let h = rs().build_with(|p, _| {
        write(p, "only_primary", &data(3, 70_000));
        std::fs::set_permissions(p.join("only_primary"), std::fs::Permissions::from_mode(0o751)).unwrap();
        raw_utimes(&p.join("only_primary"), 1_500_000_000, 1_500_000_123);
    });
    let c0 = cnt(&h);
    assert_eq!(h.try_lookup("/only_primary").unwrap().st.size, 70_000, "the primary's result");
    let m = h.expect_mismatch_on(K::Result, None, OpKind::Lookup, "/only_primary");
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "ENOENT"));
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    assert_eq!(std::fs::read(h.s_path("only_primary")).unwrap(), data(3, 70_000));
    assert_eq!(std::fs::metadata(h.s_path("only_primary")).unwrap().mode() & 0o7777, 0o751);
    assert_eq!(std::fs::metadata(h.s_path("only_primary")).unwrap().mtime(), 1_500_000_123);

    // the node was looked up while the file was missing; it has its secondary now: reads compare again
    let before = cnt(&h);
    assert_eq!(h.read_file("/only_primary"), data(3, 70_000));
    assert_quiet(&h, before);
    assert_eq!(cnt(&h).skipped, c1.skipped, "the secondary is used again");
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 9 }).path("/only_primary").once());
    assert_eq!(h.read_file("/only_primary"), data(3, 70_000));
    h.expect_mismatch_on(K::Data, None, OpKind::Read, "/only_primary");
    let c2 = cnt(&h);
    assert!(c2.resyncs > c1.resyncs && c2.failures == 0);
    h.assert_trees_equal();

    // resume: the file vanishes from the secondary again: same mismatch, repaired again
    std::fs::remove_file(h.s_path("only_primary")).unwrap();
    h.lookup("/only_primary");
    let c3 = assert_repeated_and_repaired(&h, c2);
    h.assert_trees_equal();
    let before = cnt(&h);
    h.lookup("/only_primary");
    assert_eq!(h.read_file("/only_primary"), data(3, 70_000));
    assert_quiet(&h, before);
    assert_eq!(c3.failures, 0);
}

/// Builds the subtree of the "missing subtree" tests on the primary: nested directories, symlinks (also a dangling
/// one), a fifo, hard links inside the subtree and one to a file outside of it, xattrs, odd modes and times.
fn build_tree(r: &Path, with_xattr: bool) {
    write(r, "tree/a/f1", &data(10, 3000));
    std::fs::hard_link(r.join("tree/a/f1"), {
        std::fs::create_dir_all(r.join("tree/b")).unwrap();
        r.join("tree/b/f1_link")
    })
    .unwrap();
    std::fs::hard_link(r.join("tree/a/f1"), r.join("tree/b/f1_link2")).unwrap();
    write(r, "tree/a/sub/deep/file", &data(11, 200_000));
    write(r, "tree/a/sub/big", &data(12, 2_500_000)); // more than one copy chunk
    std::fs::create_dir_all(r.join("tree/c/empty")).unwrap();
    std::fs::set_permissions(r.join("tree/c/empty"), std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink("a/f1", r.join("tree/s_rel")).unwrap();
    symlink("/nowhere/at/all", r.join("tree/s_dangling")).unwrap();
    mkfifo(&r.join("tree/c/fifo"));
    write(r, "tree/c/mode_time", b"mt");
    std::fs::set_permissions(r.join("tree/c/mode_time"), std::fs::Permissions::from_mode(0o4751)).ok();
    raw_utimes(&r.join("tree/c/mode_time"), 1_500_000_000, 1_500_000_777);
    raw_utimes(&r.join("tree/a/sub"), 1_400_000_000, 1_400_000_001);
    if with_xattr {
        raw_setxattr(&r.join("tree/a/f1"), "user.tag", b"hard-linked").unwrap();
        raw_setxattr(&r.join("tree/c/empty"), "user.dir", b"d").unwrap();
    }
}

#[test]
fn entry_missing_subtree_is_copied_with_hard_links_preserved() {
    let xattrs = xattrs_supported(&fast_base());
    // (directory link counts are not compared: the name lookup is what has to notice the missing subtree)
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        // the file outside of the subtree exists on both sides, as a hard link on the primary only: the engine never
        // looks it up, so the repair has to find it on its own
        both(p, s, |r| write(r, "outside/shared", &data(13, 5000)));
        build_tree(p, xattrs);
        std::fs::hard_link(p.join("outside/shared"), p.join("tree/c/ext_link")).unwrap();
    });
    let shared_ino = ino(&h.s_path("outside/shared"));
    let shared_content = std::fs::read(h.s_path("outside/shared")).unwrap();
    assert_eq!(nlink(&h.s_path("outside/shared")), 1);

    let c0 = cnt(&h);
    h.lookup("/tree"); // the only thing the application looked at
    h.expect_mismatch_on(K::Result, None, OpKind::Lookup, "/tree");
    assert_repaired_once(&h, c0);
    h.assert_trees_equal();

    // linked, not copied: the outside file keeps its identity and content, and the new name is another name of it
    let s = |rel: &str| h.s_path(rel);
    assert_eq!(ino(&s("outside/shared")), shared_ino, "the existing secondary object must be kept");
    assert_eq!(ino(&s("tree/c/ext_link")), shared_ino, "ext_link must be a link to outside/shared");
    assert_eq!(nlink(&s("outside/shared")), 2);
    assert_eq!(std::fs::read(s("tree/c/ext_link")).unwrap(), shared_content);
    // links inside the subtree: three names of one inode, not three copies
    let f1 = ino(&s("tree/a/f1"));
    assert_eq!(ino(&s("tree/b/f1_link")), f1);
    assert_eq!(ino(&s("tree/b/f1_link2")), f1);
    assert_eq!(nlink(&s("tree/a/f1")), 3);
    assert_ne!(f1, shared_ino);
    // the special objects
    assert!(std::fs::symlink_metadata(s("tree/c/fifo")).unwrap().file_type().is_fifo_like());
    assert_eq!(std::fs::read_link(s("tree/s_dangling")).unwrap(), Path::new("/nowhere/at/all"));
    assert_eq!(std::fs::read(s("tree/a/sub/big")).unwrap(), data(12, 2_500_000));

    // later operations inside the repaired subtree compare cleanly (and the nodes get their secondaries)
    let before = cnt(&h);
    assert_eq!(h.read_file("/tree/a/sub/deep/file"), data(11, 200_000));
    assert_eq!(h.read_file("/tree/b/f1_link2"), data(10, 3000));
    assert_eq!(h.readlink("/tree/s_rel").unwrap(), b"a/f1");
    assert_eq!(h.readdir("/tree/c"), vec!["empty", "ext_link", "fifo", "mode_time"]);
    assert_quiet(&h, before);
    h.assert_trees_equal();

    // resume: the whole subtree vanishes again
    std::fs::remove_dir_all(h.s_path("tree")).unwrap();
    let c = cnt(&h);
    h.lookup("/tree");
    assert_repeated_and_repaired(&h, c);
    h.assert_trees_equal();
    // nodes of the old subtree (looked up before, their secondary objects were removed) follow the new objects
    let before = cnt(&h);
    assert_eq!(h.read_file("/tree/a/sub/deep/file"), data(11, 200_000));
    assert_eq!(h.read_file("/tree/b/f1_link2"), data(10, 3000));
    assert_eq!(h.readlink("/tree/s_rel").unwrap(), b"a/f1");
    assert_eq!(h.readdir("/tree/c"), vec!["empty", "ext_link", "fifo", "mode_time"]);
    assert_quiet(&h, before);
    h.assert_trees_equal();
}

/// `FileType` has no `is_fifo` without the unix extension trait in scope; a tiny shim keeps the assertion readable.
trait FifoLike {
    fn is_fifo_like(&self) -> bool;
}
impl FifoLike for std::fs::FileType {
    fn is_fifo_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_fifo()
    }
}

#[test]
fn entry_extra_entries_and_subtrees_on_the_secondary_are_removed() {
    // (directory link counts are not compared: the listing is what has to notice the extra subdirectories)
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        both(p, s, |r| write(r, "keep/file", b"keep"));
        write(s, "extra_file", b"x");
        write(s, "extra_dir/sub/deeper/f", b"x");
        write(s, "extra_dir/sub/g", b"x");
        symlink("elsewhere", s.join("extra_link")).unwrap();
        mkfifo(&s.join("extra_fifo"));
        // an extra entry inside a directory that exists on both sides
        write(s, "keep/extra_inside", b"x");
        // the entry exists on both sides with another type: dir on the primary, file on the secondary and vice versa
        std::fs::create_dir(p.join("pdir_sfile")).unwrap();
        write(p, "pdir_sfile/x", b"x");
        write(s, "pdir_sfile", b"file");
        write(p, "pfile_sdir", b"file");
        write(s, "pfile_sdir/x/y", b"y");
    });
    let c0 = cnt(&h);
    // the listing notices everything at once ...
    assert_eq!(h.readdir("/"), vec!["keep", "pdir_sfile", "pfile_sdir"], "the application sees the primary's listing");
    let m = h.expect_mismatch(K::Readdir, None);
    assert!(m.detail.contains("extra_dir") && m.detail.contains("extra_fifo"), "{}", m.detail);
    let c1 = assert_repaired_once(&h, c0);
    // ... the listing of /keep is still wrong (it was not looked at)
    assert!(h.s_path("keep/extra_inside").exists());
    assert_eq!(h.readdir("/keep"), vec!["file"]);
    let c2 = assert_repaired_once(&h, c1);
    h.assert_trees_equal();
    assert_eq!(std::fs::read_dir(h.s_root()).unwrap().count(), 3);

    // resume: extra entries again, this time found by a name lookup (ENOENT on the primary, success on the secondary)
    write(h.s_root(), "extra_again/f", b"x");
    assert_eq!(h.try_lookup("/extra_again").unwrap_err(), libc::ENOENT);
    let m = h.expect_mismatch_on(K::Result, None, OpKind::Lookup, "/extra_again");
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("ENOENT", "OK"));
    let c3 = assert_repaired_once(&h, c2);
    h.assert_trees_equal();
    write(h.s_root(), "extra_again/f", b"x");
    assert_eq!(h.try_lookup("/extra_again").unwrap_err(), libc::ENOENT);
    let c4 = assert_repeated_and_repaired(&h, c3);
    h.assert_trees_equal();
    // and the listing again
    write(h.s_root(), "extra_listing", b"x");
    assert_eq!(h.readdir("/").len(), 3);
    assert!(cnt(&h).resyncs > c4.resyncs);
    h.assert_trees_equal();
    let before = cnt(&h);
    assert_eq!(h.readdir("/"), vec!["keep", "pdir_sfile", "pfile_sdir"]);
    assert_eq!(h.read_file("/keep/file"), b"keep");
    assert_quiet(&h, before);
}

#[test]
fn entry_repair_of_one_name_is_no_failure_while_other_names_still_differ() {
    // Only the looked-up name is repaired; the directory's listing and link count still show the other differences,
    // which have their own mismatches. That must not make this repair "fail" (and the node primary-only).
    let h = rs().config(|c| c.dir_nlink = true).build_with(|p, s| {
        both(p, s, |r| std::fs::create_dir(r.join("d")).unwrap());
        write(p, "d/missing1", b"1");
        write(p, "d/missing2", b"2");
        std::fs::create_dir(p.join("d/missing_dir")).unwrap();
        write(s, "d/extra", b"x");
        std::fs::create_dir(s.join("d/extra_dir1")).unwrap();
        std::fs::create_dir(s.join("d/extra_dir2")).unwrap();
    });
    // (the directory itself differs in its link count: that is repaired as a whole, before the names)
    let c0 = cnt(&h);
    assert_eq!(h.try_lookup("/d/missing1").unwrap().st.size, 1);
    let c1 = cnt(&h);
    assert_eq!(c1.failures, c0.failures, "{}", h.describe_mismatches());
    h.assert_trees_equal();
    assert!(c1.resyncs > c0.resyncs);
}

#[test]
fn entry_one_name_among_several_differing_names() {
    // dir_nlink off: the directory looks fine from the outside, only names differ
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        both(p, s, |r| std::fs::create_dir(r.join("d")).unwrap());
        write(p, "d/missing1", b"1");
        write(p, "d/missing2", b"2");
        write(s, "d/extra", b"x");
    });
    let c0 = cnt(&h);
    assert_eq!(h.try_lookup("/d/missing1").unwrap().st.size, 1);
    let c1 = cnt(&h);
    assert_eq!((c1.failures, c1.resyncs), (c0.failures, c0.resyncs + 1), "{}", h.describe_mismatches());
    assert!(h.s_path("d/missing1").exists() && !h.s_path("d/missing2").exists() && h.s_path("d/extra").exists());
    // the others are repaired by their own lookups / the listing
    assert_eq!(h.try_lookup("/d/missing2").unwrap().st.size, 1);
    assert_eq!(h.readdir("/d"), vec!["missing1", "missing2"]);
    h.assert_trees_equal();
    assert_eq!(cnt(&h).failures, c0.failures);
}

#[test]
fn entry_hard_link_that_is_a_copy_on_the_secondary_is_relinked() {
    for first in ["/a", "/b", "/c"] {
        let h = rs().build_with(|p, s| {
            write(p, "a", b"shared data");
            std::fs::hard_link(p.join("a"), p.join("b")).unwrap();
            std::fs::hard_link(p.join("a"), p.join("c")).unwrap();
            write(s, "a", b"shared data");
            std::fs::hard_link(s.join("a"), s.join("b")).unwrap();
            write(s, "c", b"shared data"); // a copy where the primary has a link
        });
        let c0 = cnt(&h);
        h.lookup(first);
        for p in ["/a", "/b", "/c"] {
            h.lookup(p);
        }
        // the first name looked at shows the link count (the repair makes the names right, whichever it is)
        h.expect_mismatch(K::Attr, Some("nlink"));
        let c1 = assert_repaired_once(&h, c0);
        h.assert_trees_equal();
        assert_eq!(nlink(&h.s_path("a")), 3, "first={first}");
        assert_eq!(ino(&h.s_path("a")), ino(&h.s_path("c")), "first={first}");
        let before = cnt(&h);
        for p in ["/a", "/b", "/c"] {
            h.lookup(p);
            assert_eq!(h.read_file(p), b"shared data");
        }
        assert_quiet(&h, before);

        // resume: the secondary breaks the link again (replaces c by a copy)
        std::fs::remove_file(h.s_path("c")).unwrap();
        write(h.s_root(), "c", b"shared data");
        let c = cnt(&h);
        h.lookup("/c");
        // (found by the identity check of the name this time, a mismatch of its own)
        assert!(cnt(&h).resyncs > c.resyncs && cnt(&h).failures == c.failures, "{}", h.describe_mismatches());
        h.assert_trees_equal();
        assert_eq!(nlink(&h.s_path("c")), 3);
        assert_eq!(c1.failures, 0);
    }
}

#[test]
fn entry_primary_copy_that_is_a_link_on_the_secondary_is_repaired() {
    // the other direction: separate files on the primary, one inode on the secondary
    let h = rs().build_with(|p, s| {
        write(p, "a", b"data");
        write(p, "b", b"data");
        write(s, "a", b"data");
        std::fs::hard_link(s.join("a"), s.join("b")).unwrap();
    });
    let c0 = cnt(&h);
    h.lookup("/a");
    h.lookup("/b");
    h.expect_mismatch(K::Attr, Some("nlink"));
    assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    assert_ne!(ino(&h.s_path("a")), ino(&h.s_path("b")));
    assert_eq!(nlink(&h.s_path("a")), 1);
}

// =================================================================================================================
// 3. Namespace operations that diverge on the secondary
// =================================================================================================================

/// The same divergence struck again: the policy counted a repeat, the repair ran again and worked. (Unlike
/// `assert_repeated_and_repaired` this tolerates new history entries: objects created again have new inode numbers.)
#[track_caller]
fn assert_again_repaired(h: &Harness, before: Cnt) -> Cnt {
    let now = cnt(h);
    assert!(now.repeats > before.repeats, "the repeat was not noticed ({before:?} vs {now:?})\n  {}", h.describe_mismatches());
    assert!(now.resyncs > before.resyncs, "the repeat was not repaired ({before:?} vs {now:?})");
    assert_eq!(now.failures, before.failures, "a repair failed");
    now
}

/// One namespace operation whose secondary half fails (`fault`) in a thorough-checked harness: `setup` prepares
/// the state, `act` is the application's operation (it must succeed: the primary's result is returned),
/// `undo` brings the state back for the second round.
#[track_caller]
fn ns_case(
    fault: impl Fn() -> Fault,
    setup: impl Fn(&Harness),
    act: impl Fn(&Harness),
    undo: impl Fn(&Harness),
    check: impl Fn(&Harness),
) {
    let h = rs().level(CheckLevel::Thorough).build();
    setup(&h);
    h.assert_no_mismatches();
    h.assert_trees_equal();

    // round 1
    let c0 = cnt(&h);
    h.inject(fault());
    act(&h);
    assert_eq!(h.fault.hits(FaultId(1)), 1, "the fault did not fire exactly once");
    h.clear_faults();
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    check(&h);

    // the nodes touched are usable and compared again
    undo(&h);
    h.assert_trees_equal();

    // round 2: the same fault on the same names is noticed again (repeat) and repaired again
    h.inject(fault());
    act(&h);
    h.clear_faults();
    let c2 = assert_again_repaired(&h, c1);
    h.assert_trees_equal();
    check(&h);

    // no fault: nothing to report, nothing to repair
    undo(&h);
    h.assert_trees_equal();
    let before = cnt(&h);
    act(&h);
    assert_quiet(&h, before);
    h.assert_trees_equal();
    assert_eq!(c2.giveups, 0);
}

use xcheckfs::backend::fault::FaultId;

fn ignore<T>(_: T) {}

type NamedEffect = (&'static str, fn() -> Effect);

fn ns_effects() -> [NamedEffect; 2] {
    [("errno", || Effect::Errno(libc::EIO)), ("skip", || Effect::Skip)]
}

#[test]
fn namespace_mkdir_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("mkdir/{name}");
        ns_case(
            || Fault::new(FaultOp::Mkdir, eff()).once(),
            |h| ignore(h.mkdir("/d")),
            |h| {
                h.mkdir("/d/sub");
                h.write_file("/d/sub/inside", b"x"); // the new directory's node compares again
            },
            |h| {
                h.unlink("/d/sub/inside");
                h.rmdir("/d/sub");
            },
            |h| assert!(h.s_path("/d/sub").is_dir()),
        );
    }
}

#[test]
fn namespace_create_diverges() {
    // (a create cannot be "skipped": a skipped create has no descriptor to return, so only errno)
    ns_case(
        || Fault::new(FaultOp::Create, Effect::Errno(libc::EIO)).once(),
        |h| ignore(h.mkdir("/d")),
        |h| ignore(h.write_file("/d/new", &data(20, 30_000))),
        |h| h.unlink("/d/new"),
        |h| assert_eq!(std::fs::read(h.s_path("/d/new")).unwrap(), data(20, 30_000)),
    );
}

#[test]
fn namespace_mknod_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("mknod/{name}");
        ns_case(
            || Fault::new(FaultOp::Mknod, eff()).once(),
            |h| ignore(h.mkdir("/d")),
            |h| ignore(h.mknod_fifo("/d/fifo").unwrap()),
            |h| h.unlink("/d/fifo"),
            |h| assert!(std::fs::symlink_metadata(h.s_path("/d/fifo")).unwrap().file_type().is_fifo_like()),
        );
    }
}

#[test]
fn namespace_symlink_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("symlink/{name}");
        ns_case(
            || Fault::new(FaultOp::Symlink, eff()).once(),
            |h| ignore(h.mkdir("/d")),
            |h| ignore(h.symlink("some/target", "/d/l")),
            |h| h.unlink("/d/l"),
            |h| assert_eq!(std::fs::read_link(h.s_path("/d/l")).unwrap(), Path::new("some/target")),
        );
    }
}

#[test]
fn namespace_link_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("link/{name}");
        ns_case(
            || Fault::new(FaultOp::Link, eff()).once(),
            |h| {
                h.mkdir("/d");
                h.write_file("/d/a", b"linked");
            },
            |h| ignore(h.link("/d/a", "/d/b")),
            |h| h.unlink("/d/b"),
            |h| {
                assert_eq!(ino(&h.s_path("/d/a")), ino(&h.s_path("/d/b")), "a link, not a copy");
                assert_eq!(nlink(&h.s_path("/d/a")), 2);
            },
        );
    }
}

#[test]
fn namespace_unlink_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("unlink/{name}");
        ns_case(
            || Fault::new(FaultOp::Unlink, eff()).once(),
            |h| {
                h.mkdir("/d");
                h.write_file("/d/f", b"victim");
            },
            |h| h.unlink("/d/f"),
            |h| ignore(h.write_file("/d/f", b"victim")),
            |h| assert!(!h.s_path("/d/f").exists()),
        );
    }
}

#[test]
fn namespace_rmdir_diverges() {
    for (name, eff) in ns_effects() {
        eprintln!("rmdir/{name}");
        ns_case(
            || Fault::new(FaultOp::Rmdir, eff()).once(),
            |h| {
                h.mkdir("/d");
                h.mkdir("/d/e");
            },
            |h| h.rmdir("/d/e"),
            |h| ignore(h.mkdir("/d/e")),
            |h| assert!(!h.s_path("/d/e").exists()),
        );
    }
}

#[test]
fn namespace_rename_diverges_both_names_are_repaired() {
    for (name, eff) in ns_effects() {
        eprintln!("rename/{name}");
        // plain rename to a new name
        ns_case(
            || Fault::new(FaultOp::Rename, eff()).once(),
            |h| {
                h.mkdir("/d");
                h.write_file("/d/a", &data(30, 5000));
            },
            |h| h.rename("/d/a", "/d/b"),
            |h| h.rename("/d/b", "/d/a"),
            |h| {
                assert!(!h.s_path("/d/a").exists(), "the source name must be gone on the secondary");
                assert_eq!(std::fs::read(h.s_path("/d/b")).unwrap(), data(30, 5000), "the target name must be right");
            },
        );
        // over an existing target (the target name holds other data on the secondary)
        ns_case(
            || Fault::new(FaultOp::Rename, eff()).once(),
            |h| {
                h.mkdir("/d");
                h.write_file("/d/a", &data(31, 5000));
                h.write_file("/d/b", &data(32, 7000));
            },
            |h| h.rename("/d/a", "/d/b"),
            |h| ignore(h.write_file("/d/a", &data(31, 5000))),
            |h| {
                assert!(!h.s_path("/d/a").exists() || std::fs::read(h.s_path("/d/a")).unwrap() == data(31, 5000));
                assert_eq!(std::fs::read(h.s_path("/d/b")).unwrap(), data(31, 5000));
            },
        );
        // across directories
        ns_case(
            || Fault::new(FaultOp::Rename, eff()).once(),
            |h| {
                h.mkdir("/d1");
                h.mkdir("/d2");
                h.write_file("/d1/f", b"move me");
            },
            |h| h.rename("/d1/f", "/d2/g"),
            |h| h.rename("/d2/g", "/d1/f"),
            |h| {
                assert!(!h.s_path("/d1/f").exists());
                assert_eq!(std::fs::read(h.s_path("/d2/g")).unwrap(), b"move me");
            },
        );
    }
}

#[test]
fn namespace_rename_of_a_directory_with_known_descendants() {
    // The engine knows nodes below the renamed directory; the repair replaces the whole subtree on the secondary,
    // and those nodes must follow (they are compared again, without noise).
    for (name, eff) in ns_effects() {
        eprintln!("dir rename/{name}");
        let h = rs().level(CheckLevel::Thorough).build();
        h.mkdir_p("/d/src/sub");
        h.write_file("/d/src/f", &data(40, 9000));
        h.write_file("/d/src/sub/g", &data(41, 70_000));
        h.symlink("f", "/d/src/l");
        h.read_file("/d/src/f");
        h.read_file("/d/src/sub/g");
        h.readlink("/d/src/l").unwrap();
        h.assert_no_mismatches();
        let c0 = cnt(&h);
        h.inject(Fault::new(FaultOp::Rename, eff()).once());
        h.rename("/d/src", "/d/dst");
        h.clear_faults();
        assert_repaired_once(&h, c0);
        h.assert_trees_equal();
        assert!(!h.s_path("/d/src").exists());
        let before = cnt(&h);
        assert_eq!(h.read_file("/d/dst/f"), data(40, 9000));
        assert_eq!(h.read_file("/d/dst/sub/g"), data(41, 70_000));
        assert_eq!(h.readlink("/d/dst/l").unwrap(), b"f");
        h.write_file("/d/dst/sub/new", b"after");
        h.truncate("/d/dst/f", 100).unwrap();
        assert_quiet(&h, before);
        h.assert_trees_equal();
        // and the secondary objects below are the ones the nodes use: a fault on them is seen
        h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 4 }).path("/d/dst/sub/g").once());
        assert_eq!(h.read_file("/d/dst/sub/g"), data(41, 70_000));
        h.expect_mismatch_on(K::Data, None, OpKind::Read, "/d/dst/sub/g");
        h.assert_trees_equal();
    }
}

// =================================================================================================================
// 4. Reconnect
// =================================================================================================================

#[test]
fn reconnect_node_looked_up_while_missing_compares_again_after_the_repair() {
    // Looked up while the secondary lacks the file in a directory that was looked up before; the repair of the
    // name connects the node, and every later operation on it is compared: a fault is reported.
    let h = rs().build_with(|p, s| {
        both(p, s, |r| std::fs::create_dir(r.join("d")).unwrap());
        write(p, "d/late", &data(50, 10_000));
    });
    h.lookup("/d");
    let c0 = cnt(&h);
    let a = h.lookup("/d/late");
    assert_eq!(a.st.size, 10_000);
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    let f = h.open("/d/late", libc::O_RDWR);
    assert_eq!(h.pread(f, 0, 100), data(50, 10_000)[..100]);
    assert_eq!(cnt(&h).skipped, c1.skipped, "the node uses the secondary");
    h.close(f);
    for (i, (op, effect)) in [
        (FaultOp::Pread, Effect::CorruptRead { offset: 1 }),
        (FaultOp::Getxattr, Effect::Errno(libc::EIO)),
        (FaultOp::Open, Effect::Errno(libc::EIO)),
    ]
    .into_iter()
    .enumerate()
    {
        let c = cnt(&h);
        h.inject(if i == 1 { Fault::new(op, effect).name("user.none").once() } else { Fault::new(op, effect).path("/d/late").once() });
        match i {
            0 => assert_eq!(h.read_file("/d/late"), data(50, 10_000)),
            1 => assert!(h.getxattr("/d/late", "user.none").is_err()),
            _ => ignore(h.try_open("/d/late", libc::O_RDONLY).map(|f| h.close(f))),
        }
        h.clear_faults();
        assert!(cnt(&h).mismatches > c.mismatches, "fault {op:?} was not reported");
        assert!(cnt(&h).resyncs > c.resyncs, "fault {op:?} was not repaired");
        h.assert_trees_equal();
    }
}

#[test]
fn reconnect_after_a_failed_repair_the_next_lookup_attaches_the_secondary_again() {
    // The write path of the secondary is broken for a while: the repair cannot verify, the node becomes primary-only.
    // When the fault is gone, a lookup finds the secondary object again and reconnects the node; the divergence
    // that was never repaired is found and repaired right away.
    let content = data(51, 50_000);
    let h = rs().build_with(|p, s| {
        both(p, s, |r| write(r, "f", &data(51, 50_000)));
        flip_byte(&s.join("f"), 777);
        let mut v = data(51, 50_000);
        v.truncate(40_000);
        std::fs::write(s.join("g"), &v).unwrap(); // (a different size: found by the lookup itself)
        write(p, "g", &data(51, 50_000));
    });
    h.inject_always(FaultOp::Pwrite, Effect::DropWrite);
    let c0 = cnt(&h);
    let f = h.lookup("/f");
    let read = |ino: u64| {
        let fh = h.engine.open(&h.ctx, ino, libc::O_RDONLY).unwrap();
        let d = h.pread(Fh { ino, fh }, 0, 100_000);
        h.close(Fh { ino, fh });
        d
    };
    assert_eq!(read(f.id), content);
    h.lookup("/g");
    let c1 = cnt(&h);
    assert!(c1.failures == c0.failures + 2 && c1.resyncs == c0.resyncs, "{c1:?}");
    // (operations on the known nodes: no new lookups)
    assert_eq!(read(f.id), content);
    h.engine.getattr(&h.ctx, f.id).unwrap();
    let c2 = cnt(&h);
    assert!(c2.skipped > c1.skipped, "primary-only");
    assert_eq!((c2.mismatches, c2.resyncs, c2.failures), (c1.mismatches, c1.resyncs, c1.failures));
    h.clear_faults();

    // reconnect: the lookup attaches the secondary again; what is wrong gets repaired
    h.lookup("/f");
    h.lookup("/g"); // (the failed attempt had already fixed its size: only its content differs now)
    assert_eq!(read(f.id), content);
    assert_eq!(h.read_file("/g"), data(51, 50_000));
    h.assert_trees_equal();
    let c3 = cnt(&h);
    assert!(c3.resyncs >= c1.resyncs + 2, "{c3:?}");
    assert_eq!(c3.failures, c1.failures);
    // compared again from now on
    let before = cnt(&h);
    assert_eq!(read(f.id), content);
    assert_eq!(h.read_file("/g"), data(51, 50_000));
    assert_quiet(&h, before);
    assert_eq!(cnt(&h).skipped, before.skipped);
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 7 }).path("/f").once());
    assert_eq!(read(f.id), content);
    assert!(cnt(&h).resyncs > before.resyncs);
}

// =================================================================================================================
// 5. Open handles across a replacement of the secondary object
// =================================================================================================================

#[test]
fn open_handle_survives_the_replacement_of_its_secondary_object() {
    for replacement in ["new file", "symlink", "directory"] {
        let content = data(60, 40_000);
        let h = rs().build_with(|p, s| both(p, s, |r| write(r, "f", &data(60, 40_000))));
        let old = h.open("/f", libc::O_RDWR);
        assert_eq!(h.pread(old, 0, 10), content[..10]);
        // keep the secondary's original inode alive and watch it: the old handle must never write to it
        let orig = std::fs::File::open(h.s_path("f")).unwrap();
        std::fs::remove_file(h.s_path("f")).unwrap();
        match replacement {
            "new file" => std::fs::write(h.s_path("f"), data(61, 1234)).unwrap(),
            "symlink" => symlink("elsewhere", h.s_path("f")).unwrap(),
            _ => write(h.s_root(), "f/inner/x", b"x"),
        }
        let c0 = cnt(&h);
        h.lookup("/f"); // the kernel looks the name up again: identity / type mismatch, entry repaired
        let c1 = assert_repaired_once(&h, c0);
        h.assert_trees_equal();

        // the old handle still works, with the primary's results, no mismatch, no garbage
        let before = cnt(&h);
        assert_eq!(h.pread(old, 0, 40_000), content, "{replacement}");
        assert_eq!(h.pread(old, 39_000, 100), content[39_000..39_100]);
        h.engine.getattr(&h.ctx, old.ino).unwrap();
        assert_quiet(&h, before);
        assert!(cnt(&h).skipped > before.skipped, "the old handle must not use the new secondary object");
        // writes through it go to the primary (its secondary object is gone) and never to the old or the new object
        h.pwrite(old, 5, b"PATCHED");
        h.close(old);
        let mut orig_content = Vec::new();
        std::io::Read::read_to_end(&mut &orig, &mut orig_content).unwrap();
        assert_eq!(orig_content, content, "the replaced secondary inode must be untouched");
        assert_eq!(&std::fs::read(h.p_path("f")).unwrap()[5..12], b"PATCHED");

        // a new open uses the new secondary object: it is compared (the patch is not on the secondary yet: the next
        // observation notices and repairs that)
        let new = h.open("/f", libc::O_RDONLY);
        let got = h.pread(new, 0, 40_000);
        assert_eq!(&got[5..12], b"PATCHED");
        h.close(new);
        h.getattr("/f");
        h.assert_trees_equal();
        let before = cnt(&h);
        let again = h.open("/f", libc::O_RDONLY);
        assert_eq!(h.pread(again, 0, 40_000), got);
        h.close(again);
        assert_quiet(&h, before);
        assert!(cnt(&h).skipped == before.skipped, "new handles use the secondary");
        assert_eq!(c1.failures, 0);
    }
}

#[test]
fn open_directory_handle_survives_the_replacement_of_its_secondary_object() {
    let h = rs().build_with(|p, s| {
        both(p, s, |r| {
            write(r, "dir/a", b"a");
            write(r, "dir/b", b"b");
        })
    });
    let a = h.lookup("/dir");
    let fh = h.engine.opendir(&h.ctx, a.id).unwrap();
    // replace the secondary directory (a new inode with the same names)
    std::fs::remove_dir_all(h.s_path("dir")).unwrap();
    write(h.s_root(), "dir/a", b"a");
    write(h.s_root(), "dir/b", b"b");
    let c0 = cnt(&h);
    h.lookup("/dir");
    let c1 = assert_repaired_once(&h, c0);
    h.assert_trees_equal();
    let before = cnt(&h);
    let mut names = Vec::new();
    h.engine
        .readdir(&h.ctx, a.id, fh, 0, &mut |_, _, _, n| {
            names.push(String::from_utf8_lossy(n).into_owned());
            false
        })
        .unwrap();
    names.sort();
    assert_eq!(names, [".", "..", "a", "b"]);
    assert_quiet(&h, before);
    h.engine.releasedir(&h.ctx, a.id, fh).unwrap();
    assert_eq!(h.readdir("/dir"), ["a", "b"]);
    assert_quiet(&h, before);
    assert_eq!(c1.failures, 0);
}

// =================================================================================================================
// 6. A repair that cannot be verified
// =================================================================================================================

/// Runs `f` on a thread and fails when it does not finish in `secs` (a deadlock or an endless repair loop).
fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(Ok(v)) => v,
        Ok(Err(p)) => std::panic::resume_unwind(p),
        Err(_) => panic!("timed out after {secs}s (deadlock or endless repair?)"),
    }
}

fn snapshot_of_primary(h: &Harness) -> BTreeMap<String, String> {
    snapshot(h.p_root())
}

/// A content/attribute snapshot of a tree (everything but atime), for "was not modified" assertions.
fn snapshot(root: &Path) -> BTreeMap<String, String> {
    use std::os::unix::fs::FileTypeExt;
    fn walk(root: &Path, rel: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
        for e in rd.flatten() {
            let r = rel.join(e.file_name());
            let full = root.join(&r);
            let md = std::fs::symlink_metadata(&full).unwrap();
            let mut d = format!(
                "ino={} mode={:o} uid={} gid={} nlink={} size={} mtime={}.{:09} ctime={}.{:09}",
                md.ino(),
                md.mode(),
                md.uid(),
                md.gid(),
                md.nlink(),
                md.size(),
                md.mtime(),
                md.mtime_nsec(),
                md.ctime(),
                md.ctime_nsec()
            );
            let ft = md.file_type();
            if ft.is_symlink() {
                d += &format!(" -> {:?}", std::fs::read_link(&full).unwrap());
            } else if ft.is_file() {
                d += &format!(" xxh3={:016x}", xxhash_rust::xxh3::xxh3_64(&std::fs::read(&full).unwrap()));
            } else if ft.is_fifo() {
                d += " fifo";
            }
            if !ft.is_symlink() {
                for n in raw_listxattr(&full).unwrap_or_default() {
                    d += &format!(" {n}={:?}", raw_getxattr(&full, &n).ok());
                }
            }
            out.insert(format!("/{}", r.display()), d);
            if ft.is_dir() {
                walk(root, &r, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out);
    out
}

#[test]
fn failed_verification_makes_the_object_primary_only_without_a_loop() {
    for (name, effect) in [("corrupt", Effect::CorruptWrite { offset: 0 }), ("drop", Effect::DropWrite)] {
        let content = data(70, 200_000);
        let h = Arc::new(rs().config(|c| c.resync_limit = 3).build_with(|p, s| {
            both(p, s, |r| write(r, "f", &data(70, 200_000)));
            flip_byte(&s.join("f"), 100_000);
            both(p, s, |r| write(r, "other", b"untouched"));
        }));
        let primary_before = snapshot_of_primary(&h);
        // every write to the secondary's /f is wrong: the repair can never verify
        h.inject(Fault::new(FaultOp::Pwrite, effect).path("/f"));
        let c0 = cnt(&h);
        let h2 = h.clone();
        let c = content.clone();
        within(20, move || assert_eq!(h2.read_file("/f"), c, "the application gets the primary's data"));
        h.expect_mismatch(K::Data, None);
        let c1 = cnt(&h);
        assert_eq!(c1.failures, c0.failures + 1, "{name}: {c1:?}");
        assert_eq!(c1.resyncs, c0.resyncs, "{name}");

        // The object is primary-only now. A later lookup of the name attaches it again and tries again; that is
        // bounded by the budget (3 here), after which the object stays primary-only for the window.
        let h2 = h.clone();
        within(20, move || {
            for _ in 0..12 {
                h2.read_file("/f");
                h2.getattr("/f");
                h2.append("/f", b"+");
                h2.truncate("/f", 200_000).unwrap();
            }
        });
        let c2 = cnt(&h);
        assert_eq!(c2.resyncs, 0, "{name}: {c2:?}");
        assert!(c2.failures >= 2 && c2.failures <= 3, "{name}: attempts are bounded by the budget: {c2:?}");
        // ... and then no more comparisons, repair attempts, or calls to the secondary object
        let reads = h.fault.calls(FaultOp::Pread);
        let writes = h.fault.calls(FaultOp::Pwrite);
        for _ in 0..5 {
            assert_eq!(h.read_file("/f").len(), 200_000);
            h.getattr("/f");
            h.append("/f", b"+");
            h.truncate("/f", 200_000).unwrap();
        }
        let c3 = cnt(&h);
        assert_eq!((c3.mismatches, c3.repeats, c3.resyncs, c3.failures), (c2.mismatches, c2.repeats, c2.resyncs, c2.failures), "{name}");
        assert!(c3.skipped > c2.skipped);
        assert_eq!(h.fault.calls(FaultOp::Pread), reads, "no read on the secondary object");
        assert_eq!(h.fault.calls(FaultOp::Pwrite), writes, "no write to the secondary object");
        // the primary is correct (it has the application's own writes, nothing else), the others are still compared
        let mut want = content.clone();
        want.push(b'+');
        want.truncate(200_000);
        assert_eq!(std::fs::read(h.p_path("f")).unwrap(), want);
        let before = cnt(&h);
        assert_eq!(h.read_file("/other"), b"untouched");
        assert_quiet(&h, before);
        assert_eq!(cnt(&h).skipped, c3.skipped);
        let after = snapshot_of_primary(&h);
        let changed: Vec<_> = after.iter().filter(|(k, v)| primary_before.get(*k) != Some(v)).map(|(k, _)| k.clone()).collect();
        assert_eq!(changed, ["/f"], "{name}");
    }
}

#[test]
fn failed_entry_repair_is_bounded_and_the_primary_untouched() {
    // the secondary refuses to create anything: the copy cannot work
    let h = Arc::new(rs().config(|c| {
        c.resync_limit = 3;
        c.dir_nlink = false; // (the root's link count would make the engine repair everything at start)
    }).build_with(|p, _| {
        write(p, "only_primary", b"data");
        write(p, "dir/inner", b"data");
    }));
    let primary_before = snapshot_of_primary(&h);
    h.inject_always(FaultOp::Create, Effect::Errno(libc::EIO));
    h.inject_always(FaultOp::Mkdir, Effect::Errno(libc::EIO));
    let h2 = h.clone();
    within(20, move || {
        for _ in 0..8 {
            assert_eq!(h2.lookup("/only_primary").st.size, 4);
            assert_eq!(h2.lookup("/dir").st.mode & libc::S_IFMT, libc::S_IFDIR);
            assert_eq!(h2.try_lookup("/dir/inner").map(|a| a.st.size), Ok(4));
        }
    });
    let c = cnt(&h);
    assert_eq!(c.resyncs, 0);
    assert!(c.failures >= 3, "{c:?}");
    assert!(c.giveups > 0, "the budget must run out: {c:?}");
    assert!(c.failures <= 3 * 3, "attempts are bounded by the budget (3 per path): {c:?}");
    assert_eq!(snapshot_of_primary(&h), primary_before);
    // once the secondary works, the next lookup after the budget window... is still blocked by the budget
    h.clear_faults();
    h.lookup("/only_primary");
    assert_eq!(cnt(&h).resyncs, 0, "the budget of /only_primary is used up for ten minutes");
}

// =================================================================================================================
// 7. Repair budget
// =================================================================================================================

#[test]
fn budget_gives_up_on_an_object_that_diverges_after_every_repair() {
    let content = data(80, 20_000);
    let mode = |h: &Harness| std::fs::metadata(h.p_path("f")).unwrap().mode() & 0o7777;
    let h = Arc::new(rs().config(|c| c.resync_limit = 2).build_with(|p, s| both(p, s, |r| write(r, "f", &data(80, 20_000)))));
    let primary_before = snapshot_of_primary(&h);
    // Every lookup of /f finds the wrong mode on the secondary (a stat lie that the repair, which looks at the
    // object through its descriptor, does not see: it "works" and the object diverges again on the next lookup).
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Mode(0o600))).path("/f"));
    let h2 = h.clone();
    let c = content.clone();
    let m = mode(&h);
    within(20, move || {
        for _ in 0..6 {
            assert_eq!(h2.lookup("/f").st.mode & 0o7777, m, "the application sees the primary's attributes");
            assert_eq!(h2.read_file("/f"), c, "and the primary's data");
        }
    });
    let c1 = cnt(&h);
    assert_eq!(c1.resyncs, 2, "exactly resync_limit repairs: {c1:?}");
    assert_eq!(c1.giveups, 1, "given up once, not again and again: {c1:?}");
    assert_eq!(c1.failures, 0);
    assert!(c1.repeats >= 2);
    // the object stays primary-only for the rest of the window, also when the secondary gets healthy
    h.clear_faults();
    let before = cnt(&h);
    for _ in 0..3 {
        h.lookup("/f");
        assert_eq!(h.read_file("/f"), content);
    }
    let after = cnt(&h);
    assert_eq!((after.mismatches, after.repeats, after.resyncs, after.failures, after.giveups), (before.mismatches, before.repeats, before.resyncs, before.failures, before.giveups));
    assert!(after.skipped > before.skipped, "primary-only");
    // the primary was not touched by any of this
    assert_eq!(snapshot_of_primary(&h), primary_before);
    // other objects have their own budget
    h.write_file("/g", b"g");
    flip_byte(&h.s_path("g"), 0);
    h.lookup("/g");
    assert_eq!(h.read_file("/g"), b"g");
    assert!(cnt(&h).resyncs > after.resyncs);
}

#[test]
fn budget_for_entries_counts_per_path_and_stops_the_repairs() {
    // the secondary hides /g in every lookup: the entry is "repaired" (it exists) but looks missing again
    let h = Arc::new(rs().config(|c| c.resync_limit = 2).build_with(|p, s| both(p, s, |r| write(r, "g", b"data"))));
    h.inject(Fault::new(FaultOp::Lookup, Effect::Errno(libc::ENOENT)).name("g"));
    let h2 = h.clone();
    within(20, move || {
        for _ in 0..6 {
            assert_eq!(h2.lookup("/g").st.size, 4);
        }
    });
    let c = cnt(&h);
    assert!(c.giveups >= 1, "{c:?}");
    assert!(c.resyncs + c.failures <= 2, "at most resync_limit repairs: {c:?}");
}

#[test]
fn budget_keeps_the_secondary_of_a_directory() {
    // A directory that always looks wrong is given up, but it keeps its secondary: what is below it is still compared.
    let h = Arc::new(rs().config(|c| c.resync_limit = 1).build_with(|p, s| both(p, s, |r| write(r, "d/f", &data(81, 5000)))));
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Mode(0o700))).path("/d"));
    for _ in 0..4 {
        h.lookup("/d");
    }
    let c = cnt(&h);
    assert_eq!(c.resyncs, 1);
    assert!(c.giveups >= 1, "{c:?}");
    h.clear_faults();
    let before = cnt(&h);
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 3 }).path("/d/f").once());
    assert_eq!(h.read_file("/d/f"), data(81, 5000));
    h.expect_mismatch_on(K::Data, None, OpKind::Read, "/d/f");
    assert!(cnt(&h).resyncs > before.resyncs, "the content below the given-up directory is still compared and repaired");
    h.assert_trees_equal();
}

/// Reads everything below `dir` through the engine (lookup, readdir, getattr, readlink, xattrs, file content),
/// the way a backup or `find -exec cat` would: every comparison the engine can make is made.
fn walk_all(h: &Harness, dir: &str) {
    for n in h.readdir(dir) {
        let path = if dir == "/" { format!("/{n}") } else { format!("{dir}/{n}") };
        let a = h.getattr(&path);
        match a.st.mode & libc::S_IFMT {
            libc::S_IFDIR => walk_all(h, &path),
            libc::S_IFREG => {
                h.read_file(&path);
                if let Ok(names) = h.listxattr(&path) {
                    for x in names {
                        let _ = h.getxattr(&path, &x);
                    }
                }
            }
            libc::S_IFLNK => {
                let _ = h.readlink(&path);
            }
            _ => {}
        }
    }
}

/// Repeats `walk_all` until a pass repairs nothing (at most `max` passes); returns the number of passes.
#[track_caller]
fn converge(h: &Harness, max: usize) -> usize {
    for pass in 1..=max {
        let before = cnt(h);
        walk_all(h, "/");
        let after = cnt(h);
        if std::env::var("XCHECKFS_TEST_LOG").is_ok() {
            eprintln!("pass {pass}: {before:?} -> {after:?}; differences now: {:?}", h.tree_diff());
        }
        if (after.mismatches, after.repeats, after.resyncs, after.failures) == (before.mismatches, before.repeats, before.resyncs, before.failures) {
            return pass;
        }
    }
    panic!("no convergence in {max} passes:\n  {}\n{:?}", h.describe_mismatches(), h.tree_diff());
}

// =================================================================================================================
// 8. Quarantine
// =================================================================================================================

/// The quarantined items: (directory, mismatch.txt).
fn quarantined(q: &Path) -> Vec<(PathBuf, String)> {
    let mut v: Vec<_> = std::fs::read_dir(q)
        .unwrap()
        .flatten()
        .map(|e| (e.path(), std::fs::read_to_string(e.path().join("mismatch.txt")).unwrap_or_else(|_| "<no mismatch.txt>".into())))
        .collect();
    v.sort();
    v
}

fn quarantined_as<'a>(items: &'a [(PathBuf, String)], slug: &str) -> &'a (PathBuf, String) {
    items
        .iter()
        .find(|(d, _)| d.file_name().unwrap().to_string_lossy().ends_with(&format!("-{slug}")))
        .unwrap_or_else(|| panic!("no quarantine item for {slug}: {:?}", items.iter().map(|i| &i.0).collect::<Vec<_>>()))
}

fn qdir() -> tempfile::TempDir {
    tmp_in(&fast_base())
}

#[test]
fn quarantine_saves_the_secondary_version_before_it_is_overwritten_or_removed() {
    let q = qdir();
    let qp = q.path().to_path_buf();
    let h = rs()
        .config({
            let qp = qp.clone();
            move |c| {
                c.quarantine = Some(qp);
                c.dir_nlink = false; // (the root's link count would make the engine repair everything at start)
            }
        })
        .build_with(|p, s| {
            both(p, s, |r| {
                write(r, "keep/file", b"keep");
                write(r, "f", &data(90, 20_000));
                symlink("right", r.join("l")).unwrap();
            });
            // overwritten: a file with other content
            let mut bad = data(90, 20_000);
            bad[10] ^= 0xff;
            bad[19_999] ^= 0xff;
            std::fs::write(s.join("f"), &bad).unwrap();
            // replaced: a symlink with the wrong target
            std::fs::remove_file(s.join("l")).unwrap();
            symlink("wrong", s.join("l")).unwrap();
            // removed: a whole directory subtree, a file, a symlink and a fifo that exist on the secondary only
            write(s, "extra_dir/sub/deep.txt", b"deep content");
            write(s, "extra_dir/top.txt", b"top content");
            write(s, "extra_file", b"extra file content");
            symlink("extra-target", s.join("extra_link")).unwrap();
            mkfifo(&s.join("extra_fifo"));
        });
    // never inside the trees
    assert!(!qp.starts_with(h.p_root()) && !qp.starts_with(h.s_root()));
    let f_before = std::fs::read(h.s_path("f")).unwrap();

    // the file: read mismatch
    assert_eq!(h.read_file("/f"), data(90, 20_000));
    // the symlink: lookup + readlink
    h.lookup("/l");
    assert_eq!(h.readlink("/l").unwrap(), b"right");
    // the extras: the listing
    assert_eq!(h.readdir("/"), vec!["f", "keep", "l"]);
    h.assert_trees_equal();
    let items = quarantined(&qp);
    assert_eq!(cnt(&h).quarantined as usize, items.len(), "stats and directory agree");

    // file: contents are the secondary's pre-repair bytes; mismatch.txt names the path and the reason
    let (dir, txt) = quarantined_as(&items, "f");
    assert_eq!(std::fs::read(dir.join("object")).unwrap(), f_before);
    assert!(txt.contains("path: /f") && txt.contains("reason:") && txt.contains("data"), "{txt}");
    // symlink
    let (dir, txt) = quarantined_as(&items, "l");
    assert_eq!(std::fs::read_link(dir.join("object")).unwrap(), Path::new("wrong"));
    assert!(txt.contains("path: /l"), "{txt}");
    // removed subtree: complete copy
    let (dir, txt) = quarantined_as(&items, "extra_dir");
    assert_eq!(std::fs::read(dir.join("object/sub/deep.txt")).unwrap(), b"deep content");
    assert_eq!(std::fs::read(dir.join("object/top.txt")).unwrap(), b"top content");
    assert!(txt.contains("path: /extra_dir") && txt.contains("readdir"), "{txt}");
    let (dir, _) = quarantined_as(&items, "extra_file");
    assert_eq!(std::fs::read(dir.join("object")).unwrap(), b"extra file content");
    let (dir, _) = quarantined_as(&items, "extra_link");
    assert_eq!(std::fs::read_link(dir.join("object")).unwrap(), Path::new("extra-target"));
    // (a fifo cannot be saved: it is noted)
    let (_, txt) = quarantined_as(&items, "extra_fifo");
    assert!(txt.contains("not copied"), "{txt}");
    // healthy objects were not quarantined
    assert!(items.iter().all(|(d, _)| !d.file_name().unwrap().to_string_lossy().ends_with("-keep")));

    // repeating the damage quarantines the new damage as well, with a new directory
    let n = items.len();
    flip_byte(&h.s_path("f"), 3);
    let f2 = std::fs::read(h.s_path("f")).unwrap();
    assert_eq!(h.read_file("/f"), data(90, 20_000));
    let items = quarantined(&qp);
    assert_eq!(items.len(), n + 1);
    assert!(items.iter().any(|(d, _)| std::fs::read(d.join("object")).ok().as_deref() == Some(&f2[..])));
    h.assert_trees_equal();
    // the quarantine is not part of any tree: no stray entries there
    assert!(!h.s_path("quarantine").exists());
}

#[test]
fn quarantine_of_a_replaced_directory_entry_keeps_the_old_file() {
    // a hard link that is a copy on the secondary, a type change, an overwritten target of a failed rename
    let q = qdir();
    let qp = q.path().to_path_buf();
    let h = rs()
        .config({
            let qp = qp.clone();
            move |c| c.quarantine = Some(qp)
        })
        .level(CheckLevel::Thorough)
        .build();
    h.write_file("/a", b"content a");
    h.write_file("/b", b"content b - to be overwritten");
    h.inject(Fault::new(FaultOp::Rename, Effect::Errno(libc::EIO)).once());
    h.rename("/a", "/b");
    h.clear_faults();
    h.assert_trees_equal();
    let items = quarantined(&qp);
    // the secondary's /a (the name that should be gone) and its old /b
    assert!(items.iter().any(|(d, _)| std::fs::read(d.join("object")).ok().as_deref() == Some(b"content a".as_slice())), "{items:?}");
    assert!(items.iter().any(|(d, _)| std::fs::read(d.join("object")).ok().as_deref() == Some(b"content b - to be overwritten".as_slice())), "{items:?}");
}

#[test]
fn quarantine_cap_truncates_and_says_so() {
    let q = qdir();
    let qp = q.path().to_path_buf();
    let h = rs()
        .config({
            let qp = qp.clone();
            move |c| {
                c.quarantine = Some(qp);
                c.quarantine_cap = 1000;
            }
        })
        .build_with(|p, s| {
            both(p, s, |r| write(r, "big", &data(91, 5000)));
            flip_byte(&s.join("big"), 4000);
            // a directory whose files together exceed the cap
            for i in 0..3 {
                write(s, &format!("extra/f{i}"), &data(92 + i, 600));
            }
        });
    let big_before = std::fs::read(h.s_path("big")).unwrap();
    assert_eq!(h.read_file("/big"), data(91, 5000));
    h.readdir("/");
    h.assert_trees_equal();
    let items = quarantined(&qp);
    let (dir, txt) = quarantined_as(&items, "big");
    let saved = std::fs::read(dir.join("object")).unwrap();
    assert_eq!(saved.len(), 1000);
    assert_eq!(saved, big_before[..1000]);
    assert!(txt.contains("truncated at 1000 of 5000 bytes"), "{txt}");
    let (dir, txt) = quarantined_as(&items, "extra");
    let total: u64 = std::fs::read_dir(dir.join("object")).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum();
    assert!(total <= 1000, "the cap is per quarantined object: {total}");
    assert!(txt.contains("truncated") || txt.contains("budget exhausted"), "{txt}");
}

#[test]
fn without_quarantine_nothing_is_written_and_repairs_still_happen() {
    let h = rs().config(|c| c.dir_nlink = false).build_with(|p, s| {
        both(p, s, |r| write(r, "f", &data(93, 3000)));
        flip_byte(&s.join("f"), 1);
        write(s, "extra/x", b"x");
        symlink("t", s.join("extra_link")).unwrap();
    });
    let before = cnt(&h);
    assert_eq!(h.read_file("/f"), data(93, 3000));
    h.readdir("/");
    let after = cnt(&h);
    assert!(after.resyncs >= before.resyncs + 2);
    assert_eq!(after.quarantined, 0);
    h.assert_trees_equal();
    // (the only places a quarantine could end up are the trees and the current directory)
    assert_eq!(std::fs::read_dir(h.s_root()).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(h.p_root()).unwrap().count(), 1);
}

#[test]
fn the_cli_refuses_a_quarantine_directory_inside_a_mirrored_tree() {
    let (p, s, m) = (qdir(), qdir(), qdir());
    for (name, q) in [
        ("primary", p.path().join("q")),
        ("secondary", s.path().join("deeper/q")),
        ("mount point", m.path().join("q")),
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_xcheckfs"))
            .arg("mount")
            .arg(m.path())
            .arg(p.path())
            .arg(s.path())
            .arg("--quarantine")
            .arg(&q)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name}: accepted: {err}");
        assert!(err.contains("quarantine directory") && err.contains("inside a mirrored tree"), "{name}: {err}");
        assert!(!q.exists(), "{name}: the directory was created");
    }
}

// =================================================================================================================
// 9. The primary is never modified by repairs
// =================================================================================================================

#[test]
fn primary_is_never_modified_by_repairs() {
    let xattrs = xattrs_supported(&fast_base());
    let q = qdir();
    let qp = q.path().to_path_buf();
    let h = rs()
        .config({
            let qp = qp.clone();
            move |c| {
                c.quarantine = Some(qp);
                c.dir_nlink = false;
            }
        })
        .build_with(|p, s| {
            both(p, s, |r| write(r, "outside/shared", &data(100, 8000)));
            build_tree(p, xattrs);
            std::fs::hard_link(p.join("outside/shared"), p.join("tree/c/ext_link")).unwrap();
            // a partially existing, damaged copy on the secondary
            both(p, s, |r| {
                write(r, "damaged/content", &data(101, 100_000));
                write(r, "damaged/size", &data(102, 5000));
                write(r, "damaged/mode", b"m");
                write(r, "damaged/time", b"t");
                symlink("good", r.join("damaged/link")).unwrap();
                write(r, "damaged/a", b"a");
                std::fs::hard_link(r.join("damaged/a"), r.join("damaged/b")).unwrap();
                if xattrs {
                    raw_setxattr(&r.join("damaged/content"), "user.k", b"v").unwrap();
                }
            });
            flip_byte(&s.join("damaged/content"), 55_555);
            std::fs::write(s.join("damaged/size"), data(102, 9999)).unwrap();
            std::fs::set_permissions(s.join("damaged/mode"), std::fs::Permissions::from_mode(0o777)).unwrap();
            raw_utimes(&s.join("damaged/time"), 1_000_000_000, 1_000_000_000);
            std::fs::remove_file(s.join("damaged/link")).unwrap();
            symlink("bad", s.join("damaged/link")).unwrap();
            std::fs::remove_file(s.join("damaged/b")).unwrap();
            write(s, "damaged/b", b"a");
            write(s, "damaged/extra/x", b"x");
            if xattrs {
                raw_setxattr(&s.join("damaged/content"), "user.k", b"WRONG").unwrap();
                raw_setxattr(&s.join("damaged/content"), "user.extra", b"1").unwrap();
            }
            write(s, "extra_top", b"x");
        });
    let before = snapshot_of_primary(&h);
    assert!(before.len() > 20);
    let c0 = cnt(&h);
    let passes = converge(&h, 6);
    let c1 = cnt(&h);
    assert!(c1.resyncs >= c0.resyncs + 6, "many repairs expected: {c1:?}");
    assert_eq!(c1.failures, c0.failures);
    h.assert_trees_equal();
    assert!(passes <= 4, "{passes} passes");
    // nothing about the primary changed: not content, not mode/owner/times (mtime AND ctime to the nanosecond),
    // not xattrs, not the inodes or the link structure
    assert_eq!(snapshot_of_primary(&h), before);
    // and quarantining did not touch it either
    assert!(!quarantined(&qp).is_empty());
}

#[test]
fn mount_time_differences_of_the_root_are_repaired_at_once() {
    let h = rs().build_with(|p, s| {
        both(p, s, |r| write(r, "f", b"x"));
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o750)).unwrap();
        std::fs::set_permissions(s, std::fs::Permissions::from_mode(0o700)).unwrap();
        write(s, "extra", b"x");
    });
    // no operation yet
    assert!(h.stats.resyncs.load(Relaxed) >= 1);
    assert_eq!(std::fs::metadata(h.s_root()).unwrap().mode() & 0o7777, 0o750);
    h.assert_trees_equal();
}

// =================================================================================================================
// 10. Concurrency
// =================================================================================================================

/// One random application operation on the shared tree `/t0 .. /t{dirs-1}`. Every operation either works or fails
/// with an errno on the primary (that is the result the application gets); it must never panic or hang.
fn random_op(h: &Harness, rng: &mut Rng, dirs: u64, xattrs: bool) {
    let dir = |rng: &mut Rng| format!("/t{}", rng.below(dirs));
    let name = |rng: &mut Rng| format!("{}/n{}", dir(rng), rng.below(12));
    let sub = |rng: &mut Rng| format!("{}/s{}", dir(rng), rng.below(4));
    match rng.below(18) {
        0 | 1 => {
            let p = name(rng);
            let n = rng.below(150_000) as usize;
            let seed = rng.next();
            let f = match h.try_open(&p, libc::O_WRONLY | libc::O_TRUNC) {
                Err(libc::ENOENT) => h.try_create(&p, 0o644, 0),
                r => r,
            };
            if let Ok(f) = f {
                let _ = h.try_pwrite(f, 0, &pattern(seed, n));
                h.close(f);
            }
        }
        2 => {
            let n = 1 + rng.below(3000) as usize;
            let bytes = rng.bytes(n);
            if let Ok(f) = h.try_open(&name(rng), libc::O_WRONLY | libc::O_APPEND) {
                let _ = h.try_pwrite(f, 0, &bytes);
                h.close(f);
            }
        }
        3 => {
            let _ = h.truncate(&name(rng), rng.below(100_000));
        }
        4 | 5 => {
            if let Ok(f) = h.try_open(&name(rng), libc::O_RDONLY) {
                let _ = h.try_pread(f, rng.below(1000), 1 + rng.below(100_000) as usize);
                h.close(f);
            }
        }
        6 => {
            let _ = h.try_mkdir(&sub(rng), 0o755);
        }
        7 => {
            let _ = h.try_rmdir(&sub(rng));
        }
        8 => {
            let _ = h.try_unlink(&name(rng));
        }
        9 => {
            let (a, b) = (name(rng), name(rng));
            let _ = h.try_rename(&a, &b);
        }
        10 => {
            let (a, b) = (sub(rng), sub(rng));
            let _ = h.try_rename(&a, &b);
        }
        11 => {
            let (a, b) = (name(rng), name(rng));
            let _ = h.try_link(&a, &b);
        }
        12 => {
            let p = name(rng);
            let _ = h.try_symlink("target", &p);
        }
        13 => {
            let _ = h.try_lookup(&name(rng)).map(|a| h.engine.getattr(&h.ctx, a.id));
            let _ = h.try_lookup(&sub(rng));
        }
        14 => {
            let d = dir(rng);
            if h.try_lookup(&d).is_ok() {
                h.readdir(&d);
            }
        }
        15 => {
            let _ = h.set_mtime(&name(rng), 1_500_000_000 + rng.below(1000) as i64);
        }
        16 => {
            if xattrs {
                let p = name(rng);
                let _ = h.setxattr(&p, "user.k", &rng.bytes(8));
                let _ = h.listxattr(&p);
            }
        }
        _ => {
            let _ = h.chmod_try(&name(rng), 0o600 + rng.below(0o200) as u32);
        }
    }
}

trait ChmodTry {
    fn chmod_try(&self, path: &str, mode: u32) -> Result<xcheckfs::engine::Attr, i32>;
}
impl ChmodTry for Harness {
    fn chmod_try(&self, path: &str, mode: u32) -> Result<xcheckfs::engine::Attr, i32> {
        self.setattr(path, xcheckfs::engine::SetAttr { mode: Some(mode), ..Default::default() })
    }
}

fn workload(h: &Arc<Harness>, threads: usize, ops: usize, seed: u64, xattrs: bool) {
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let h = h.clone();
            std::thread::spawn(move || {
                let mut rng = Rng::new(seed * 1000 + t as u64);
                for _ in 0..ops {
                    random_op(&h, &mut rng, 3, xattrs);
                }
            })
        })
        .collect();
    for t in hs {
        t.join().expect("worker thread panicked");
    }
}

#[test]
fn concurrent_workload_with_intermittent_faults_ends_with_equal_trees() {
    let xattrs = xattrs_supported(&fast_base());
    let h = Arc::new(
        rs()
            .level(CheckLevel::Thorough)
            // no budget problems in this test: it is about races, the budget has its own tests
            .config(|c| c.resync_limit = 1_000_000)
            .build_with(|p, s| {
                both(p, s, |r| {
                    for i in 0..3 {
                        std::fs::create_dir(r.join(format!("t{i}"))).unwrap();
                    }
                })
            }),
    );
    let scale = soak();
    // intermittent defects of the secondary, each on its own rhythm
    let faults: Vec<Fault> = vec![
        Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 2 }).every(13),
        Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Mode(0o600))).every(17),
        Fault::new(FaultOp::Lookup, Effect::Errno(libc::ENOENT)).every(37),
        Fault::new(FaultOp::Unlink, Effect::Skip).every(11),
        Fault::new(FaultOp::Mkdir, Effect::Errno(libc::EIO)).every(9),
        Fault::new(FaultOp::Rename, Effect::Skip).every(15),
        Fault::new(FaultOp::Rename, Effect::Errno(libc::EIO)).every(21),
        Fault::new(FaultOp::Create, Effect::Errno(libc::EIO)).every(19),
        Fault::new(FaultOp::Pwrite, Effect::DropWrite).every(23),
        Fault::new(FaultOp::Link, Effect::Errno(libc::EIO)).every(7),
        Fault::new(FaultOp::Symlink, Effect::Skip).every(8),
        Fault::new(FaultOp::Setxattr, Effect::Skip).every(5),
        Fault::new(FaultOp::Readdir, Effect::AddEntry(b"phantom".to_vec())).every(31),
        Fault::new(FaultOp::Truncate, Effect::Skip).every(14),
    ];
    for f in faults {
        h.inject(f);
    }
    let t0 = std::time::Instant::now();
    let h2 = h.clone();
    within(120 * scale as u64, move || workload(&h2, 6, 350 * scale, 1, xattrs));
    eprintln!("faulty phase: {:?}, {:?}", t0.elapsed(), cnt(&h));
    let c = cnt(&h);
    assert!(c.mismatches > 5 && c.resyncs > 5, "the faults must have caused repairs: {c:?}");

    // faults are gone: a few passes through everything converge
    h.clear_faults();
    let h2 = h.clone();
    let passes = within(120, move || converge(&h2, 8));
    eprintln!("converged in {passes} passes: {:?}", cnt(&h));
    h.assert_trees_equal();

    // From now on the file systems agree: nothing is reported, nothing repaired, whatever the application does.
    let before = cnt(&h);
    let h2 = h.clone();
    within(120 * scale as u64, move || workload(&h2, 6, 300 * scale, 2, xattrs));
    let after = cnt(&h);
    assert_eq!(
        (after.mismatches, after.repeats, after.resyncs, after.failures, after.giveups),
        (before.mismatches, before.repeats, before.resyncs, before.failures, before.giveups),
        "after the faults are gone:\n  {}",
        h.describe_mismatches()
    );
    h.assert_trees_equal();
    // the final full pass is quiet as well
    let h2 = h.clone();
    within(60, move || walk_all(&h2, "/"));
    assert_eq!(cnt(&h).mismatches, after.mismatches);
}

// =================================================================================================================
// 11. Freeze mode: the operator's "resync" action
// =================================================================================================================

#[test]
fn freeze_action_resync_uses_the_same_machinery() {
    let content = data(110, 30_000);
    let h = Arc::new(
        Harness::builder()
            .mode(MismatchMode::Freeze)
            .build_with(|p, s| {
                both(p, s, |r| write(r, "f", &data(110, 30_000)));
                flip_byte(&s.join("f"), 1234);
                write(p, "missing", b"only on the primary");
            }),
    );
    // object divergence (content), found by a read: frozen until the operator decides
    let resolver = h.resolve_when_pending(Action::Resync);
    let h2 = h.clone();
    let got = within(30, move || h2.read_file("/f"));
    resolver.join().unwrap();
    assert_eq!(got, content, "the application gets the primary's data");
    assert!(h.stats.resyncs.load(Relaxed) >= 1);
    assert_eq!(h.stats.resync_failures.load(Relaxed), 0);
    assert_eq!(std::fs::read(h.s_path("f")).unwrap(), content);
    // entry divergence (a name that is missing), found by a lookup
    let resolver = h.resolve_when_pending(Action::Resync);
    let h2 = h.clone();
    let size = within(30, move || h2.lookup("/missing").st.size);
    resolver.join().unwrap();
    assert_eq!(size, 19);
    assert_eq!(std::fs::read(h.s_path("missing")).unwrap(), b"only on the primary");
    h.assert_trees_equal();
    assert!(!h.policy.is_frozen());
    // the node was reconnected: compared again (and a new problem freezes again)
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 0 }).path("/missing").once());
    let resolver = h.resolve_when_pending(Action::Resync);
    let h2 = h.clone();
    within(30, move || h2.read_file("/missing"));
    resolver.join().unwrap();
    h.assert_trees_equal();
    assert!(h.stats.resyncs.load(Relaxed) >= 3);
    // "continue" does not repair
    let before = h.stats.resyncs.load(Relaxed);
    h.inject(Fault::new(FaultOp::Mkdir, Effect::Errno(libc::EIO)).once());
    let resolver = h.resolve_when_pending(Action::Continue);
    let h2 = h.clone();
    within(30, move || h2.mkdir("/dir"));
    resolver.join().unwrap();
    assert_eq!(h.stats.resyncs.load(Relaxed), before);
    assert!(!h.s_path("dir").exists());
}
