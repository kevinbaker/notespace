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

## `crates/worker/src/cache.rs`

A Worker's response goes straight to the client; Cloudflare's CDN only caches `fetch()`
subrequests the Worker makes, or what it explicitly stores through the Cache API. So the
`s-maxage=60` the thread page had been sending since M0 was inert — confirmed on the deployed
instance, where the response carried no `cf-cache-status` at all while an ordinary Cloudflare
asset returned `HIT`.

That made D1 rows the binding constraint rather than requests:

| | |
|---|---|
| rows read per thread pageview | 404 |
| D1 free tier | 5,000,000 rows/day |
| → thread pageviews/day | **12,376** |
| Workers free tier | 100,000 requests/day (8.1× more than D1 allowed) |

**The key is built, not taken from the URL.** Keying on the raw request URL would let `?x=1`,
`?x=2`, … miss forever, and each miss is the full 404-row read — a few thousand requests to
exhaust a day's budget, from one client. Only the canonical thread id and the parsed cursor
reach the key.

**The key carries the thread's bake version, so a write invalidates by itself.** The public URL
has to stay `/t/{id}` — permalinks — but the Cache API key is internal and arbitrary, so the
version can live there instead. `cache_version` was already in the schema and already bumped by
`BUMP_THREAD` on every insert; nothing read it until now.

That costs one D1 row per pageview to learn the version, and buys zero staleness. One row against
404 is not a trade worth agonising over: 100k requests/day × 1 row is 2% of the daily budget, and
the TTL stops being a correctness knob — a stale entry is simply never looked up again, so
`s-maxage` is now only about how long a busy thread stays resident.

The version read and the page read are not atomic, and do not need to be. A write landing between
them stores fresh content under the *old* version's key, which no reader will ever build again —
an orphan, not a stale hit. Versions only increase, so there is no interleaving that serves stale
content.

Only sound because the baked page is user-agnostic. `baked_page_contains_no_viewer_identity`
is the test holding that invariant, and caching is applied to the thread page alone — `/login`
cannot be reached by this path at all.

Measured locally: first request `rows_read=404, cache;desc="miss"`, every subsequent request
`cache;desc="hit"` with no D1 work, junk query parameters hitting the same entry, and a distinct
cursor correctly getting its own.

---

## `crates/core/src/reply.rs` and the reply form

**The form cannot live in the baked thread page.** That page is shared byte-for-byte with every
reader, so a per-visitor CSRF token in it would be handed to all of them — and would make the
page uncacheable besides. The affordance in the baked page is therefore a plain *link* to
`/t/{id}/reply`, which is uncached and carries the token.
`the_reply_affordance_is_a_link_and_carries_no_token` pins it.

**Ordering, for the same reason login has one.** Validate, then rate limit, then write. The body
bound is checked before anything else so an oversized body never reaches the renderer or the
counters; the limiter runs before the insert because the write is the expensive part.

**A path collision is retried, not absorbed.** Two replies to the same parent in the same moment
compute the same ordinal and `UNIQUE(thread_id, path)` rejects the loser. The adapter must not
quietly pick another ordinal — a retry has to re-read the parent to get a correct one. The retry
is bounded at `MAX_PATH_RETRIES`, because an unbounded loop under contention is a way to spend a
10 ms budget. Each retry uses a *fresh* public id: reusing the one that just lost could not win
the second time either.

**Length is counted in characters, not bytes.** A byte limit rejects the same number of words
differently depending on the language they are written in.

`csrf`, `client_address` and `ids::generate_many` were moved out of the `password` feature gate:
they are form protection and write-path plumbing, not password machinery, and an OIDC deployment
needs all three.

## Compression

Available: `CompressionStream`/`DecompressionStream` in Workers (`gzip`, `deflate`,
`deflate-raw`; `br` only behind the `brotli_content_encoding` flag). Responses to browsers are
already compressed by Cloudflare for free — the 200-post page measures 154 KB of HTML for
**8.7 KB** on the wire. D1 has no compression and no extensions. R2 has none at rest.

