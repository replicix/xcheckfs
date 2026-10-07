//! Policy tests: what happens after a mismatch (log / fail / freeze / detach), allow rules, de-duplication,
//! and the freeze actions (continue / retry / resync / fail / detach / allow).

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::policy::{Action, MismatchKind as K, Rule};
use xcheckfs::stats::OpKind;

const B: CheckLevel = CheckLevel::Basic;
const T: CheckLevel = CheckLevel::Thorough;

fn data(n: usize) -> Vec<u8> {
    pattern(5, n)
}

fn harness(level: CheckLevel, mode: MismatchMode) -> Arc<Harness> {
    let h = Arc::new(Harness::new(level, mode));
    h.write_file("/f", &data(10_000));
    h.write_file("/g", &data(10_000));
    h.mark();
    h
}

/// Makes every read of `path` on the secondary return a corrupted byte.
fn corrupt_reads(h: &Harness, path: &str) {
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 3 }).path(path));
}

fn read_all(h: &Harness, path: &str) -> Result<Vec<u8>, i32> {
    let f = h.try_open(path, libc::O_RDONLY)?;
    let r = h.try_pread(f, 0, 100_000);
    h.close(f);
    r
}

// ------------------------------------------------------------------------------------------------ log / fail

#[test]
fn log_mode_reports_and_returns_the_primary_result() {
    let h = harness(B, MismatchMode::Log);
    corrupt_reads(&h, "/f");
    assert_eq!(read_all(&h, "/f").unwrap(), data(10_000));
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    assert!(!h.policy.is_frozen() && !h.stats.detached.load(Relaxed));
}

#[test]
fn fail_mode_returns_eio_and_keeps_failing_the_same_object() {
    let h = harness(B, MismatchMode::Fail);
    corrupt_reads(&h, "/f");
    assert_eq!(read_all(&h, "/f").unwrap_err(), libc::EIO);
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    // the very same mismatch again: not re-reported, but still an error
    assert_eq!(read_all(&h, "/f").unwrap_err(), libc::EIO);
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    assert_eq!(h.stats.repeats.load(Relaxed), 1);
    // unrelated files and operations are fine
    assert_eq!(read_all(&h, "/g").unwrap(), data(10_000));
    h.getattr("/f");
}

#[test]
fn fail_mode_on_a_mutation_means_the_primary_applied_it_anyway() {
    // write whose read-back (thorough) fails on the secondary
    let h = harness(T, MismatchMode::Fail);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f"));
    let f = h.open("/f", libc::O_RDWR);
    assert_eq!(h.try_pwrite(f, 0, b"HELLO").unwrap_err(), libc::EIO);
    assert_eq!(&std::fs::read(h.p_path("/f")).unwrap()[..5], b"HELLO", "primary has applied the write");
    // mkdir where the secondary says EIO (separate harness: the trees diverge, so everything else fails too)
    let h2 = harness(T, MismatchMode::Fail);
    h2.fault.inject(FaultOp::Mkdir, Effect::Errno(libc::EIO));
    assert_eq!(h2.try_mkdir("/dir", 0o755).unwrap_err(), libc::EIO);
    assert!(h2.p_path("/dir").is_dir());
    // unlink silently skipped on the secondary, caught by thorough read-back
    let h3 = harness(T, MismatchMode::Fail);
    h3.fault.inject(FaultOp::Unlink, Effect::Skip);
    assert_eq!(h3.try_unlink("/g").unwrap_err(), libc::EIO);
    assert!(!h3.p_path("/g").exists());
}

// ------------------------------------------------------------------------------------------------- detach

