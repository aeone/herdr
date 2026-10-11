"""Print a bench results table and flag regressions against the baseline.

    python3 perf/report.py RESULTS.tsv BASELINE_LABEL

A build regressed in a scenario when its total or main-thread CPU exceeds the
baseline's by more than 25% plus 3 points; the slack absorbs run-to-run noise,
which on a quiet machine is a few points. A handoff row is compared with the
baseline's idle row, since it measures the same fleet after the handoff.
Exits 1 if anything regressed or a handoff failed or lost panes.
"""
import csv
import sys

RATIO = 1.25
SLACK = 3

path, base = sys.argv[1], sys.argv[2]
with open(path) as f:
    rows = list(csv.DictReader(f, delimiter="\t"))

baseline = {r["scenario"]: r for r in rows if r["label"] == base}
cols = ["scenario", "label", "total_pct", "main_pct", "full_renders_per_s", "full_render_avg_ms", "note"]
heads = ["scenario", "build", "total %", "main %", "full renders/s", "render ms", "note"]
widths = [max(len(h), *(len(r[c]) for r in rows)) for h, c in zip(heads, cols)] if rows else [len(h) for h in heads]
print("  ".join(h.ljust(w) for h, w in zip(heads, widths)))
for r in rows:
    print("  ".join(r[c].ljust(w) for c, w in zip(cols, widths)))

problems = []
for r in rows:
    if r["label"] == base:
        continue
    if r["scenario"] == "handoff" and ("FAILED" in r["note"] or "CHANGED" in r["note"]):
        problems.append(f"handoff to {r['label']}: {r['note']}")
        continue
    ref = baseline.get("idle" if r["scenario"] == "handoff" else r["scenario"])
    if ref is None or r["total_pct"] == "-":
        continue
    for col, name in (("total_pct", "total"), ("main_pct", "main thread")):
        new, old = float(r[col]), float(ref[col])
        if new > old * RATIO + SLACK:
            problems.append(f"{r['scenario']}: {r['label']} {name} {new:.0f}% vs {base} {old:.0f}%")

print()
if problems:
    print("REGRESSIONS:")
    for p in problems:
        print(f"  {p}")
    sys.exit(1)
print(f"no regressions against {base}")
