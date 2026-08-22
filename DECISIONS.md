# Decisions

Per-file notes on *why* the code is the way it is: measurements, alternatives that were tried
and rejected, and bugs that shaped a design.

This is the only place that reasoning lives. The source files carry contracts and invariants —
what a caller must do, what must stay true — and nothing else. They do not reference this file,
and they should not: code that needs a second document open to be read is code that will drift
from it.

Measurements were taken on the development machine unless stated otherwise. Treat ratios as
durable and absolute figures as needing a re-check against a deployed instance.

---

## `crates/core/src/password.rs`

### OWASP-grade password hashing does not fit a free Worker

Password hashing is deliberately slow; the Workers free plan allows **10 ms of CPU per request**.
Measured in wasm under V8 (`scripts/kdf-bench.mjs`):

| candidate | p50 | vs 10 ms budget |
|---|---|---|
| Argon2id, OWASP minimum (19 MiB, t=2) | 25.2 ms | **2.5× over** |
| Argon2id, RFC 9106 (64 MiB, t=3) | 134.9 ms | 13.5× over |
| PBKDF2-SHA256, OWASP minimum (600k) | 456.1 ms | 45.6× over |
| PBKDF2-SHA256 at workerd's 100k cap | 75.8 ms | 7.6× over |
| …the same via native WebCrypto | 12.7 ms | 1.3× over |