So compression is worth adding **only for archived content**, where the object is read whole and
passed straight to the client: store gzipped bytes with `content-encoding: gzip` and never
decompress in the Worker, which costs zero CPU. It is the wrong move for anything the Worker has
to read — fragments that get stitched server-side must be decompressed on every miss, so they
stay uncompressed and the assembled page is what gets cached.

---

## Fragmenting the thread cache — measured, then dropped

The idea: split a thread's post tree into fragments (contiguous runs of top-level subtrees) so a
reply dirties one fragment instead of the whole page. It was built, measured, and removed. The
code is in git history at `de9f353` if the numbers below ever change.

**Rendering is not the cost.** Measured in wasm under V8, 25 samples:

| posts | p50 | µs/post |
|---|---|---|
| 15 | 0.021 ms | 1.4 |
| 29 | 0.032 ms | 1.1 |
| 58 | 0.045 ms | 0.8 |
| 204 | 0.174 ms | 0.9 |

A whole 204-post page renders in **0.174 ms** — 1.7% of the request budget. A rebuild is ~2.5 ms
of D1 round trip plus that. Fragmenting shrinks only the part that was already free, and makes
the cold path *worse*: seven fragments rendered separately cost 0.221 ms against 0.174 ms for one
whole-page render, plus seven statements in the batch instead of two. A 30-row query is not
proportionally cheaper than a 404-row one, because most of the 2.5 ms is round trip.

**What it would buy is D1 rows, which stopped being scarce.** With version-keyed caching the
daily cost is roughly `pageviews × 1 + writes × 404`. At the Workers cap of 100k requests/day
with 1,000 writes, that is ~504k of 5M rows — 10%. Rows would only bind at ~12,000 writes/day,
which is far more traffic than 100k requests/day can carry. The optimization targets a constraint
that is not binding.

**When to revisit:** thread *size*, not forum activity. At 5,000 posts in one thread a single
rebake is 5,000 rows, and ~1,000 writes against it hits the daily cap. That is the trigger.

Two things worth keeping from the attempt, both recorded in case it is rebuilt:

- The range scan is sound because `.` (0x2E) sorts below the lowest alphabet byte `0` (0x30), so
  a whole subtree falls between its root and the next root and can never straddle a boundary.
- A JSON **array** version vector silently no-ops: `json_set('[1,2,3]', '$[7]', 99)` returns the
  array unchanged, so any fragment past the current length would never bump its version and
  would serve a stale entry forever. A sparse **object** keyed by index behaves correctly —
  verified against D1's SQLite, along with increment-existing, NULL-for-missing, and a path
  built by concatenation from a bind.

---

## `crates/core/src/register.rs`

**Registration is an enumeration oracle and cannot not be.** `login.rs` works hard to make "no
such account" and "wrong password" indistinguishable; this gives that away, because a signup
form that will not say "that name is taken" is unusable. The cost is bounded rather than
removed: the limit is per *client*, not per name, because an enumerator supplies a different
name every time.

**The check and the insert are not atomic.** `user_by_name` then `create_user` is a TOCTOU
window. The `UNIQUE` index on `user.name` is the real guard; the advisory check exists only to
avoid spending an Argon2 hash on a doomed insert. Neither adapter mapped a duplicate name to
`Conflict` before this — both fell through to `Backend`, so the race would have surfaced as a
500 rather than "that name is taken". `a_duplicate_username_is_a_conflict` pins it.

Signup logs you in. The rejected name is echoed back into the form; the password never is,
because re-rendering it puts it in the page and from there into anything that caches it.

## `crates/core/src/email/`, `account.rs`, and the mail providers

**A Worker has no SMTP, so mail is an API call, and which API is configuration.** The first
cut was Resend only. It is now a `Provider` table in `email/providers.rs` -- Cloudflare's Email
Service over REST, Resend, Postmark, SendGrid, Mailgun, Brevo -- plus Cloudflare's
`[[send_email]]` binding, which is not an HTTP call at all. Each provider is *data*: the URL,
where the key goes, the field names it insists on (`address` for Cloudflare, `From`/`TextBody`
for Postmark, `personalizations` for SendGrid, a form body for Mailgun), and what success looks
like. The transport in `crates/worker/src/mail.rs` POSTs whatever it is handed and passes the
status and body back, so every provider's shape is pinned by a unit test rather than found out
in production. The self-hosted binary can put SMTP behind the same `Mailer` trait.

