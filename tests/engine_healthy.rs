//! Healthy-tree tests: two identical, well-behaved trees must never produce a mismatch, at any check level,
//! single-threaded or under heavy concurrency. These are the false-positive tests of the locking design.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::*;
use xcheckfs::config::{CheckLevel, MismatchMode};
use xcheckfs::engine::SetAttr;

const LEVELS: [CheckLevel; 3] = [CheckLevel::Basic, CheckLevel::Thorough, CheckLevel::Paranoid];

/// A broad deterministic workload touching every engine operation, including the error paths.
fn broad_workload(h: &Harness, xattrs: bool, exotic: bool) {
    // --- directories
    h.mkdir("/d");
    h.mkdir("/d/sub");
    h.mkdir_p("/d/a/b/c");
    assert_eq!(h.try_mkdir("/d", 0o755).unwrap_err(), libc::EEXIST);
    assert_eq!(h.try_rmdir("/d").unwrap_err(), libc::ENOTEMPTY);
    assert_eq!(h.try_lookup("/nope/x").unwrap_err(), libc::ENOENT);
    h.rmdir("/d/a/b/c");

    // --- regular files
    let data = pattern(1, 300_000);
    h.write_file("/d/f", &data);
    assert_eq!(h.read_file("/d/f"), data);
    h.append("/d/f", b"tail");
    assert_eq!(h.read_file("/d/f").len(), 300_004);
    let f = h.open("/d/f", libc::O_RDWR);
    h.pwrite(f, 100, b"hello");
    h.pwrite(f, 1_000_000, b"sparse"); // creates a hole
    assert_eq!(h.pread(f, 100, 5), b"hello");
    assert_eq!(h.pread(f, 999_990, 100).len(), 16);
    assert!(h.pread(f, 5_000_000, 10).is_empty(), "read past EOF");
    h.close(f);
    h.truncate("/d/f", 10).unwrap();
    h.truncate("/d/f", 5000).unwrap();
    assert_eq!(h.getattr("/d/f").st.size, 5000);
    assert_eq!(h.try_create("/d/f", 0o644, libc::O_EXCL).unwrap_err(), libc::EEXIST);
    // create over an existing name without O_EXCL (the race the kernel can produce)
    let f = h.try_create("/d/f", 0o644, libc::O_TRUNC).unwrap();
    h.pwrite(f, 0, b"short");
    h.close(f);
    assert_eq!(h.read_file("/d/f"), b"short");
    assert_eq!(h.try_open("/d/missing", libc::O_RDONLY).unwrap_err(), libc::ENOENT);

    // --- attributes
    h.chmod("/d/f", 0o600);
    h.chmod("/d/f", 0o4755);
    h.chmod("/d/f", 0o644);
    h.utimes("/d/f", 1_000_000_000, 1_100_000_000).unwrap();
    assert_eq!(h.getattr("/d/f").st.mtime.sec, 1_100_000_000);
    if is_root() {
        h.setattr("/d/f", SetAttr { uid: Some(1234), gid: Some(4321), ..Default::default() }).unwrap();
    } else {
        let c = h.ctx;
        h.setattr("/d/f", SetAttr { uid: Some(c.uid), gid: Some(c.gid), ..Default::default() }).unwrap();
    }
    h.setattr("/d/f", SetAttr { mtime: Some(xcheckfs::backend::TimeSpec::Now), ..Default::default() }).unwrap();

    // --- links
    h.link("/d/f", "/d/hard1");
    h.link("/d/f", "/d/sub/hard2");
    assert_eq!(h.getattr("/d/f").st.nlink, 3);
    h.write_file("/d/hard1", b"through a hard link");
    assert_eq!(h.read_file("/d/sub/hard2"), b"through a hard link");
    h.unlink("/d/hard1");
    assert_eq!(h.getattr("/d/f").st.nlink, 2);
    h.symlink("f", "/d/sl");
    h.symlink("/absolute/dangling", "/d/dangling");
    assert_eq!(h.readlink("/d/sl").unwrap(), b"f");
    assert_eq!(h.readlink("/d/dangling").unwrap(), b"/absolute/dangling");
    // (the errno for a non-symlink is whatever the kernel says; the VFS filters this case before FUSE anyway)
    assert!(h.readlink("/d/f").is_err());
    assert_eq!(h.try_link("/d/sub", "/d/dirlink").unwrap_err(), libc::EPERM);
    h.mknod_fifo("/d/fifo").unwrap();

    // --- rename in all its flavours
    h.write_file("/d/r1", b"one");
    h.write_file("/d/r2", b"two");
    h.rename("/d/r1", "/d/r1b");
    h.rename("/d/r1b", "/d/r2"); // replace
    assert_eq!(h.read_file("/d/r2"), b"one");
    h.write_file("/d/x1", b"x1");
    h.write_file("/d/x2", b"x2");
    h.try_rename_flags("/d/x1", "/d/x2", libc::RENAME_EXCHANGE).unwrap();
    assert_eq!(h.read_file("/d/x1"), b"x2");
    assert_eq!(h.try_rename_flags("/d/x1", "/d/x2", libc::RENAME_NOREPLACE).unwrap_err(), libc::EEXIST);
    h.rename("/d/a", "/d/sub/a"); // directory across parents
    assert_eq!(h.try_rename("/d/sub", "/d/sub/a/inside").unwrap_err(), libc::EINVAL);
    assert_eq!(h.try_rename("/d/missing_src", "/d/zz").unwrap_err(), libc::ENOENT);
    h.rename("/d/sub/hard2", "/d/f"); // rename between two links of one inode: a no-op
    assert!(h.exists("/d/sub/hard2") && h.exists("/d/f"));

    // --- readdir (including a large directory)
    for i in 0..300 {
        h.write_file(&format!("/d/sub/m{i:04}"), b"m");
    }
    let l = h.readdir("/d/sub");
    assert!(l.len() >= 300);
    assert_eq!(h.readdir("/"), vec!["d"]);

    // --- fallocate / copy_file_range / lseek
    let f = h.open("/d/f", libc::O_RDWR);
    h.pwrite(f, 0, &pattern(2, 100_000));
    if exotic {
        // tmpfs updates mtime on a plain fallocate, ZFS does not: only compare this on equal file systems
        h.engine.fallocate(&h.ctx, f.ino, f.fh, 0, 200_000, 0).unwrap();
    } else {
        h.truncate("/d/f", 200_000).unwrap();
    }
    h.engine.fallocate(&h.ctx, f.ino, f.fh, 4096, 8192, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE).unwrap();
    if exotic {
        // tmpfs: EOPNOTSUPP on both sides is fine (but differs between file systems)
        h.engine.fallocate(&h.ctx, f.ino, f.fh, 20000, 5000, libc::FALLOC_FL_ZERO_RANGE).ok();
    }
    assert_eq!(h.engine.lseek(&h.ctx, f.ino, f.fh, 0, libc::SEEK_END).unwrap(), 200_000);
    assert_eq!(h.engine.lseek(&h.ctx, f.ino, f.fh, 10, libc::SEEK_SET).unwrap(), 10);
    h.engine.fsync(&h.ctx, f.ino, f.fh, false).unwrap();
    h.engine.fsync(&h.ctx, f.ino, f.fh, true).unwrap();
    let g = h.create("/d/copy");
    let n = h.engine.copy_file_range(&h.ctx, f.fh, 1000, g.fh, 10, 50_000, 0).unwrap();
    assert_eq!(n, 50_000);
    h.close(g);
    h.close(f);

    // --- xattrs
    if xattrs {
        h.setxattr("/d/f", "user.a", b"alpha").unwrap();
        h.setxattr("/d/f", "user.b", &pattern(3, 3000)).unwrap();
        assert_eq!(h.getxattr("/d/f", "user.a").unwrap(), b"alpha");
        assert_eq!(h.listxattr("/d/f").unwrap(), vec!["user.a", "user.b"]);
        h.setxattr("/d/f", "user.a", b"changed").unwrap();
        assert_eq!(h.getxattr("/d/f", "user.nope").unwrap_err(), libc::ENODATA);
        h.removexattr("/d/f", "user.a").unwrap();
        assert_eq!(h.removexattr("/d/f", "user.a").unwrap_err(), libc::ENODATA);
        h.setxattr("/d", "user.dir", b"on a directory").unwrap();
    }

    // --- statfs / access / getattr of everything
    h.engine.statfs(&h.ctx, 1).unwrap();
    let f = h.lookup("/d/f");
    h.engine.access(&h.ctx, f.id, libc::R_OK).unwrap();
    h.engine.access(&h.ctx, f.id, libc::X_OK).unwrap_err();

    // --- removal
    h.unlink("/d/dangling");
    h.unlink("/d/sl");
    h.unlink("/d/fifo");
    assert_eq!(h.try_unlink("/d/sub").unwrap_err(), libc::EISDIR);
    assert_eq!(h.try_rmdir("/d/f").unwrap_err(), libc::ENOTDIR);
    for n in h.readdir("/d/sub") {
        let p = format!("/d/sub/{n}");
        if h.getattr(&p).st.mode & libc::S_IFMT == libc::S_IFDIR {
            h.rmdir_tree(&p);
        } else {
            h.unlink(&p);
        }
    }
    h.rmdir("/d/sub");
}