#[test]
fn detach_mode_stops_using_the_secondary() {
    let h = harness(T, MismatchMode::Detach);
    let open_before = h.open("/g", libc::O_RDWR);
    corrupt_reads(&h, "/f");
    // the mismatching op itself still returns the primary's result
    assert_eq!(read_all(&h, "/f").unwrap(), data(10_000));
    assert!(h.stats.detached.load(Relaxed));
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);

    // from now on the secondary backend is never called again, whatever we do
    h.fault.reset_calls();
    h.write_file("/new", b"primary only");
    assert_eq!(h.read_file("/new"), b"primary only");
    h.mkdir("/dir");
    h.rename("/new", "/dir/new");
    h.symlink("x", "/dir/l");
    h.link("/dir/new", "/dir/hl");
    h.setxattr("/f", "user.k", b"v").ok();
    h.truncate("/f", 10).unwrap();
    h.unlink("/dir/hl");
    h.readdir("/dir");
    h.getattr("/f");
    h.pwrite(open_before, 0, b"after detach"); // handles opened before the detach keep working
    h.close(open_before);
    let f = h.open("/f", libc::O_RDWR);
    h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 1000, 0).unwrap();
    h.close(f);
    assert_eq!(h.fault.total_calls(), 0, "secondary was called after detach");
    assert!(h.p_path("/dir/new").exists() && !h.s_path("/dir/new").exists());
    // faults on the secondary are no longer reported (and nothing is compared)
    h.fault.inject(FaultOp::Mkdir, Effect::Errno(libc::EIO));
    h.mkdir("/dir2");
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
}

#[test]
fn manual_detach() {
    let h = harness(B, MismatchMode::Log);
    h.policy.detach();
    h.fault.reset_calls();
    h.write_file("/x", b"x");
    assert_eq!(h.fault.total_calls(), 0);
    assert!(h.stats.detached.load(Relaxed));
}

// ------------------------------------------------------------------------------------------------- freeze

/// Starts `f` on another thread, waits for it to freeze, returns (join handle, pending mismatch id).
fn freeze_with<T: Send + 'static>(
    h: &Arc<Harness>,
    f: impl FnOnce(&Harness) -> T + Send + 'static,
) -> (std::thread::JoinHandle<T>, u64) {
    let hh = h.clone();
    let t = std::thread::spawn(move || f(&hh));
    let id = h.wait_pending(Duration::from_secs(10));
    (t, id)
}

#[test]
fn freeze_blocks_every_operation_until_resolved_continue() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    let (reader, id) = freeze_with(&h, |h| read_all(h, "/f"));
    assert!(h.policy.is_frozen());
    assert_eq!(h.policy.pending().len(), 1);
    assert_eq!(h.policy.pending()[0].mismatch.kind, K::Data);

    // an unrelated operation started now blocks at the gate
    let (tx, rx) = std::sync::mpsc::channel();
    let hh = h.clone();
    let other = std::thread::spawn(move || {
        let r = hh.engine.getattr(&hh.ctx, 1).map(|_| ());
        tx.send(r).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "operation passed the freeze gate");
    assert!(!reader.is_finished());

    assert!(h.policy.resolve(id, Action::Continue));
    assert_eq!(reader.join().unwrap().unwrap(), data(10_000));
    assert!(!h.policy.resolve(id, Action::Continue), "already resolved");
    rx.recv_timeout(Duration::from_secs(5)).expect("gate released").unwrap();
    other.join().unwrap();
    assert!(!h.policy.is_frozen());
    assert!(h.policy.pending().is_empty());
}

#[test]
fn freeze_fail_action_returns_eio() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    let (t, id) = freeze_with(&h, |h| read_all(h, "/f"));
    h.policy.resolve(id, Action::Fail);
    assert_eq!(t.join().unwrap().unwrap_err(), libc::EIO);
    assert!(!h.policy.is_frozen());
}

#[test]
fn freeze_detach_action_detaches() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    let (t, id) = freeze_with(&h, |h| read_all(h, "/f"));
    h.policy.resolve(id, Action::Detach);
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    assert!(h.stats.detached.load(Relaxed));
    h.fault.reset_calls();
    h.write_file("/after", b"x");
    assert_eq!(h.fault.total_calls(), 0);
    assert!(!h.policy.is_frozen());
}

#[test]
fn freeze_retry_action_reexecutes_a_read_only_operation() {
    let h = harness(B, MismatchMode::Freeze);
    let fid = h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 3 }).path("/f"));
    let (t, id) = freeze_with(&h, |h| read_all(h, "/f"));
    assert!(h.policy.pending()[0].mismatch.retryable);
    let reads_before = h.fault.calls(FaultOp::Pread);
    // the operator fixed the cause; the retry now agrees
    h.fault.remove(fid);
    h.policy.resolve(id, Action::Retry);
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    assert!(h.fault.calls(FaultOp::Pread) > reads_before, "the read was executed again");
    assert_eq!(h.stats.mismatches.load(Relaxed), 1, "the retry produced no new mismatch");
    assert!(!h.policy.is_frozen());
}