**The Cloudflare binding is the default when it exists.** No key to rotate, no third party's
free tier, and the sending domain is onboarded with one wrangler command. It is in open beta,
which is the reason the others are there: if it changes or the quota does not suit, switching
is one variable. Absent a `MAIL_PROVIDER`, the binding wins, then a `RESEND_API_KEY` (the
original configuration keeps working), then nothing.

**Success is what the provider says, not the status.** SendGrid answers 202 with an empty
body; Cloudflare's REST API can answer 200 with `success: false`; Postmark puts its verdict in
`ErrorCode`. A 2xx alone is therefore not a send, and each provider's parser checks for the
thing that means one. The free tiers are noted in `wrangler.toml` where they are chosen, so
the numbers sit next to the decision they inform.

**Nothing fails because mail did.** Signup creates the account and signs in before it tries to
send; `Delivery` reports how the send went and the page says so. A provider outage, a missing
key, or an unverified sending domain cost a confirmation, never an account.
`a_failed_send_does_not_fail_the_signup` pins it.

**Uniqueness is on *verified* addresses only.** A unique index over every stored address would
let anyone squat on anyone else's by typing it into the signup form, and would force the form
to say "that address is taken" -- an oracle over every member's email. So any number of accounts
may *claim* an address and exactly one may *prove* it: the partial index in 0009 is the guard,
`MARK_EMAIL_VERIFIED` trips it, and the adapters report that as `Conflict`. Removing the address
from the account that verified it releases it. Signup therefore never reveals whether an address
is in use.

**Links land on a form.** Mail scanners follow links, so a `GET` that acts would let a corporate
proxy verify addresses and, worse, spend reset tokens before their owner sees them. `/verify` and
`/reset` show a button; the `POST` spends the token. The token is hashed in the database for the
same reason session tokens are: a leaked copy yields no usable links.

**A reset request is silent.** Known, unknown, unverified, and banned all get "if that address
belongs to a confirmed account, a link is on its way" -- and the same rate-limit bookkeeping, so
timing does not separate them either. Only a *verified* address gets mail, because a reset to an
unverified one hands the account to whoever typed it at signup. The per-address limit exists so
a known address cannot be used to flood its owner's inbox.

**Spending a token is one statement.** `UPDATE ... WHERE used_at IS NULL AND expires_at > ?
RETURNING` is atomic on both targets; D1 has no interactive transaction, and a select-then-update
would let two clicks both succeed. Resets end every session and retire every other reset link,
so a leaked older link is dead the moment a newer one is used.

**Plain text only.** These mails are read on phones, in terminals, and by people who have just
lost a password. None of them wants a layout, and a text-only message is what spam filters
trust most.

## `crates/core/src/compose.rs`

**The body is the first post.** There is no `thread.body`; the thread row carries the title and
URL and the body takes the reply path unchanged, so validation, rate limiting, path allocation
and moderation are one code path rather than two that drift. The cost is two round trips on
D1 (thread, then post), which is nowhere near the budget.

**The title is scanned, but only the post can be held.** Tier 0 reads `title + body`, so a
blocklisted word or a link farm in the title holds the first post. The title itself stays
visible in the index under a `[awaiting review]` body, because a thread has no pending state
and adding one means teaching review, the sweep and the index about it. Title-only spam is
therefore bounded by the per-author limit (five threads an hour) rather than caught; if that
turns out to matter, the fix is a `thread.state = 'pending'` that review resolves alongside the
post. `the_title_is_scanned_by_tier_zero` pins what is in place.

**Rejections re-render in place.** The reply form redirects on error and loses the draft, which
is tolerable for a reply. A thread body can be 32 KB and a query string is no place for it, so
a rejected compose comes back as a 200 with the form filled in. A reload resubmits a form that
was already refused, which is harmless.

