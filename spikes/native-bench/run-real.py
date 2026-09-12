#!/usr/bin/env python3
"""Rival benchmark on the real dataset: facetful (cold/warm) vs SQLite vs
DuckDB (1 thread and default) over the canonical bench/queries.sql suite.

Prep (once): a typed duckdb table and a typed sqlite table built from the CSV
(see WORK paths below). Dialect tweaks for duckdb: LIKE -> ILIKE (facetful and
sqlite are ASCII-case-insensitive), `as int` -> `as bigint` (int is 32-bit
there). Timing: each engine's own timer, 3 warmups, median of 10.
"""
import os
import re
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
IMG = ROOT / "data/units-2026-08.facetful"
QUERIES = ROOT / "bench/queries.sql"
WORK = Path(os.environ.get("RIVALS_WORK", Path(__file__).parent / "work-real"))

blocks = []
for chunk in QUERIES.read_text().split("#")[1:]:
    name, _, sql = chunk.partition("\n")
    if sql.strip():
        blocks.append((name.strip(), " ".join(sql.split())))

REPS = 13  # 3 warmup + 10 measured


def median_of_measured(times):
    return statistics.median(times[3:]) if len(times) > 3 else statistics.median(times)


def bench_facetful():
    out = subprocess.run(
        [str(ROOT / "target/release/facetful"), "query", str(IMG), "--bench", str(QUERIES)],
        capture_output=True, text=True, check=True,
    ).stdout
    cold, warm = {}, {}
    for line in out.strip().splitlines():
        if line.startswith("#"):
            continue
        name, c, w = line.split("\t")
        cold[name], warm[name] = float(c), float(w)
    return cold, warm


def timer_lane(cmd, prelude, sql_tweak):
    script = prelude + ".timer on\n"
    for _, sql in blocks:
        script += (sql_tweak(sql) + ";\n") * REPS
    out = subprocess.run(cmd, input=script, capture_output=True, text=True, check=True)
    reals = [float(m) * 1000 for m in re.findall(r"Run Time.*?real ([\d.]+)", out.stdout + out.stderr)]
    assert len(reals) == len(blocks) * REPS, f"expected {len(blocks)*REPS} timings, got {len(reals)}"
    return {
        name: median_of_measured(reals[i * REPS:(i + 1) * REPS])
        for i, (name, _) in enumerate(blocks)
    }


def duck_sql(sql):
    return sql.replace(" like ", " ilike ").replace("as int)", "as bigint)")


engines = [("facetful cold", None), ("facetful warm", None)]
fc, fw = bench_facetful()
results = [fc, fw]

results.append(timer_lane(["sqlite3", str(WORK / "units.db")], "", lambda s: s))
engines.append(("sqlite", None))

for threads, label in [(1, "duckdb@1"), (0, "duckdb")]:
    prelude = f"SET threads TO {threads};\n" if threads else ""
    results.append(timer_lane(["duckdb", str(WORK / "units.duckdb")], prelude, duck_sql))
    engines.append((label, None))

names = [n for n, _ in blocks]
labels = [e for e, _ in engines]
w = max(len(n) for n in names)
print("| " + "query".ljust(w) + " | " + " | ".join(f"{l:>13}" for l in labels) + " |")
print("|" + "-" * (w + 2) + "|" + "|".join("-" * 15 for _ in labels) + "|")
for name in names:
    cells = " | ".join(f"{r.get(name, float('nan')):>10.2f} ms" for r in results)
    print(f"| {name.ljust(w)} | {cells} |")
