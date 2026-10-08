#!/usr/bin/env python3
"""Build results.json (schema 1, see tests/apps/README.md) from the per-scenario output directories of
`run.sh ci`.  Stdlib only.  Never raises on missing or broken input: anything that is missing is a failure
recorded in the result, and the file is always written.  Exit status: 0 if every run is ok, else 1.
"""
import argparse
import json
import os
import re
import sys


def load(path, default=None):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:  # noqa: BLE001
        return default


def read(path):
    try:
        with open(path, errors="replace") as f:
            return f.read()
    except Exception:  # noqa: BLE001
        return ""


def is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def add_status(total, st):
    """Sum the numeric fields of one `ctl status` object into total; non-numeric ones keep the last value."""
    for k, v in st.items():
        if is_num(v):
            total[k] = round(total.get(k, 0) + v, 3) if isinstance(v, float) or isinstance(total.get(k), float) \
                else total.get(k, 0) + v
        else:
            total[k] = v


def scenario(cdir, cfg):
    """Everything known about one scenario directory."""
    r = load(os.path.join(cdir, "result.json"), {}) or {}
    rc = load(os.path.join(cdir, "rc.json"), {}) or {}
    return dict(dir=cdir, res=r, rc=rc, fatal=read(os.path.join(cdir, "fatal.txt")).strip(),
                status=load(os.path.join(cdir, "status-final.json")),
                stats=load(os.path.join(cdir, "stats-final.json")),
                verify=read(os.path.join(cdir, "verify.txt")).strip(),
                xclog=read(os.path.join(cdir, "xc.log")))


def hang_count(s):
    r = s["res"]
    n = len(r.get("stuck") or [])
    if s["rc"].get("workload_rc") == 124:  # `timeout` killed the workload driver
        n += 1
    return n


def errors_of(s):
    r = s["res"]
    if not r:
        return 1
    n = (r.get("violations", 0) + r.get("errors", 0) + hang_count(s) + r.get("unexpected_exit_count", 0)
         + r.get("final_problems", 0))
    if n == 0 and not r.get("ok", False):
        n = 1
    return n


def invariants_ok(s):
    r = s["res"]
    if not r:
        return False
    # problems are strings; "N stuck worker report(s)" belongs to the hang check
    return all(re.match(r"\d+ stuck worker", p) for p in r.get("problems", []))


