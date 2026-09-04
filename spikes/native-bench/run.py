#!/usr/bin/env python3
"""Native engine-vs-engine benchmark: facetful vs SQLite vs DuckDB (threads=1
and default) on the same data and the same dialect-intersection queries.
Each engine times queries with its own facilities (in-process; no spawn noise):
facetful --bench, sqlite3/.timer, duckdb/.timer. 3 warmups, median of 10.
"""
import re
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
N = sys.argv[1] if len(sys.argv) > 1 else "1000000"
CSV = ROOT / f"spikes/facet-spike/data-{N}.csv"
IMG = ROOT / f"spikes/facet-spike/data-{N}.facetful"
QUERIES = Path(__file__).parent / "queries.sql"
WORK = Path(__file__).parent / "work"
WORK.mkdir(exist_ok=True)

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
    return {name: float(ms) for name, ms in (l.split("\t") for l in out.strip().splitlines())}


def bench_sqlite():
    db = WORK / f"bench-{N}.db"
    if not db.exists():
        setup = (
            "create table t (country TEXT, status TEXT, fuel TEXT, region TEXT, owner TEXT, "
            "year TEXT, capacity REAL, id INTEGER);\n"
            f".mode csv\n.import --skip 1 '{CSV}' t\n"
            "update t set capacity = NULL where capacity = '';\nanalyze;"
        )
        subprocess.run(["sqlite3", str(db)], input=setup, capture_output=True, text=True, check=True)
    script = ".timer on\n"
    for _, sql in blocks:
        script += (sql + ";\n") * REPS
    r = subprocess.run(["sqlite3", str(db)], input=script, capture_output=True, text=True, check=True)
    reals = [float(m) * 1000 for m in re.findall(r"Run Time: real ([\d.]+)", r.stdout + r.stderr)]
    res = {}
    for i, (name, _) in enumerate(blocks):
        res[name] = median_of_measured(reals[i * REPS:(i + 1) * REPS])
    return res


def bench_duckdb(threads):
    db = WORK / f"bench-{N}.duckdb"
    if not db.exists():
        setup = f"create table t as select * from read_csv_auto('{CSV}');"
        subprocess.run(["duckdb", str(db)], input=setup, capture_output=True, text=True, check=True)
    script = f"SET threads TO {threads};\n.timer on\n"
    for _, sql in blocks:
        script += (sql + ";\n") * REPS
    out = subprocess.run(["duckdb", str(db)], input=script, capture_output=True, text=True, check=True)
    text = out.stdout + out.stderr
    reals = [float(m) * 1000 for m in re.findall(r"real ([\d.]+)", text)]
    res = {}
    for i, (name, _) in enumerate(blocks):
        res[name] = median_of_measured(reals[i * REPS:(i + 1) * REPS])
    return res


engines = [
    ("facetful (native)", bench_facetful()),
    ("sqlite 3.53", bench_sqlite()),
    ("duckdb 1.5 (1 thread)", bench_duckdb(1)),
    ("duckdb 1.5 (16 threads)", bench_duckdb(16)),
]

names = [n for n, _ in blocks]
w = max(len(n) for n in names)
header = "| query".ljust(w + 2) + " | " + " | ".join(f"{e:>22}" for e, _ in engines) + " |"
print(header)
print("|" + "-" * (w + 1) + "|" + "|".join("-" * 24 for _ in engines) + "|")
for name in names:
    cells = " | ".join(f"{r.get(name, float('nan')):>19.2f} ms" for _, r in engines)
    print(f"| {name.ljust(w)} | {cells} |")
