//! Detection tests: every fault effect of `FaultBackend` on the secondary must be reported as the expected
//! `MismatchKind`/field in the expected check level, and (documented weaker-mode behaviour) must NOT be reported
//! by operations whose level does not look.
//!
//! Each test builds a fresh harness (the policy de-duplicates mismatches per object/op/kind/field), prepares state
//! without faults, injects, acts, asserts. Add new cases by copying one of the small tests below.

mod common;

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::engine::SetAttr;
use xcheckfs::policy::MismatchKind as K;
use xcheckfs::stats::OpKind;

const B: CheckLevel = CheckLevel::Basic;
const T: CheckLevel = CheckLevel::Thorough;
const P: CheckLevel = CheckLevel::Paranoid;

/// Discards a value (avoids `drop` lints on Copy types).
fn ignore<T>(_: T) {}

fn data(n: usize) -> Vec<u8> {
    pattern(99, n)
}

/// Harness with `/f` (10000 bytes), `/d/` and a symlink `/l -> target` created before any fault is injected.
fn prepared(level: CheckLevel) -> Harness {
    let h = Harness::new(level, MismatchMode::Log);
    h.write_file("/f", &data(10_000));
    h.mkdir("/d");
    h.symlink("target", "/l");
    h.assert_no_mismatches();
    h
}

fn xattr_ok(h: &Harness) -> bool {
    xattrs_supported(h.p_root()) && xattrs_supported(h.s_root())
}

// ------------------------------------------------------------------------------------------------ errno

/// One entry per backend method: the secondary failing with EIO while the primary succeeds is a `Result`
/// mismatch of the matching engine operation, at every check level.
#[test]
fn errno_on_secondary_is_a_result_mismatch_for_every_op() {
    type Act = Box<dyn Fn(&Harness)>;
    let cases: Vec<(&str, FaultOp, OpKind, Act)> = vec![
        ("mkdir", FaultOp::Mkdir, OpKind::Mkdir, Box::new(|h| ignore(h.try_mkdir("/nd", 0o755)))),
        ("create", FaultOp::Create, OpKind::Create, Box::new(|h| ignore(h.try_create("/nf", 0o644, 0)))),
        ("mknod", FaultOp::Mknod, OpKind::Mknod, Box::new(|h| ignore(h.mknod_fifo("/fifo")))),
        ("symlink", FaultOp::Symlink, OpKind::Symlink, Box::new(|h| ignore(h.try_symlink("x", "/s2")))),
        ("link", FaultOp::Link, OpKind::Link, Box::new(|h| ignore(h.try_link("/f", "/hl")))),
        ("unlink", FaultOp::Unlink, OpKind::Unlink, Box::new(|h| h.try_unlink("/f").unwrap())),
        ("rmdir", FaultOp::Rmdir, OpKind::Rmdir, Box::new(|h| h.try_rmdir("/d").unwrap())),
        ("rename", FaultOp::Rename, OpKind::Rename, Box::new(|h| h.try_rename("/f", "/f2").unwrap())),
        ("chmod", FaultOp::Chmod, OpKind::Setattr, Box::new(|h| ignore(h.chmod("/f", 0o600)))),
        ("chown", FaultOp::Chown, OpKind::Setattr, Box::new(|h| {
            let c = h.ctx;
            h.setattr("/f", SetAttr { uid: Some(c.uid), gid: Some(c.gid), ..Default::default() }).unwrap();
        })),
        ("truncate", FaultOp::Truncate, OpKind::Setattr, Box::new(|h| ignore(h.truncate("/f", 10).unwrap()))),
        ("utimens", FaultOp::Utimens, OpKind::Setattr, Box::new(|h| ignore(h.utimes("/f", 1000, 2000).unwrap()))),
        ("open", FaultOp::Open, OpKind::Open, Box::new(|h| ignore(h.open("/f", libc::O_RDONLY)))),
        ("pread", FaultOp::Pread, OpKind::Read, Box::new(|h| ignore(h.read_file("/f")))),
        ("pwrite", FaultOp::Pwrite, OpKind::Write, Box::new(|h| {
            let f = h.open("/f", libc::O_RDWR);
            h.pwrite(f, 0, b"abc");
        })),
        ("stat", FaultOp::Stat, OpKind::Getattr, Box::new(|h| ignore(h.getattr("/f")))),
        ("lookup", FaultOp::Lookup, OpKind::Lookup, Box::new(|h| ignore(h.lookup("/f")))),
        ("readlink", FaultOp::Readlink, OpKind::Readlink, Box::new(|h| ignore(h.readlink("/l").unwrap()))),
        ("opendir", FaultOp::Opendir, OpKind::Opendir, Box::new(|h| ignore(h.readdir("/d")))),
        ("readdir", FaultOp::Readdir, OpKind::Readdir, Box::new(|h| ignore(h.readdir("/")))),
        ("access", FaultOp::Access, OpKind::Access, Box::new(|h| {
            let a = h.lookup("/f");
            h.engine.access(&h.ctx, a.id, libc::R_OK).unwrap();
        })),
        ("fallocate", FaultOp::Fallocate, OpKind::Fallocate, Box::new(|h| {
            let f = h.open("/f", libc::O_RDWR);
            h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 20_000, 0).unwrap();
        })),
        ("lseek", FaultOp::Lseek, OpKind::Lseek, Box::new(|h| {
            let f = h.open("/f", libc::O_RDONLY);
            h.engine.lseek(&h.ctx, f.ino, f.fh, 0, libc::SEEK_END).unwrap();
        })),
        ("copy_file_range", FaultOp::CopyFileRange, OpKind::CopyFileRange, Box::new(|h| {
            let (a, b) = (h.open("/f", libc::O_RDONLY), h.create("/g"));
            h.engine.copy_file_range(&h.ctx, a.fh, 0, b.fh, 0, 100, 0).unwrap();
        })),
        ("statfs", FaultOp::Statfs, OpKind::Statfs, Box::new(|h| ignore(h.engine.statfs(&h.ctx, 1).unwrap()))),
        ("flush", FaultOp::Flush, OpKind::Flush, Box::new(|h| {
            let f = h.open("/f", libc::O_RDONLY);
            h.engine.flush(&h.ctx, f.ino, f.fh, 1).unwrap();
        })),
        ("fsync", FaultOp::Fsync, OpKind::Fsync, Box::new(|h| {
            let f = h.open("/f", libc::O_RDWR);
            h.engine.fsync(&h.ctx, f.ino, f.fh, false).unwrap();
        })),
    ];
    for level in [B, T, P] {
        for (name, op, kind, act) in &cases {
            let h = prepared(level);
            let id = h.inject_always(*op, Effect::Errno(libc::EIO));
            act(&h);
            assert!(h.fault.hits(id) > 0, "{name}: the fault never fired (wrong FaultOp for this action?)");
            let m = h.expect_mismatch(K::Result, None);
            assert_eq!(m.op, *kind, "{name} @{level:?}: {}", m.summary());
            assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "EIO"), "{name} @{level:?}");
        }
    }
}