def run_config(args, cfg, entries):
    base = cfg[:-7] if cfg.endswith("-strict") else cfg
    strict = cfg.endswith("-strict")
    scs = []
    for e in entries:
        sc, _, mode = e.partition("/")
        scs.append((e, scenario(os.path.join(args.out, "%s-%s-%s" % (cfg, sc, mode)), cfg)))
    xc_args = ""
    if base != "baseline":
        xc_args = "--check %s -m log --threads 8" % base + (" --serialize strict" if strict else "")
        if args.mount_args:
            xc_args += " " + args.mount_args
    checks = []

    def check(name, ok, detail=""):
        checks.append(dict(name=name, ok=bool(ok), detail=detail))

    workloads = []
    for name, s in scs:
        r = s["res"]
        w = dict(name=name, unit="tx/s", throughput=r.get("commits_per_s", 0), count=r.get("commits", 0),
                 errors=errors_of(s))
        lat = r.get("tx_latency_ms")
        if lat:
            w["latency_ms"] = {k: lat[k] for k in ("avg", "p50", "p95", "p99", "max")}
        workloads.append(w)

    def per_scenario_detail(bad):
        return "failed in: " + ", ".join(bad) if bad else ""

    fatals = ["%s (%s)" % (n, s["fatal"] or "no result.json, see workload.log") for n, s in scs
              if s["fatal"] or not s["res"]]
    bad_inv = [n for n, s in scs if not invariants_ok(s)]
    check("invariants", not bad_inv,
          per_scenario_detail(bad_inv) if bad_inv else "%d scenario(s), %d transactions committed" % (
              len(scs), sum(s["res"].get("commits", 0) for _, s in scs)))
    bad_hang = [n for n, s in scs if hang_count(s) or not s["res"]]
    check("no hangs", not bad_hang, per_scenario_detail(bad_hang))
    if fatals:
        check("scenarios ran", False, "; ".join(fatals))
    sec_dirs = {"secondary check": ("secondary_rc", "check-secondary.txt"),
                "primary check": ("primary_rc", "check-primary.txt")}
    if base != "baseline":
        bad = [n for n, s in scs if s["rc"].get("verify_rc") != "0"]
        diffs = [s["verify"].splitlines()[-1] if s["verify"] else "no output" for _, s in scs]
        check("xcheckfs verify", not bad,
              per_scenario_detail(bad) if bad else "; ".join(sorted(set(d.split(":", 1)[-1].strip() for d in diffs))))
        for cname in ("secondary check", "primary check"):
            key, f = sec_dirs[cname]
            bad = [n for n, s in scs if s["rc"].get(key) != "0"]
            check(cname, not bad, per_scenario_detail(bad) if bad else "workload.py check: integrity, sums, history")
        bad = [n for n, s in scs if s["rc"].get("digest_eq") != "1"]
        check("digests equal", not bad, per_scenario_detail(bad) if bad else "logical content of the databases is identical")
        mm = [(n, (s["status"] or {}).get("mismatches")) for n, s in scs]
        bad = ["%s=%s" % (n, m) for n, m in mm if m != 0]
        check("no mismatches", not bad, ", ".join(bad) if bad else "0 mismatches")
    else:
        bad = [n for n, s in scs if s["rc"].get("primary_rc") != "0"]
        check("primary check", not bad, per_scenario_detail(bad) if bad else "workload.py check: integrity, sums, history")

    # locks: informational, ok unless the scenarios that run lockers did no lock work at all
    dead = sum(s["res"].get("deadlocks_edeadlk", 0) for _, s in scs)
    lock_ops = sum(s["res"].get("lock_ops_ok", 0) for _, s in scs)
    waiters = max([(s["res"].get("xcheckfs") or {}).get("max_lock_waiters", 0) for _, s in scs] or [0])
    setlk = errno_setlk = 0
    for _, s in scs:
        for op in (s["stats"] or {}).get("ops", []):
            if op.get("op") in ("setlk", "setlkw"):
                setlk += op.get("count", 0)
                errno_setlk += op.get("errno_results", 0)
    locker_scs = [n for n, s in scs if s["res"].get("scenario") in ("kill", "signal")]
    no_lock = [n for n, s in scs if n in locker_scs and s["res"].get("lock_ops_ok", 0) == 0]
    detail = "EDEADLK seen by lockers %d, lock operations %d, max lock_waiters %d" % (dead, lock_ops, waiters)
    if base != "baseline":
        detail += ", setlk %d (%d with errno)" % (setlk, errno_setlk)
    if no_lock:
        detail += "; no lock operations completed in " + ", ".join(no_lock)
    check("lock stats", not no_lock, detail)

    xcheckfs = None
    if base != "baseline":
        xcheckfs = {}
        for _, s in scs:
            if s["status"]:
                add_status(xcheckfs, s["status"])
        if not xcheckfs:
            xcheckfs = None
    return dict(config=cfg, xcheckfs_args=xc_args, ok=all(c["ok"] for c in checks) and
                all(not w["errors"] for w in workloads),
                wall_s=float(sum(s["rc"].get("wall_s", 0) for _, s in scs)), workloads=workloads,
                xcheckfs=xcheckfs, checks=checks)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--configs", required=True)
    ap.add_argument("--scenarios", required=True)
    ap.add_argument("--duration", type=float, default=0)
    ap.add_argument("--procs", type=int, default=0)
    ap.add_argument("--image", default="")
    ap.add_argument("--cpus", default="")
    ap.add_argument("--mount-args", default="")
    args = ap.parse_args()
    configs = args.configs.replace(",", " ").split()
    entries = args.scenarios.replace(",", " ").split()
    runs = []
    for cfg in configs:
        try:
            runs.append(run_config(args, cfg, entries))
        except Exception as e:  # noqa: BLE001
            runs.append(dict(config=cfg, xcheckfs_args="", ok=False, wall_s=0.0, workloads=[], xcheckfs=None,
                             checks=[dict(name="results", ok=False, detail="cannot build results: %r" % (e,))]))
    fatal = read(os.path.join(args.out, "fatal.txt")).strip()
    if fatal:  # the harness could not even start: one check per run instead of a pile of consequential ones
        for r in runs:
            r["ok"] = False
            r["checks"] = [dict(name="environment", ok=False, detail=fatal)]
    ver = ""
    for cfg in configs:
        for e in entries:
            sc, _, mode = e.partition("/")
            r = load(os.path.join(args.out, "%s-%s-%s" % (cfg, sc, mode), "result.json"), {}) or {}
            if r.get("sqlite_version"):
                ver = r["sqlite_version"]
                break
        if ver:
            break
    out = dict(schema=1, app="sqlite",
               title="SQLite %s, multi-process bank workload" % ver if ver else "SQLite, multi-process bank workload",
               params=dict(procs=args.procs, duration_s=args.duration, scenarios=entries, image=args.image,
                           cpus=args.cpus),
               runs=runs)
    with open(os.path.join(args.out, "results.json"), "w") as f:
        json.dump(out, f, indent=2)
        f.write("\n")
    for r in runs:
        print("%-16s %-4s wall %5.0fs  %s" % (r["config"], "ok" if r["ok"] else "FAIL", r["wall_s"],
              ", ".join("%s %.0f" % (w["name"], w["throughput"]) for w in r["workloads"])))
        for c in r["checks"]:
            if not c["ok"]:
                print("    FAILED check %s: %s" % (c["name"], c["detail"]))
    return 0 if runs and all(r["ok"] for r in runs) else 1


if __name__ == "__main__":
    sys.exit(main())
