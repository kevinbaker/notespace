// Password hashing against the Worker CPU budget. Run via scripts/spike.sh.
import { createRequire } from 'node:module';
import { webcrypto } from 'node:crypto';
const dir = process.env.BENCH_DIR;
if (!dir) { console.error('BENCH_DIR not set'); process.exit(1); }
const wasm = createRequire(import.meta.url)(`${dir}/bench.js`);

const BUDGET_MS = 10;   // Workers free plan, per request

function bench(fn, iters = 7) {
  fn();
  const s = [];
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    fn();
    s.push(Number(process.hrtime.bigint() - t0) / 1e6);
  }
  s.sort((a, b) => a - b);
  return s[Math.floor(s.length / 2)];
}
const verdict = ms => ms < BUDGET_MS * 0.5 ? 'fits'
                    : ms < BUDGET_MS       ? 'tight'
                    : `OVER by ${(ms / BUDGET_MS).toFixed(1)}x`;

console.log(`\nPASSWORD HASHING vs the ${BUDGET_MS} ms free-plan CPU budget\n`);
console.log('candidate                              p50 ms   % budget   verdict');
console.log('-'.repeat(72));

for (const iters of [10_000, 100_000, 210_000, 600_000]) {
  const ms = bench(() => wasm.kdf_pbkdf2(iters));
  const label = `PBKDF2-SHA256, ${iters.toLocaleString()} iters` +
                (iters === 100_000 ? ' *cap*' : iters === 600_000 ? ' *OWASP*' : '');
  console.log(`${label.padEnd(38)} ${ms.toFixed(2).padStart(7)} ${(ms/BUDGET_MS*100).toFixed(0).padStart(9)}%   ${verdict(ms)}`);
}
console.log();
for (const [m, t, note] of [[8, 1, 'minimal'], [19456, 2, 'OWASP 19 MiB'], [65536, 3, 'RFC 9106 64 MiB']]) {
  let ms;
  try { ms = bench(() => wasm.kdf_argon2(m, t), 3); }
  catch (e) { console.log(`Argon2id m=${m} t=${t} (${note}) -> failed: ${String(e).slice(0,60)}`); continue; }
  const label = `Argon2id m=${(m/1024).toFixed(0)} MiB t=${t} (${note})`;
  console.log(`${label.padEnd(38)} ${ms.toFixed(2).padStart(7)} ${(ms/BUDGET_MS*100).toFixed(0).padStart(9)}%   ${verdict(ms)}`);
}

// Native WebCrypto, the closest proxy for crypto.subtle inside a Worker.
console.log('\nNative WebCrypto PBKDF2 (proxy for crypto.subtle, which runs outside wasm):');
async function nativePbkdf2(iterations) {
  const key = await webcrypto.subtle.importKey('raw',
    new TextEncoder().encode('correct horse battery staple'), 'PBKDF2', false, ['deriveBits']);
  const t0 = process.hrtime.bigint();
  await webcrypto.subtle.deriveBits(
    { name: 'PBKDF2', salt: new TextEncoder().encode('a-salt-16-bytes!'), iterations, hash: 'SHA-256' },
    key, 256);
  return Number(process.hrtime.bigint() - t0) / 1e6;
}
for (const it of [100_000, 600_000]) {
  const runs = [];
  for (let i = 0; i < 5; i++) runs.push(await nativePbkdf2(it));
  runs.sort((a,b)=>a-b);
  const ms = runs[2];
  console.log(`  ${it.toLocaleString().padEnd(10)} iters  ${ms.toFixed(2).padStart(7)} ms  ${(ms/BUDGET_MS*100).toFixed(0).padStart(4)}% of budget   ${verdict(ms)}`);
}

const csrfN = 1000;
const csrfMs = bench(() => wasm.csrf_hmac(csrfN));
console.log(`\nCSRF token (HMAC-SHA256): ${(csrfMs/csrfN*1000).toFixed(2)} us each, ` +
            `${(csrfMs/csrfN/BUDGET_MS*100).toFixed(5)}% of budget per request`);
