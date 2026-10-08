#!/usr/bin/env python3
"""Multi-process SQLite stress workload for xcheckfs (stdlib only).

Sub-commands
  run     driver: create the databases, start the worker processes, supervise them
          (kill/restart, signals, hang detection, xcheckfs status sampling), run the
          final consistency checks and print/write a JSON result.
  check   open database files directly and run the integrity / invariant checks
          (used on the primary and the secondary tree after unmounting).
  worker  internal: one worker process (writer, reader, maint, holder or locker).

The "bank": table accounts (NACC rows, constant total balance), history (one row per committed
transfer, with a gapless sequence), meta.ops (a counter incremented in every transfer).  Invariants:
sum(balance) constant, history.seq == 1..N without gaps or duplicates, meta.ops == N,
balance == initial + replay(history), PRAGMA integrity_check == ok, rows per worker >= commits the
worker confirmed.  Lockers use blocking fcntl record locks (F_SETLKW) on a side file and protect
counters with them (mutual exclusion check; opposite lock orders provoke EDEADLK).
"""
import argparse
import errno
import fcntl
import glob
import hashlib
import json
import math
import os
import random
import signal
import sqlite3
import struct
import subprocess
import sys
import time

NACC = 1000
INIT = 1000
TOTAL = NACC * INIT
BUSY_MS = 30000
HB_STUCK_S = 60.0  # no heartbeat for this long => stuck
LOCK_A, LOCK_B, LOCK_C = 0, 1, 2  # byte offsets locked in lock.dat
CNT_A, CNT_AB, CNT_C = 64, 72, 80  # counter offsets in lock.dat (outside the locked bytes)

SCHEMA = """
CREATE TABLE accounts(id INTEGER PRIMARY KEY, balance INTEGER NOT NULL, pad TEXT NOT NULL);
CREATE TABLE history(seq INTEGER NOT NULL, src INTEGER NOT NULL, dst INTEGER NOT NULL,
                     amount INTEGER NOT NULL, worker TEXT NOT NULL, ts REAL NOT NULL, memo TEXT);
CREATE INDEX history_seq ON history(seq);
CREATE TABLE meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL);
INSERT INTO meta VALUES('ops', 0);
"""


def now():
    return time.time()


def journal_of(mode):
    return mode.split("-")[0]