#[test]
fn broad_workload_all_levels_no_mismatch() {
    for level in LEVELS {
        let h = Harness::new(level, MismatchMode::Log);
        let xattrs = xattrs_supported(h.p_root()) && xattrs_supported(h.s_root());
        broad_workload(&h, xattrs, true);
        h.assert_no_mismatches();
        // The workload leaves /d/f, /d/copy, /d/x1, ... behind: both trees must be identical.
        h.assert_trees_equal();
        assert!(h.stats.verifications.load(Ordering::Relaxed) > 0 || level == CheckLevel::Basic);
    }
}

#[test]
fn check_levels_do_more_work() {
    let run = |level| {
        let h = Harness::new(level, MismatchMode::Log);
        h.write_file("/f", &pattern(7, 100_000));
        h.mkdir("/d");
        h.rename("/f", "/d/f");
        h.unlink("/d/f");
        h.stats.verifications.load(Ordering::Relaxed)
    };
    assert_eq!(run(CheckLevel::Basic), 0);
    let t = run(CheckLevel::Thorough);
    let p = run(CheckLevel::Paranoid);
    assert!(t > 0 && p > t, "thorough={t} paranoid={p}");
}

/// Randomised single-threaded workload: a mix of valid and invalid operations over a small namespace.
fn random_ops(h: &Harness, seed: u64, ops: usize, xattrs: bool, exotic: bool) {
    let mut rng = Rng::new(seed);
    let dirs = ["/", "/a", "/b", "/a/x", "/b/y"];
    for d in &dirs[1..] {
        if !h.exists(d) {
            let _ = h.try_mkdir(d, 0o755);
        }
    }
    let names = ["f0", "f1", "f2", "f3", "g0", "g1", "dd", "ee"];
    for _ in 0..ops {
        let path = |rng: &mut Rng| {
            let d = *rng.pick(&dirs);
            let n = *rng.pick(&names);
            if d == "/" { format!("/{n}") } else { format!("{d}/{n}") }
        };
        let p = path(&mut rng);
        match rng.below(18) {
            0 | 1 => {
                let len = *rng.pick(&[0usize, 1, 100, 4096, 70_000]);
                if h.try_lookup(&p).map(|a| a.st.mode & libc::S_IFMT == libc::S_IFREG).unwrap_or(true) {
                    let data = rng.bytes(len);
                    let f = match h.try_open(&p, libc::O_WRONLY | libc::O_TRUNC) {
                        Ok(f) => Some(f),
                        Err(libc::ENOENT) => h.try_create(&p, 0o644, 0).ok(),
                        Err(_) => None,
                    };
                    if let Some(f) = f {
                        let _ = h.try_pwrite(f, 0, &data);
                        h.close(f);
                    }
                }
            }
            2 => {
                if let Ok(f) = h.try_open(&p, libc::O_RDWR) {
                    let off = rng.below(20_000);
                    let _ = h.try_pwrite(f, off, &rng.blob(9000));
                    let _ = h.try_pread(f, rng.below(10_000), 5000);
                    h.close(f);
                }
            }
            3 => {
                if let Ok(f) = h.try_open(&p, libc::O_RDONLY) {
                    let _ = h.try_pread(f, 0, 1 << 20);
                    h.close(f);
                }
            }
            4 => {
                let _ = h.try_lookup(&p).and_then(|a| h.engine.getattr(&h.ctx, a.id));
            }
            5 => {
                let q = path(&mut rng);
                let flags = *rng.pick(&[0u32, 0, libc::RENAME_NOREPLACE, libc::RENAME_EXCHANGE]);
                let _ = h.try_rename_flags(&p, &q, flags);
            }
            6 => {
                let _ = h.try_unlink(&p);
            }
            7 => {
                let _ = h.try_mkdir(&p, 0o755);
            }
            8 => {
                let _ = h.try_rmdir(&p);
            }
            9 => {
                let q = path(&mut rng);
                let _ = h.try_link(&p, &q);
            }
            10 => {
                let _ = h.try_symlink(&path(&mut rng), &p);
            }
            11 => {
                let _ = h.readlink(&p);
            }
            12 => {
                let _ = h.truncate(&p, rng.below(30_000));
            }
            13 => {
                let _ = h.setattr(&p, SetAttr { mode: Some(*rng.pick(&[0o644, 0o600, 0o755, 0o444])), ..Default::default() });
            }
            14 => {
                let d = *rng.pick(&dirs);
                if h.exists(d) {
                    let _ = h.readdir(d);
                }
            }
            15 if xattrs => {
                let n = format!("user.k{}", rng.below(3));
                match rng.below(3) {
                    0 => {
                        let _ = h.setxattr(&p, &n, &rng.blob(200));
                    }
                    1 => {
                        let _ = h.getxattr(&p, &n);
                        let _ = h.listxattr(&p);
                    }
                    _ => {
                        let _ = h.removexattr(&p, &n);
                    }
                }
            }
            16 => {
                let q = path(&mut rng);
                match (h.try_open(&p, libc::O_RDONLY), h.try_open(&q, libc::O_RDWR)) {
                    (Ok(f), Ok(g)) => {
                        let _ = h.engine.copy_file_range(&h.ctx, f.fh, rng.below(5000), g.fh, rng.below(5000), 20_000, 0);
                        h.close(f);
                        h.close(g);
                    }
                    (f, g) => {
                        f.into_iter().chain(g).for_each(|x| h.close(x));
                        let _ = h.set_mtime(&p, 2_000_000 + rng.below(1000) as i64);
                    }
                }
            }
            // fallocate modes update mtime differently on tmpfs, ZFS, ...: only exercised on equal file systems
            _ if exotic => {
                if let Ok(f) = h.try_open(&p, libc::O_RDWR) {
                    let _ = h.engine.fallocate(&h.ctx, f.ino, f.fh, rng.below(10_000), rng.below(20_000), *rng.pick(&[0, libc::FALLOC_FL_KEEP_SIZE, libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, libc::FALLOC_FL_ZERO_RANGE]));
                    h.close(f);
                }
            }
            _ => {
                let _ = h.try_lookup(&p).and_then(|a| h.engine.getattr(&h.ctx, a.id));
            }
        }
    }
}

