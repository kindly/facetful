#!/usr/bin/env python3
"""Tabulate size-bench `cols` output: per column part, the stored form and
each encoding's saving after brotli and gzip.  summarize.py out/x-cols.tsv [min_kb]"""
import sys, csv, collections
rows = list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
min_kb = float(sys.argv[2]) if len(sys.argv) > 2 else 0
parts = collections.OrderedDict()
for r in rows:
    parts.setdefault((r["column"], r["kind"]), []).append(r)
tot = {"raw": 0, "br": 0, "gz": 0}; best = {"br": 0, "gz": 0}
out = []
for (col, kind), rs in parts.items():
    st = next(r for r in rs if r["variant"] == "stored")
    for k in tot: tot[k] += int(st[k])
    b = min(rs, key=lambda r: int(r["br"]))
    best["br"] += int(b["br"]); best["gz"] += min(int(r["gz"]) for r in rs)
    out.append((int(st["br"]), col, kind, st, rs))
out.sort(key=lambda x: -x[0])
kb = lambda n: f"{int(n)/1000:9.1f}"
print(f"stored total: raw {kb(tot['raw'])} KB, br {kb(tot['br'])} KB, gz {kb(tot['gz'])} KB; best per part: br {kb(best['br'])} gz {kb(best['gz'])}")
for sbr, col, kind, st, rs in out:
    if sbr / 1000 < min_kb: continue
    print(f"\n{col} [{kind}]  raw {kb(st['raw'])}  br {kb(st['br'])}  gz {kb(st['gz'])}")
    for r in sorted(rs, key=lambda r: int(r["br"])):
        if r is st: continue
        print(f"    {r['variant']:14} raw {kb(r['raw'])}  br {kb(r['br'])} ({(int(r['br'])-sbr)/1000:+8.1f})  gz {kb(r['gz'])} ({(int(r['gz'])-int(st['gz']))/1000:+8.1f})")
