// CPU benchmark for the notespace read/write paths, run against real wasm under V8.
// Invoked by scripts/spike.sh; BENCH_DIR must contain bench.js and the fixtures.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const dir = process.env.BENCH_DIR;
if (!dir) {
  console.error('BENCH_DIR not set; run via scripts/spike.sh');
  process.exit(1);
}
const wasm = createRequire(import.meta.url)(`${dir}/bench.js`);

const CPU_BUDGET_MS = 10; // Cloudflare Workers free plan, per request.

function bench(fn, iters) {
  for (let i = 0; i < 25; i++) fn(); // warm up past V8's tier-up
  const s = [];
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    fn();
    s.push(Number(process.hrtime.bigint() - t0) / 1e6);
  }
  s.sort((a, b) => a - b);
  return {
    min: s[0],
    p50: s[Math.floor(s.length * 0.5)],
    p99: s[Math.floor(s.length * 0.99)],
    max: s[s.length - 1],
  };
}

console.log(`\nnode ${process.version} · wasm via V8 · CPU budget ${CPU_BUDGET_MS} ms/request\n`);

console.log('READ PATH — assemble a thread page from pre-rendered post HTML');
console.log('posts   page KB     p50 ms     p99 ms    p99 as % of budget');
for (const n of [50, 200, 500, 1000, 2000]) {
  wasm.load(readFileSync(`${dir}/fixture_${n}.json`, 'utf8'));
  const kb = wasm.page_bytes() / 1024;
  const r = bench(() => wasm.render_read_path(), 400);
  console.log(
    `${String(n).padStart(5)}  ${kb.toFixed(1).padStart(9)}  ${r.p50.toFixed(3).padStart(9)}  ` +
    `${r.p99.toFixed(3).padStart(9)}    ${((r.p99 / CPU_BUDGET_MS) * 100).toFixed(2)}%`
  );
}

console.log('\nWRITE PATH — markdown -> sanitized HTML (paid once per post, at submit time)');
const n = wasm.load(readFileSync(`${dir}/fixture_200.json`, 'utf8'));
const w = bench(() => wasm.render_write_path(), 100);
console.log(`  ${n} posts in one batch:  p50 ${w.p50.toFixed(3)} ms   p99 ${w.p99.toFixed(3)} ms`);
console.log(`  per single post:         p50 ${(w.p50 / n).toFixed(4)} ms   p99 ${(w.p99 / n).toFixed(4)} ms`);