#[test]
fn random_single_thread_all_levels() {
    for (i, level) in LEVELS.into_iter().enumerate() {
        let h = Harness::new(level, MismatchMode::Log);
        let xattrs = xattrs_supported(h.p_root()) && xattrs_supported(h.s_root());
        random_ops(&h, 42 + i as u64, 4000, xattrs, true);
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

/// Many threads, one shared small namespace: every conflicting pair of operations races. With correct locking the
/// two file systems always see the same order, so the result must be ZERO mismatches.
fn stress(level: CheckLevel, threads: usize, ops: usize) {
    stress_with(level, threads, ops, false)
}

/// `slow`: random 1 ms delays on both backends' calls widen every race window between the two halves of an
/// operation; the locks must still keep both sides in the same order.
fn stress_with(level: CheckLevel, threads: usize, ops: usize, slow: bool) {
    let ops = ops * soak();
    // (slow runs record the engine's event stream: on a failure it is the timeline of what happened to the object)
    let h = Arc::new(Harness::builder().level(level).events(if slow { 500_000 } else { 1 }).build());
    if slow {
        use xcheckfs::backend::fault::{Effect, Fault, FaultOp::*};
        for op in [Pwrite, Pread, Rename, Unlink, Create, Mkdir, Link, Stat, StatAt, Lookup, Readdir, Truncate, Open, Rmdir, Symlink] {
            h.fault.add(Fault::new(op, Effect::Delay(std::time::Duration::from_micros(700))).every(2));
            h.pfault.add(Fault::new(op, Effect::Delay(std::time::Duration::from_micros(400))).every(5));
        }
    }
    let xattrs = xattrs_supported(h.p_root()) && xattrs_supported(h.s_root());
    for d in ["/a", "/b", "/a/x", "/b/y"] {
        h.mkdir(d);
    }
    let total = Arc::new(AtomicU64::new(0));
    let ts: Vec<_> = (0..threads)
        .map(|t| {
            let h = h.clone();
            let total = total.clone();
            std::thread::spawn(move || {
                random_ops(&h, 1000 + t as u64, ops, xattrs, true);
                total.fetch_add(ops as u64, Ordering::Relaxed);
            })
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    let ops_done: u64 = xcheckfs::stats::OpKind::ALL.iter().map(|&k| h.stats.op(k).count.load(Ordering::Relaxed)).sum();
    let errors: u64 = xcheckfs::stats::OpKind::ALL.iter().map(|&k| h.stats.op(k).errors.load(Ordering::Relaxed)).sum();
    eprintln!("stress {level:?} slow={slow}: {ops_done} engine ops, {errors} returned errors");
    if slow && !h.mismatches().is_empty() {
        use xcheckfs::events::UiEvent;
        let evs: Vec<_> = h.events.as_ref().unwrap().try_iter().collect();
        for m in h.mismatches() {
            eprintln!("TIMELINE for {}", m.summary());
            let mut mine: Vec<_> = evs
                .iter()
                .filter_map(|e| if let UiEvent::Op(o) = e { Some(o) } else { None })
                .filter(|o| o.detail.contains(&format!("[{}]", m.ino)) || o.ino == m.ino)
                .collect();
            mine.sort_by_key(|o| o.time.checked_sub(std::time::Duration::from_nanos(o.total_ns)));
            let last = mine.len().saturating_sub(40);
            for o in &mine[last..] {
                let start = o.time.checked_sub(std::time::Duration::from_nanos(o.total_ns)).unwrap();
                eprintln!("  {:?} +{}us {} errno={} sec={:?} mismatch={} {}", start.duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() % 100_000_000, o.total_ns / 1000, o.op.name(), o.errno, o.sec_errno, o.mismatch, o.detail);
            }
        }
    }
    for m in h.mismatches() {
        // diagnostics for a failing run: what do the two trees look like at the offending path now?
        let (p, s) = (std::fs::symlink_metadata(h.p_path(&m.path)), std::fs::symlink_metadata(h.s_path(&m.path)));
        eprintln!("DIAG {}\n  primary {:?}\n  secondary {:?}", m.summary(), p.map(|x| (x.file_type(), std::os::unix::fs::MetadataExt::ino(&x))), s.map(|x| (x.file_type(), std::os::unix::fs::MetadataExt::ino(&x))));
    }
    // The workload must actually exercise the engine (and also hit error paths), not fail early.
    assert!(ops_done > (threads * ops) as u64 && errors > 0 && errors < ops_done / 2, "{ops_done} ops, {errors} errors");
    h.assert_no_mismatches();
    h.assert_trees_equal();
}

#[test]
fn stress_mixed_slow_secondary() {
    stress_with(CheckLevel::Thorough, 12, 400, true);
    stress_with(CheckLevel::Paranoid, 8, 300, true);
}

#[test]
fn stress_mixed_basic() {
    stress(CheckLevel::Basic, 12, 1500);
}

#[test]
fn stress_mixed_thorough() {
    stress(CheckLevel::Thorough, 12, 1200);
}

#[test]
fn stress_mixed_paranoid() {
    stress(CheckLevel::Paranoid, 8, 800);
}

/// Focused races on a single hot directory and a few hot files: writers, readers, getattr, rename, unlink, create.
#[test]
fn stress_hot_directory() {
    for level in LEVELS {
        let h = Arc::new(Harness::new(level, MismatchMode::Log));
        h.mkdir("/hot");
        for i in 0..4 {
            h.write_file(&format!("/hot/f{i}"), &pattern(i, 20_000));
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut ts = Vec::new();
        // writers
        for t in 0..3 {
            let (h, stop) = (h.clone(), stop.clone());
            ts.push(std::thread::spawn(move || {
                let mut rng = Rng::new(t);
                while !stop.load(Ordering::Relaxed) {
                    let p = format!("/hot/f{}", rng.below(4));
                    if let Ok(f) = h.try_open(&p, libc::O_RDWR) {
                        let _ = h.try_pwrite(f, rng.below(30_000), &rng.blob(4000));
                        h.close(f);
                    }
                }
            }));
        }
        // readers + getattr + readdir
        for t in 0..3 {
            let (h, stop) = (h.clone(), stop.clone());
            ts.push(std::thread::spawn(move || {
                let mut rng = Rng::new(100 + t);
                while !stop.load(Ordering::Relaxed) {
                    let p = format!("/hot/f{}", rng.below(4));
                    match rng.below(3) {
                        0 => {
                            if let Ok(f) = h.try_open(&p, libc::O_RDONLY) {
                                let _ = h.try_pread(f, rng.below(10_000), 16_384);
                                h.close(f);
                            }
                        }
                        1 => {
                            let _ = h.try_lookup(&p).and_then(|a| h.engine.getattr(&h.ctx, a.id));
                        }
                        _ => {
                            let _ = h.readdir("/hot");
                        }
                    }
                }
            }));
        }
        // renamers / unlinkers / creators
        for t in 0..3 {
            let (h, stop) = (h.clone(), stop.clone());
            ts.push(std::thread::spawn(move || {
                let mut rng = Rng::new(200 + t);
                while !stop.load(Ordering::Relaxed) {
                    let a = format!("/hot/f{}", rng.below(4));
                    let b = format!("/hot/f{}", rng.below(4));
                    match rng.below(5) {
                        0 => {
                            let _ = h.try_rename(&a, &b);
                        }
                        1 => {
                            let _ = h.try_unlink(&a);
                        }
                        2 => {
                            if let Ok(f) = h.try_create(&a, 0o644, libc::O_EXCL) {
                                let _ = h.try_pwrite(f, 0, &rng.bytes(5000));
                                h.close(f);
                            }
                        }
                        3 => {
                            let _ = h.try_link(&a, &b);
                        }
                        _ => {
                            let _ = h.truncate(&a, rng.below(25_000));
                        }
                    }
                }
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500 * soak() as u64));
        stop.store(true, Ordering::Relaxed);
        for t in ts {
            t.join().unwrap();
        }
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

/// Rename storms across directories with subtree moves, concurrent with operations deep inside the moved trees.
#[test]
fn stress_rename_trees() {
    let h = Arc::new(Harness::new(CheckLevel::Thorough, MismatchMode::Log));
    for d in ["/t0", "/t1", "/t2", "/t0/s", "/t1/s", "/t2/s"] {
        h.mkdir(d);
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut ts = Vec::new();
    for t in 0..4 {
        let (h, stop) = (h.clone(), stop.clone());
        ts.push(std::thread::spawn(move || {
            let mut rng = Rng::new(300 + t);
            while !stop.load(Ordering::Relaxed) {
                let a = format!("/t{}/s", rng.below(3));
                let b = format!("/t{}/s", rng.below(3));
                let c = format!("/t{}/q", rng.below(3));
                match rng.below(3) {
                    0 => {
                        let _ = h.try_rename(&a, &c);
                    }
                    1 => {
                        let _ = h.try_rename(&c, &b);
                    }
                    _ => {
                        let _ = h.try_rename(&a, &b);
                    }
                }
            }
        }));
    }
    for t in 0..4 {
        let (h, stop) = (h.clone(), stop.clone());
        ts.push(std::thread::spawn(move || {
            let mut rng = Rng::new(400 + t);
            while !stop.load(Ordering::Relaxed) {
                let d = format!("/t{}/{}", rng.below(3), if rng.below(2) == 0 { "s" } else { "q" });
                let f = format!("{d}/file{}", rng.below(3));
                match rng.below(4) {
                    0 => {
                        let _ = h.try_mkdir(&format!("{d}/in"), 0o755);
                    }
                    1 => {
                        if let Ok(fh) = h.try_create(&f, 0o644, 0) {
                            let _ = h.try_pwrite(fh, 0, &rng.bytes(1000));
                            h.close(fh);
                        }
                    }
                    2 => {
                        let _ = h.try_unlink(&f);
                    }
                    _ => {
                        let _ = h.try_lookup(&f).and_then(|a| h.engine.getattr(&h.ctx, a.id));
                    }
                }
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(1500 * soak() as u64));
    stop.store(true, Ordering::Relaxed);
    for t in ts {
        t.join().unwrap();
    }
    h.assert_no_mismatches();
    h.assert_trees_equal();
}

/// Append-only writers on the same file from many threads: the kernel serialises O_APPEND writes, so must we.
#[test]
fn concurrent_appends_same_file() {
    for level in LEVELS {
        let h = Arc::new(Harness::new(level, MismatchMode::Log));
        h.write_file("/log", b"");
        let ts: Vec<_> = (0..8)
            .map(|t| {
                let h = h.clone();
                std::thread::spawn(move || {
                    let f = h.open("/log", libc::O_WRONLY | libc::O_APPEND);
                    for i in 0..100 {
                        h.pwrite(f, 0, format!("t{t} line {i:03}\n").as_bytes());
                    }
                    h.close(f);
                })
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(h.read_file("/log").len(), 8 * 100 * "t0 line 000\n".len());
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

/// Different file systems for the two sides (tmpfs vs the disk-backed temp dir): must also be clean.
#[test]
fn cross_filesystem_pair_no_mismatch() {
    let (a, b) = (std::path::PathBuf::from("/dev/shm"), std::env::temp_dir());
    let candidates = [b.clone(), "/var/tmp".into(), std::env::current_dir().unwrap().join("target")];
    let Some(other) = candidates.iter().find(|c| c.is_dir() && dev_of(c) != dev_of(&a) && dev_of(&a) != 0) else {
        eprintln!("SKIP: no second file system found next to /dev/shm");
        return;
    };
    for level in LEVELS {
        let h = Harness::builder()
            .level(level)
            .bases(&a, other)
            // Directory link counts are file-system specific (ZFS, btrfs report other values than tmpfs/ext4).
            .config(|c| c.dir_nlink = false)
            .build();
        let xattrs = xattrs_supported(h.p_root()) && xattrs_supported(h.s_root());
        broad_workload(&h, xattrs, false);
        random_ops(&h, 7, 1500, xattrs, false);
        h.assert_no_mismatches();
        let opts = TreeOpts { dir_nlink: false, ..Default::default() };
        let d = tree_diff(h.p_root(), h.s_root(), &opts);
        assert!(d.is_empty(), "trees differ on {}: {d:?}", other.display());
    }
}

/// Without the lock-step guarantee this would fail: two writers racing on the same offsets must leave the same
/// final data on both sides.
#[test]
fn racing_overlapping_writers_end_identical() {
    let h = Arc::new(Harness::new(CheckLevel::Paranoid, MismatchMode::Log));
    h.write_file("/o", &vec![0u8; 8192]);
    let ts: Vec<_> = (0..6)
        .map(|t| {
            let h = h.clone();
            std::thread::spawn(move || {
                let f = h.open("/o", libc::O_RDWR);
                for i in 0..300 {
                    let off = (i * 37 + t * 11) % 4000;
                    h.pwrite(f, off as u64, &vec![t as u8 + 1; 3000 + (i % 50)]);
                    let _ = h.pread(f, 0, 8192);
                }
                h.close(f);
            })
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    h.assert_no_mismatches();
    h.assert_trees_equal();
}

/// The kernel evicts and re-looks-up inodes all the time: lookups and forgets of the very same inode racing in several
/// threads must never make a node that somebody still holds a reference to disappear (that would show up as
/// ESTALE for the next operation on it).
#[test]
fn concurrent_lookup_and_forget_of_the_same_inode() {
    for level in [CheckLevel::Basic, CheckLevel::Thorough] {
        let h = Arc::new(Harness::new(level, MismatchMode::Log));
        h.write_file("/f", b"data");
        h.mkdir("/d");
        h.link("/f", "/d/f-link");
        let stale = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ts: Vec<_> = (0..12)
            .map(|t| {
                let (h, stop, stale) = (h.clone(), stop.clone(), stale.clone());
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        // alternate between the two names of the same inode (same node id)
                        let p = if (n + t) % 2 == 0 { "/f" } else { "/d/f-link" };
                        let a = match h.try_lookup(p) {
                            Ok(a) => a,
                            Err(e) => panic!("lookup {p}: {e}"),
                        };
                        // we hold one lookup reference now: the node must be there
                        match h.engine.getattr(&h.ctx, a.id) {
                            Ok(_) => {}
                            Err(libc::ESTALE) => {
                                stale.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => panic!("getattr: {e}"),
                        }
                        h.engine.forget(a.id, 1);
                        n += 1;
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(std::env::var("RACE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500)));
        stop.store(true, Ordering::Relaxed);
        for t in ts {
            t.join().unwrap();
        }
        assert_eq!(stale.load(Ordering::Relaxed), 0, "a node vanished while a lookup reference was held");
        h.assert_no_mismatches();
    }
}

/// Sequential execution of the two halves and a tiny stripe table (every operation collides with many others)
/// are different code paths with the same guarantees.
#[test]
fn stress_sequential_halves_and_few_stripes() {
    for (parallel, stripes) in [(false, 4096usize), (true, 16), (false, 16)] {
        let h = Arc::new(
            Harness::builder()
                .level(CheckLevel::Thorough)
                .config(move |c| {
                    c.parallel = parallel;
                    c.lock_stripes = stripes;
                })
                .build(),
        );
        for d in ["/a", "/b", "/a/x", "/b/y"] {
            h.mkdir(d);
        }
        let xattrs = xattrs_supported(h.p_root()) && xattrs_supported(h.s_root());
        let ts: Vec<_> = (0..8)
            .map(|t| {
                let h = h.clone();
                std::thread::spawn(move || random_ops(&h, 2000 + t, 700 * soak(), xattrs, true))
            })
            .collect();
        for t in ts {
            t.join().unwrap();
        }
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

/// Combined setattr requests (the kernel sends mode+size+times in one SETATTR for e.g. `cp -p`) and ftruncate
/// through a file handle.
#[test]
fn setattr_combinations_and_handles() {
    use xcheckfs::backend::TimeSpec;
    use xcheckfs::sys::Ts;
    for level in LEVELS {
        let h = Harness::new(level, MismatchMode::Log);
        h.write_file("/f", &pattern(1, 10_000));
        let c = h.ctx;
        let a = h.setattr(
            "/f",
            SetAttr {
                mode: Some(0o640),
                uid: Some(c.uid),
                gid: Some(c.gid),
                size: Some(777),
                atime: Some(TimeSpec::Set(Ts { sec: 1_000_000_000, nsec: 5 })),
                mtime: Some(TimeSpec::Set(Ts { sec: 1_200_000_000, nsec: 7 })),
            },
        )
        .unwrap();
        assert_eq!((a.st.perm(), a.st.size, a.st.mtime.sec), (0o640, 777, 1_200_000_000));
        // ftruncate through the handle, grow and shrink
        let f = h.open("/f", libc::O_RDWR);
        for size in [5000u64, 0, 123_456] {
            let a = h.engine.setattr(&h.ctx, f.ino, SetAttr { size: Some(size), ..Default::default() }, Some(f.fh)).unwrap();
            assert_eq!(a.st.size, size);
        }
        h.close(f);
        // an empty setattr is a getattr
        h.setattr("/f", SetAttr::default()).unwrap();
        h.assert_no_mismatches();
        h.assert_trees_equal();
    }
}

/// Handles, directory handles, in-flight records and lock waiters must all be back to zero after a workload.
#[test]
fn bookkeeping_returns_to_zero() {
    let h = Arc::new(Harness::new(CheckLevel::Thorough, MismatchMode::Log));
    for d in ["/a", "/b", "/a/x", "/b/y"] {
        h.mkdir(d);
    }
    let ts: Vec<_> = (0..6)
        .map(|t| {
            let h = h.clone();
            std::thread::spawn(move || random_ops(&h, 3000 + t, 500, false, true))
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    assert_eq!(h.stats.open_files.load(Ordering::Relaxed), 0, "leaked file handles");
    assert_eq!(h.stats.open_dirs.load(Ordering::Relaxed), 0, "leaked directory handles");
    assert_eq!(h.stats.lock_waiters.load(Ordering::Relaxed), 0);
    assert!(h.stats.inflight.snapshot().is_empty());
    h.assert_no_mismatches();
}

/// Differences in the roots themselves are found when the engine starts.
#[test]
fn root_directory_mismatch_is_reported_at_startup() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::builder().build_with(|_, s| std::fs::set_permissions(s, std::fs::Permissions::from_mode(0o700)).unwrap());
    let m = h.expect_mismatch(xcheckfs::policy::MismatchKind::Attr, Some("mode"));
    assert_eq!(m.path, "/");
    assert!(m.detail.contains("mount root"));
}

/// The two halves run concurrently, so their timestamps differ by up to the operation's execution window (here
/// the secondary's injected latency, far above the 30 ms tolerance). The engine widens the tolerance of the
/// changed objects by that window, also after the kernel forgot the node: no false mtime/ctime mismatches.
#[test]
fn slow_secondary_does_not_cause_timestamp_mismatches() {
    use xcheckfs::backend::fault::{Effect, Fault, FaultOp};
    let h = Harness::builder().config(|c| c.time_tolerance = std::time::Duration::from_millis(30)).build();
    h.write_file("/f", b"x");
    h.fault.add(Fault::new(FaultOp::Pwrite, Effect::Delay(std::time::Duration::from_millis(200))));
    h.write_file("/f", b"y");
    let a = h.getattr("/f");
    // forget the node completely, then look it up again: the slack must survive
    h.engine.forget(a.id, u64::MAX >> 1);
    h.lookup("/f");
    h.getattr("/f");
    h.fault.clear();
    // a slow mkdir changes the parent's mtime: same for directories
    h.fault.add(Fault::new(FaultOp::Mkdir, Effect::Delay(std::time::Duration::from_millis(200))));
    h.mkdir_p("/d/e");
    h.getattr("/d");
    h.getattr("/");
    h.assert_no_mismatches();
}

/// A genuinely wrong mtime is still caught when nothing was slow.
#[test]
fn wrong_mtime_is_still_reported_with_a_small_tolerance() {
    use xcheckfs::backend::fault::{Effect, Fault, FaultOp, StatLie};
    let h = Harness::builder().config(|c| c.time_tolerance = std::time::Duration::from_millis(30)).build();
    h.write_file("/f", b"x");
    h.fault.add(Fault::new(FaultOp::Stat, Effect::Stat(StatLie::MtimeShift(5))));
    h.getattr("/f");
    h.expect_mismatch(xcheckfs::policy::MismatchKind::Attr, Some("mtime"));
}