#[test]
fn xattr_errno_on_secondary_is_a_result_mismatch() {
    for op in [FaultOp::Setxattr, FaultOp::Getxattr, FaultOp::Listxattr, FaultOp::Removexattr] {
        let h = prepared(B);
        if !xattr_ok(&h) {
            eprintln!("SKIP: no user xattrs");
            return;
        }
        h.setxattr("/f", "user.k", b"v").unwrap();
        h.inject_always(op, Effect::Errno(libc::EIO));
        match op {
            FaultOp::Setxattr => h.setxattr("/f", "user.k2", b"v").unwrap(),
            FaultOp::Getxattr => ignore(h.getxattr("/f", "user.k").unwrap()),
            FaultOp::Listxattr => ignore(h.listxattr("/f").unwrap()),
            _ => h.removexattr("/f", "user.k").unwrap(),
        }
        let m = h.expect_mismatch(K::Result, None);
        assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "EIO"), "{op:?}");
    }
}

#[test]
fn errno_on_primary_and_differing_errnos() {
    // primary fails, secondary does not
    let h = prepared(B);
    h.pfault.inject(FaultOp::Mkdir, Effect::Errno(libc::ENOSPC));
    assert_eq!(h.try_mkdir("/x", 0o755).unwrap_err(), libc::ENOSPC, "the primary's errno is returned");
    let m = h.expect_mismatch(K::Result, None);
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("ENOSPC", "OK"));

    // both fail, differently
    let h = prepared(B);
    h.pfault.inject(FaultOp::Unlink, Effect::Errno(libc::EPERM));
    h.fault.inject(FaultOp::Unlink, Effect::Errno(libc::EACCES));
    assert_eq!(h.try_unlink("/f").unwrap_err(), libc::EPERM);
    let m = h.expect_mismatch(K::Result, None);
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("EPERM", "EACCES"));

    // both fail identically: that is agreement
    let h = prepared(T);
    h.pfault.inject(FaultOp::Pread, Effect::Errno(libc::EIO));
    h.fault.inject(FaultOp::Pread, Effect::Errno(libc::EIO));
    let f = h.open("/f", libc::O_RDONLY);
    assert_eq!(h.try_pread(f, 0, 100).unwrap_err(), libc::EIO);
    h.assert_no_mismatches();
}

/// "Applied but reported failure": the mutation took place on both sides, the secondary just says it did not.
#[test]
fn applied_then_errno_is_reported_although_both_trees_changed() {
    for level in [B, T, P] {
        let h = prepared(level);
        h.fault.inject(FaultOp::Unlink, Effect::AppliedThenErrno(libc::EIO));
        h.fault.inject(FaultOp::Mkdir, Effect::AppliedThenErrno(libc::EIO));
        h.fault.inject(FaultOp::Rename, Effect::AppliedThenErrno(libc::EIO));
        h.unlink("/f");
        h.mkdir("/made");
        h.rename("/l", "/l2");
        for (op, name) in [(OpKind::Unlink, "/f"), (OpKind::Mkdir, "/made"), (OpKind::Rename, "/l")] {
            let m = h.expect_mismatch_on(K::Result, None, op, name);
            assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "EIO"));
        }
        // the effects really are there on both sides
        h.fault.clear();
        let d: Vec<_> = h.tree_diff().into_iter().filter(|d| !d.contains("/made")).collect();
        assert!(d.is_empty(), "{d:?}");
    }
}

// ------------------------------------------------------------------------------------------------- data

/// Offset of the first difference as reported in a data mismatch's detail.
fn first_diff_offset(m: &xcheckfs::policy::Mismatch) -> u64 {
    let d = &m.detail;
    let i = d.find("offset ").unwrap_or_else(|| panic!("no offset in {d}")) + 7;
    d[i..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

#[test]
fn corrupt_read_is_a_data_mismatch_with_the_right_offset() {
    for (offset, read_off) in [(0usize, 0u64), (5, 100), (4095, 0)] {
        for level in [B, T, P] {
            let h = prepared(level);
            h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset }).path("/f"));
            let f = h.open("/f", libc::O_RDONLY);
            let got = h.pread(f, read_off, 8192);
            assert_eq!(got, data(10_000)[read_off as usize..(read_off as usize + 8192).min(10_000)], "client gets primary's data");
            let m = h.expect_mismatch(K::Data, None);
            assert_eq!(m.op, OpKind::Read);
            assert_eq!(first_diff_offset(&m), read_off + offset as u64, "{}", m.detail);
            assert!(m.path.ends_with("/f"));
        }
    }
}

#[test]
fn corrupt_read_beyond_returned_data_is_not_applied() {
    // a corruption offset past the end of what is read does not alter anything
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 50_000 }).path("/f"));
    assert_eq!(h.read_file("/f"), data(10_000));
    h.assert_no_mismatches();
}

