import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
const dir = process.env.BENCH_DIR;
if (!dir) { console.error('BENCH_DIR not set; run via scripts/spike.sh'); process.exit(1); }
const wasm = createRequire(import.meta.url)(`${dir}/bench.js`);

function bench(fn, iters) {
  for (let i = 0; i < 30; i++) fn();
  const s = [];
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    fn();
    s.push(Number(process.hrtime.bigint() - t0) / 1e6);
  }
  s.sort((a, b) => a - b);
  return { p50: s[Math.floor(s.length * 0.5)], p99: s[Math.floor(s.length * 0.99)] };
}
const us = (ms) => (ms * 1000).toFixed(1);

console.log('NESTING DEPTH vs COST — 200 posts, identical bodies, only tree shape varies\n');
console.log('profile    mean  max   pathB   page KB |  render p50   parse p50   build p50   sort p50   descend p50');
console.log('                                        |  (ms)        (us)        (us)        (us)       (us)');
for (const prof of ['flat', 'shallow', 'mixed', 'deep']) {
  wasm.load(readFileSync(`${dir}/fixture_prof_${prof}.json`, 'utf8'));
  const [mean, max] = wasm.depth_stats();
  const pb = wasm.path_bytes();
  const kb = wasm.page_bytes() / 1024;
  const r = bench(() => wasm.render_read_path(), 400);
  const pa = bench(() => wasm.bench_path_parse(), 400);
  const bu = bench(() => wasm.bench_path_build(), 400);
  const so = bench(() => wasm.bench_path_sort(), 400);
  const de = bench(() => wasm.bench_path_descendant(), 400);
  console.log(
    `${prof.padEnd(9)} ${mean.toFixed(2).padStart(5)} ${String(max).padStart(4)} ${String(pb).padStart(7)} ` +
    `${kb.toFixed(1).padStart(9)} | ${r.p50.toFixed(3).padStart(10)}  ${us(pa.p50).padStart(10)}  ` +
    `${us(bu.p50).padStart(10)}  ${us(so.p50).padStart(9)}  ${us(de.p50).padStart(10)}`
  );
}
console.log('\npathB = total bytes of materialized-path text across all 200 posts.');
console.log('parse/build/sort/descend are for ALL 200 posts, in microseconds.');
