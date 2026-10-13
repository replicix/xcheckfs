//! Secondary-fault episodes: the secondary failing as a whole is one finding (`xcheckfs::episode`). A burst of
//! secondary faults opens an episode, which counts the faults and their knock-on mismatches (the first ones kept
//! verbatim) instead of reporting them one by one, defers repairs until it ends, and is itself reported; isolated
//! faults, faults of one object and occasional faults stay ordinary mismatches.

mod common;

use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use common::*;
use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
use xcheckfs::config::{EpisodeConfig, FaultMode, MismatchMode};
use xcheckfs::episode::{Episode, FIRST_RECORDS};
use xcheckfs::policy::{Action, MismatchKind as K};
use xcheckfs::stats::OpKind;

/// Files in /d that the outage workload writes to, and files it creates there.
const FILES: usize = 20;
const NEW: usize = 10;
/// Faults that open an episode (the default).
const BURST: u64 = 8;

fn harness(mode: MismatchMode, f: impl FnOnce(&mut EpisodeConfig)) -> Arc<Harness> {
    Arc::new(
        Harness::builder()
            .mode(mode)
            .episodes(|e| {
                *e = EpisodeConfig { quiet: Duration::from_millis(300), ..EpisodeConfig::default() };
                f(e)
            })
            .build_with(|p, s| {
                for root in [p, s] {
                    std::fs::create_dir(root.join("d")).unwrap();
                    for i in 0..FILES {
                        std::fs::write(root.join(format!("d/f{i}")), pattern(i as u64, 4096)).unwrap();
                    }
                    std::fs::write(root.join("other"), pattern(99, 4096)).unwrap();
                }
            }),
    )
}

/// Opens the files of /d, then, with the secondary failing every call, writes a byte to each of them and creates
/// `NEW` files in /d: one secondary fault per operation, on `FILES + NEW` objects. Every operation gets the
/// primary's result.
fn outage_workload(h: &Harness) -> Vec<Fh> {
    let d = h.lookup("/d").id;
    let files: Vec<Fh> = (0..FILES).map(|i| h.open(&format!("/d/f{i}"), libc::O_RDWR)).collect();
    h.outage(libc::EIO);
    for f in &files {
        assert_eq!(h.engine.write(&h.ctx, f.ino, f.fh, 0, b"X"), Ok(1), "the primary's result");
    }
    for i in 0..NEW {
        let r = h.engine.create(&h.ctx, d, OsStr::new(&format!("n{i}")), 0o644, libc::O_RDWR | libc::O_EXCL);
        assert!(r.is_ok(), "the primary's result: {r:?}");
    }
    files
}

