import { createRequire } from 'node:module';
const wasm = createRequire(import.meta.url)(`${process.env.BENCH_DIR}/bench.js`);
const BUDGET = 10;
function bench(fn, n = 5) {
  fn(); const s = [];
  for (let i = 0; i < n; i++) { const t = process.hrtime.bigint(); fn(); s.push(Number(process.hrtime.bigint()-t)/1e6); }
  s.sort((a,b)=>a-b); return s[Math.floor(s.length/2)];
}
console.log('\nStrongest Argon2id that fits the 10 ms budget (m = memory KiB, t = passes)\n');
console.log('  m (KiB)     t    p50 ms   % of 10ms budget');
console.log('  ' + '-'.repeat(46));
const fits = [];
for (const m of [1024, 2048, 4096, 8192, 12288, 16384, 19456]) {
  for (const t of [1, 2, 3]) {
    const ms = bench(() => wasm.kdf_argon2(m, t), 3);
    const pct = ms / BUDGET * 100;
    const mark = ms < BUDGET ? (pct < 60 ? ' ok' : ' tight') : ' OVER';
    if (ms < BUDGET) fits.push([m, t, ms]);
    console.log(`  ${String(m).padStart(7)} ${String(t).padStart(5)} ${ms.toFixed(2).padStart(9)} ${pct.toFixed(0).padStart(12)}%${mark}`);
  }
}
if (fits.length) {
  const best = fits.reduce((a,b) => (b[0]*b[1] > a[0]*a[1] ? b : a));
  console.log(`\n  strongest that fits: m=${best[0]} KiB (${(best[0]/1024).toFixed(0)} MiB), t=${best[1]}, ${best[2].toFixed(2)} ms`);
  console.log(`  OWASP minimum is    m=19456 KiB (19 MiB), t=2`);
}