#[test]
fn short_read_is_a_length_mismatch() {
    for level in [B, T, P] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Pread, Effect::ShortRead(100)).path("/f"));
        let f = h.open("/f", libc::O_RDONLY);
        assert_eq!(h.pread(f, 0, 4096).len(), 4096);
        let m = h.expect_mismatch(K::Length, None);
        assert_eq!(m.op, OpKind::Read);
        assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("4096", "100"));
    }
}

#[test]
fn short_write_is_a_length_mismatch() {
    for level in [B, T, P] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Pwrite, Effect::ShortWrite(10)).path("/f"));
        let f = h.open("/f", libc::O_RDWR);
        let n = h.engine.write(&h.ctx, f.ino, f.fh, 0, &data(100)).unwrap();
        assert_eq!(n, 100, "the primary's count is returned");
        let m = h.expect_mismatch(K::Length, None);
        assert_eq!(m.op, OpKind::Write);
        assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("100", "10"));
        // the truthful short write is consistent with its read-back: no Verify mismatch
        h.assert_no_mismatch_kind(K::Verify);
    }
}

/// A write that reports success but never reached the file (extending the file).
#[test]
fn dropped_write_basic_sees_it_on_a_later_read_or_getattr_thorough_at_once() {
    // Basic: the write op itself cannot tell...
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f"));
    let f = h.open("/f", libc::O_RDWR);
    h.pwrite(f, 10_000, &data(500)); // extends the file
    h.close(f);
    h.assert_no_mismatches();
    // ... but any later getattr (size) and read (length) does.
    h.getattr("/f");
    let m = h.expect_mismatch(K::Attr, Some("size"));
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("10500", "10000"));
    assert_eq!(h.read_file("/f").len(), 10_500);
    h.expect_mismatch(K::Length, None);

    // overwrite in place: the size stays equal, only reads see the stale data
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f"));
    let f = h.open("/f", libc::O_RDWR);
    h.pwrite(f, 100, &[0xAB; 200]);
    h.close(f);
    h.getattr("/f");
    h.assert_no_mismatches(); // attributes alone cannot see it
    h.clear_faults();
    let got = h.read_file("/f");
    assert_eq!(&got[100..300], &[0xAB; 200]);
    let m = h.expect_mismatch(K::Data, None);
    assert_eq!(first_diff_offset(&m), 100, "{}", m.detail);
}

#[test]
fn dropped_write_is_caught_at_once_by_thorough_and_paranoid() {
    for level in [T, P] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f"));
        let f = h.open("/f", libc::O_RDWR);
        h.pwrite(f, 100, &[0xAB; 200]); // in-place overwrite: only the read-back can see it
        let m = h.expect_mismatch_on(K::Verify, Some("write read-back"), OpKind::Write, "/f");
        assert_eq!(m.primary, "ok", "{}", m.summary());
        assert!(m.secondary.contains("read-back differs from written data"), "{}", m.summary());
    }
}

#[test]
fn corrupt_write_basic_later_read_thorough_immediately() {
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::CorruptWrite { offset: 7 }).path("/f"));
    let f = h.open("/f", libc::O_RDWR);
    h.pwrite(f, 0, &data(64));
    h.assert_no_mismatches();
    h.clear_faults();
    let got = h.pread(f, 0, 64);
    assert_eq!(got, data(64));
    let m = h.expect_mismatch(K::Data, None);
    assert_eq!(first_diff_offset(&m), 7);

    for level in [T, P] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Pwrite, Effect::CorruptWrite { offset: 7 }).path("/f"));
        let f = h.open("/f", libc::O_RDWR);
        h.pwrite(f, 0, &data(64));
        let m = h.expect_mismatch_on(K::Verify, Some("write read-back"), OpKind::Write, "/f");
        assert!(m.secondary.contains("differs"), "{}", m.summary());
    }
}

/// Paranoid compares the complete file when it is closed; thorough/basic do not (only reads would).
#[test]
fn paranoid_compares_whole_file_on_close() {
    for (level, detected) in [(B, false), (T, false), (P, true)] {
        let h = prepared(level);
        let f = h.open("/f", libc::O_RDWR);
        h.pwrite(f, 0, &data(100));
        h.assert_no_mismatches();
        // the secondary's file content goes bad *after* the write was verified: only the close-time compare sees it
        h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 9000 }).path("/f"));
        h.close(f);
        if detected {
            let m = h.expect_mismatch_on(K::Content, None, OpKind::Release, "/f");
            assert_eq!(first_diff_offset(&m), 9000, "{}", m.detail);
        } else {
            h.assert_no_mismatches();
        }
    }
}

/// Behind-the-engine divergence (the secondary's file changed on its own): paranoid sees it on the next close of
/// a writer, basic on the next read.
#[test]
fn silent_divergence_of_secondary_content() {
    let h = prepared(P);
    std::fs::write(h.s_path("/f"), data(10_000).iter().map(|b| b ^ 1).collect::<Vec<_>>()).unwrap();
    let f = h.open("/f", libc::O_RDWR);
    h.pwrite(f, 20_000, b"x");
    h.close(f);
    h.expect_mismatch(K::Content, None);
}

// ------------------------------------------------------------------------------------------------- attrs

#[test]
fn stat_lies_are_attr_mismatches_with_the_field_name() {
    let lies: Vec<(&str, StatLie, &str)> = vec![
        ("size", StatLie::Size(777), "size"),
        ("mode", StatLie::Mode(0o600), "mode"),
        ("nlink", StatLie::Nlink(5), "nlink"),
        ("uid", StatLie::Uid(4242), "uid"),
        ("gid", StatLie::Gid(4242), "gid"),
        ("mtime", StatLie::MtimeShift(3600), "mtime"),
        ("type", StatLie::Type(libc::S_IFLNK), "type"),
    ];
    for level in [B, T, P] {
        for (name, lie, field) in &lies {
            let h = prepared(level);
            h.inject(Fault::new(FaultOp::Stat, Effect::Stat(lie.clone())).path("/f"));
            h.getattr("/f");
            let m = h.expect_mismatch(K::Attr, Some(field));
            assert_eq!(m.op, OpKind::Getattr, "{name}");
            assert_ne!(m.primary, m.secondary, "{name}");
        }
    }
}