fn wait_until(what: &str, timeout: Duration, cond: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(t0.elapsed() < timeout, "{what}: not within {timeout:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for the open episode to end (the watcher ends it after the quiet period) and returns it.
fn ended(h: &Harness) -> Episode {
    wait_until("the episode ends", Duration::from_secs(10), || !h.policy.episodes().is_open());
    let ep = h.policy.episodes().list(1).remove(0);
    assert!(!ep.is_open());
    ep
}

#[test]
fn a_burst_of_secondary_faults_is_one_episode() {
    // (a quiet period long enough for the knock-ons below to come before the end, also on a loaded machine)
    let h = harness(MismatchMode::Log, |e| e.quiet = Duration::from_secs(2));
    let files = outage_workload(&h);

    // The faults of the burst were reported as mismatches (the episode was not known yet), and opened one episode;
    // every later fault is counted in it.
    let s = &h.stats;
    assert_eq!(s.episodes.load(Relaxed), 1);
    assert_eq!(s.mismatches.load(Relaxed), BURST, "{}", h.describe_mismatches());
    for m in h.mismatches() {
        assert_eq!((m.op, m.kind, m.primary.as_str(), m.secondary.as_str()), (OpKind::Write, K::Result, "OK", "EIO"));
    }
    let counted = FILES as u64 - BURST + NEW as u64;
    assert_eq!(s.episode_mismatches.load(Relaxed), counted);
    let ep = h.policy.episodes().current().expect("an open episode");
    assert_eq!(ep.id, 1);
    assert_eq!(ep.burst.len() as u64, BURST);
    assert_eq!((ep.burst[0].op, ep.burst[0].errno), (OpKind::Write, "EIO"));
    assert_eq!((ep.mismatches, ep.faults, ep.knock_ons), (counted, counted, 0));
    assert_eq!(ep.by_op.get("write"), Some(&(FILES as u64 - BURST)));
    assert_eq!(ep.by_op.get("create"), Some(&(NEW as u64)));
    assert_eq!(ep.by_errno.get("EIO"), Some(&counted));
    // the first ones verbatim, then one of each kind (the creates are among the first already)
    assert_eq!(ep.records.len(), FIRST_RECORDS);
    assert!(ep.records.iter().all(|r| r.cause == "fault" && r.mismatch.secondary == "EIO"));
    assert_eq!(ep.records[0].mismatch.op, OpKind::Write);
    // every write and every new name, plus the directory of the names
    assert_eq!((ep.objects, ep.names), (FILES as u64 + 1, NEW as u64));
    assert!(ep.describe().starts_with("open"), "{}", ep.describe());

    // The secondary works again. Knock-ons of what diverged in the episode are counted in it: a written file reads
    // differently, a created name is missing on the secondary.
    h.clear_faults();
    let read0 = || {
        let mut got = Vec::new();
        h.engine.read(&h.ctx, files[0].ino, files[0].fh, 0, 10, &mut |d| got.extend_from_slice(d)).unwrap();
        assert_eq!(got[0], b'X', "the primary's data");
    };
    read0();
    assert!(h.try_lookup("/d/n0").is_ok());
    // ... but anything else is reported as always
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/other").once());
    h.read_file("/other");
    assert_eq!(s.mismatches.load(Relaxed), BURST + 1);
    let m = h.mismatches().pop().unwrap();
    assert_eq!((m.op, m.kind, m.path.as_str()), (OpKind::Read, K::Data, "/other"));
    let ep = h.policy.episodes().current().expect("still open");
    assert_eq!(ep.knock_ons, 2, "{}", ep.describe());
    assert!(ep.records.iter().any(|r| r.cause == "knock-on" && r.mismatch.kind == K::Data), "examples of every kind");

    // After the quiet period, with the secondary answering, the episode ends.
    let ep = ended(&h);
    assert_eq!(ep.end_reason, Some("quiet"));
    assert_eq!((ep.faults, ep.knock_ons), (counted, 2), "{}", ep.describe());
    assert_eq!(s.episodes.load(Relaxed), 1);
    // In log mode nothing is repaired. A difference counted in the episode, found again, is a repeat; one that
    // was not found during the episode is reported.
    let repeats = s.repeats.load(Relaxed);
    read0();
    assert!(h.try_lookup("/d/n0").is_ok());
    assert_eq!(s.repeats.load(Relaxed), repeats + 2);
    assert!(h.try_lookup("/d/n1").is_ok());
    assert_eq!(s.mismatches.load(Relaxed), BURST + 2, "{}", h.describe_mismatches());
    let m = h.mismatches().pop().unwrap();
    assert_eq!((m.op, m.path.as_str(), m.secondary.as_str()), (OpKind::Lookup, "/d/n1", "ENOENT"));
    assert_eq!(s.episode_mismatches.load(Relaxed), counted + 2);
    assert_eq!(s.resyncs.load(Relaxed) + s.resync_failures.load(Relaxed), 0);
}

#[test]
fn repairs_wait_for_the_episode_to_end() {
    let h = harness(MismatchMode::Resync, |_| {});
    outage_workload(&h);
    let s = &h.stats;
    // The burst's repairs went into the failing secondary and failed, before the burst was known to be an episode.
    // From the fault that opened it on, repairs wait: those failed ones, the opener's and every later write's
    // (objects), and the new names (merged into one repair of /d).
    assert_eq!(s.episodes.load(Relaxed), 1);
    assert_eq!(s.resync_failures.load(Relaxed), BURST - 1);
    assert_eq!(s.resyncs.load(Relaxed), 0);
    let objects = FILES as u64;
    assert_eq!(s.deferred_repairs.load(Relaxed), objects + NEW as u64);
    assert_eq!(h.policy.episodes().current().unwrap().repairs_deferred, objects + NEW as u64);
    let summary = xcheckfs::control::summary(s);
    assert!(summary.contains(&format!("{} deferred repairs not run yet", objects + NEW as u64)), "{summary}");
    // Still failing: the episode stays open, nothing is repaired.
    std::thread::sleep(Duration::from_millis(600));
    assert!(h.policy.episodes().is_open(), "the secondary does not answer yet");
    assert_eq!(s.resyncs.load(Relaxed), 0);

    // The secondary is back: after the quiet period the episode ends, and the deferred repairs run (and verify).
    h.clear_faults();
    ended(&h);
    let repairs = objects + 1; // (the names of /d are one repair)
    wait_until("the deferred repairs ran", Duration::from_secs(20), || s.deferred_repairs.load(Relaxed) == 0);
    assert_eq!(s.resyncs.load(Relaxed), repairs);
    assert_eq!(s.resync_failures.load(Relaxed), BURST - 1, "the deferred repairs verified");
    h.assert_trees_equal();
    // nothing but the burst was reported one by one
    assert_eq!(s.mismatches.load(Relaxed), BURST, "{}", h.describe_mismatches());

    // What was repaired is compared as usual again: a new difference there is reported (and repaired), also on
    // the directory whose names the episode had tainted.
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/d/f19").once());
    h.read_file("/d/f19");
    assert_eq!(s.mismatches.load(Relaxed), BURST + 1, "{}", h.describe_mismatches());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(h.s_path("d"), std::fs::Permissions::from_mode(0o700)).unwrap();
    h.getattr("/d");
    let m = h.mismatches().pop().unwrap();
    assert_eq!((m.kind, m.field.as_deref(), m.path.as_str()), (K::Attr, Some("mode"), "/d"), "{}", h.describe_mismatches());
    assert_eq!(s.mismatches.load(Relaxed), BURST + 2);
    h.assert_trees_equal();
    assert_eq!(s.episode_mismatches.load(Relaxed), FILES as u64 - BURST + NEW as u64);
}

/// A partial outage: the secondary is full. Stats work, so the repairs of the burst get as far as writing, fail, and
/// detach their objects from the secondary; deferred when the episode opens, they find them again by their paths.
#[test]
fn repairs_of_the_burst_are_deferred_too() {
    let h = harness(MismatchMode::Resync, |_| {});
    let s = &h.stats;
    let files: Vec<Fh> = (0..FILES).map(|i| h.open(&format!("/d/f{i}"), libc::O_RDWR)).collect();
    h.inject_always(FaultOp::Pwrite, Effect::Errno(libc::ENOSPC));
    for f in &files {
        assert_eq!(h.engine.write(&h.ctx, f.ino, f.fh, 0, b"X"), Ok(1));
    }
    h.clear_faults();
    assert_eq!(s.episodes.load(Relaxed), 1);
    assert_eq!(s.resync_failures.load(Relaxed), BURST - 1);
    assert_eq!(s.deferred_repairs.load(Relaxed), FILES as u64);
    ended(&h);
    wait_until("the deferred repairs ran", Duration::from_secs(20), || s.deferred_repairs.load(Relaxed) == 0);
    assert_eq!(s.resyncs.load(Relaxed), FILES as u64);
    h.assert_trees_equal();
    // the objects whose repairs had failed are compared again
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/d/f0").once());
    h.read_file("/d/f0");
    assert_eq!(s.mismatches.load(Relaxed), BURST + 1, "{}", h.describe_mismatches());
}

/// Faults of read-only operations are counted, but change nothing: what they touched is not a knock-on.
#[test]
fn read_only_faults_taint_nothing() {
    let h = harness(MismatchMode::Log, |_| {});
    let s = &h.stats;
    let ids: Vec<u64> = (0..12).map(|i| h.lookup(&format!("/d/f{i}")).id).collect();
    h.inject_always(FaultOp::Stat, Effect::Errno(libc::EIO));
    for id in &ids {
        assert!(h.engine.getattr(&h.ctx, *id).is_ok());
    }
    h.clear_faults();
    assert_eq!(s.episodes.load(Relaxed), 1);
    assert_eq!((s.mismatches.load(Relaxed), s.episode_mismatches.load(Relaxed)), (BURST, 12 - BURST));
    assert!(h.policy.episodes().is_open());
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 1 }).path("/d/f0").once());
    h.read_file("/d/f0");
    assert_eq!(s.mismatches.load(Relaxed), BURST + 1, "reported during the episode: {}", h.describe_mismatches());
    ended(&h);
    h.inject(Fault::new(FaultOp::Pread, Effect::CorruptRead { offset: 2 }).path("/d/f1").once());
    h.read_file("/d/f1");
    assert_eq!(s.mismatches.load(Relaxed), BURST + 2, "and after it: {}", h.describe_mismatches());
}