def log_line(path, text):
    fd = os.open(path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
    try:
        os.write(fd, (text.rstrip("\n") + "\n").encode())
    finally:
        os.close(fd)


# ----------------------------------------------------------------------------------------------
# connection helpers


def configure(con, mode, busy_ms=BUSY_MS):
    """Per-connection pragmas for the journal mode under test."""
    con.execute("PRAGMA busy_timeout=%d" % busy_ms)
    jm = journal_of(mode)
    for attempt in range(20):
        try:
            got = con.execute("PRAGMA journal_mode=%s" % jm).fetchone()[0]
            break
        except sqlite3.OperationalError:
            time.sleep(0.05)
    else:
        raise RuntimeError("cannot set journal_mode=%s" % jm)
    if got.lower() != jm:
        raise RuntimeError("journal_mode is %s, wanted %s" % (got, jm))
    if mode == "wal-full":
        con.execute("PRAGMA synchronous=FULL")
    elif jm == "wal":
        con.execute("PRAGMA synchronous=NORMAL")
    else:
        con.execute("PRAGMA synchronous=FULL")
    if mode == "wal-mmap":
        con.execute("PRAGMA mmap_size=268435456")


def open_db(path, mode, busy_ms=BUSY_MS):
    con = sqlite3.connect(path, timeout=busy_ms / 1000.0, isolation_level=None)
    configure(con, mode, busy_ms)
    return con


def is_busy(e):
    m = str(e).lower()
    return "locked" in m or "busy" in m


# ----------------------------------------------------------------------------------------------
# invariant checks (shared by readers, the final check and the `check` sub-command)


def snapshot_problems(con, deep):
    """One consistent read transaction; returns a list of violation strings."""
    probs = []
    con.execute("BEGIN")
    try:
        s = con.execute("SELECT SUM(balance), COUNT(*) FROM accounts").fetchone()
        c, mx, mn = con.execute("SELECT COUNT(*), MAX(seq), MIN(seq) FROM history").fetchone()
        ops = con.execute("SELECT v FROM meta WHERE k='ops'").fetchone()[0]
        dup = None
        if deep:
            dup = con.execute(
                "SELECT COUNT(*) FROM (SELECT seq FROM history GROUP BY seq HAVING COUNT(*)>1)"
            ).fetchone()[0]
    finally:
        con.execute("COMMIT")
    if s[0] != TOTAL or s[1] != NACC:
        probs.append("balance sum=%s accounts=%s (want %d/%d)" % (s[0], s[1], TOTAL, NACC))
    if c:
        if not (c == mx == ops and mn == 1):
            probs.append("history count=%s max=%s min=%s meta.ops=%s" % (c, mx, mn, ops))
    elif ops != 0:
        probs.append("history empty but meta.ops=%s" % ops)
    if dup:
        probs.append("%d duplicate history seq values" % dup)
    return probs


def full_check(path, mode="wal", per_worker=None):
    """Heavy offline-style check of one database.  Returns (problems, info)."""
    probs = []
    info = {"path": path}
    con = sqlite3.connect(path, timeout=BUSY_MS / 1000.0, isolation_level=None)
    con.execute("PRAGMA busy_timeout=%d" % BUSY_MS)
    ic = [r[0] for r in con.execute("PRAGMA integrity_check").fetchall()]
    info["integrity_check"] = ic if ic != ["ok"] else "ok"
    if ic != ["ok"]:
        probs.append("integrity_check: %s" % ic[:5])
    fk = con.execute("PRAGMA foreign_key_check").fetchall()
    if fk:
        probs.append("foreign_key_check: %s" % fk[:3])
    probs += snapshot_problems(con, True)
    # replay history: balance == INIT + incoming - outgoing for every account
    delta = {}
    for src, dst, amt in con.execute("SELECT src,dst,amount FROM history"):
        delta[src] = delta.get(src, 0) - amt
        delta[dst] = delta.get(dst, 0) + amt
    bad = 0
    for aid, bal in con.execute("SELECT id,balance FROM accounts"):
        if bal != INIT + delta.get(aid, 0):
            bad += 1
            if bad <= 3:
                probs.append("account %d balance %d != replay %d" % (aid, bal, INIT + delta.get(aid, 0)))
    if bad:
        probs.append("%d accounts differ from history replay" % bad)
    rows = {w: n for w, n in con.execute("SELECT worker, COUNT(*) FROM history GROUP BY worker")}
    info["history_rows"] = sum(rows.values())
    info["pagecount"] = con.execute("PRAGMA page_count").fetchone()[0]
    h = hashlib.sha256()
    for r in con.execute("SELECT id,balance,pad FROM accounts ORDER BY id"):
        h.update(repr(r).encode())
    for r in con.execute("SELECT rowid,seq,src,dst,amount,worker,memo FROM history ORDER BY seq, worker, src, dst"):
        h.update(repr(r[1:]).encode())
    info["digest"] = h.hexdigest()
    con.close()
    if per_worker is not None:
        for name, w in per_worker.items():
            n = rows.pop(name, 0)
            lo = w["commits"]
            hi = w["commits"] + w["commit_errors"] + (1 if w["terminated"] else 0) + w["errors"]
            if not lo <= n <= hi:
                probs.append("worker %s: %d history rows, confirmed commits %d (allowed up to %d)" % (name, n, lo, hi))
        if rows:
            probs.append("history rows from unknown workers: %s" % dict(list(rows.items())[:5]))
    return probs, info


# ----------------------------------------------------------------------------------------------
# worker


class State:
    """Per-worker counters in a small fixed-size file on a local file system; survives SIGKILL."""

    SIZE = 384

    def __init__(self, d, name):
        self.path = os.path.join(d, name + ".cnt")
        self.fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o644)
        self.phase = "start"
        self.c = dict(commits=0, rollbacks=0, reads=0, busy=0, errors=0, commit_errors=0, viol=0,
                      deadlocks=0, succ_a=0, succ_ab=0, succ_c=0, sigs=0, sig_in_wait=0, maint=0,
                      ckpt_busy=0)

    def flush(self):
        doc = json.dumps(dict(hb=round(now(), 3), phase=self.phase, **self.c), separators=(",", ":"))
        os.pwrite(self.fd, doc.encode().ljust(self.SIZE)[: self.SIZE], 0)


def read_state(d, name):
    try:
        with open(os.path.join(d, name + ".cnt"), "rb") as f:
            return json.loads(f.read().decode().strip("\0 \n"))
    except Exception:
        return None


class Interrupted(Exception):
    pass


LAT_SCALE = 20  # histogram buckets per e-fold of microseconds (about 5% wide)


class LatHist:
    """Commit latencies of one worker as a log-scale histogram (microseconds).  Written to <name>.hist at most every
    2 s (and at exit), so the numbers of a SIGKILLed worker survive; the driver merges all files."""

    def __init__(self, d, name):
        self.path = os.path.join(d, name + ".hist")
        self.b, self.n, self.sum, self.max, self.last = {}, 0, 0.0, 0.0, time.monotonic()

    def add(self, sec):
        k = int(math.log(max(sec * 1e6, 1.0)) * LAT_SCALE)
        self.b[k] = self.b.get(k, 0) + 1
        self.n += 1
        self.sum += sec
        self.max = max(self.max, sec)

    def flush(self, force=False):
        t = time.monotonic()
        if not force and t - self.last < 2.0:
            return
        self.last = t
        doc = json.dumps(dict(n=self.n, sum=self.sum, max=self.max, b=self.b), separators=(",", ":"))
        tmp = self.path + ".tmp"
        with open(tmp, "w") as f:
            f.write(doc)
        os.replace(tmp, self.path)