**Limits are namespaced.** `thread:{author}` rather than the reply bucket: starting threads is
the scarcer act and gets the tighter limit, and one should not spend the other's allowance.

## `crates/core/src/edit.rs`

**Deletion is a tombstone.** The row stays, its path stays, and replies keep their parent;
`[deleted]` is what the page shows. Removing the row would either orphan replies or renumber
the tree, and both break permalinks. `post_count` counts tombstones for the same reason.

**Edits go back through Tier 0.** The text a moderator approved is not the text a reader sees
after an edit, and an approved post is the obvious place to put something afterwards. A
re-held edit is the same `pending` state and the same queue; the duplicate check is skipped
because a body is not a duplicate of itself.

**Moderators delete but do not edit.** Rewriting someone's words under their name is not a
moderation action this system offers; hiding is. A moderator's deletion of someone else's post
is in the public log, an author's own is not.

**The edit link is on every post.** The baked page cannot know who is reading it, so the
choice is a link everyone sees or no link at all. It is a link; a non-author who follows it is
told, without a form. When the personalisation layer (§7.1) exists it can hide the link for
everyone else, and until then the reply and report links are already in the same position.

## Closing the site for a test: `SIGNUP_CODE`, and what was cut for the request budget

**Registration is the gate, so the invite code sits there and nowhere else.** Reading is free
and must stay so; every write needs an account. A `SIGNUP_CODE` secret makes `/register` ask
for it and refuse without it, checked before the limiter and before the hash so a wrong guess
costs nothing. A secret rather than a var only so it stays out of version control; it is a
shared word, not a credential. Heavier gates -- Cloudflare Access in front of the hostname, WAF
rate-limiting rules -- live outside the code and are documented in `docs/DEPLOY.md`.

**Two dead requests per pageview were cut.** The baked page carried a `<script>` tag for the
personalisation layer that was never built, and every browser asks for `/favicon.ico`. Both
404ed, and on a budget of 100k requests a day that tripled the cost of a pageview. The script
tag is gone (§7.1 says the enhancement degrades by removal; nothing was there to remove yet),
and every page carries `<link rel="icon" href="data:,">`, which browsers honour as "there is
no icon" and ask no further -- `/favicon.ico` answers 204 with a week's cache for the ones that
ask anyway.

**Timestamps render in UTC**, because a baked page is the same bytes for every reader and
cannot know a zone. The `datetime` attribute carries the ISO form for a script to localise.

**A rejected reply re-renders with the draft**, as compose already did. The redirect-with
`?error=` pattern lost the text, which for a reply written on a phone is a real loss; the
form comes back as a 200 under a fresh token, and a reload resubmits a form that was already
refused, which is harmless.

**`/login` while signed in goes to `/settings`.** The nav cannot say who you are (baked
pages), so "sign in" is the link people press to find out; the account page is the answer.

## `crates/core/src/sql.rs` -- space listings

`SPACE_THREADS` is the subtree range scan the 0003 migration was designed for, and that
nothing had used: the seed never wrote `thread.space_path`, so the column was NULL on every
seeded row. 0009 backfills it from the space and the seed now writes it. Listing `/s/sports`
therefore includes `/s/sports/hockey`, which is what a section page of a hierarchical forum
means.

## Permalinks and the page cursor

`/p/{id}` used to redirect to `/t/{id}#p{post}`, which is page one. For any post past the page
size the anchor is not in the document, so the reader lands at the top of the thread with no
sign their post exists — indistinguishable from it having failed to save. Demonstrated with a
reply at path `001R`, post 206 of 206.

The handler's old comment said this needed real pagination and that deriving a cursor from the
path was wrong. Half right: *truncating the path* is wrong, because an earlier sibling with a
large subtree pushes the post off the page. The rank is exactly computable and cheap.

```sql
SELECT COUNT(*) FROM post WHERE thread_id = ?1 AND path < ?2      -- rank
SELECT path FROM post WHERE thread_id = ?1
  ORDER BY path LIMIT 1 OFFSET (page * size - 1)                  -- the cursor
```

