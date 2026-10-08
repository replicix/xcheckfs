#!/usr/bin/env python3
"""Render application benchmark results (results.json, see README.md) as Markdown.

    report.py RESULTS... [--base RESULTS...] [--meta TEXT] [--fail-on-error]

RESULTS are results.json files or directories searched for them. With --base,
every throughput is also compared with the same application, configuration
and workload in the base results (CI: the latest successful run on main).
--fail-on-error exits 1 when any run failed (the report is printed anyway).
"""

import argparse
import json
import math
import sys
from pathlib import Path

MARKER = "<!-- xcheckfs-app-benchmarks -->"
CONFIG_ORDER = ["baseline", "basic", "basic-strict", "thorough", "thorough-strict", "paranoid", "paranoid-strict"]
LAT_KEYS = ["avg", "p50", "p95", "p99", "max"]
# Changes against the base smaller than this are runner noise.
NOISE = 0.15


def load(paths):
    """{app: results} from files or directories (a later file for the same app wins)."""
    out = {}
    for p in map(Path, paths):
        files = sorted(p.rglob("results.json")) if p.is_dir() else [p]
        for f in files:
            try:
                r = json.loads(f.read_text())
            except (OSError, ValueError) as e:
                print(f"report.py: skipping {f}: {e}", file=sys.stderr)
                continue
            if isinstance(r, dict) and r.get("schema") == 1 and r.get("app"):
                out[r["app"]] = r
    return out


def config_key(c):
    return (CONFIG_ORDER.index(c), c) if c in CONFIG_ORDER else (len(CONFIG_ORDER), c)


def configs(res):
    return sorted({r["config"] for r in res.get("runs", [])}, key=config_key)


def run_of(res, config):
    for r in (res or {}).get("runs", []):
        if r.get("config") == config:
            return r
    return None


def workload_of(run, name):
    for w in (run or {}).get("workloads", []):
        if w.get("name") == name:
            return w
    return None


def workload_names(res):
    names = []
    for r in res.get("runs", []):
        for w in r.get("workloads", []):
            if w.get("name") not in names:
                names.append(w.get("name"))
    return names


def num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


def fmt(v):
    """A measurement, 3-4 significant digits."""
    if not num(v):
        return "–"
    a = abs(v)
    if a >= 1000:
        return f"{v:,.0f}"
    if a >= 100:
        return f"{v:.0f}"
    if a >= 10:
        return f"{v:.1f}"
    return f"{v:.3g}"


def compact(v):
    """A counter: 950, 12.3k, 4.56M."""
    if not num(v):
        return "–"
    for div, suffix in ((1e9, "G"), (1e6, "M"), (1e3, "k")):
        if abs(v) >= div:
            return f"{v / div:.3g}{suffix}"
    return f"{v:g}"