def hist_stats(files):
    """Merge LatHist files; returns {n, avg, p50, p95, p99, max} in ms (or None without samples)."""
    b, n, tot, mx = {}, 0, 0.0, 0.0
    for f in files:
        try:
            with open(f) as fh:
                j = json.load(fh)
        except Exception:  # noqa: BLE001
            continue
        for k, c in j["b"].items():
            b[int(k)] = b.get(int(k), 0) + c
        n += j["n"]
        tot += j["sum"]
        mx = max(mx, j["max"])
    if not n:
        return None

    def q(p):
        want, acc = max(1, math.ceil(n * p)), 0
        for k in sorted(b):
            acc += b[k]
            if acc >= want:
                return min(mx, math.exp((k + 0.5) / LAT_SCALE) / 1e6) * 1000
        return mx * 1000

    return dict(n=n, avg=round(tot / n * 1000, 3), p50=round(q(0.5), 3), p95=round(q(0.95), 3),
                p99=round(q(0.99), 3), max=round(mx * 1000, 3))


def worker_main(a):
    rng = random.Random(a.seed)
    st = State(a.state, a.name)
    lat = LatHist(a.state, a.name)
    exit_how = "end"
    con = None

    def violation(msg):
        st.c["viol"] += 1
        log_line(os.path.join(a.state, "violations.log"), "%.3f %s %s: %s" % (now(), a.name, a.role, msg))

    def error(msg):
        st.c["errors"] += 1
        if st.c["errors"] <= 20:
            log_line(os.path.join(a.state, "errors.log"), "%.3f %s %s: %s" % (now(), a.name, a.role, msg))

    if a.graceful:
        def handler(sig, frm):
            st.c["sigs"] += 1
            if st.phase in ("begin", "lockw", "commit", "sleep"):
                st.c["sig_in_wait"] += 1
            raise Interrupted(sig)

        signal.signal(signal.SIGTERM, handler)
        signal.signal(signal.SIGINT, handler)
    else:
        signal.signal(signal.SIGINT, signal.SIG_DFL)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)

    hold = a.hold_ms / 1000.0
    try:
        st.flush()
        if a.role == "locker":
            locker_loop(a, st, rng, violation, error)
        else:
            con = open_db(a.db, a.mode, a.busy_ms)
            con.execute("PRAGMA wal_autocheckpoint=%d" % rng.choice([100, 250, 500, 1000, 1000]))
            if a.role in ("writer", "holder"):
                if a.role == "holder":
                    hold = max(hold, 0.3)
                writer_loop(a, st, con, rng, hold, lat, violation, error)
            elif a.role == "reader":
                reader_loop(a, st, con, rng, violation, error)
            elif a.role == "maint":
                maint_loop(a, st, con, rng, violation, error)
    except Interrupted:
        exit_how = "signal"
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    except Exception as e:  # noqa: BLE001
        exit_how = "exception"
        error("worker died: %r" % (e,))
        raise
    finally:
        if con is not None:
            try:
                if con.in_transaction:
                    con.execute("ROLLBACK")
            except Exception:
                pass
            try:
                con.close()
            except Exception:
                pass
        st.phase = "done"
        st.flush()
        lat.flush(True)
        hs = hist_stats([lat.path]) or {}
        with open(os.path.join(a.state, a.name + ".json"), "w") as f:
            json.dump(dict(name=a.name, role=a.role, exit=exit_how, lat_n=lat.n,
                           p50=hs.get("p50", 0) / 1000 if hs else None, p99=hs.get("p99", 0) / 1000 if hs else None,
                           pmax=lat.max if lat.n else None, **st.c), f)


def writer_loop(a, st, con, rng, hold, lat, violation, error):
    n = 0
    while now() < a.end:
        x, y = rng.sample(range(1, NACC + 1), 2)
        amt = rng.randint(1, 50)
        rollback = rng.random() < 0.1
        t0 = time.perf_counter()
        st.phase = "begin"
        st.flush()
        try:
            con.execute("BEGIN IMMEDIATE")
            st.phase = "txn"
            st.flush()
            con.execute("UPDATE accounts SET balance=balance-?, pad=? WHERE id=?", (amt, os.urandom(40).hex(), x))
            con.execute("UPDATE accounts SET balance=balance+?, pad=? WHERE id=?", (amt, os.urandom(40).hex(), y))
            seq = con.execute("SELECT COALESCE(MAX(seq),0)+1 FROM history").fetchone()[0]
            con.execute("INSERT INTO history VALUES(?,?,?,?,?,?,?)",
                        (seq, x, y, amt, a.name, now(), os.urandom(24).hex()))
            con.execute("UPDATE meta SET v=v+1 WHERE k='ops'")
            if hold:
                st.phase = "sleep"
                st.flush()
                time.sleep(rng.uniform(0, hold))
                st.phase = "txn"
            if rollback:
                con.execute("ROLLBACK")
                st.c["rollbacks"] += 1
            else:
                st.phase = "commit"
                st.flush()
                con.execute("COMMIT")
                st.c["commits"] += 1
                lat.add(time.perf_counter() - t0)
                lat.flush()
        except sqlite3.Error as e:
            was_commit = st.phase == "commit"
            try:
                if con.in_transaction:
                    con.execute("ROLLBACK")
            except sqlite3.Error:
                pass
            if is_busy(e):
                st.c["busy"] += 1
            else:
                if was_commit:
                    st.c["commit_errors"] += 1
                error("%s: %s" % (type(e).__name__, e))
                if st.c["errors"] > 50:
                    raise
        st.phase = "idle"
        n += 1
        if n % 20 == 0:
            try:
                for p in snapshot_problems(con, False):
                    violation("writer view: " + p)
                st.c["reads"] += 1
            except sqlite3.Error as e:
                if is_busy(e):
                    st.c["busy"] += 1
                else:
                    error("%s: %s" % (type(e).__name__, e))
        st.flush()