#[test]
fn isolated_and_occasional_faults_are_reported_one_by_one() {
    let h = harness(MismatchMode::Log, |_| {});
    let s = &h.stats;
    // a single EIO
    h.inject(Fault::new(FaultOp::Pwrite, Effect::Errno(libc::EIO)).once());
    let f = h.open("/d/f0", libc::O_RDWR);
    h.pwrite(f, 0, b"X");
    let m = h.expect_mismatch(K::Result, None);
    assert_eq!((m.op, m.secondary.as_str()), (OpKind::Write, "EIO"));
    // one object failing again and again: deduplicated, not an outage
    h.inject(Fault::new(FaultOp::Pwrite, Effect::Errno(libc::EIO)).path("/d/f1"));
    let g = h.open("/d/f1", libc::O_RDWR);
    for _ in 0..20 {
        h.pwrite(g, 0, b"Y");
    }
    assert_eq!((s.mismatches.load(Relaxed), s.repeats.load(Relaxed)), (2, 19));
    // a secondary that fails one create in eight, between writes that work: defects, each reported
    h.clear_faults();
    h.inject(Fault::new(FaultOp::Create, Effect::Errno(libc::EIO)).every(8));
    for i in 0..80 {
        let f = h.create(&format!("/c{i}"));
        for k in 0..4 {
            h.pwrite(f, k * 10, b"data");
        }
        h.close(f);
    }
    assert_eq!(s.mismatches.load(Relaxed), 2 + 10, "{}", h.describe_mismatches());
    assert_eq!(s.episodes.load(Relaxed), 0);
    assert_eq!(s.episode_mismatches.load(Relaxed), 0);
    assert!(h.policy.episodes().current().is_none());
}