`EXPLAIN QUERY PLAN` reports `SEARCH ... USING COVERING INDEX idx_post_thread_path` for both, so
no table row is touched. Measured: a deep post costs **3 statements, 408 rows**; a first-page
post costs 2 and 7, because the cursor lookup is skipped. The redirect then lands on a page the
cache already holds.

Two couplings that would drift silently:

- `page_size` given to `locate_post` must match what the read path paginates by. The arithmetic
  is `store::page_cursor_offset`, in core with its own tests rather than copied into both
  adapters — an off-by-one here is a wrong page, not a crash.
- The rank count must see exactly the posts `thread_page` renders. Neither filters by state
  today; if one starts, the other must too.

The conformance check runs at page sizes 1 and 3 as well as the real one, because a fixture
smaller than a page never leaves page zero and would pass without exercising a cursor.

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

Cargo features reach the build through `NOTESPACE_FEATURES`, because wrangler has no flag for
them and `wrangler dev` runs the build itself, overwriting anything built by hand — which is
how a Worker built with `--features password` came to serve without it. An empty
`--features ""` is accepted by cargo, so the variable can be unset.

Mail is configured by `[vars]` plus one secret rather than a binding, and a misspelt variable
is silently "no mail". `the_mail_variable_names_match_wrangler_toml` checks the names in
`mail.rs` against the file.

Cloudflare's own build environment (Workers Builds, the dashboard's Git integration) ships Node
but not rustup, cargo or worker-build, so a dashboard-driven build fails and the Worker keeps
serving whatever it had. Deploy from a developer machine or from CI that installs Rust.

---

## `crates/core/src/moderation/`

**Why the pipeline came before ranking and search (M3).** The write path existed and every
reply was publishing unconditionally, which for a project whose pitch is moderation meant the
first spammer would have had the run of the place. Ranking can wait; an unmoderated write path
cannot.

**The neuron budget decided the shape.** Workers AI pricing (September 2026): Llama 3.1 8B is
25.6 neurons per thousand input tokens, Llama Guard 3 8B is 44. A classification of a typical
post is ~600 tokens in and ~60 out -- ~20 neurons -- so the free 10,000 a day is **about 500
classifications**, and a 70B model is out of the question. That makes Tier 0 the design rather
than a preamble to it: the model is consulted only about posts the heuristics held, and the
heuristics have to hold few enough that the budget lasts a day. New-account age and a link cap
are the two that matter; blocklist and manipulation markers are cheap extras.

**Reports are a Signal, the queue is a table, the log is append-only.** Three of the eight
primitives, straight from DESIGN.md §2. `signal` has a `UNIQUE(post_id, user_id, kind)` index,
which is what makes reporting idempotent -- the second click is a no-op rather than a second
report. `review_item` is one row per post with an upsert on `post_id`, so a post held, hidden,
appealed and reported is one item with several reasons rather than four items to de-duplicate.
`action_log` is never updated; a reversal is a new row, and the public view is a query.

**Per-space policy is JSON in `space.config`, not columns.** A preset is a config bundle; a
preset that needed a column would be a hole in the primitive model. `ModerationPolicy` is
`#[serde(default)]` so every field is optional, unknown fields are ignored, and a broken config
falls back to the defaults rather than taking the space offline.

**The database is the source of truth; the Queue is an accelerator.** A pending post with no
open review item is work wherever the message went, and a cron sweep every five minutes picks
up whatever the queue did not deliver. This is what lets the same `core` code run on a native
binary that has no queue at all, and it is why the Queue binding is optional in the Worker. The
sweep excludes posts with an open review item so a post is classified at most once by
automation: a classifier failure hands the post to a human rather than retrying on every pass
and burning the budget on a model that is down.

**A verdict is logged before it is acted on.** A crash between the two leaves a verdict without
an action, which a human can see and finish; the other order leaves an action nobody can
explain.

### The live evaluation, and the two rules it forced