def reader_loop(a, st, con, rng, violation, error):
    n = 0
    while now() < a.end:
        n += 1
        st.phase = "begin"
        st.flush()
        try:
            for p in snapshot_problems(con, n % 10 == 0):
                violation(p)
            st.c["reads"] += 1
        except sqlite3.Error as e:
            try:
                if con.in_transaction:
                    con.execute("ROLLBACK")
            except sqlite3.Error:
                pass
            if is_busy(e):
                st.c["busy"] += 1
            else:
                error("%s: %s" % (type(e).__name__, e))
        st.phase = "sleep"
        st.flush()
        time.sleep(rng.uniform(0, 0.02))


def maint_loop(a, st, con, rng, violation, error):
    due = {"ckpt": now() + 3, "vac": now() + 8, "integ": now() + 5}
    every = {"ckpt": 6.0, "vac": 20.0, "integ": 12.0}
    while now() < a.end:
        for k in sorted(due, key=due.get):
            if now() < due[k]:
                continue
            due[k] = now() + every[k] * rng.uniform(0.7, 1.3)
            st.phase = "maint-" + k
            st.flush()
            try:
                if k == "ckpt":
                    r = con.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
                    if r[0]:
                        st.c["ckpt_busy"] += 1
                elif k == "vac":
                    con.execute("VACUUM")
                else:
                    con.execute("BEGIN")
                    try:
                        rows = [r[0] for r in con.execute("PRAGMA integrity_check").fetchall()]
                    finally:
                        con.execute("COMMIT")
                    if rows != ["ok"]:
                        violation("integrity_check: %s" % rows[:5])
                st.c["maint"] += 1
            except sqlite3.Error as e:
                try:
                    if con.in_transaction:
                        con.execute("ROLLBACK")
                except sqlite3.Error:
                    pass
                if is_busy(e):
                    st.c["busy"] += 1
                    due[k] = now() + 1.0
                else:
                    error("%s %s: %s" % (k, type(e).__name__, e))
        st.phase = "sleep"
        st.flush()
        time.sleep(0.2)


def locker_loop(a, st, rng, violation, error):
    fd = os.open(a.lockfile, os.O_RDWR)
    hold = max(a.hold_ms, 3) / 1000.0

    def lock(off, ex):
        st.phase = "lockw"
        st.flush()
        fcntl.lockf(fd, fcntl.LOCK_EX if ex else fcntl.LOCK_SH, 1, off, 0)
        st.phase = "locked"

    def unlock(off):
        fcntl.lockf(fd, fcntl.LOCK_UN, 1, off, 0)

    def rd(off):
        return struct.unpack("<Q", os.pread(fd, 8, off))[0]

    def bump(off, key):
        v = rd(off)
        time.sleep(rng.uniform(0, hold))
        os.pwrite(fd, struct.pack("<Q", v + 1), off)
        st.c[key] += 1
        st.flush()

    while now() < a.end:
        held = []
        try:
            if a.lk_kind == "a":
                lock(LOCK_A, True)
                held.append(LOCK_A)
                bump(CNT_A, "succ_a")
            elif a.lk_kind in ("ab", "ba"):
                order = [LOCK_A, LOCK_B] if a.lk_kind == "ab" else [LOCK_B, LOCK_A]
                for off in order:
                    lock(off, True)
                    held.append(off)
                    time.sleep(rng.uniform(0, 0.002))
                bump(CNT_AB, "succ_ab")
            else:  # rw
                if rng.random() < 0.7:
                    lock(LOCK_C, False)
                    held.append(LOCK_C)
                    v1 = rd(CNT_C)
                    time.sleep(rng.uniform(0, hold))
                    v2 = rd(CNT_C)
                    if v1 != v2:
                        violation("shared lock held but counter changed %d -> %d (writer got in)" % (v1, v2))
                    st.c["reads"] += 1
                else:
                    lock(LOCK_C, True)
                    held.append(LOCK_C)
                    bump(CNT_C, "succ_c")
        except OSError as e:
            if e.errno == errno.EDEADLK:
                st.c["deadlocks"] += 1
            elif e.errno == errno.EINTR:
                st.c["busy"] += 1
            else:
                error("lock error %r" % (e,))
        finally:
            for off in held:
                try:
                    unlock(off)
                except OSError as e:
                    error("unlock %d: %r" % (off, e))
        st.phase = "idle"
        st.flush()
        time.sleep(rng.uniform(0, 0.003))


