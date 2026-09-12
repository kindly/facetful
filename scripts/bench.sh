#!/usr/bin/env bash
# Performance regression gate: run the canonical suite, compare to a baseline.
#
#   ./scripts/bench.sh [data.facetful]        run + compare vs bench/baseline.tsv
#   ./scripts/bench.sh --save-baseline [data] adopt the current numbers as baseline
#
# Baselines are machine-specific and stay untracked (see .gitignore). A query
# regresses when cold or warm exceeds baseline * THRESHOLD (default 1.5) and
# the absolute slowdown is over 0.3 ms (guards against noise on sub-ms medians).
set -euo pipefail
cd "$(dirname "$0")/.."

THRESHOLD=${THRESHOLD:-1.5}
SAVE=0
[ "${1:-}" = "--save-baseline" ] && { SAVE=1; shift; }
DATA=${1:-data/units-2026-08.facetful}
[ -f "$DATA" ] || { echo "no data file: $DATA" >&2; exit 1; }

cargo build --release -p facetful-cli --quiet
mkdir -p bench
./target/release/facetful query "$DATA" --bench bench/queries.sql > bench/results.tsv
column -t bench/results.tsv

if [ "$SAVE" = 1 ]; then
    cp bench/results.tsv bench/baseline.tsv
    echo "baseline saved: bench/baseline.tsv"
    exit 0
fi

[ -f bench/baseline.tsv ] || {
    echo "no baseline — run ./scripts/bench.sh --save-baseline to adopt these numbers"
    exit 0
}

awk -F'\t' -v thr="$THRESHOLD" '
    NR==FNR { if ($0 !~ /^#/) { bc[$1]=$2; bw[$1]=$3 } next }
    /^#/ { next }
    {
        fail = 0
        if ($1 in bc) {
            if ($2 > bc[$1]*thr && $2-bc[$1] > 0.3) { printf "REGRESSION %s cold: %.2f -> %.2f ms\n", $1, bc[$1], $2; fail=1 }
            if ($3 > bw[$1]*thr && $3-bw[$1] > 0.3) { printf "REGRESSION %s warm: %.2f -> %.2f ms\n", $1, bw[$1], $3; fail=1 }
        } else printf "note: %s not in baseline (new query?)\n", $1
        bad += fail
    }
    END { if (bad) { printf "%d quer%s regressed (threshold %.1fx)\n", bad, bad==1?"y":"ies", thr; exit 1 }
          print "OK: no regressions vs baseline" }
' bench/baseline.tsv bench/results.tsv