#[test]
fn freeze_retry_that_still_mismatches_freezes_again() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    let (t, id1) = freeze_with(&h, |h| read_all(h, "/f"));
    h.policy.resolve(id1, Action::Retry);
    // the retry mismatches again and must freeze again (not be swallowed as a repeat)
    let t0 = std::time::Instant::now();
    let id2 = loop {
        if let Some(p) = h.policy.pending().first() {
            if p.mismatch.id != id1 {
                break p.mismatch.id;
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "did not freeze again");
        std::thread::sleep(Duration::from_millis(2));
    };
    h.policy.resolve(id2, Action::Continue);
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    assert_eq!(h.stats.mismatches.load(Relaxed), 2);
}

#[test]
fn freeze_retry_of_a_mutation_continues() {
    // writes cannot be re-executed: Retry degrades to Continue
    let h = harness(T, MismatchMode::Freeze);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f"));
    let f = h.open("/f", libc::O_RDWR);
    let (t, id) = freeze_with(&h, move |h| h.try_pwrite(f, 0, b"abc"));
    assert!(!h.policy.pending()[0].mismatch.retryable);
    h.policy.resolve(id, Action::Retry);
    assert_eq!(t.join().unwrap().unwrap(), 3);
}

#[test]
fn freeze_resync_repairs_a_corrupted_secondary_file() {
    let h = harness(B, MismatchMode::Freeze);
    // secondary content diverges behind the engine's back
    let mut bad = data(10_000);
    bad[1234] ^= 0xff;
    bad[9999] ^= 0xff;
    std::fs::write(h.s_path("/f"), &bad).unwrap();
    assert!(!h.tree_diff().is_empty());
    let (t, id) = freeze_with(&h, |h| read_all(h, "/f"));
    assert!(h.policy.pending()[0].mismatch.resyncable);
    h.policy.resolve(id, Action::Resync);
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    h.assert_trees_equal();
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    // and the next read is clean
    assert_eq!(h.read_file("/f"), data(10_000));
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
}

#[test]
fn freeze_resync_repairs_length_mode_and_xattrs() {
    let h = harness(B, MismatchMode::Freeze);
    use std::os::unix::fs::PermissionsExt;
    // shorter + different mode
    std::fs::write(h.s_path("/f"), data(5000)).unwrap();
    std::fs::set_permissions(h.s_path("/f"), std::fs::Permissions::from_mode(0o600)).unwrap();
    let xattrs = xattrs_supported(h.s_root());
    if xattrs {
        raw_setxattr(&h.s_path("/f"), "user.stale", b"1").unwrap();
        raw_setxattr(&h.p_path("/f"), "user.keep", b"2").unwrap();
    }
    let (t, id) = freeze_with(&h, |h| {
        let a = h.lookup("/f"); // attr mismatch (mode/size) at lookup, a read-only op
        a.st.size
    });
    h.policy.resolve(id, Action::Resync);
    // resync may need several rounds when more than one attribute differs: keep resolving
    let t0 = std::time::Instant::now();
    while !t.is_finished() {
        if let Some(p) = h.policy.pending().first() {
            h.policy.resolve(p.mismatch.id, Action::Resync);
        }
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(t.join().unwrap(), 10_000);
    h.assert_trees_equal();
}

#[test]
fn freeze_resync_in_place_after_a_dropped_write() {
    // a mutation (write) holds the node exclusively, so the resync runs in place
    let h = harness(T, MismatchMode::Freeze);
    h.inject(Fault::new(FaultOp::Pwrite, Effect::DropWrite).path("/f").once());
    let f = h.open("/f", libc::O_RDWR);
    let (t, id) = freeze_with(&h, move |h| h.try_pwrite(f, 100, &[0xCD; 300]));
    assert!(h.policy.pending()[0].mismatch.resyncable);
    h.policy.resolve(id, Action::Resync);
    assert_eq!(t.join().unwrap().unwrap(), 300);
    h.close(f);
    h.assert_trees_equal();
}

/// A failed mkdir on the secondary is a namespace divergence: resync copies the directory over.
#[test]
fn resync_repairs_result_mismatches_of_namespace_operations() {
    let h = harness(B, MismatchMode::Freeze);
    h.fault.add(Fault::new(FaultOp::Mkdir, Effect::Errno(libc::EIO)).once());
    let (t, id) = freeze_with(&h, |h| h.try_mkdir("/d", 0o755).map(|_| ()));
    assert!(h.policy.pending()[0].mismatch.resyncable);
    h.policy.resolve(id, Action::Resync);
    assert!(t.join().unwrap().is_ok());
    assert_eq!(h.stats.resyncs.load(Relaxed), 1);
    h.assert_trees_equal();
}

#[test]
fn freeze_allow_action_adds_a_rule_and_suppresses_repeats() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    corrupt_reads(&h, "/g");
    let (t, id) = freeze_with(&h, |h| read_all(h, "/f"));
    let m = h.policy.pending()[0].mismatch.clone();
    h.policy.resolve(id, Action::Allow(Rule::from_mismatch(&m, false)));
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    assert_eq!(h.policy.rules().len(), 1);
    // same kind of mismatch on another file: allowed, so no new freeze
    assert_eq!(read_all(&h, "/g").unwrap(), data(10_000));
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    assert_eq!(h.stats.allowed.load(Relaxed), 1);
    assert!(!h.policy.is_frozen());
}