#[test]
fn stat_lie_values_are_reported() {
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Mode(0o600))).path("/f"));
    h.getattr("/f");
    let m = h.expect_mismatch(K::Attr, Some("mode"));
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("0644", "0600"));
}

#[test]
fn lookup_stat_lie_is_detected_at_lookup() {
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Size(1))).name("f"));
    h.lookup("/f");
    let m = h.expect_mismatch(K::Attr, Some("size"));
    assert_eq!(m.op, OpKind::Lookup);
}

/// ctime is not comparable in absolute terms, but its *changes* are: one side jumping while the other stays put.
#[test]
fn ctime_change_on_one_side_only() {
    let h = prepared(B);
    h.getattr("/f"); // remember the ctimes
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::CtimeShift(60))).path("/f").once());
    h.getattr("/f");
    h.expect_mismatch(K::Attr, Some("ctime"));
    // ... and a ctime that moves on BOTH sides is fine
    let h = prepared(B);
    h.getattr("/f");
    h.chmod("/f", 0o600);
    h.getattr("/f");
    h.assert_no_mismatches();
}

#[test]
fn skipped_setattr_steps_are_caught_by_the_attr_comparison_after_setattr() {
    // chmod / truncate / utimens / chown that the secondary silently skips: the stat taken at the end of
    // setattr differs, in every mode.
    #[allow(clippy::type_complexity)]
    let cases: Vec<(FaultOp, &str, Box<dyn Fn(&Harness)>)> = vec![
        (FaultOp::Chmod, "mode", Box::new(|h| ignore(h.chmod("/f", 0o600)))),
        (FaultOp::Truncate, "size", Box::new(|h| ignore(h.truncate("/f", 10).unwrap()))),
        (FaultOp::Utimens, "mtime", Box::new(|h| ignore(h.utimes("/f", 1000, 5_000_000).unwrap()))),
    ];
    for level in [B, T, P] {
        for (op, field, act) in &cases {
            let h = prepared(level);
            h.inject_always(*op, Effect::Skip);
            act(&h);
            let m = h.expect_mismatch(K::Attr, Some(field));
            assert_eq!(m.op, OpKind::Setattr, "{op:?}");
        }
    }
    if is_root() {
        let h = prepared(B);
        h.inject_always(FaultOp::Chown, Effect::Skip);
        h.setattr("/f", SetAttr { uid: Some(1234), ..Default::default() }).unwrap();
        h.expect_mismatch(K::Attr, Some("uid"));
    }
}

/// Thorough additionally checks the requested values themselves, on both sides: a file system that applies
/// something *else* than asked, consistently on both sides, is still wrong... but only if one side deviates is
/// it a mismatch (both failing the check is only a warning, since the FS may legitimately round, e.g. times).
#[test]
fn setattr_applied_verification_needs_thorough() {
    let h = prepared(T);
    // secondary applies a *different* mode but reports the same attributes afterwards
    h.inject(Fault::new(FaultOp::Chmod, Effect::Skip).path("/f"));
    h.chmod("/f", 0o640);
    let ms = h.new_mismatches();
    assert!(ms.iter().any(|m| m.kind == K::Attr && m.field.as_deref() == Some("mode")), "{}", h.describe_mismatches());
    assert!(ms.iter().any(|m| m.kind == K::Verify && m.field.as_deref() == Some("setattr applied")), "{}", h.describe_mismatches());
    // basic: only the attr comparison
    let h = prepared(B);
    h.inject(Fault::new(FaultOp::Chmod, Effect::Skip).path("/f"));
    h.chmod("/f", 0o640);
    h.assert_no_mismatch_kind(K::Verify);
    h.expect_mismatch(K::Attr, Some("mode"));
}

// ------------------------------------------------------------------------------------------- directories

fn detail_has(m: &xcheckfs::policy::Mismatch, s: &str) {
    assert!(m.detail.contains(s), "detail {:?} lacks {s:?}", m.detail);
}

#[test]
fn readdir_faults() {
    for level in [B, T, P] {
        // an entry that the secondary hides
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Readdir, Effect::DropEntry(b"f".to_vec())));
        h.readdir("/");
        let m = h.expect_mismatch(K::Readdir, None);
        detail_has(&m, "only in primary: [f]");

        // an entry that does not exist
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Readdir, Effect::AddEntry(b"ghost".to_vec())));
        h.readdir("/");
        let m = h.expect_mismatch(K::Readdir, None);
        detail_has(&m, "only in secondary: [ghost]");

        // an entry of another type
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Readdir, Effect::ChangeEntryKind(b"f".to_vec(), xcheckfs::sys::FileKind::Dir)));
        h.readdir("/");
        let m = h.expect_mismatch(K::Readdir, None);
        detail_has(&m, "type differs: [f]");
    }
}

/// Paranoid compares the complete parent listing right after every namespace change; the other levels only when a
/// listing is read.
#[test]
fn paranoid_compares_listing_after_namespace_changes() {
    type Act = Box<dyn Fn(&Harness)>;
    let cases: Vec<(&str, Act)> = vec![
        ("create", Box::new(|h| ignore(h.write_file("/new", b"x")))),
        ("mkdir", Box::new(|h| ignore(h.mkdir("/newdir")))),
        ("symlink", Box::new(|h| ignore(h.symlink("t", "/newl")))),
        ("link", Box::new(|h| ignore(h.link("/f", "/newhl")))),
        ("unlink", Box::new(|h| h.unlink("/f"))),
        ("rename", Box::new(|h| h.rename("/f", "/f2"))),
    ];
    for (name, act) in &cases {
        for (level, detected) in [(B, false), (T, false), (P, true)] {
            let h = prepared(level);
            // the secondary's listing always loses the entry "ghostly": every listing is wrong, but only the
            // paranoid post-operation listing comparison looks at it
            h.inject(Fault::new(FaultOp::Readdir, Effect::AddEntry(b"ghostly".to_vec())));
            act(&h);
            let seen = h.find_mismatch(K::Readdir, None);
            assert_eq!(seen.is_some(), detected, "{name} @{level:?}: {}", h.describe_mismatches());
            if let Some(m) = seen {
                assert_eq!(m.op.name(), if *name == "create" { "create" } else { name }, "{}", m.summary());
            }
        }
    }
}

