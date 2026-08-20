#!/usr/bin/env bash
# Reproduce the M0 measurements end to end. See docs/M0-findings.md for what the numbers mean.
#
#   ./scripts/spike.sh          # build, seed, serve, measure
#   ./scripts/spike.sh bench    # CPU benchmark only (no wrangler, no D1)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

WRANGLER="${WRANGLER:-npx --yes wrangler@4}"
BENCH_OUT="${BENCH_OUT:-$ROOT/target/bench}"
POSTS="${POSTS:-200}"

have() { command -v "$1" >/dev/null 2>&1; }

wasm_bindgen_bin() {
  # worker-build caches a matching wasm-bindgen; reuse it rather than installing a second copy.
  local cached
  cached="$(find "${HOME}/.cache/worker-build" -name wasm-bindgen -type f 2>/dev/null | head -1)"
  if [[ -n "$cached" ]]; then echo "$cached"; return; fi
  if have wasm-bindgen; then command -v wasm-bindgen; return; fi
  echo "wasm-bindgen not found. Run 'cargo install worker-build' first." >&2
  exit 1
}

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

bench() {
  step "Building the CPU benchmark for wasm32"
  cargo build --release --target wasm32-unknown-unknown -p notespace-bench
  mkdir -p "$BENCH_OUT"
  "$(wasm_bindgen_bin)" target/wasm32-unknown-unknown/release/notespace_bench.wasm \
    --out-dir "$BENCH_OUT" --target nodejs --no-typescript --out-name bench

  step "Generating fixtures"
  for n in 50 200 500 1000 2000; do
    cargo run -q -p notespace-seed -- "$n" json > "$BENCH_OUT/fixture_$n.json"
  done

  step "Measuring render CPU in wasm under V8"
  BENCH_DIR="$BENCH_OUT" node "$ROOT/scripts/bench.mjs"
}

if [[ "${1:-all}" == "bench" ]]; then bench; exit 0; fi

step "Unit tests (core + render)"
cargo test

step "Building the Worker"
(cd crates/worker && worker-build --release)
WASM=crates/worker/build/index_bg.wasm
printf 'wasm:        %8.1f KB (%.1f KB gzipped)\n' \
  "$(stat -c%s "$WASM" | awk '{print $1/1024}')" \
  "$(gzip -c "$WASM" | wc -c | awk '{print $1/1024}')"

step "Applying migrations + seed to local D1"
$WRANGLER d1 execute notespace --local --file=migrations/0001_init.sql >/dev/null
cargo run -q -p notespace-seed -- "$POSTS" sql > seed.sql
$WRANGLER d1 execute notespace --local --file=seed.sql >/dev/null
echo "seeded $POSTS posts"

step "Query plan for the thread-page read (must be an index range scan)"
$WRANGLER d1 execute notespace --local --json --command="EXPLAIN QUERY PLAN \
  SELECT p.id FROM post p JOIN user u ON u.id=p.author_id \
  WHERE p.thread_id=1 AND p.path>'' ORDER BY p.path LIMIT 201" \
  | python3 -c "import json,sys; [print('   ', r['detail']) for r in json.load(sys.stdin)[0]['results']]"

bench

step "Done. Start the worker with:  $WRANGLER dev --local"