#[test]
fn several_frozen_operations_at_once() {
    let h = Arc::new(Harness::new(B, MismatchMode::Freeze));
    for i in 0..4 {
        h.write_file(&format!("/f{i}"), &data(5000));
        corrupt_reads(&h, &format!("/f{i}"));
    }
    let ts: Vec<_> = (0..4)
        .map(|i| {
            let h = h.clone();
            std::thread::spawn(move || read_all(&h, &format!("/f{i}")))
        })
        .collect();
    let t0 = std::time::Instant::now();
    while ts.iter().any(|t| !t.is_finished()) {
        for p in h.policy.pending() {
            h.policy.resolve(p.mismatch.id, Action::Continue);
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck");
        std::thread::sleep(Duration::from_millis(5));
    }
    for t in ts {
        assert_eq!(t.join().unwrap().unwrap(), data(5000));
    }
    assert_eq!(h.stats.mismatches.load(Relaxed), 4);
    assert!(!h.policy.is_frozen());
}

#[test]
fn leaving_freeze_mode_releases_everything() {
    let h = harness(B, MismatchMode::Freeze);
    corrupt_reads(&h, "/f");
    let (t, _) = freeze_with(&h, |h| read_all(h, "/f"));
    h.policy.set_mode(MismatchMode::Log);
    assert_eq!(t.join().unwrap().unwrap(), data(10_000));
    assert!(!h.policy.is_frozen());
}

// ------------------------------------------------------------------------------------------------- rules

#[test]
fn allow_rules_suppress_matching_mismatches_only() {
    let h = Harness::builder()
        .mode(MismatchMode::Fail)
        .rule(Rule { kind: Some(K::Data), op: Some("read".into()), path: Some("/skip/**".into()), ..Default::default() })
        .rule(Rule {
            kind: Some(K::Result),
            op: Some("mkdir".into()),
            primary: Some("OK".into()),
            secondary: Some("EIO".into()),
            ..Default::default()
        })
        .rule(Rule { kind: Some(K::Attr), field: Some("mode".into()), path: Some("/**/modeonly".into()), ..Default::default() })
        .build();
    h.mkdir("/skip");
    h.mkdir("/keep");
    for p in ["/skip/f", "/keep/f"] {
        h.write_file(p, &data(1000));
    }
    h.write_file("/modeonly", &data(10));
    h.write_file("/sizeonly", &data(10));
    h.mark();

    // data: allowed under /skip (Fail mode would have returned EIO), flagged under /keep
    for p in ["/skip/f", "/keep/f"] {
        corrupt_reads(&h, p);
    }
    assert_eq!(read_all(&h, "/skip/f").unwrap(), data(1000));
    assert_eq!(h.stats.mismatches.load(Relaxed), 0);
    assert_eq!(h.stats.allowed.load(Relaxed), 1);
    assert_eq!(read_all(&h, "/keep/f").unwrap_err(), libc::EIO);
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);

    // result: mkdir OK/EIO allowed, other errno pair not
    h.fault.inject(FaultOp::Mkdir, Effect::Errno(libc::EIO));
    h.mkdir("/d1");
    assert_eq!(h.stats.allowed.load(Relaxed), 2);
    h.clear_faults();
    h.fault.inject(FaultOp::Mkdir, Effect::Errno(libc::ENOSPC));
    assert_eq!(h.try_mkdir("/d2", 0o755).unwrap_err(), libc::EIO);

    // field-specific rule
    h.clear_faults();
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Mode(0o600))).path("/modeonly"));
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Mode(0o600))).path("/sizeonly"));
    h.getattr("/modeonly");
    let allowed = h.stats.allowed.load(Relaxed);
    assert_eq!(allowed, 3);
    assert_eq!(h.engine.getattr(&h.ctx, h.lookup("/sizeonly").id).unwrap_err(), libc::EIO, "different path: not allowed");
}