def size(v):
    if not num(v):
        return "–"
    for div, suffix in ((1 << 40, "TiB"), (1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")):
        if v >= div:
            return f"{v / div:.3g} {suffix}"
    return f"{v:g} B"


def duration(s):
    if not num(s):
        return "–"
    s = int(round(s))
    return f"{s // 60}m {s % 60:02d}s" if s >= 60 else f"{s}s"


def ratio(v, ref):
    return v / ref if num(v) and num(ref) and ref > 0 else None


def change(v, ref):
    """' ▲12%' against the base value, empty within the noise band."""
    r = ratio(v, ref)
    if r is None:
        return ""
    pct = (r - 1) * 100
    if abs(r - 1) < NOISE:
        return f" <sub>±{abs(pct):.0f}%</sub>"
    return f" **{'▲' if pct > 0 else '▼'}{abs(pct):.0f}%**"


def geomean(xs):
    xs = [x for x in xs if x is not None and x > 0]
    return math.exp(sum(map(math.log, xs)) / len(xs)) if xs else None


def overhead(res, config):
    """Geometric mean over the workloads of throughput(config) / throughput(baseline)."""
    base, run = run_of(res, "baseline"), run_of(res, config)
    if not base or not run:
        return None
    return geomean(
        ratio(w.get("throughput"), (workload_of(base, w.get("name")) or {}).get("throughput"))
        for w in run.get("workloads", [])
    )


def failed_checks(run):
    return [c for c in run.get("checks", []) if not c.get("ok")]


def run_ok(run):
    return bool(run.get("ok")) and not failed_checks(run)


def esc(s):
    return str(s).replace("|", "\\|").replace("\n", " ")


def table(header, rows, align=None):
    align = align or ["---"] + ["--:"] * (len(header) - 1)
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(align) + "|"]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(lines)


def overview(results, base):
    cols = sorted({c for res in results.values() for c in configs(res) if c != "baseline"}, key=config_key)
    rows = []
    for app, res in results.items():
        row = [esc(res.get("title") or app)]
        for c in cols:
            o = overhead(res, c)
            cell = "–" if o is None else f"{o:.2f}×"
            if o is not None and app in base:
                b = overhead(base[app], c)
                if b is not None:
                    cell += f" <sub>(main {b:.2f}×)</sub>"
            row.append(cell)
        runs = res.get("runs", [])
        mism = sum((r.get("xcheckfs") or {}).get("mismatches", 0) or 0 for r in runs)
        checks = [c for r in runs for c in r.get("checks", [])]
        good = sum(1 for c in checks if c.get("ok"))
        bad_runs = [r["config"] for r in runs if not run_ok(r)]
        row.append(("✅ 0" if mism == 0 else f"❌ {mism}"))
        row.append(f"✅ {good}/{len(checks)}" if not bad_runs else f"❌ {good}/{len(checks)} ({', '.join(bad_runs)})")
        rows.append(row)
    header = ["Application"] + [f"{c} vs baseline" for c in cols] + ["Mismatches", "Checks"]
    return table(header, rows, ["---"] + ["--:"] * len(cols) + [":-:", ":-:"])


def app_section(app, res, base_res):
    out = []
    params = res.get("params") or {}
    title = esc(res.get("title") or app)
    out.append(f"### {title}")
    if params:
        out.append("<sub>" + " · ".join(f"{esc(k)} {esc(v)}" for k, v in params.items()) + "</sub>")
    cfgs = configs(res)
    names = workload_names(res)
    base_run = run_of(res, "baseline")

    # Throughput, relative to the baseline and to main.
    rows = []
    for n in names:
        unit = next((w.get("unit") for r in res["runs"] if (w := workload_of(r, n)) and w.get("unit")), "")
        row = [f"{esc(n)} <sub>{esc(unit)}</sub>" if unit else esc(n)]
        for c in cfgs:
            w = workload_of(run_of(res, c), n)
            if not w:
                row.append("–")
                continue
            v = w.get("throughput")
            cell = fmt(v)
            if c != "baseline":
                r = ratio(v, (workload_of(base_run, n) or {}).get("throughput"))
                if r is not None:
                    cell += f" <sub>{r:.2f}×</sub>"
            if base_res:
                cell += change(v, (workload_of(run_of(base_res, c), n) or {}).get("throughput"))
            if w.get("errors"):
                cell += f" <sub>⚠️ {compact(w['errors'])} err</sub>"
            row.append(cell)
        rows.append(row)
    if rows:
        out.append(table(["Throughput"] + cfgs, rows))

    # Latency percentiles, the fields the tool reports.
    keys = [k for k in LAT_KEYS if any(k in (w.get("latency_ms") or {}) for r in res["runs"] for w in r.get("workloads", []))]
    if keys:
        rows = []
        for n in names:
            row = [esc(n)]
            for c in cfgs:
                lat = (workload_of(run_of(res, c), n) or {}).get("latency_ms") or {}
                row.append(" / ".join(fmt(lat.get(k)) for k in keys) if lat else "–")
            rows.append(row)
        out.append(f"<details><summary>Latency (ms): {' / '.join(keys)}</summary>\n\n" + table(["Workload"] + cfgs, rows) + "\n\n</details>")

    # xcheckfs counters.
    rows = []
    for c in cfgs:
        r = run_of(res, c)
        x = r.get("xcheckfs") or {}
        if not x and c == "baseline":
            continue
        ops, wall = x.get("ops"), r.get("wall_s")
        rows.append([
            c,
            duration(wall),
            compact(ops),
            ("✅ 0" if not x.get("mismatches") else f"❌ {x['mismatches']}"),
            compact(x.get("concurrent_data_ops")),
            compact(x.get("range_waits")),
            compact(x.get("attr_time_skipped")),
            compact(x.get("verifications")),
            compact(x.get("resyncs")),
            size(x.get("bytes_written")),
        ])
    if rows:
        out.append("<details><summary>xcheckfs counters</summary>\n\n" + table(
            ["Config", "Wall", "Ops", "Mismatches", "Concurrent data ops", "Range waits", "Racy stats", "Verifications", "Resyncs", "Written"],
            rows) + "\n\n<sub>Concurrent data ops: reads and writes that ran on a file while another was in flight on it "
            "(relaxed serialization). Racy stats: stat comparisons whose times were skipped because a write overlapped them.</sub>\n\n</details>")

    # Checks: failures in full, the rest folded.
    for c in cfgs:
        r = run_of(res, c)
        for chk in failed_checks(r) or ([] if r.get("ok") else [{"name": "run", "detail": "failed"}]):
            out.append(f"> ❌ **{esc(c)}**: {esc(chk.get('name'))}" + (f": {esc(chk['detail'])}" if chk.get("detail") else ""))
    rows = [
        [c, ("✅" if chk.get("ok") else "❌") + " " + esc(chk.get("name")), esc(chk.get("detail") or "")]
        for c in cfgs for chk in run_of(res, c).get("checks", [])
    ]
    if rows:
        out.append("<details><summary>Checks</summary>\n\n" + table(["Config", "Check", "Detail"], rows, ["---", "---", "---"]) + "\n\n</details>")
    return "\n\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("results", nargs="+")
    ap.add_argument("--base", nargs="*", default=[], help="results to compare with (e.g. the latest main run)")
    ap.add_argument("--meta", default="", help="a line under the heading (commit, runner, base)")
    ap.add_argument("--fail-on-error", action="store_true")
    args = ap.parse_args()

    results, base = load(args.results), load(args.base)
    if not results:
        print(f"{MARKER}\n## Application benchmarks\n\n❌ No results were produced (see the job logs).")
        return 1 if args.fail_on_error else 0
    bad = [f"{app}/{r['config']}" for app, res in results.items() for r in res.get("runs", []) if not run_ok(r)]
    n = sum(len(res.get("runs", [])) for res in results.values())
    status = f"✅ all {n} runs passed" if not bad else f"❌ {len(bad)} of {n} runs failed: {', '.join(bad)}"

    parts = [MARKER, f"## Application benchmarks: {status}"]
    if args.meta:
        parts.append(f"<sub>{args.meta}</sub>")
    parts.append(overview(results, base))
    parts.append(
        "<sub>Throughput through xcheckfs relative to the same workload on a plain volume (geometric mean over the "
        "workloads). Every configuration also checks that the two file systems end up identical (`xcheckfs verify`) and "
        f"that each is a consistent database on its own. Shared CI runners vary by about ±{NOISE * 100:.0f}%; smaller "
        "changes against main are shown in small print.</sub>"
    )
    for app, res in results.items():
        parts.append(app_section(app, res, base.get(app)))
    print("\n\n".join(parts))
    return 1 if bad and args.fail_on_error else 0


if __name__ == "__main__":
    sys.exit(main())
