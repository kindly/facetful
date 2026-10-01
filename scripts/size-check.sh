#!/usr/bin/env bash
# Size-budget gate: the wasm engine must stay within BUDGET_GZ bytes gzipped,
# and the lite build (no default features: no search()) within LITE_BUDGET_GZ.
# Runs wasm-opt -O3 when available (CI installs it; locally it degrades gracefully).
# The full build runs last, so it is the artifact left for build-package.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

BUDGET_GZ=${BUDGET_GZ:-512000}           # 500 KB (raised from 300 KB on 2026-09-30 for search)
LITE_BUDGET_GZ=${LITE_BUDGET_GZ:-307200} # 300 KB: the lite build keeps the old budget

WASM=target/wasm32-unknown-unknown/release/facetful_wasm.wasm
status=0

check() { # <label> <budget> <cargo feature flags…>
    local label=$1 budget=$2
    shift 2
    cargo build --release --target wasm32-unknown-unknown -p facetful-wasm "$@"
    local out=$WASM
    if command -v wasm-opt >/dev/null 2>&1; then
        # -O3, not -Oz: size-first reoptimization would undo the speed the
        # opt-level=3 build paid 15KB for. Feature flags must cover what rustc
        # emits (bulk memory + trunc_sat since LLVM 20 defaults).
        wasm-opt -O3 --enable-simd --enable-bulk-memory --enable-nontrapping-float-to-int \
            "$WASM" -o "$WASM.opt"
        out="$WASM.opt"
    else
        echo "note: wasm-opt not found — measuring unoptimized build (CI uses wasm-opt)"
        rm -f "$WASM.opt" # a stale optimized build would otherwise be packed instead of this one
    fi
    local raw gz
    raw=$(stat -c%s "$out")
    gz=$(gzip -9 -c "$out" | wc -c)
    echo "facetful-wasm ($label): raw ${raw} bytes, gzipped ${gz} bytes (budget ${budget})"
    if [ "$gz" -gt "$budget" ]; then
        echo "FAIL: $label gzipped size ${gz} exceeds budget ${budget}" >&2
        status=1
    else
        echo "OK: $label within budget ($((100 * gz / budget))% used)"
    fi
}

check lite "$LITE_BUDGET_GZ" --no-default-features
check full "$BUDGET_GZ"
exit $status
