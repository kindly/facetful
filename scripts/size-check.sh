#!/usr/bin/env bash
# Size-budget gate: the wasm engine must stay within BUDGET_GZ bytes gzipped.
# Runs wasm-opt -Oz when available (CI installs it; locally it degrades gracefully).
set -euo pipefail
cd "$(dirname "$0")/.."

BUDGET_GZ=${BUDGET_GZ:-307200} # 300 KB

cargo build --release --target wasm32-unknown-unknown -p facetful-wasm
WASM=target/wasm32-unknown-unknown/release/facetful_wasm.wasm

if command -v wasm-opt >/dev/null 2>&1; then
    # -O3, not -Oz: size-first reoptimization would undo the speed the
    # opt-level=3 build paid 15KB for. Feature flags must cover what rustc
    # emits (bulk memory + trunc_sat since LLVM 20 defaults).
    wasm-opt -O3 --enable-simd --enable-bulk-memory --enable-nontrapping-float-to-int \
        "$WASM" -o "$WASM.opt"
    WASM="$WASM.opt"
else
    echo "note: wasm-opt not found — measuring unoptimized build (CI uses wasm-opt)"
fi

RAW=$(stat -c%s "$WASM")
GZ=$(gzip -9 -c "$WASM" | wc -c)
echo "facetful-wasm: raw ${RAW} bytes, gzipped ${GZ} bytes (budget ${BUDGET_GZ})"

if [ "$GZ" -gt "$BUDGET_GZ" ]; then
    echo "FAIL: gzipped size ${GZ} exceeds budget ${BUDGET_GZ}" >&2
    exit 1
fi
echo "OK: within budget ($((100 * GZ / BUDGET_GZ))% used)"