#[test]
fn readlink_target_fault() {
    for level in [B, T, P] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Readlink, Effect::ReadlinkTarget(b"elsewhere".to_vec())));
        assert_eq!(h.readlink("/l").unwrap(), b"target");
        let m = h.expect_mismatch(K::Readlink, None);
        assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("target", "elsewhere"));
    }
}

/// Symlink creation is verified by reading the target back in thorough mode.
#[test]
fn symlink_target_verified_at_creation_only_by_thorough() {
    for (level, detected) in [(B, false), (T, true), (P, true)] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Readlink, Effect::ReadlinkTarget(b"elsewhere".to_vec())));
        h.symlink("somewhere", "/newlink");
        let m = h.find_mismatch(K::Verify, Some("symlink target"));
        assert_eq!(m.is_some(), detected, "@{level:?}: {}", h.describe_mismatches());
        if let Some(m) = m {
            assert_eq!(m.op, OpKind::Symlink);
            assert!(m.secondary.contains("elsewhere"), "{}", m.summary());
        }
    }
}

// ----------------------------------------------------------------------------------------------- xattrs

#[test]
fn xattr_value_and_list_faults() {
    let h0 = prepared(B);
    if !xattr_ok(&h0) {
        eprintln!("SKIP: no user xattrs");
        return;
    }
    for level in [B, T, P] {
        // value
        let h = prepared(level);
        h.setxattr("/f", "user.k", b"value").unwrap();
        h.inject(Fault::new(FaultOp::Getxattr, Effect::XattrValue(b"other".to_vec())).name("user.k"));
        assert_eq!(h.getxattr("/f", "user.k").unwrap(), b"value");
        let m = h.expect_mismatch(K::Xattr, Some("user.k"));
        assert_eq!(m.op, OpKind::Getxattr);

        // list: extra name
        let h = prepared(level);
        h.setxattr("/f", "user.k", b"value").unwrap();
        h.inject(Fault::new(FaultOp::Listxattr, Effect::XattrListAdd(b"user.phantom".to_vec())));
        assert_eq!(h.user_xattrs("/f").unwrap(), vec!["user.k"]);
        let m = h.expect_mismatch(K::Xattr, Some("list"));
        assert!(m.secondary.contains("user.phantom"), "{}", m.summary());

        // list: dropped name
        let h = prepared(level);
        h.setxattr("/f", "user.k", b"value").unwrap();
        h.setxattr("/f", "user.k2", b"value").unwrap();
        h.inject(Fault::new(FaultOp::Listxattr, Effect::XattrListDrop(b"user.k2".to_vec())));
        h.listxattr("/f").unwrap();
        h.expect_mismatch(K::Xattr, Some("list"));
    }
}

#[test]
fn xattr_set_and_remove_verified_by_thorough() {
    let h0 = prepared(B);
    if !xattr_ok(&h0) {
        eprintln!("SKIP: no user xattrs");
        return;
    }
    // secondary silently skips setxattr / removexattr
    for (level, detected) in [(B, false), (T, true), (P, true)] {
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Setxattr, Effect::Skip));
        h.setxattr("/f", "user.k", b"value").unwrap();
        assert_eq!(h.find_mismatch(K::Verify, Some("xattr set")).is_some(), detected, "set @{level:?}");
        // a later getxattr notices in any mode
        h.clear_faults();
        let _ = h.getxattr("/f", "user.k");
        h.expect_mismatch(K::Result, None);

        let h = prepared(level);
        h.setxattr("/f", "user.k", b"value").unwrap();
        h.inject(Fault::new(FaultOp::Removexattr, Effect::Skip));
        h.removexattr("/f", "user.k").unwrap();
        assert_eq!(h.find_mismatch(K::Verify, Some("xattr removed")).is_some(), detected, "remove @{level:?}");
    }
    // secondary stores a corrupted value
    let h = prepared(T);
    h.inject(Fault::new(FaultOp::Getxattr, Effect::XattrValue(b"corrupt".to_vec())));
    h.setxattr("/f", "user.k", b"value").unwrap();
    h.expect_mismatch(K::Verify, Some("xattr set"));
}

// -------------------------------------------------------------------------------- "silently not applied"

/// The secondary reports success for a namespace change it did not make. Detected immediately in thorough mode
/// (read-back of the name), in basic mode as soon as anything looks at the name again.
#[test]
fn skipped_unlink_rmdir_rename() {
    for (level, immediate) in [(B, false), (T, true), (P, true)] {
        // unlink
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Unlink, Effect::Skip));
        h.unlink("/f");
        let m = h.find_mismatch(K::Verify, Some("removed"));
        assert_eq!(m.is_some(), immediate, "unlink @{level:?}: {}", h.describe_mismatches());
        if let Some(m) = m {
            assert_eq!(m.op, OpKind::Unlink);
            assert_eq!(m.secondary, "name still exists");
        } else {
            h.assert_no_mismatches();
        }
        // the next lookup of the name exposes it in every mode
        let h2 = prepared(level);
        h2.inject(Fault::new(FaultOp::Unlink, Effect::Skip));
        h2.unlink("/f");
        h2.mark();
        assert!(h2.try_lookup("/f").is_err());
        let m = h2.expect_mismatch(K::Result, None);
        assert_eq!((m.op, m.primary.as_str(), m.secondary.as_str()), (OpKind::Lookup, "ENOENT", "OK"));

        // rmdir
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Rmdir, Effect::Skip));
        h.rmdir("/d");
        assert_eq!(h.find_mismatch(K::Verify, Some("removed")).is_some(), immediate, "rmdir @{level:?}");

        // rename
        let h = prepared(level);
        h.inject(Fault::new(FaultOp::Rename, Effect::Skip));
        h.rename("/f", "/f2");
        assert_eq!(h.find_mismatch(K::Verify, Some("renamed")).is_some(), immediate, "rename @{level:?}: {}", h.describe_mismatches());
        let h2 = prepared(level);
        h2.inject(Fault::new(FaultOp::Rename, Effect::Skip));
        h2.rename("/f", "/f2");
        h2.mark();
        h2.lookup("/f2");
        h2.expect_mismatch(K::Result, None);
    }
}