The last two are the interesting pair. `crypto.subtle` runs natively rather than in wasm and is
six times faster, but workerd caps PBKDF2 at 100,000 iterations to limit DoS
([workerd#1346](https://github.com/cloudflare/workerd/issues/1346), open since 2023) — and OWASP
asks for 600,000. The fastest legal configuration is both *below* the recommended strength and
*over* the CPU budget.

There is no way to do OWASP-grade password login on a free-plan Worker. That is a property of
the platform, not of this code. OIDC is the answer for a Workers deployment: verifying a signed
assertion is sub-millisecond.

### `CONSTRAINED` is 4 MiB, not 8

An earlier revision used 8 MiB t=1 on the strength of a three-sample run reporting 5.98 ms.
Fifteen samples put its p95 at **11.99 ms — over budget**. Chosen on p95 rather than median
because a request that overruns is a failed login, not a slow one.

| setting | p95 | % of budget |
|---|---|---|
| 4 MiB, t=1 | 4.33 ms | 43% |
| 4 MiB, t=2 | 9.12 ms | 91% |
| 6 MiB, t=1 | 9.83 ms | 98% |
| 8 MiB, t=1 | 11.99 ms | **over** |

The neighbours are all too tight to be safe: the same request still has to render a page and
talk to D1.

### The pepper, and why it makes `CONSTRAINED` tolerable

A pepper is stored outside the database, so against the threat that actually matters here — SQL
injection, an exposed backup, one of D1's seven days of Time Travel snapshots — the hashes are
uncrackable *whatever the KDF cost is*. The weak parameters only matter to an attacker who
already holds both the data and the secret.

It buys nothing against a full server compromise, and nothing against online guessing (that is
rate limiting's job).

Cost of the pepper itself: **0.3%**. An early sequential A/B suggested 27%, which was drift
between runs; interleaving the measurements gave the real figure.

### Why each hash records its pepper id

Verifying a *wrong* password against a list of peppers means trying each in turn, and each try
is a full Argon2 run. At `CONSTRAINED` that is 3.34 ms per attempt: two peppers fit the 10 ms
budget, three do not (10.03 ms). A list therefore caps rotation at one generation on the
failed-login path — the path an attacker controls.

Recording the id makes verification cost constant however many peppers are held, which is what
makes keeping all of them practical. No pepper has to be retired on a deadline, and no account
is stranded by a rotation that finished before its owner came back.

### `PASSWORD_PEPPER` as one variable, and fatal parse errors

One variable rather than one binding per pepper because the set is a single fact about the
deployment: a scan over numbered bindings cannot distinguish "id 3 was never used" from "id 3
failed to load", and silently holding fewer peppers than intended strands accounts.

Every parse failure is fatal for the same reason. A pepper set that is *partly* right is the
worst outcome available — it authenticates some accounts and permanently rejects others, and to
the users it looks like they mistyped their passwords.

### `CLIENT_ARGON`, and why it is a `Scheme` rather than a `Params`

The scarce CPU is the Worker's; the browser's is not scarce. Running OWASP-grade Argon2id on the
client and peppering the result restores the work factor `CONSTRAINED` gives up.

| scheme | server-side cost (p95, 15 samples) | % of budget |
|---|---|---|
| `CONSTRAINED` (4 MiB, t=1) | 4.18 ms | 42% |
| `HANDOFF` (1 MiB, t=1) | 1.25 ms | 12% |

So it buys back 3.3× of the request's CPU as well as the work factor. (OWASP's 19 MiB
re-measured at 48.5 ms p50 on that run against the 25.2 ms in the table above — machine noise,
not a change in the code.)

It is a separate `Scheme` because the two produce stored records that *look alike* — both end in
a cheap Argon2id PHC string — and mean opposite things. A cheap hash is sound over a key that
already cost 19 MiB to derive and close to worthless over a plaintext password. If one `verify`
accepted both, a config flipped back to `CONSTRAINED`, or one non-JS client posting a raw
password, would silently write the worthless kind alongside the sound kind and nothing would
ever say so.

Hence two independent guards: the `c` marker, and domain separation of the hashed input. Both
fail closed.

The honest trade:

- **Gained.** A stolen database costs a full 19 MiB Argon2id run per guess.
- **Lost.** Login requires a client that performs the derivation. The salt must be fetched
  before submit, so that endpoint has to answer for unknown accounts too, or it becomes the
  enumeration oracle `login.rs` exists to avoid.
- **Unchanged.** Anyone reading the derived key in flight holds a password-equivalent — exactly
  their position with the password itself.

`MIN_PASSWORD_CHARS` cannot apply server-side under this scheme, because the server never sees
the password. That is a real transfer of responsibility to the client, not a detail.

---

## `crates/core/src/login.rs`

The flow lives in `core` rather than in a handler because the order of operations *is* the
security property, and an order is worth a test. Handlers translate HTTP; this decides.

**The rate limit runs before the hash.** Checking afterwards would let an attacker spend 3.34 ms
of a 10 ms budget per attempt for free — CPU exhaustion, not just guessing.

**Unknown accounts still hash.** Returning early is measurably faster than reaching Argon2 —
a few milliseconds, trivially detectable over a handful of requests. So an absent account and an
account with no local password both verify against a dummy hash first.

**The dummy hash nearly broke silently.** It must satisfy the configured scheme's own shape
rules. Under `CLIENT_ARGON` a passphrase is not a valid key, so `hash` fails,
`unwrap_or_default()` yields `""`, and an empty stored hash verifies instantly against
everything — restoring the exact enumeration oracle it exists to close. Hence
`dummy_hash_is_real()`.

---

## `crates/core/src/ratelimit.rs`

A weak KDF is an *offline* problem; online guessing is a different attack with a different
defence, and no amount of Argon2 cost helps because the attacker pays nothing for a wrong guess.

Two buckets, because one attack is not the other. Per-identity catches a password list against
one account. Per-client catches credential stuffing — one common password sprayed across many
accounts — which never trips a per-account limit because no account sees two attempts. The
per-client limit is looser because a shared NAT or a university proxy is one client here.

Windows are fixed rather than sliding: a sliding window needs per-attempt timestamps, a fixed
one needs a counter and an instant. The approximation lets an attacker straddle a boundary for
`2 × limit` attempts, which at a limit of 5 means 10 and changes nothing. Precision is not worth
a row per attempt on a 500 MB database.

---

## `crates/core/src/session.rs`

Sliding expiry is what users expect and what makes a session store expensive — naively one write
per request. Refreshing only when the last refresh is older than 24 h turns that into roughly one
write per user per day. Against D1's 100,000 rows written/day that is the difference between a
few thousand pageviews and a few tens of thousands of daily users.

---

## `crates/core/src/csrf.rs`

Measured at **1.78 µs** to mint or verify — 0.018% of the request budget, which is why this is an
HMAC rather than anything stored. A CSRF token in the database would cost a query on every form
render and every submit, for a value that has to be derivable anyway.

Kept independent of `SameSite=Lax` deliberately. That attribute already blocks cross-site form
posts, but it does not cover same-site subdomain takeover, and it is one browser default away
from being the only thing standing between a forum and a mass-post attack.

---

## `crates/core/src/cookie.rs`

Moved here from `crates/worker`. That crate is not in `default-members`, so `cargo test` never
ran its tests — the cookie tests were "passing" only in the sense that nobody executed them.
Security logic with dead tests is worse than none, because it looks covered.

---

## `crates/core/src/id.rs`

Ids are a truncated ULID: 48-bit millisecond prefix, 32 random bits, 16 Crockford base32
characters.

**The time prefix is load-bearing.** M0 measured a time-sortable text id costing the same as an
integer to look up (2 ms per 5000), while a random one costs double — random keys scatter across
the index instead of appending at its right edge.

**32 random bits is enough** because two ids can only collide if they share a millisecond, so the
birthday bound applies per-millisecond rather than over the database's lifetime. At 32 bits that
is ~65,536 ids in one millisecond for a 50% chance. For ten threads in one millisecond the
probability is about 1 in 10⁸.

**Bottom-alignment, not ULID's top-alignment.** Widening appends bits below the existing ones, so
a 16-char id is a literal prefix of the 26-char id built from the same inputs — 16 characters
shared, against 1 for a canonical ULID. This is what makes widening later a non-migration.

---

## `crates/core/src/path.rs`

**Base32 over decimal.** Four base32 digits address 1,048,576 siblings per level, slightly more
than the 1,000,000 six decimal digits buy, while storing 30% fewer bytes. The `(thread_id, path)`
index is the read path's whole mechanism and D1's free tier caps the database at 500 MB.

**Base62 was measured and rejected.** It stores the same 4 bytes as base32 at this width, so its
only gain is headroom nothing needs, and it pays with case sensitivity. Anything that lowercases
a path — a URL normaliser, a `NOCASE` collation, a stray `to_lowercase()` — would silently
destroy the ordering invariant. Base32 is closed under case folding; base62 is not.

**`depth()` counts separators, so a root post is depth 0.** An `ancestor_at_depth` written
assuming it counted segments made every allocation fall through to "first post" and collide.
Pinned by a test that names the convention.

---

## `crates/core/src/space_key.rs`

Resolution is one indexed lookup on the materialized path. Measured at depth 3: **1.6 µs and a
single query**, against 32.9 µs and three queries for walking parent by parent.

**Stored paths carry a trailing separator.** Without one, the prefix range `['sports', 'sports0')`
swallows siblings: `-` (0x2D) sorts below `/` (0x2F), so `sports-betting` fell inside the range
for `sports`. Pinned by a test.

**`UNIQUE(parent_id, key)` does not work** for per-parent uniqueness, because SQLite treats NULLs
as distinct and root spaces have a NULL parent. Verified empirically; the full path is indexed
instead.

**Renames get a redirect, not a history table.** A space is a place, not an identity: no post is
attributed to a space in a way a rename could falsify, so the impersonation argument that makes
usernames permanent does not apply. A nullable `moved_to` on a tombstone row costs zero extra
queries, because the lookup that resolves any path already finds it.

---

## `crates/core/src/store.rs`

The trait was decorative before it was load-bearing: one method nothing called, while the Worker
used two inherent methods on `D1Store` with different signatures. It compiled and carried no
weight. Every storage call now goes through it, and `crates/store-sqlite` plus the shared
conformance suite are what make the dual-target promise checkable rather than merely stated.

Measured read path in production: **2 statements, 404 rows, 2.52 ms p50**. One statement per post
would be ~0.5 s.

---

## `crates/worker/src/auth_config.rs`

The pepper cannot be auto-generated on this target, for three independent reasons:

1. **Nowhere safe to put it.** A Worker has no persistent local storage. The only writable place
   is D1 — and a pepper in the database is not a pepper. Auto-generating into D1 would produce
   the appearance of protection with none of the substance, which is worse than none: an
   operator would see "pepper configured" and stop worrying.
2. **A Worker cannot write its own secrets.** Doing so needs an API token with account access,
   and shipping one would be a far larger hole than the one it closes.
3. **Isolates are plural.** Many run concurrently and are recycled constantly. Each would
   generate a different value, so a password hashed by one would fail against every other. Not
   merely insecure — broken.

So the rule here is the other half of the choice: refuse. But "refuse" is scoped to what it
protects. A Worker does not start, it serves, and taking a public forum offline because a login
secret is missing turns a security control into an outage — which is how security controls come
to be switched off. Anything touching a password returns 503; reading continues.

The self-hosted target has none of these constraints and should generate a pepper on first run,
writing it to a mode-0600 file beside the database. That belongs in `crates/server` (M5).

An unrecognised `PASSWORD_SCHEME` refuses rather than falling back: an operator who asked for the
stronger scheme and silently got the weaker one is the failure this whole module is about.

---

## `crates/worker/src/store.rs`

**D1 rejects bigint bindings** with `D1_TYPE_ERROR`; values go through `JsValue::from_f64`.
rusqlite accepts `i64` happily, so only the shared conformance suite caught it.

**D1 has no `last_insert_rowid()`.** The new id comes from `meta().last_row_id` on the batch
result. Before that it returned a placeholder `0`.

`rows_read` is reported by miniflare as well as production — an earlier claim that it was
production-only was wrong. Local 404 matched the derived ~403.

---

## `crates/render/src/page.rs`

The permalink anchor used to emit `id="p{post.id}"`, baking the internal sequential integer into
every page — leaking the post count and making the table enumerable, which is the whole reason
public ids exist. The test that should have caught it asserted `id="p1"`, pinning the bug in
place. Replaced with a property test over the public id.

---

## `crates/worker/src/subtle_kdf.rs`

Kept for the size comparison it demonstrates: WebCrypto is a *platform* API, so using it links no
cryptography into the bundle — only the wasm-bindgen glue. Argon2, a Rust crate, is compiled in
and costs 10.6 KB gzipped.

Bundle-size measurement misled three separate times before that number was trustworthy:
dead-stripping (an unreferenced KDF costs 0), Cargo feature unification (a member crate cannot
disable default features on an inherited workspace dependency — it must be set at the root, and
Cargo only warns), and a measure script that grepped for success and swallowed a build failure.

---

## `wrangler.toml`

Wrangler runs `[build]` through `/bin/sh`, which does not source a shell profile, so
`~/.cargo/bin` is absent from `PATH` even when `worker-build` is installed — `exit 127`,
`worker-build: not found`. The build command prepends it.

Cloudflare's own build environment (Workers Builds, the dashboard's Git integration) ships Node
but not rustup, cargo or worker-build, so a dashboard-driven build fails and the Worker keeps
serving whatever it had. Deploy from a developer machine or from CI that installs Rust.
