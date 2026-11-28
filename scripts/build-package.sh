#!/usr/bin/env bash
# Build the npm package: wasm (optimized) + JS + types -> js/facetful, npm pack.
set -euo pipefail
cd "$(dirname "$0")/.."

./scripts/size-check.sh   # builds + wasm-opt (when available) + enforces budget

WASM=target/wasm32-unknown-unknown/release/facetful_wasm.wasm
# an .opt from an earlier build must not shadow a fresh raw build (it did once:
# the worker and browser smokes ran against a wasm without the new exports)
[ "$WASM.opt" -nt "$WASM" ] && WASM="$WASM.opt"
cp "$WASM" js/facetful/facetful_wasm.wasm
# the lite and sql builds and the command's own, kept by size-check.sh
# (already optimized when wasm-opt ran)
for b in lite sql cli; do
  cp target/wasm32-unknown-unknown/release/facetful_$b.wasm js/facetful/facetful_$b.wasm
done

node js/facetful/node-smoke.mjs
node js/facetful/node-parquet-diff.mjs
node js/facetful/node-worker-smoke.mjs   # worker.js through its message protocol
node js/facetful/browser-smoke.mjs       # the public API in headless Chromium: module worker, OPFS, Blob streaming
node js/facetful/node-builds-smoke.mjs   # lite and sql: what each runs, and what each refuses by name

# the command itself: a streamed conversion must reproduce the checked-in image,
# and a query with a ready-made function must run without flags
SPIKE=spikes/facet-spike
node js/facetful/bin/facetful.mjs convert $SPIKE/data-200000.csv "${TMPDIR:-/tmp}/gate.facetful" 2>/dev/null
cmp "${TMPDIR:-/tmp}/gate.facetful" $SPIKE/data-200000.facetful
node js/facetful/bin/facetful.mjs query "${TMPDIR:-/tmp}/gate.facetful" \
  "select count(*) as n from t where regexp(country, '^country_1[0-9]$')" | grep -q "^25947" || { echo "FAIL: facetful command"; exit 1; }
node js/facetful/bin/facetful.mjs inspect "${TMPDIR:-/tmp}/gate.facetful" | grep -q "^owner .*utf8/dict (u16 codes) .* 1981 values" || { echo "FAIL: facetful inspect"; exit 1; }
# packed and unpacked again it is the same image, and a packed file queries
node js/facetful/bin/facetful.mjs pack "${TMPDIR:-/tmp}/gate.facetful" "${TMPDIR:-/tmp}/gate.packed" 2>/dev/null
node js/facetful/bin/facetful.mjs unpack "${TMPDIR:-/tmp}/gate.packed" "${TMPDIR:-/tmp}/gate.back" 2>/dev/null
cmp "${TMPDIR:-/tmp}/gate.back" $SPIKE/data-200000.facetful
node js/facetful/bin/facetful.mjs query "${TMPDIR:-/tmp}/gate.packed" "select count(*) as n from t" | grep -q "^200000" || { echo "FAIL: facetful pack"; exit 1; }
rm -f "${TMPDIR:-/tmp}/gate.facetful" "${TMPDIR:-/tmp}/gate.packed" "${TMPDIR:-/tmp}/gate.back"
echo "facetful command: OK"

mkdir -p dist
(cd js/facetful && npm pack --cache "${TMPDIR:-/tmp}/npm-cache" --pack-destination ../../dist)
echo "packed:"
ls -la dist/*.tgz | tail -1