#[test]
fn rules_added_at_runtime_apply_to_later_mismatches() {
    let h = harness(B, MismatchMode::Log);
    // (applied on both sides but reported as failed: the trees stay equal, no follow-up mismatches)
    h.fault.inject(FaultOp::Mkdir, Effect::AppliedThenErrno(libc::EIO));
    h.mkdir("/first");
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    h.policy
        .add_rule(Rule { kind: Some(K::Result), op: Some("mkdir".into()), ..Default::default() }, false)
        .unwrap();
    h.mkdir("/other");
    h.mkdir("/third");
    assert_eq!(h.stats.mismatches.load(Relaxed), 1);
    assert_eq!(h.stats.allowed.load(Relaxed), 2);
    // an invalid rule is refused
    assert!(h.policy.add_rule(Rule { op: Some("no_such_op".into()), ..Default::default() }, false).is_err());
}

#[test]
fn allowed_mismatches_never_freeze_or_detach() {
    for mode in [MismatchMode::Freeze, MismatchMode::Detach] {
        let h = Harness::builder().mode(mode).rule(Rule::default()).build(); // matches everything
        h.write_file("/f", &data(1000));
        corrupt_reads(&h, "/f");
        assert_eq!(read_all(&h, "/f").unwrap(), data(1000));
        assert!(!h.policy.is_frozen() && !h.stats.detached.load(Relaxed));
        assert_eq!(h.stats.mismatches.load(Relaxed), 0);
    }
}

// --------------------------------------------------------------------------------------------- de-dup

#[test]
fn repeated_mismatches_are_counted_not_reported() {
    let h = harness(B, MismatchMode::Log);
    corrupt_reads(&h, "/f");
    corrupt_reads(&h, "/g");
    for _ in 0..5 {
        read_all(&h, "/f").unwrap();
    }
    assert_eq!(h.mismatches().len(), 1);
    assert_eq!(h.stats.repeats.load(Relaxed), 4);
    // another object is a new mismatch
    read_all(&h, "/g").unwrap();
    assert_eq!(h.mismatches().len(), 2);
    // another kind on the same object is a new mismatch
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Size(1))).path("/f"));
    h.getattr("/f");
    assert_eq!(h.mismatches().len(), 3);
    // another field of the same kind too
    h.inject(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::Mode(0o600))).path("/f"));
    h.getattr("/f");
    assert_eq!(h.mismatches().len(), 4);
}

/// Name-based mismatches are reported against the directory, so different names must not hide each other.
#[test]
fn different_names_in_one_directory_are_separate_mismatches() {
    let h = Harness::builder().build_with(|p, _| {
        for n in ["a", "b", "c"] {
            std::fs::write(p.join(n), n).unwrap();
        }
    });
    for n in ["a", "b", "c"] {
        h.lookup(&format!("/{n}"));
    }
    let paths: Vec<_> = h.mismatches().iter().filter(|m| m.kind == K::Result).map(|m| m.path.clone()).collect();
    assert_eq!(paths, vec!["/a", "/b", "/c"]);
    // looking at /a again is a repeat
    h.lookup("/a");
    assert_eq!(h.mismatches().len(), 3);
}

