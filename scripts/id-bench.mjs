// Compares two internal representations for PublicId: the shipping canonical `String`
// against a packed `u128 + width`. Run via scripts/spike.sh; BENCH_DIR holds bench.js.
import { createRequire } from 'node:module';
const dir = process.env.BENCH_DIR;
if (!dir) { console.error('BENCH_DIR not set; run via scripts/spike.sh'); process.exit(1); }
const wasm = createRequire(import.meta.url)(`${dir}/bench.js`);

const N = 2000;
const n = wasm.id_setup(N);
const bad = wasm.id_cross_check();
console.log(`\nPUBLIC ID REPRESENTATION: canonical String vs packed u128`);
console.log(`${n} ids at mixed widths (16/18/20/22/25).`);
console.log(`cross-check mismatches: ${bad}` + (bad === 0 ? '  (representations agree)' : '  <-- TIMINGS INVALID'));
if (bad !== 0) process.exit(1);

const [szString, szPacked] = wasm.id_sizes();
console.log(`size_of: String form ${szString} B (+ heap per id), packed form ${szPacked} B (no heap)\n`);

function bench(fn, iters = 300) {
  for (let i = 0; i < 30; i++) fn();
  const s = [];
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    fn();
    s.push(Number(process.hrtime.bigint() - t0) / 1e6);
  }
  s.sort((a, b) => a - b);
  return s[Math.floor(s.length * 0.5)];
}

const OPS = [
  ['parse, canonical (from D1)', wasm.id_parse_canonical_string, wasm.id_parse_canonical_packed],
  ['parse, messy (from URL)',    wasm.id_parse_messy_string,     wasm.id_parse_messy_packed],
  ['encode',                     wasm.id_encode_string,          wasm.id_encode_packed],
  ['timestamp_ms',               wasm.id_timestamp_string,       wasm.id_timestamp_packed],
  ['sort',                       wasm.id_sort_string,            wasm.id_sort_packed],
  ['page mix, .encode() x4',     wasm.id_page_mix_string,        wasm.id_page_mix_packed],
  ['page mix, as template runs',  wasm.id_page_mix_string_display, wasm.id_page_mix_packed],
];

console.log(`per ${n} ids, p50:\n`);
console.log('operation                       String (ms)   packed (ms)   packed is');
console.log('-'.repeat(74));
const rows = [];
for (const [label, fa, fb] of OPS) {
  const a = bench(fa), b = bench(fb);
  const ratio = a / b;
  const verdict = ratio > 1.05 ? `${ratio.toFixed(2)}x faster`
                : ratio < 0.95 ? `${(1 / ratio).toFixed(2)}x SLOWER`
                : 'about the same';
  rows.push([label, a, b]);
  console.log(`${label.padEnd(30)} ${a.toFixed(4).padStart(11)}   ${b.toFixed(4).padStart(11)}   ${verdict}`);
}

// Scale to what a single request actually does, rather than a batch of 2000.
const [, mixA, mixB] = rows[rows.length - 1];
console.log(`\nPer single request (one thread page does the "page mix" once, not ${n} times):`);
console.log(`  String form: ${((mixA / n) * 1000).toFixed(3)} us`);
console.log(`  packed form: ${((mixB / n) * 1000).toFixed(3)} us`);
console.log(`  difference:  ${(Math.abs(mixA - mixB) / n * 1000).toFixed(3)} us  ` +
            `= ${((Math.abs(mixA - mixB) / n) / 10 * 100).toFixed(5)}% of the 10 ms CPU budget`);
