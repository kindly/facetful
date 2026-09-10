#!/usr/bin/env bash
# Build the npm package: wasm (optimized) + JS + types -> js/facetful, npm pack.
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/size-check.sh   # builds + wasm-opt (when available) + enforces budget

WASM=target/wasm32-unknown-unknown/release/facetful_wasm.wasm
[ -f "$WASM.opt" ] && WASM="$WASM.opt"
cp "$WASM" js/facetful/facetful_wasm.wasm

node js/facetful/node-smoke.mjs
node js/facetful/node-parquet-diff.mjs

mkdir -p dist
(cd js/facetful && npm pack --cache "${TMPDIR:-/tmp}/npm-cache" --pack-destination ../../dist)
echo "packed:"
ls -la dist/*.tgz | tail -1