`crates/core/tests/live_classifier.rs` runs the prompt over a labelled corpus against a real
model. It is `#[ignore]` because it costs money, but it was run against Llama 3.1 8B while the
prompt was written, three times, and it changed the design twice:

- **Run 1** (prompt `2026-09-01`): every bad post caught at confidence 1.0, both prompt-injection
  cases flagged, every answer parsed. And five of six *clean* posts flagged as `off_topic`, two of
  them at confidence 1.0 -- which the policy as then written would have **hidden**. The model had
  read the space rules ("Technical discussion about forum software") as an allowlist and found a
  post about D1 bindings off topic for it.
- **Rule 1, structural:** a flag hides only for a *severe* category. `off_topic` and `other` can
  hold a post for a human at any confidence; they cannot remove it. This is `Category::is_severe`
  and `ModerationPolicy::decide`, pinned by `a_flag_with_only_mild_categories_holds_but_never_hides`.
  It does not depend on the model behaving.
- **Run 2** (prompt `2026-09-02`, off-topic softened to "a weak signal"): zero harmful
  dispositions, but the model still flagged all six clean posts off-topic. Softening was not
  enough; the model needed the rule stated as a rule.
- **Run 3** (prompt `2026-09-03`, "topicality is NOT a violation and never makes the decision
  flag"): 13 of 14 obvious cases right, zero harmful dispositions. The one miss is a clean post
  flagged off-topic at 1.0, which under Rule 1 is a hold for a human, not a removal.

The eval scores two things separately and asserts on them separately. *Harmful dispositions* --
a clean post the policy would hide, a bad post it would publish -- must be zero. *Call accuracy*
has a floor of seven in ten, because the model is a triage step and a miss there costs a
moderator a look, not a user a post. Scoring on the model's word alone would have hidden the
difference between "wrong but safe" and "wrong and damaging", and that difference is the whole
design.

Llama Guard is offered but not the default: its taxonomy is LLM safety (specialised advice,
intellectual property, code interpreter abuse) and it cannot see spam or harassment, which is
most of a forum's work. It also gives no confidence, so it is assigned one below the hide
threshold and can only ever hold.

### Three layers against text addressed to the model

The classifier reads user-generated text, so the text can try to talk to it. Tier 0 holds
anything matching `MANIPULATION_MARKERS` on sight (`ignore previous instructions`, forged
`</post>` tags, JSON aimed at the parser). The prompt fences the body in `<post>` tags with any
embedded closing tag defanged, and tells the model the content is data. And a verdict carrying
`manipulation` can never publish, however confident. The first two are heuristics and a model
following instructions; the third is the one that holds when they do not. Run 1 showed the model
does not always name `manipulation` even when it catches the injection (the fake-close case came
back as `spam` only) -- which is why Tier 0 has to catch it too, and does, pinned by
`tier_zero_holds_every_injection_case_in_the_corpus`.

### Metamoderation as threshold adjustment, not a separate system

Slashdot's insight, applied to the model: humans rate its calls, the ratings tune how far it is
trusted. Here that is one aggregate query over resolved review items (`clean`/approve and
`flag`/reject agree; the other diagonal disagrees; `unsure` is neither) and a pure function that
moves the thresholds. Disagreement tightens after ten items; agreement loosens after thirty, and
loosens less, because loosening is the direction that publishes bad posts. The hide threshold
never loosens. The adjustment happens at decision time from the current statistics, so there is
no tuning job to run or forget. `prompt_version` travels with every verdict so the statistics
can be read per prompt when the prompt changes.

### Real Hacker News comments, and what they did to the prompt

The labelled corpus is sixteen sentences somebody wrote to be classified. `examples/hn_moderate.rs`
runs the same two tiers over a seeded sample of the 51,000 HN comments the importer fetched --
prose nobody wrote as a test case -- through OpenRouter, where the same 8B model costs $0.05 per
million tokens and a 30-comment run is a fraction of a cent. Same 30 comments, seed 3, throughout:

| prompt | stack | publish | hold | hide |
|---|---|---|---|---|
| `2026-09-03` | Llama 3.1 8B | 20 | 10 | 0 |
| `2026-09-03` | gpt-4o-mini | 22 | 8 | 0 |
| `2026-09-04` | Llama 3.1 8B | 15 | 15 | 0 |
| `2026-09-05` | Llama 3.1 8B | 21 | 8 | **1** |
| `2026-09-05` | Guard 4 + Llama 3.1 8B | 19 | 10 | **1** |
| `2026-09-05` | gpt-4o-mini | 28 | 2 | 0 |
| `2026-09-05` | Guard 4 + gpt-4o-mini | 27 | 3 | 0 |

All thirty are fine comments; the right column is the one that matters, and the middle one is
moderator time.

**Prompt `2026-09-04` made things worse, instructively.** The 8B model was labelling comments
that *discussed* prompts, or quoted another comment with `>`, as `manipulation`. The fix seemed
obvious: tell it that discussing AI, quoting and sarcasm are not manipulation. The hold rate
went from 10 to 15, because the model now labelled every quoting or sarcastic comment
`manipulation` and explained, in its rationale, that "the mention of 'manipulation' is because
the post quotes another comment". A negated instruction is an instruction to a small model.
`2026-09-05` replaced the list of non-examples with one positive definition -- an instruction
aimed at the moderation system itself -- and the clean side cleared. The same revision told the
model that most posts are fine and `unsure` is for a listed violation it cannot decide, which is
what moved gpt-4o-mini from 8 holds to 2: it had been taking "prefer unsure" literally.

**The one false hide** is the 8B model reading a quoted personal attack, posted by the person
complaining about it, as the poster's own words, and flagging `harassment` at 0.9. `harassment`
is not in Llama Guard's taxonomy, so the layer below cannot veto it; only a better back model
does. gpt-4o-mini flagged the same comment at 0.8, a hold. This is the reason the README
recommends the Guard + gpt-4o-mini stack over the free default when a key is available.

### Layers: what a Guard in front is for, and what it is not

`layers::Layered` runs a safety classifier and an instruct model on every post and combines the
verdicts by a pure rule (`combine`), which is where the trust arithmetic lives and is tested:

- Two flags corroborate: confidence is the noisy-or, categories the union. This is what lets a
  flag reach the hide bar -- neither an 8B instruct model at 0.8 nor Guard at its fixed 0.85
  gets there alone, both together do (0.97).
- A flag one layer made and the other contradicted is scaled by 0.8, below the hide bar. On the
  HN sample this turned the 8B model's `hate`/`violence` flags on political comments from 0.8
  to 0.64 -- holds either way, but a Guard that disagrees now shows in the rationale.
- A Guard's `safe` is a veto on hiding for the categories it is trained on, and for nothing
  else. Spam and harassment are not in its taxonomy; its `safe` says nothing about them.
- A Guard that flags on its own also scales down: Guard 4 called a comment *about* a data
  breach `doxxing` (S7) and a political comment `harassment` (S5, defamation). Both held for a
  human at 0.68 rather than hiding.

The Guard is a frontend, not a gate. Skipping the instruct model when the Guard says safe would
save a call and let every spammer through, since spam is the thing a safety model cannot see.

### What the Anthropic provider defaults to

`claude-opus-5`, configurable with `MOD_MODEL`. A cheaper model is the operator's call; the
default should be the one that does not need apologising for. The request uses structured
output (`output_config.format`) rather than asking nicely for JSON, and a `refusal` stop reason
is read as `unsure`, not as an error -- the model declined to look, so a human should.

### Bundle size

The moderation slice adds ~150 KB gzipped to the Worker (598 KB → ~750 KB on this machine). The
README's 149 KB figure was the M0 read-path-only measurement and has been stale since the auth
work; both numbers are now recorded where they are measured. 746 KB is a quarter of the 3 MB
limit; the growth is worth watching but not yet worth acting on.

---

## `crates/hn-import` and `scripts/hn-fetch.mjs`

Two stages with a file between them, rather than one program that downloads and inserts. The
NDJSON mirrors HN's own shape and holds no notespace decisions at all, so re-deciding one — how
deep to nest, which space a Show HN lands in, whether `*` is escaped — costs a re-import and not
a re-download. At 2.5 requests/second that difference is fifteen minutes each time.

BigQuery was the obvious source and turned out to be the wrong default. `bigquery-public-data.
hacker_news.full` needs the `bq` CLI, a Google account and a billing-enabled project — queries
against public datasets bill the *querying* project — and the public copy stopped updating in
2022, so a window ending "now" returns nothing at all. It also lacks the `top_level_parent`
column the older `comments` table had, so comment trees have to be rebuilt from `parent` locally
and replies whose parent falls outside the window are lost. Algolia's HN API needs no
credentials, is current, and returns a whole comment tree in one request. Both are implemented;
Algolia is the default.

Algolia answers **403** when you exceed its rate limit, not 429. The first attempt at this
treated anything under 500 as permanent and burned through 1,900 stories in a few seconds
without fetching one of them. The retry set now includes 403, and pacing is enforced up front
rather than discovered by being cut off.

Its search endpoint caps any one query at 1,000 hits, so the window is walked backwards by
timestamp rather than paged. The cursor has to be *inclusive* of the oldest hit seen — two
stories can share a second, and an exclusive cursor silently drops the second one.

### Fidelity, and where it stops

Comment bodies go through `notespace_render::markdown_to_html`, for the reason the seed crate
does: `body_html` measured against hand-written HTML measures nothing.

Getting there means HN's HTML becomes markdown first, and that conversion has one real decision
in it. HN renders `*` and `[` literally; CommonMark does not. Escaping them keeps a comment
looking like the comment. `_` is deliberately *not* escaped — CommonMark already ignores
intraword underscores, and escaping it puts a backslash in the middle of every `snake_case`
identifier in a corpus full of them. A leading `>` is deliberately not escaped either, which is
the one place fidelity is knowingly traded: HN shows the character, we render a blockquote,
because that is what the author meant and what a notespace user would have typed.

### Identity

Imported threads and posts take explicit ids from 1,000,000 up. That is what makes `--reset` a
range delete scoped to this import, which is what makes a re-import idempotent without touching
the spike seed or a real account.

Users cannot work that way: `user.name` is UNIQUE, so an id chosen here collides with whatever
already holds the name. They are inserted `ON CONFLICT(name) DO NOTHING` and referenced through
a subquery on the name index instead — one extra indexed lookup per row, on a one-off import,
in exchange for composing with any existing database.

HN's name rules are looser than notespace's, so some names must be rewritten, and a rewrite can
collide. Two distinct HN accounts folding into one notespace user would misattribute posts, so
collisions get a numeric suffix rather than being allowed to merge. `hn-anon` is claimed up
front for `[dead]` and `[deleted]` comments, which HN returns with no author — a real account of
that name gets the suffix instead.

Dead and deleted comments are kept as tombstones rather than skipped. Skipping them would
orphan every reply beneath them.

### Nesting

`post.path` allows 32 levels and HN allows fewer, so the clamp never fires on real data. It is
there anyway because `--max-depth 4` is how you generate a shallow corpus to measure render cost
against nesting, and because "deeper than allowed" must not mean "dropped": a dropped comment
takes its whole subtree with it. It attaches to the deepest ancestor that fits.

### Two things real data had that invented data did not

A comment can arrive twice. HN merges threads, and the merged comments come back under both
stories — once in 51,439 across a 7-day window, which is exactly often enough to abort an import
on `post.public_id` and exactly rare enough that no fixture would have contained it. Items are
therefore deduplicated by HN id across the whole import rather than per story. The second copy is
dropped with its subtree, which is the same subtree.

For the same reason a post can be older than the thread it sits in: the merged comment above was
from eight months before the story that now holds it. That is not corruption and is not
corrected — the comment really does live there now.

The `&`-lookahead in `decode_entities` scanned a fixed *byte* window for the closing `;` and
sliced it, which panics the first time a smart quote or an em dash falls inside it. Every slice
there now lands on `&` or `;`, both ASCII, however many multi-byte characters lie between. The
regression test is a sentence with curly quotes in it.