#[test]
fn skipped_rename_exchange_and_overwrite() {
    for flags in [libc::RENAME_EXCHANGE, 0] {
        let h = prepared(T);
        h.write_file("/g", b"gg");
        h.inject(Fault::new(FaultOp::Rename, Effect::Skip));
        h.try_rename_flags("/f", "/g", flags).unwrap();
        h.expect_mismatch(K::Verify, Some("renamed"));
    }
}

/// A creating operation the secondary skips: the secondary's follow-up lookup of the new name fails while the
/// secondary claimed success. That must be reported in every mode.
#[test]
fn skipped_creates_are_reported() {
    type Act = Box<dyn Fn(&Harness)>;
    let cases: Vec<(&str, FaultOp, Act)> = vec![
        ("mkdir", FaultOp::Mkdir, Box::new(|h| ignore(h.mkdir("/new")))),
        ("mknod", FaultOp::Mknod, Box::new(|h| ignore(h.mknod_fifo("/new").unwrap()))),
        ("symlink", FaultOp::Symlink, Box::new(|h| ignore(h.symlink("t", "/new")))),
        ("link", FaultOp::Link, Box::new(|h| ignore(h.link("/f", "/new")))),
    ];
    for level in [B, T, P] {
        for (name, op, act) in &cases {
            let h = prepared(level);
            h.inject_always(*op, Effect::Skip);
            act(&h);
            assert!(!h.new_mismatches().is_empty(), "{name} @{level:?}: skipped create went unnoticed");
            h.expect_mismatch(K::Result, None);
        }
    }
}

#[test]
fn skipped_fallocate_and_punch_hole() {
    // extending fallocate: size differs right after, in thorough
    for (level, immediate) in [(B, false), (T, true), (P, true)] {
        let h = prepared(level);
        let f = h.open("/f", libc::O_RDWR);
        h.inject(Fault::new(FaultOp::Fallocate, Effect::Skip));
        h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 50_000, 0).unwrap();
        assert_eq!(h.find_mismatch(K::Attr, Some("size")).is_some(), immediate, "extend @{level:?}");
        h.getattr("/f");
        h.expect_mismatch(K::Attr, Some("size")); // basic notices at the next getattr
    }
    // punch hole: the secondary keeps the old data
    for (level, immediate) in [(B, false), (T, true), (P, true)] {
        let h = prepared(level);
        let f = h.open("/f", libc::O_RDWR);
        h.inject(Fault::new(FaultOp::Fallocate, Effect::Skip));
        h.engine
            .fallocate(&h.ctx, f.ino, f.fh, 1000, 4000, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE)
            .unwrap();
        assert_eq!(h.find_mismatch(K::Verify, Some("zeroed range")).is_some(), immediate, "punch @{level:?}");
        h.clear_faults();
        h.pread(f, 0, 10_000);
        h.expect_mismatch(K::Data, None);
    }
}

#[test]
fn skipped_copy_file_range() {
    for (level, immediate) in [(B, false), (T, true), (P, true)] {
        let h = prepared(level);
        let (a, b) = (h.open("/f", libc::O_RDONLY), h.create("/g"));
        h.inject(Fault::new(FaultOp::CopyFileRange, Effect::Skip));
        let n = h.engine.copy_file_range(&h.ctx, a.fh, 0, b.fh, 0, 5000, 0).unwrap();
        assert_eq!(n, 5000);
        let m = h.find_mismatch(K::Verify, Some("copied range"));
        assert_eq!(m.is_some(), immediate, "@{level:?}: {}", h.describe_mismatches());
        if !immediate {
            h.assert_no_mismatches();
            h.clear_faults();
            h.getattr("/g");
            h.expect_mismatch(K::Attr, Some("size"));
        }
    }
    // short copies are legal (and file systems differ in how much one call copies): the secondary is driven to
    // the primary's count, which is not a mismatch
    for level in [B, T, P] {
        let h = prepared(level);
        let (a, b) = (h.open("/f", libc::O_RDONLY), h.create("/g"));
        h.inject(Fault::new(FaultOp::CopyFileRange, Effect::ShortWrite(100)));
        assert_eq!(h.engine.copy_file_range(&h.ctx, a.fh, 0, b.fh, 0, 5000, 0).unwrap(), 5000);
        h.clear_faults();
        h.getattr("/g");
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
    // ... but a secondary that cannot get there is
    let h = prepared(B);
    let (a, b) = (h.open("/f", libc::O_RDONLY), h.create("/g"));
    h.inject(Fault::new(FaultOp::CopyFileRange, Effect::ShortWrite(100)));
    h.inject(Fault::new(FaultOp::CopyFileRange, Effect::Errno(libc::EIO)).nth(2));
    h.engine.copy_file_range(&h.ctx, a.fh, 0, b.fh, 0, 5000, 0).unwrap();
    let m = h.expect_mismatch(K::Length, None);
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("5000", "100"));
}

// ------------------------------------------------------------------------------------- lseek / delays

#[test]
fn lseek_offset_fault() {
    let h = prepared(B);
    let f = h.open("/f", libc::O_RDONLY);
    h.inject(Fault::new(FaultOp::Lseek, Effect::Offset(42)));
    assert_eq!(h.engine.lseek(&h.ctx, f.ino, f.fh, 0, libc::SEEK_END).unwrap(), 10_000);
    let m = h.expect_mismatch(K::Length, Some("offset"));
    assert_eq!(m.op, OpKind::Lseek);
    // SEEK_DATA / SEEK_HOLE granularity is file-system specific and never a mismatch
    let h = prepared(B);
    let f = h.open("/f", libc::O_RDONLY);
    h.inject(Fault::new(FaultOp::Lseek, Effect::Offset(42)));
    let _ = h.engine.lseek(&h.ctx, f.ino, f.fh, 0, libc::SEEK_DATA);
    h.assert_no_mismatches();
}