#[test]
fn episodes_can_be_switched_off() {
    let h = harness(MismatchMode::Log, |e| e.errors = 0);
    outage_workload(&h);
    assert_eq!(h.stats.episodes.load(Relaxed), 0);
    assert_eq!(h.stats.mismatches.load(Relaxed), (FILES + NEW) as u64);
}

#[test]
fn status_and_episodes_over_the_control_socket() {
    let h = harness(MismatchMode::Log, |_| {});
    outage_workload(&h);
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("ctl.sock");
    let _srv = xcheckfs::control::serve(
        &sock,
        Arc::new(xcheckfs::control::Shared { stats: h.stats.clone(), policy: h.policy.clone(), info: serde_json::json!({}) }),
    )
    .unwrap();
    let st = xcheckfs::control::request(&sock, "status").unwrap();
    let counted = (FILES - BURST as usize + NEW) as u64;
    assert_eq!(st["episodes"], 1, "{st}");
    assert_eq!(st["episode_mismatches"], counted, "{st}");
    assert_eq!(st["mismatches"], BURST, "{st}");
    let ep = &st["episode"];
    assert_eq!(ep["id"], 1, "{st}");
    assert_eq!(ep["faults"], counted, "{st}");
    assert!(ep["ended"].is_null() && ep.get("records").is_none(), "{st}");
    assert!(ep["summary"].as_str().unwrap().starts_with("open"), "{st}");
    assert_eq!(ep["burst"].as_array().unwrap().len() as u64, BURST, "{st}");

    let v = xcheckfs::control::request(&sock, "episodes").unwrap();
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1, "{v}");
    let records = eps[0]["records"].as_array().unwrap();
    assert_eq!(records.len(), FIRST_RECORDS, "{v}");
    assert_eq!(records[0]["cause"], "fault", "{v}");
    assert_eq!(records[0]["mismatch"]["op"], "write", "{v}");
    assert_eq!(records[0]["mismatch"]["secondary"], "EIO", "{v}");

    let summary = xcheckfs::control::summary(&h.stats);
    assert!(summary.contains(&format!("1 secondary-fault episodes ({counted} mismatches counted in them)")), "{summary}");

    h.clear_faults();
    ended(&h);
    let st = xcheckfs::control::request(&sock, "status").unwrap();
    assert!(st["episode"].is_null(), "{st}");
    assert_eq!(st["episodes"], 1, "{st}");
}