#[test]
fn history_ids_are_sequential_and_serializable() {
    let h = harness(B, MismatchMode::Log);
    corrupt_reads(&h, "/f");
    corrupt_reads(&h, "/g");
    read_all(&h, "/f").unwrap();
    read_all(&h, "/g").unwrap();
    let ms = h.mismatches();
    assert_eq!(ms.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2]);
    let j = serde_json::to_value(&*ms[0]).unwrap();
    assert_eq!(j["kind"], "data");
    assert_eq!(j["op"], serde_json::to_value(OpKind::Read).unwrap());
    assert!(j["path"].as_str().unwrap().ends_with("/f"));
}

// ------------------------------------------------------------------------------------- forget races

/// A lookup that is held up between finding the node and counting its lookup reference (here: frozen on an attribute
/// mismatch) must not lose that reference when the kernel's FORGET for the previous reference arrives meanwhile.
#[test]
fn forget_racing_with_a_lookup_of_the_same_inode_keeps_the_node() {
    let h = Arc::new(Harness::new(B, MismatchMode::Freeze));
    // exactly one lookup reference ("the kernel's"), no open handles
    let (first, fh) = h.engine.create(&h.ctx, 1, std::ffi::OsStr::new("fresh"), 0o644, libc::O_RDWR).unwrap();
    h.engine.release(&h.ctx, first.id, fh).unwrap();
    h.inject(Fault::new(FaultOp::Lookup, Effect::Stat(StatLie::Mode(0o600))).name("fresh").once());
    let hh = h.clone();
    let t = std::thread::spawn(move || hh.engine.lookup(&hh.ctx, 1, std::ffi::OsStr::new("fresh")));
    let id = h.wait_pending(Duration::from_secs(10)); // the lookup is inside the engine, mid-way
    h.engine.forget(first.id, 1); // the old reference goes away
    h.policy.resolve(id, Action::Continue);
    let second = t.join().unwrap().unwrap();
    assert_eq!(second.id, first.id);
    // the new reference is valid: the node must still be known
    h.engine.getattr(&h.ctx, second.id).expect("node vanished although a lookup reference is held");
    h.engine.forget(second.id, 1);
    assert!(h.engine.getattr(&h.ctx, second.id).is_err(), "node is gone once all references are forgotten");
}

// ---------------------------------------------------------------------------------------------- events

#[test]
fn event_stream_carries_operations_and_mismatches() {
    use xcheckfs::events::UiEvent;
    let h = Harness::builder().events(10_000).build();
    h.write_file("/f", &data(5000));
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 2 }).path("/f"));
    assert_eq!(h.try_unlink("/nope").unwrap_err(), libc::ENOENT);
    h.read_file("/f");
    let evs: Vec<UiEvent> = h.events.as_ref().unwrap().try_iter().collect();
    let ops: Vec<_> = evs.iter().filter_map(|e| if let UiEvent::Op(o) = e { Some(o) } else { None }).collect();
    let mms: Vec<_> = evs.iter().filter_map(|e| if let UiEvent::Mismatch(m) = e { Some(m.clone()) } else { None }).collect();
    assert_eq!(mms.len(), 1);
    assert_eq!(mms[0].kind, K::Data);
    let read = ops.iter().find(|o| o.op == OpKind::Read && o.mismatch).expect("read event flagged as mismatch");
    assert_eq!((read.errno, read.sec_errno), (0, Some(0)));
    assert!(read.detail.contains("/f"), "{}", read.detail);
    let unlink = ops.iter().find(|o| o.op == OpKind::Unlink).unwrap();
    assert_eq!((unlink.errno, unlink.sec_errno, unlink.mismatch), (libc::ENOENT, Some(libc::ENOENT), false), "both sides fail alike: no mismatch");
    assert!(ops.iter().any(|o| o.op == OpKind::Create) && ops.iter().any(|o| o.op == OpKind::Write));
    assert!(ops.iter().all(|o| o.total_ns > 0 && o.primary_ns <= o.total_ns));
    assert!(h.stats.inflight.snapshot().is_empty());
    assert_eq!(h.stats.events_dropped.load(Relaxed), 0);
}

#[test]
fn a_slow_event_consumer_never_blocks_the_engine() {
    let h = Harness::builder().events(2).build();
    for i in 0..50 {
        h.write_file(&format!("/f{i}"), b"x");
    }
    assert!(h.stats.events_dropped.load(Relaxed) > 0, "events must be dropped, not queued without bound");
    h.assert_no_mismatches();
}