#[test]
fn delays_are_not_mismatches_and_are_visible_in_the_latency_statistics() {
    for level in [B, T, P] {
        let h = prepared(level);
        let w = h.stats.op(OpKind::Write);
        let (p0, s0) = (w.primary.snapshot(), w.secondary.snapshot());
        for op in FaultOp::ALL {
            h.fault.add(Fault::new(*op, Effect::Delay(std::time::Duration::from_millis(3))));
        }
        h.write_file("/slow", &data(5000));
        assert_eq!(h.read_file("/slow"), data(5000));
        h.rename("/slow", "/slow2");
        h.unlink("/slow2");
        h.assert_no_mismatches();
        let (p, s) = (w.primary.snapshot().delta(&p0).mean(), w.secondary.snapshot().delta(&s0).mean());
        assert!(s > p + 2_000_000, "secondary mean {s}ns should exceed primary {p}ns by about 3 ms");
        h.assert_trees_equal();
    }
}

// ------------------------------------------------------------------------- fault harness self-checks

#[test]
fn fault_triggers_and_counters() {
    use xcheckfs::backend::fault::Trigger;
    let h = prepared(B);
    let once = h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Size(1))).path("/f").trigger(Trigger::Once));
    let nth = h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Nlink(9))).path("/f").nth(3));
    let after = h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Uid(9))).path("/f").after(4));
    let every = h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Gid(9))).path("/f").every(2));
    let other = h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Gid(9))).path("/nonexistent"));
    h.fault.reset_calls();
    for _ in 0..6 {
        h.getattr("/f");
    }
    assert_eq!(h.fault.hits(once), 1);
    assert_eq!(h.fault.hits(nth), 1);
    assert_eq!(h.fault.hits(after), 2);
    assert_eq!(h.fault.hits(every), 3);
    assert_eq!(h.fault.hits(other), 0, "a path filter that does not match never fires");
    assert_eq!(h.fault.calls(FaultOp::Stat), 6);
    assert_eq!(h.fault.remove(once), 1);
    h.fault.clear();
    assert_eq!(h.fault.hits(nth), 0);
    h.mark();
    h.getattr("/f");
    h.assert_no_mismatches();
}

#[test]
fn faults_can_change_while_the_engine_runs() {
    let h = std::sync::Arc::new(prepared(B));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let t = {
        let (h, stop) = (h.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let id = h.fault.add(Fault::new(FaultOp::Pread, Effect::Delay(std::time::Duration::from_micros(50))));
                std::thread::yield_now();
                h.fault.remove(id);
            }
        })
    };
    for _ in 0..200 {
        assert_eq!(h.read_file("/f"), data(10_000));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    t.join().unwrap();
    h.assert_no_mismatches();
}

// --------------------------------------------------------------------- pre-seeded divergence

/// The two trees differ before the engine ever sees them: every kind of difference is found when the objects are
/// first looked at / read.
#[test]
fn preseeded_differences_are_found() {
    let h = Harness::builder().build_with(|p, s| {
        for r in [p, s] {
            std::fs::write(r.join("same"), b"same content").unwrap();
            std::fs::write(r.join("datadiff"), b"AAAAAAAAAA").unwrap();
            std::fs::write(r.join("modediff"), b"x").unwrap();
            std::fs::write(r.join("sizediff"), b"short").unwrap();
            std::os::unix::fs::symlink("t1", r.join("linkdiff")).unwrap();
        }
        std::fs::write(s.join("datadiff"), b"AAAAABAAAA").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(s.join("modediff"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(s.join("sizediff"), b"shorter").unwrap();
        std::fs::remove_file(s.join("linkdiff")).unwrap();
        std::os::unix::fs::symlink("t2", s.join("linkdiff")).unwrap();
        std::fs::write(p.join("only_primary"), b"p").unwrap();
        std::fs::write(s.join("only_secondary"), b"s").unwrap();
        std::fs::create_dir(p.join("dirvsfile")).unwrap();
        std::fs::write(s.join("dirvsfile"), b"f").unwrap();
    });
    h.readdir("/");
    let m = h.expect_mismatch(K::Readdir, None);
    assert!(m.detail.contains("only_primary") && m.detail.contains("only_secondary") && m.detail.contains("dirvsfile"), "{}", m.detail);
    h.lookup("/same");
    h.assert_no_mismatch_kind(K::Result);
    assert_eq!(h.read_file("/same"), b"same content");
    assert_eq!(h.read_file("/datadiff"), b"AAAAAAAAAA");
    h.expect_mismatch_on(K::Data, None, OpKind::Read, "/datadiff");
    h.lookup("/modediff");
    h.expect_mismatch_on(K::Attr, Some("mode"), OpKind::Lookup, "/modediff");
    h.lookup("/sizediff");
    h.expect_mismatch_on(K::Attr, Some("size"), OpKind::Lookup, "/sizediff");
    assert_eq!(h.readlink("/linkdiff").unwrap(), b"t1");
    h.expect_mismatch_on(K::Readlink, None, OpKind::Readlink, "/linkdiff");
    h.lookup("/only_primary");
    let m = h.expect_mismatch_on(K::Result, None, OpKind::Lookup, "/only_primary");
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "ENOENT"));
    assert_eq!(h.try_lookup("/only_secondary").unwrap_err(), libc::ENOENT);
    let m = h.expect_mismatch_on(K::Result, None, OpKind::Lookup, "/only_secondary");
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("ENOENT", "OK"));
    h.lookup("/dirvsfile");
    h.expect_mismatch_on(K::Attr, Some("type"), OpKind::Lookup, "/dirvsfile");
}