#[test]
fn freeze_holds_the_episode_once() {
    let h = harness(MismatchMode::Log, |e| e.mode = FaultMode::Freeze);
    let resolver = h.resolve_when_pending(Action::Continue);
    let h2 = h.clone();
    let t = std::thread::spawn(move || outage_workload(&h2));
    resolver.join().unwrap();
    t.join().unwrap();
    // the opener was held (and continued); every fault after it was counted, none froze
    assert_eq!(h.stats.episodes.load(Relaxed), 1);
    assert_eq!(h.stats.mismatches.load(Relaxed), BURST);
    assert_eq!(h.stats.episode_mismatches.load(Relaxed), (FILES - BURST as usize + NEW) as u64);
    assert!(h.policy.pending().is_empty() && !h.policy.is_frozen());
}

#[test]
fn detach_stops_using_the_secondary() {
    let h = harness(MismatchMode::Resync, |e| e.mode = FaultMode::Detach);
    outage_workload(&h);
    assert!(h.stats.detached.load(Relaxed));
    assert_eq!(h.stats.episodes.load(Relaxed), 1);
    assert_eq!(h.stats.mismatches.load(Relaxed), BURST);
    // nothing after the detach reached the secondary, was compared or deferred
    assert_eq!(h.stats.episode_mismatches.load(Relaxed), 0);
    assert_eq!(h.stats.deferred_repairs.load(Relaxed), 0);
    let ep = h.policy.episodes().list(1).remove(0);
    assert_eq!(ep.end_reason, Some("detached"));
}

/// Fail mode returns EIO for every mismatch: also for the one that opened an episode and was continued in freeze.
#[test]
fn fail_mode_fails_the_frozen_opener_too() {
    let h = harness(MismatchMode::Fail, |e| e.mode = FaultMode::Freeze);
    let files: Vec<Fh> = (0..FILES).map(|i| h.open(&format!("/d/f{i}"), libc::O_RDWR)).collect();
    h.outage(libc::EIO);
    let resolver = h.resolve_when_pending(Action::Continue);
    let h2 = h.clone();
    let results = std::thread::spawn(move || files.iter().map(|f| h2.engine.write(&h2.ctx, f.ino, f.fh, 0, b"X")).collect::<Vec<_>>())
        .join()
        .unwrap();
    resolver.join().unwrap();
    assert_eq!(h.stats.episodes.load(Relaxed), 1);
    assert!(results.iter().all(|r| *r == Err(libc::EIO)), "{results:?}");
}

/// Switched to detach at runtime, a mismatch counted in an episode detaches like any other.
#[test]
fn detach_mode_set_during_an_episode_detaches() {
    let h = harness(MismatchMode::Log, |_| {});
    let files: Vec<Fh> = (0..FILES).map(|i| h.open(&format!("/d/f{i}"), libc::O_RDWR)).collect();
    h.outage(libc::EIO);
    for f in &files[..BURST as usize] {
        h.engine.write(&h.ctx, f.ino, f.fh, 0, b"X").unwrap();
    }
    assert!(h.policy.episodes().is_open());
    h.policy.set_mode(MismatchMode::Detach);
    h.engine.write(&h.ctx, files[BURST as usize].ino, files[BURST as usize].fh, 0, b"X").unwrap();
    assert!(h.stats.detached.load(Relaxed));
    assert_eq!(h.policy.episodes().list(1)[0].end_reason, Some("detached"));
}