# ----------------------------------------------------------------------------------------------
# driver


def sh(cmd, timeout=15):
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return None, "", "timeout after %ds" % timeout


def proc_diag(pid):
    out = {}
    for f in ("wchan", "stat", "syscall"):
        try:
            with open("/proc/%d/%s" % (pid, f)) as fh:
                out[f] = fh.read().strip()[:200]
        except OSError as e:
            out[f] = "unreadable: %s" % e
    try:
        out["state"] = [l.strip() for l in open("/proc/%d/status" % pid) if l.startswith("State")][0]
    except Exception:
        pass
    try:
        with open("/proc/%d/stack" % pid) as fh:
            out["stack"] = fh.read().strip()[:600]
    except OSError:
        pass
    try:
        out["locks"] = [l.strip() for l in open("/proc/locks") if (" %d " % pid) in l][:10]
    except OSError:
        pass
    return out


class Proc:
    def __init__(self, name, role, db, popen, graceful, kind=None):
        self.name, self.role, self.db, self.p, self.graceful, self.kind = name, role, db, popen, graceful, kind
        self.t0 = now()
        self.signalled = None
        self.stuck_reported = False


def driver(a):
    t_start = now()
    mode = a.mode
    state = a.state
    os.makedirs(state, exist_ok=True)
    os.makedirs(a.dir, exist_ok=True)
    ndb = a.dbs
    dbs = [os.path.join(a.dir, "bank%d.db" % i) for i in range(ndb)]
    for d in dbs:
        for suf in ("", "-wal", "-shm", "-journal"):
            if os.path.exists(d + suf):
                os.unlink(d + suf)
    # ---- setup
    t0 = now()
    for d in dbs:
        con = sqlite3.connect(d, timeout=BUSY_MS / 1000.0, isolation_level=None)
        con.execute("PRAGMA page_size=4096")
        con.execute("PRAGMA journal_mode=%s" % ("wal" if journal_of(mode) == "wal" else "delete"))
        con.executescript(SCHEMA)
        con.execute("BEGIN")
        con.executemany("INSERT INTO accounts VALUES(?,?,?)",
                        [(i, INIT, os.urandom(40).hex()) for i in range(1, NACC + 1)])
        con.execute("COMMIT")
        con.close()
    lockfile = os.path.join(a.dir, "lock.dat")
    with open(lockfile, "wb") as f:
        f.write(b"\0" * 4096)
    setup_s = now() - t0
    print("setup: %d db(s) in %.2fs, mode=%s scenario=%s" % (ndb, setup_s, mode, a.scenario), flush=True)

    # ---- plan the processes
    plan = []  # (role, db_index, lk_kind)
    if a.scenario == "multidb":
        # two writers per database, plus a reader (even) or a maintenance process (odd)
        for i in range(ndb):
            plan += [("writer", i, None)] * 2
            plan.append(("reader", i, None) if i % 2 == 0 else ("maint", i, None))
    else:
        nread = max(2, a.procs // 4)
        nlock = 0
        if a.scenario in ("kill", "signal"):
            nlock = 5 if a.procs >= 12 else 3
        nhold = 1 if a.scenario == "signal" else 0
        nwrite = a.procs - nread - 1 - nlock - nhold
        plan += [("writer", 0, None)] * nwrite + [("holder", 0, None)] * nhold
        plan += [("reader", 0, None)] * nread + [("maint", 0, None)]
        kinds = ["a", "ab", "ba", "rw", "rw"] if nlock >= 5 else ["a", "ab", "rw"][:nlock]
        plan += [("locker", 0, k) for k in kinds[:nlock]]
    # Python signal handlers only run when a sqlite3 call returns; SQLite's busy handler sleeps in C for the whole
    # busy_timeout.  In the signal scenario use a short timeout (retried) so signals are seen while "waiting".
    busy_ms = 250 if a.scenario == "signal" else BUSY_MS
    end = now() + a.duration
    procs, all_names, spawn_count = [], {}, [0]
    wlog = open(os.path.join(state, "workers.log"), "a")

    def spawn(role, dbi, kind, graceful=None):
        spawn_count[0] += 1
        n = spawn_count[0]
        name = "%s%d" % (role[0], n)
        if graceful is None:
            graceful = (n % 2 == 0)
        cmd = [sys.executable, os.path.abspath(__file__), "worker", "--name", name, "--role", role,
               "--db", dbs[dbi], "--mode", mode, "--end", repr(end), "--hold-ms", str(a.hold_ms),
               "--state", state, "--seed", str(a.seed * 1000 + n), "--lockfile", lockfile,
               "--busy-ms", str(busy_ms)]
        if kind:
            cmd += ["--lk-kind", kind]
        if graceful:
            cmd.append("--graceful")
        p = subprocess.Popen(cmd, stdout=wlog, stderr=wlog)
        pr = Proc(name, role, dbi, p, graceful, kind)
        procs.append(pr)
        all_names[name] = pr
        return pr

    for role, dbi, kind in plan:
        spawn(role, dbi, kind, graceful=None if a.scenario == "signal" else False)
    events, stuck, unexpected = [], [], []
    sig_lat, kills, sigs = [], 0, {"TERM": 0, "INT": 0}
    xc = {"samples": 0, "max_lock_waiters": 0, "max_mismatches": 0, "last": None, "ctl_timeouts": 0}
    last_status = last_print = 0.0
    prev_commits, prev_t = 0, now()
    next_action = now() + 3
    rng = random.Random(a.seed)

    def sample_xc():
        if not a.status_cmd:
            return
        rc, out, err = sh(a.status_cmd, 20)
        if rc is None:
            xc["ctl_timeouts"] += 1
            events.append("%.1f ctl status timed out (%s)" % (now() - t_start, err))
            print("!! xcheckfs ctl status timed out", flush=True)
            return
        try:
            d = json.loads(out)
        except Exception:
            events.append("ctl status unparsable: %r %r" % (out[:100], err[:100]))
            return
        xc["samples"] += 1
        xc["last"] = d
        xc["max_lock_waiters"] = max(xc["max_lock_waiters"], d.get("lock_waiters", 0))
        xc["max_mismatches"] = max(xc["max_mismatches"], d.get("mismatches", 0))

    def totals():
        t = dict(commits=0, reads=0, busy=0, errors=0, viol=0)
        for n in all_names:
            s = read_state(state, n)
            if s:
                for k in t:
                    t[k] += s.get(k, 0)
        return t

    def diagnose(pr, why):
        d = proc_diag(pr.p.pid)
        rc, out, err = sh(a.status_cmd, 15) if a.status_cmd else (None, "", "")
        rec = dict(name=pr.name, role=pr.role, pid=pr.p.pid, why=why, diag=d,
                   state=read_state(state, pr.name), xc_status=out.strip()[:600] or err)
        stuck.append(rec)
        print("!! STUCK %s" % json.dumps(rec), flush=True)

    def respawn(pr):
        if now() < end - 2:
            spawn(pr.role, pr.db, pr.kind, graceful=None if a.scenario == "signal" else False)

    # ---- supervise
    while True:
        t = now()
        alive = [p for p in procs if p.p.poll() is None]
        for pr in procs:
            if pr.p.poll() is not None and not getattr(pr, "reaped", False):
                pr.reaped = True
                rc = pr.p.returncode
                if pr.signalled is None and rc != 0:
                    unexpected.append((pr.name, pr.role, rc))
                    print("!! worker %s (%s) exited rc=%s unexpectedly" % (pr.name, pr.role, rc), flush=True)
        alive = [p for p in procs if p.p.poll() is None]
        if t >= end and not alive:
            break
        if t >= end + a.grace:
            for pr in alive:
                diagnose(pr, "still running %ds after the end" % a.grace)
            for pr in alive:
                pr.p.kill()
            time.sleep(5)
            for pr in alive:
                if pr.p.poll() is None:
                    events.append("worker %s did not die after SIGKILL (pid %d)" % (pr.name, pr.p.pid))
                    print("!! %s survived SIGKILL: %s" % (pr.name, proc_diag(pr.p.pid)), flush=True)
            break
        # heartbeat check
        for pr in alive:
            s = read_state(state, pr.name)
            if s and not pr.stuck_reported and t - s["hb"] > HB_STUCK_S and s["phase"] != "done":
                pr.stuck_reported = True
                diagnose(pr, "no heartbeat for %.0fs" % (t - s["hb"]))
        # scenario actions
        if t < end - 3 and t >= next_action and alive:
            if a.scenario == "kill":
                weights = {"writer": 4, "holder": 4, "locker": 3, "maint": 2, "reader": 1}
                victim = rng.choices(alive, [weights[p.role] for p in alive])[0]
                victim.signalled = "KILL"
                victim.p.kill()
                victim.p.wait()
                kills += 1
                events.append("%.1f SIGKILL %s(%s) phase=%s" % (t - t_start, victim.name, victim.role,
                               (read_state(state, victim.name) or {}).get("phase")))
                time.sleep(rng.uniform(0, 0.3))
                respawn(victim)
                next_action = now() + rng.uniform(1.0, 3.0)
            elif a.scenario == "signal":
                cands = [p for p in alive if p.role != "holder"]
                if cands:
                    victim = rng.choice(cands)
                    sname = rng.choice(["TERM", "INT"])
                    victim.signalled = sname
                    sigs[sname] += 1
                    ts = now()
                    st0 = (read_state(state, victim.name) or {}).get("phase")
                    victim.p.send_signal(signal.SIGTERM if sname == "TERM" else signal.SIGINT)
                    try:
                        victim.p.wait(timeout=20)
                        lat_s = now() - ts
                        sig_lat.append(lat_s)
                        if lat_s > 2.0:
                            events.append("%.1f SIG%s %s(%s) phase=%s took %.2fs to die" %
                                          (t - t_start, sname, victim.name, victim.role, st0, lat_s))
                    except subprocess.TimeoutExpired:
                        diagnose(victim, "did not exit within 20s of SIG%s" % sname)
                        victim.p.kill()
                        victim.p.wait(timeout=20)
                    respawn(victim)
                next_action = now() + rng.uniform(0.6, 2.0)
        if t - last_status >= 5:
            last_status = t
            sample_xc()
        if t - last_print >= 10:
            last_print = t
            tt = totals()
            rate = (tt["commits"] - prev_commits) / max(1e-9, t - prev_t)
            prev_commits, prev_t = tt["commits"], t
            xs = xc["last"] or {}
            print("[%4ds] procs=%d commits=%d (%.0f/s) reads=%d busy=%d errors=%d viol=%d | xc ops=%s mm=%s lw=%s" %
                  (t - t_start, len(alive), tt["commits"], rate, tt["reads"], tt["busy"], tt["errors"],
                   tt["viol"], xs.get("ops"), xs.get("mismatches"), xs.get("lock_waiters")), flush=True)
        time.sleep(0.25)
    run_s = now() - (end - a.duration)
    wlog.close()
    sample_xc()

    # ---- aggregate worker state
    agg = dict(commits=0, rollbacks=0, reads=0, busy=0, errors=0, commit_errors=0, viol=0, deadlocks=0,
               succ_a=0, succ_ab=0, succ_c=0, sig_in_wait=0, maint=0, ckpt_busy=0)
    per_db_w = [dict() for _ in dbs]
    lats = []
    hist_files = [os.path.join(state, n + ".hist") for n in all_names]
    term = {"a": 0, "ab": 0, "c": 0}
    for n, pr in all_names.items():
        s = read_state(state, n) or {}
        for k in agg:
            agg[k] += s.get(k, 0)
        terminated = pr.signalled is not None
        if pr.role != "locker":
            per_db_w[pr.db][n] = dict(commits=s.get("commits", 0), commit_errors=s.get("commit_errors", 0),
                                    errors=s.get("errors", 0), terminated=terminated)
        elif terminated:
            if pr.kind == "a":
                term["a"] += 1
            elif pr.kind in ("ab", "ba"):
                term["ab"] += 1
            else:
                term["c"] += 1
        try:
            with open(os.path.join(state, n + ".json")) as f:
                j = json.load(f)
            if j.get("p50") is not None:
                lats.append((j["lat_n"], j["p50"], j["p99"], j["pmax"]))
        except Exception:
            pass

    # ---- final checks (through the mount, all worker processes are gone)
    problems = []
    if os.path.exists(os.path.join(state, "violations.log")):
        vl = open(os.path.join(state, "violations.log")).read().splitlines()
        problems.append("%d live violations, first: %s" % (len(vl), vl[:5]))
    if unexpected:
        problems.append("unexpected worker exits: %s" % unexpected[:10])
    if stuck:
        problems.append("%d stuck worker report(s)" % len(stuck))
    infos = []
    n_final = 0
    signal.alarm(600)
    for i, d in enumerate(dbs):
        try:
            pr_, info = full_check(d, mode, per_db_w[i])
        except Exception as e:  # noqa: BLE001
            pr_, info = ["final check raised %r" % (e,)], {"path": d}
        infos.append(info)
        problems += ["%s: %s" % (os.path.basename(d), x) for x in pr_]
        n_final += len(pr_)
    signal.alarm(0)
    # lock counters
    cnt = {}
    with open(lockfile, "rb") as f:
        raw = f.read(128)
    for key, off in (("a", CNT_A), ("ab", CNT_AB), ("c", CNT_C)):
        cnt[key] = struct.unpack("<Q", raw[off:off + 8])[0]
    for key, succ in (("a", agg["succ_a"]), ("ab", agg["succ_ab"]), ("c", agg["succ_c"])):
        if a.scenario in ("kill", "signal") and not (succ <= cnt[key] <= succ + term[key] + 1):
            problems.append("lock counter %s=%d, confirmed successes=%d, terminated lockers=%d" %
                            (key, cnt[key], succ, term[key]))
            n_final += 1
    # lock-file db leftovers
    left = sorted(os.path.basename(x) for x in glob.glob(os.path.join(a.dir, "*")) if x.endswith(("-wal", "-shm", "-journal")))

    summ = dict(label=a.label, scenario=a.scenario, mode=mode, procs=a.procs, dbs=ndb, duration=a.duration,
                run_s=round(run_s, 2), setup_s=round(setup_s, 2), commits=agg["commits"],
                commits_per_s=round(agg["commits"] / run_s, 1), reads=agg["reads"],
                reads_per_s=round(agg["reads"] / run_s, 1), rollbacks=agg["rollbacks"], busy_errors=agg["busy"],
                errors=agg["errors"], commit_errors=agg["commit_errors"], violations=agg["viol"],
                deadlocks_edeadlk=agg["deadlocks"], lock_ops_ok=agg["succ_a"] + agg["succ_ab"] + agg["succ_c"],
                lock_counters=cnt, maint_ops=agg["maint"], ckpt_busy=agg["ckpt_busy"],
                tx_latency_ms=hist_stats(hist_files), final_problems=n_final, unexpected_exit_count=len(unexpected),
                sqlite_version=sqlite3.sqlite_version, python_version=sys.version.split()[0],
                kills=kills, signals=sigs, signals_while_waiting=agg["sig_in_wait"],
                sig_latency_max=round(max(sig_lat), 3) if sig_lat else None,
                sig_latency_avg=round(sum(sig_lat) / len(sig_lat), 3) if sig_lat else None,
                lat_p50_ms=round(1000 * sorted(l[1] for l in lats)[len(lats) // 2], 2) if lats else None,
                lat_p99_ms=round(1000 * max(l[2] for l in lats), 2) if lats else None,
                lat_max_ms=round(1000 * max(l[3] for l in lats), 2) if lats else None,
                xcheckfs=dict(samples=xc["samples"], max_lock_waiters=xc["max_lock_waiters"],
                              max_mismatches=xc["max_mismatches"], ctl_timeouts=xc["ctl_timeouts"],
                              final={k: (xc["last"] or {}).get(k) for k in
                                     ("ops", "mismatches", "lock_waiters", "state", "verifications",
                                      "secondary_skipped", "resyncs", "bytes_read", "bytes_written", "open_files")}),
                stuck=stuck, events=events[:60], unexpected_exits=unexpected, leftover_files=left,
                db_info=infos, problems=problems, ok=not problems)
    if a.out:
        with open(a.out, "w") as f:
            json.dump(summ, f, indent=1)
    print("RESULT " + json.dumps({k: v for k, v in summ.items() if k not in ("db_info", "events", "stuck")}), flush=True)
    for p in problems:
        print("PROBLEM: " + p, flush=True)
    for e in events[:30]:
        print("event: " + e, flush=True)
    return 0 if not problems else 1


def check_main(a):
    rc = 0
    out = []
    paths = []
    for p in a.paths:
        paths += sorted(glob.glob(p)) or [p]
    for p in paths:
        if not os.path.exists(p):
            print("%s: missing" % p)
            rc = 1
            continue
        try:
            probs, info = full_check(p, "wal")
        except Exception as e:  # noqa: BLE001
            probs, info = ["raised %r" % (e,)], {"path": p}
        info["problems"] = probs
        out.append(info)
        print("%s: integrity=%s rows=%s digest=%s pages=%s %s" % (
            p, info.get("integrity_check"), info.get("history_rows"), (info.get("digest") or "")[:16],
            info.get("pagecount"), "OK" if not probs else "PROBLEMS " + "; ".join(probs)), flush=True)
        if probs:
            rc = 1
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=1)
    return rc


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--dir", required=True)
    r.add_argument("--mode", default="wal", choices=["wal", "wal-mmap", "wal-full", "delete", "truncate", "persist"])
    r.add_argument("--scenario", default="normal", choices=["normal", "multidb", "kill", "signal"])
    r.add_argument("--procs", type=int, default=12)
    r.add_argument("--dbs", type=int, default=1)
    r.add_argument("--duration", type=float, default=120)
    r.add_argument("--hold-ms", type=float, default=0)
    r.add_argument("--grace", type=float, default=90)
    r.add_argument("--seed", type=int, default=1)
    r.add_argument("--label", default="")
    r.add_argument("--status-cmd", default="")
    r.add_argument("--out", default="")
    r.add_argument("--state", default="/dev/shm/xcw")
    c = sub.add_parser("check")
    c.add_argument("paths", nargs="+")
    c.add_argument("--json", default="")
    w = sub.add_parser("worker")
    w.add_argument("--name", required=True)
    w.add_argument("--role", required=True)
    w.add_argument("--db", required=True)
    w.add_argument("--mode", required=True)
    w.add_argument("--end", type=float, required=True)
    w.add_argument("--hold-ms", type=float, default=0)
    w.add_argument("--state", required=True)
    w.add_argument("--seed", type=int, default=1)
    w.add_argument("--lockfile", default="")
    w.add_argument("--lk-kind", default="a")
    w.add_argument("--graceful", action="store_true")
    w.add_argument("--busy-ms", type=int, default=BUSY_MS)
    a = ap.parse_args()
    if a.cmd == "worker":
        worker_main(a)
        return 0
    if a.cmd == "check":
        return check_main(a)
    if a.scenario == "multidb" and a.dbs < 2:
        a.dbs = 5
    signal.signal(signal.SIGALRM, lambda *_: (_ for _ in ()).throw(TimeoutError("final check timed out")))
    return driver(a)


if __name__ == "__main__":
    sys.exit(main())