/// A file that exists only on the primary diverged: it is handled primary-only from then on (counted, silent).
#[test]
fn objects_missing_on_the_secondary_are_primary_only() {
    let h = Harness::builder().build_with(|p, _| std::fs::write(p.join("only_primary"), b"p").unwrap());
    h.lookup("/only_primary");
    h.expect_mismatch(K::Result, None);
    let before = h.stats.mismatches.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(h.read_file("/only_primary"), b"p");
    h.getattr("/only_primary");
    h.write_file("/only_primary", b"updated");
    assert_eq!(h.stats.mismatches.load(std::sync::atomic::Ordering::Relaxed), before, "no follow-up noise");
    assert!(h.stats.secondary_skipped.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

// ---------------------------------------------------------------------------------- hard-link identity

#[test]
fn hard_link_structure_mismatch_primary_linked_secondary_copied() {
    let h = Harness::builder().build_with(|p, s| {
        std::fs::write(p.join("a"), b"data").unwrap();
        std::fs::hard_link(p.join("a"), p.join("b")).unwrap();
        std::fs::write(s.join("a"), b"data").unwrap();
        std::fs::write(s.join("b"), b"data").unwrap(); // a copy instead of a link
    });
    h.lookup("/a");
    h.assert_no_mismatch_kind(K::Identity);
    h.lookup("/b");
    let m = h.expect_mismatch(K::Identity, None);
    assert_eq!(m.op, OpKind::Lookup);
    assert!(m.secondary.contains("different inode") || m.primary.contains("same inode"), "{}", m.summary());
    // the link count differs as well
    h.expect_mismatch(K::Attr, Some("nlink"));
}

#[test]
fn hard_link_structure_mismatch_primary_copied_secondary_linked() {
    let h = Harness::builder().build_with(|p, s| {
        std::fs::write(p.join("a"), b"data").unwrap();
        std::fs::write(p.join("b"), b"data").unwrap();
        std::fs::write(s.join("a"), b"data").unwrap();
        std::fs::hard_link(s.join("a"), s.join("b")).unwrap();
    });
    h.lookup("/a");
    h.lookup("/b");
    let m = h.expect_mismatch(K::Identity, None);
    assert!(m.secondary.contains("same inode as /a"), "{}", m.summary());
}

/// The engine's own `link` with a secondary that links to the wrong object (a skipped link plus a pre-existing
/// copy under the new name would look like this).
#[test]
fn link_to_a_different_object_on_the_secondary() {
    let h = Harness::builder().build_with(|p, s| {
        for r in [p, s] {
            std::fs::write(r.join("a"), b"data").unwrap();
        }
        std::fs::write(s.join("b"), b"data").unwrap();
    });
    // primary: b does not exist -> link creates it; secondary: b exists -> EEXIST
    let _ = h.try_link("/a", "/b");
    let m = h.expect_mismatch(K::Result, None);
    assert_eq!((m.primary.as_str(), m.secondary.as_str()), ("OK", "EEXIST"));
}

#[test]
fn healthy_hard_links_are_not_flagged() {
    for level in [B, T, P] {
        let h = prepared(level);
        h.link("/f", "/hl1");
        h.link("/hl1", "/d/hl2");
        h.lookup("/hl1");
        h.lookup("/d/hl2");
        h.unlink("/f");
        h.rename("/hl1", "/d/hl3");
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

// ------------------------------------------------------------------------------------------- ctime rules

/// POSIX does not say that ctime changes when the last link of a file is removed (tmpfs updates it, ZFS does not).
/// A secondary that behaves like that must not be flagged, neither at the unlink nor at a later getattr through an
/// open handle, nor after a rename that replaced the file.
#[test]
fn ctime_of_a_file_without_links_is_not_compared() {
    use std::sync::{Arc, Mutex};
    for level in [B, T, P] {
        let h = Harness::builder()
            .level(level)
            .config(|c| c.time_tolerance = std::time::Duration::from_millis(200))
            .build();
        h.write_file("/victim", b"data");
        h.write_file("/replacer", b"new");
        h.write_file("/replaced", b"old");
        // the secondary freezes the ctime of link-less files at the last value seen while they had links
        let last: Arc<Mutex<std::collections::HashMap<u64, xcheckfs::sys::Ts>>> = Default::default();
        let lie = StatLie::Custom(Arc::new(move |st| {
            let mut g = last.lock().unwrap();
            if st.nlink > 0 {
                g.insert(st.ino, st.ctime);
            } else if let Some(t) = g.get(&st.ino) {
                st.ctime = *t;
            }
        }));
        h.inject(Fault::new(FaultOp::Stat, Effect::Stat(lie.clone())));
        h.inject(Fault::new(FaultOp::StatAt, Effect::Stat(lie)));
        let victim = h.open("/victim", libc::O_RDWR);
        let replaced = h.open("/replaced", libc::O_RDWR);
        h.engine.getattr(&h.ctx, victim.ino).unwrap();
        h.engine.getattr(&h.ctx, replaced.ino).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        h.unlink("/victim");
        h.rename("/replacer", "/replaced");
        std::thread::sleep(std::time::Duration::from_millis(400));
        h.engine.getattr(&h.ctx, victim.ino).unwrap();
        h.engine.getattr(&h.ctx, replaced.ino).unwrap();
        h.pwrite(victim, 0, b"still writable");
        h.close(victim);
        h.close(replaced);
        h.assert_no_mismatches();
    }
}

/// Running out of descriptors is a limit of the xcheckfs process: when it strikes only one half of an operation
/// (e.g. the secondary's open of a create), that half is retried after freeing the descriptor reserve, so both
/// file systems see the operation and nothing is reported.
#[test]
fn descriptor_exhaustion_of_one_half_is_retried() {
    for errno in [libc::EMFILE, libc::ENFILE] {
        let h = prepared(B);
        h.inject(Fault::new(FaultOp::Create, Effect::Errno(errno)).once());
        h.write_file("/new", b"data");
        h.pfault.add(Fault::new(FaultOp::Lookup, Effect::Errno(errno)).once());
        h.try_lookup("/new").unwrap();
        h.read_file("/new");
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}
