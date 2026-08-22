# M0 spike — findings

> **Verdict: the free-tier premise holds, with room to spare.** CPU is not the binding
> constraint and is not close to being one. Reproduce with `./scripts/spike.sh`.

DESIGN.md §8 sets M0's task: *"Prove a Rust Worker can render a 200-post thread page from D1
within the 10ms CPU limit. Measure actual CPU, wasm binary size, and D1 rows read. If this
fails, the free-tier premise changes and the whole design needs revisiting."*

It does not fail. The read path uses about **1% of the CPU budget** and **4.5% of the script
size budget**.

## Headline numbers

| Budget (free plan) | Limit | Measured | Headroom |
|---|---|---|---|
| Worker CPU / request | 10 ms | **0.048 ms** p50, 0.109 ms p99 | ~90x at p99 |
| Worker script size | 3 MB | **139.6 KB** gzipped | 22x |
| D1 queries / invocation | 50 | **2**, in one batched round trip | 25x |
| D1 rows read / page | 5M/day | **404** (observed via `Server-Timing`) | — |

Rendering a 200-post thread page costs roughly **one two-hundredth** of the per-request CPU
allowance. The original worry behind M0 — that wasm rendering would blow the 10 ms limit —
was unfounded by about two orders of magnitude.

## How it was measured

Timing a Worker from inside itself does not work: `Date.now()` in a Worker is deliberately
coarse and advances only on I/O, to defeat timing attacks. So CPU is measured by compiling
the same render code to `wasm32-unknown-unknown` and timing it under Node, which embeds the
same V8 that workerd does. Same wasm, same JIT, same engine class.

This is a proxy, not production truth. Production hardware differs from this container, and
workerd's isolate is not Node's. It is accurate enough for the question M0 asks — *is this
near 10 ms, or nowhere near?* — and the answer has enough margin that the imprecision does
not matter. **Confirm on real hardware before relying on the exact figures.**

Correctness is verified separately and for real: the Worker runs under `wrangler dev` against
a local SQLite-backed D1, and the rendered page's post order is diffed against
`SELECT id FROM post ORDER BY path`.

## Read path scaling

Assembling a page from post HTML that was rendered at write time (DESIGN.md §3.3):

| posts | page KB | p50 ms | p99 ms | p99 as % of 10 ms |
|---|---|---|---|---|
| 50 | 35.5 | 0.017 | 0.050 | 0.50% |
| 200 | 142.5 | 0.048 | 0.109 | 1.09% |
| 500 | 360.0 | 0.124 | 0.235 | 2.35% |
| 1000 | 723.7 | 0.293 | 0.548 | 5.48% |
| 2000 | 1434.8 | 0.567 | 0.990 | 9.90% |

Linear in post count, as expected for a single pass over pre-rendered fragments. CPU only
approaches the budget at ~2000 posts per page — by which point the page is a 1.4 MB HTML
document and **response size, not CPU, is the reason to paginate**. The 200-post page size in
DESIGN.md is comfortable; the practical ceiling is set by what a phone should download.

## Write path

Markdown → sanitized HTML via pulldown-cmark + ammonia, paid once per post at submit time:

- **0.039 ms per post** (p50), 0.046 ms (p99)
- 200 posts in one batch: 7.8 ms

A single post submission spends ~0.04 ms rendering. Even a full cold rebuild of a 200-post
thread fits inside one request's budget, which makes the "no baked object → render from D1,
bake, return" cold path in DESIGN.md §3.3 viable as a fallback rather than a cliff.

## Nesting depth

200 posts, identical bodies, varying only tree shape:

| profile | mean depth | max | path bytes | render p50 | path parse (200) | sort (200) |
|---|---|---|---|---|---|---|
| flat | 0.00 | 0 | 800 B | 0.046 ms | 11.9 µs | 3.5 µs |
| shallow | 1.37 | 10 | 2,170 B | 0.051 ms | 15.4 µs | 4.4 µs |
| mixed | 2.85 | 13 | 3,650 B | 0.052 ms | 21.4 µs | 4.8 µs |
| deep | 10.54 | 31 | 11,345 B | 0.057 ms | 53.7 µs | 12.7 µs |

**Nesting is nearly free for CPU.** A 10x increase in mean depth costs 24% more render time,
and the tree operations themselves (parse, build, sort, ancestor tests) are all tens of
microseconds for a whole page.

What depth *does* cost is **storage**: path text grows 14x from flat to deep. Since the
`(thread_id, path)` index is the entire read-path mechanism and D1's free ceiling is 500 MB,
path encoding is worth optimising — which is what prompted the change below.

## Path encoding: decimal-6 → base32-4

The original sketch used six zero-padded decimal digits per path segment. Measured against
the alternatives:

| encoding | bytes (deep profile) | siblings/level | note |
|---|---|---|---|
| decimal-6 | 15,963 B | 1,000,000 | original |
| **base32-4** | **11,345 B (−29%)** | **1,048,576** | adopted |
| base62-4 | 11,345 B (−29%) | 14,776,336 | same size, case-sensitive |
| base62-3 | 9,036 B (−43%) | 238,328 | too few siblings |

**base32-4 strictly dominates decimal-6**: 29% smaller *and* slightly more capacity. After
adding a compile-time decode table it is also marginally faster to parse (53.7 µs vs 61.1 µs
on the deep fixture), so there is no axis on which the old encoding wins.

Base62 was rejected despite its headroom. At this width it stores the same four bytes as
base32, so the extra capacity buys nothing, and it costs case sensitivity: any URL
normaliser, `NOCASE` collation, or stray `to_lowercase()` would silently destroy the ordering
invariant the whole read path depends on. Base32 (Crockford, excluding `I`/`L`/`O`/`U`) is
closed under case folding and unambiguous when read aloud.

The ordering invariant is property-tested — `lexicographic_order_equals_preorder` and
`subtree_range_is_exact` in `crates/core/src/path.rs`. Those tests were written before the
encoding change and passed unmodified afterwards, which is the whole reason they exist.

## ID scheme: integer vs alphanumeric

Measured on 20k synthetic threads in local D1 — 5000 point lookups, best of 5:

| scheme | lookup | storage (20k ids) | query plan |
|---|---|---|---|
| `INTEGER PRIMARY KEY` | **2 ms** | ~60 KB | `SEARCH USING INTEGER PRIMARY KEY (rowid=?)` |
| random text id (uuid/nanoid) | **4 ms** | 320 KB | `SEARCH USING INDEX (public_id=?)` |
| time-sortable text id (ULID/KSUID) | **2 ms** | 320 KB | `SEARCH USING INDEX (public_id=?)` |

Two things fall out:

1. **Sortability matters more than the alphabet.** A time-sortable id costs the same as an
   integer; a random one costs 2x, because random keys scatter across the index instead of
   appending at its right edge. If notespace adopts alphanumeric ids, they must be
   time-prefixed. A bare UUID or nanoid is the one option measurably worse than what we have.
2. **Text ids cost ~5x the key storage** and add one B-tree descent, since the row must still
   be reached via the index rather than being the rowid itself.

### Recommendation: two-tier ids

Keep `INTEGER PRIMARY KEY` internally and add an opaque, time-sortable base32 `public_id`
used only in URLs.

Rationale:

- Foreign keys stay integers. `post.thread_id`, `post.author_id` and every future
  `signal.target_id` appear on every row and in every index; widening them from ~3 bytes to
  16 is the single most expensive change available against a 500 MB ceiling, and it buys
  nothing, because those columns are never user-visible.
- The extra index probe is paid **once per pageview**, on the thread lookup — not once per
  post. Against ~400 rows read for a 200-post page, that is under 0.3%.
- Public ids are worth having: URLs stop being enumerable (`/t/1..N` no longer reveals forum
  size or lets a scraper walk everything), and ids survive the phpBB/Discourse imports in M5,
  where colliding integer sequences from separate source forums are otherwise a real problem.
- The materialized path is unaffected either way: path segments are **per-level sibling
  ordinals, not ids**. This is worth stating explicitly, because it is the thing that makes
  the two questions independent.

Posts need addressing too (`POST /p/{id}/signal` in DESIGN.md §7). Cheapest option is to
address them within their thread — `/t/{thread_pub}/p/{n}` — so that a global unique index
over every post's public id is never needed. **Implemented since:** threads now carry a 16-character lowercase public id
(48-bit ms + 32 random), served at `/t/{public_id}`. Posts are addressed within their thread,
so no global post-id index exists. The public id resolves inside the posts query via a scalar
subquery, which measured as `SEARCH thread USING COVERING INDEX idx_thread_public_id` — served
from the index without touching the table, and keeping both statements in one `batch()`.

Widening the id later stays in base32 and works cleanly, provided the 48-bit timestamp stays in
the top bits and extra characters are *appended* at the bottom. Every shorter id is then a literal
prefix of its wider form, so mixed widths still sort by creation time — verified for 16/18/20/22/26
characters, including widths interleaved per id as they would be mid-deploy. The parser already
accepts 16-26 characters while generation emits 16, so widening needs no parser change and strands
no URLs. The one thing to avoid is adopting a *canonical* ULID, whose two padding bits re-align
every character boundary. See DESIGN.md §4.3, backed by tests.

## Public id representation: canonical `String` vs packed `u128`

Both representations were implemented and cross-checked (identical encodings, timestamps, widths
and sort order across mixed widths — 0 mismatches), then timed in wasm under V8 over 2000 ids at
widths 16/18/20/22/25:

| operation (per 2000 ids) | `String` | packed `u128` | packed is |
|---|---|---|---|
| parse, canonical (from D1) | 0.348 ms | 0.136 ms | **2.56x faster** |
| parse, messy (from URL) | 0.359 ms | 0.157 ms | **2.28x faster** |
| encode | 0.059 ms | 0.346 ms | 5.89x slower |
| `timestamp_ms` | 0.058 ms | 0.012 ms | **5.00x faster** |
| sort | 0.033 ms | 0.032 ms | about the same |
| **page mix, as the template runs** | **0.673 ms** | 0.995 ms | 1.48x slower |

**Decision: keep the canonical `String`, and expose the integer form as an accessor.**

The last row is the one that decides it. A thread page parses two ids (one from the URL, one
from D1) and renders one several times — canonical URL, RSS link, pager, personalisation hook.
The template renders through `Display`, which *borrows* from the string form but forces the
packed form to materialise a `String` every time. Packed wins every isolated operation that
matters except encoding, and loses the mix because encoding is what a page does most.

Measuring `.encode()` instead of the template's `Display` reverses none of this but does flatter
the packed form (1.27x slower rather than 1.48x); the table above uses the honest comparison.

**Neither choice is a performance decision.** The whole difference is **0.161 µs per request** —
0.0016% of the 10 ms CPU budget. The real reasons to prefer the string form are that `Ord` is
plain string comparison, which is automatically correct across mixed widths where a width-aware
integer comparison is fiddly and easy to get subtly wrong, and that it cannot overflow.

What the packed experiment did change: `PublicId` now uses `u128` as its internal *primitive*
even though it stores the string. `to_u128` and `from_u128` are exact at every width, and
`timestamp_ms` and `random` derive from them rather than walking the string bit by bit — which
made `timestamp_ms` 3.4x faster than the original implementation.

## The widest width, and why the spare bits sit at the bottom

Making the integer form exact needs the payload to stop at 128 bits, but 26 characters is 130
bits of base32 space. Those two spare bits have to go somewhere, and the choice is load-bearing:

| | 26-char rendering | chars shared with the 16-char id |
|---|---|---|
| top-aligned (canonical ULID) | `01jgfjjz00krvqkebz99y1a4hm` | **1** |
| bottom-aligned (this format) | `06a1yabw02f3eydsfx57r58j6g` | **16** |

Canonical ULID pads at the top, which shifts every character boundary along by two and destroys
the prefix relationship the whole widening story rests on. Reserving at the bottom keeps the
48-bit timestamp in the top bits at every width, so a 16-character id stays a literal prefix of
a 26-character one. `parse` rejects a 26-character id whose reserved bits are set — such an id
was never issued here, and the most likely cause is someone pasting a canonical ULID.

The payload at 26 characters is then 48 bits of timestamp and **80 of randomness — exactly a
ULID's budget**, since the two bits ULID spends on top padding are the two reserved at the bottom.

## Importing a ULID or UUIDv7

`from_ulid` / `from_uuid` / `from_u128_payload` import a full 128-bit id; `to_ulid` / `to_uuid`
render it back. `cargo run -p notespace-core --example ids` prints the round trip:

```
UUIDv7  source text   01890a5d-ac96-774b-bcce-b302099a8057
        stored as     064gmqdcjsvmqf6epc10k6m0aw   <- re-aligned, still one of ours
        to_u128()     0x01890a5dac96774bbcceb302099a8057
        timestamp_ms  1688096058518
        back to UUID  01890a5d-ac96-774b-bcce-b302099a8057
```

**The value round-trips exactly; the text does not.** An imported id keeps all 128 bits and its
real creation time — both formats put a 48-bit millisecond timestamp in the top bits — but it is
stored re-aligned, so its rendered form is not the canonical ULID string. That is the deliberate
trade: an exact value *and* prefix-stable widening, rather than byte-identical text. Imported ids
sort and interleave correctly with natively generated ones, which is what the M5 import path
(§8) needs and what top-alignment would have cost.

The packed implementation is kept in `crates/bench-wasm/src/packed.rs` so the comparison stays
reproducible via `./scripts/spike.sh`. If a future workload ever became parse-heavy and
render-light — many ids loaded and sorted but rarely rendered — the tradeoff would flip. Posts
deliberately have no public ids, so that workload does not currently exist.

## D1 access

The thread page is **two statements in a single `batch()` round trip**:

```
SEARCH p USING INDEX idx_post_thread_path (thread_id=? AND path>?)
SEARCH u USING INTEGER PRIMARY KEY (rowid=?)
```

An indexed range scan, with no `USE TEMP B-TREE FOR ORDER BY` — SQLite walks the index in
order and stops at `LIMIT`. Confirmed against the trace, which shows exactly **one `d1_batch`
span per request**, not one per post. The N+1 failure mode DESIGN.md §3.1 warns about is
absent, and there is now a query-plan assertion in `scripts/spike.sh` to keep it that way.

### Production numbers

The spike is deployed at `dev.notespace.org`. 40 samples of the 200-post page, read from the
`Server-Timing` header the Worker now emits:

| metric | p50 | p99 | min | max |
|---|---|---|---|---|
| D1 duration | **2.52 ms** | 6.24 ms | 1.34 ms | 6.24 ms |
| statements | 2 | 2 | 2 | 2 |
| rows_read | 404 | 404 | 404 | 404 |

Three things this settles:

- **`rows_read` is 404 in production, exactly as it is locally.** Cloudflare's accounting and
  miniflare's agree, so the local number was never a proxy — it was the answer.
- **The batch holds under a real network.** `statements=2` on every sample. Had the read path
  been the N+1 shape DESIGN.md §3.1 warns about, 201 round trips at 2.52 ms would be **0.5 s of
  D1 wait per pageview**. That single design decision is worth half a second a page.
- **D1 latency is not the risk it looked like.** ~2.5 ms is a round trip, not CPU: Workers bills
  CPU time and excludes time blocked on I/O, so this does not eat into the 10 ms budget that
  M0 existed to protect.

Not measured here: actual billed CPU in production, which is visible in the Worker's dashboard
metrics rather than in a response header. End-to-end latency was sampled at ~320 ms p50, but
from a remote container through a proxy to a WNAM database — that number describes the test
harness, not a reader.

### Rows read

**Measured: 404 rows per uncached 200-post pageview, locally and in production.** The Worker now reports D1's own
`rows_read` and `duration` on every response via `Server-Timing` (see `QueryStats` in
`crates/worker/src/store.rs`), so this is observed rather than inferred.

An earlier revision of this document claimed D1 reports `rows_read` only in production and
estimated ~403 from the query plan. The first half was wrong — miniflare populates it locally
too — and the estimate turned out to be right to within one row:

- posts query: ~200 post rows + ~200 `user` rowid lookups
- thread query: ~3 rows (thread + author + space)
- estimated **~403**, measured **404**

Against the 5M rows/day free allowance that is ~12,400 such pageviews/day. Most real threads
are far smaller: at ~30 posts a page costs ~65 rows, or ~77,000 pageviews/day — at which
point the **100k Worker requests/day cap binds first**.

So for a small forum the free tier is limited by request count, not by CPU or by D1. Confirmed
against the deployed instance: Cloudflare reports the same 404.

**Cheap win available:** denormalising `author_name` onto `post` would drop the `user` join
and roughly halve rows read per page. Not done yet — it trades correctness-on-rename for
budget, and now has a real `rows_read` figure to be decided against: the `user` join is
roughly half of the 404.

## What this means for the free tier

For a small forum, the free plan is comfortable:

- **CPU**: a non-issue. ~1% of budget per page.
- **Script size**: a non-issue. 4.5% of the limit, including a full HTML sanitizer.
- **Binding constraint**: 100k Worker requests/day, which is roughly 50–65k pageviews once
  the personalisation fetch in DESIGN.md §3.3 is counted. That is a healthy small community.
- **Next lever**: the R2 baking in §3.3/M6 removes D1 reads entirely on a cache hit, moving
  the ceiling up to the request cap.

## Incidental findings

- **`ammonia`/`html5ever` compile to wasm cleanly** and cost little: the whole bundle,
  sanitizer included, is 139.6 KB gzipped. This closes DESIGN.md open question #2 — no
  hand-rolled Markdown-AST sanitizer is needed.
- **`strip = true` breaks the wasm build.** Stripping removes the externref table, after
  which `wasm-bindgen` fails with `externref table required for catch wrappers`. `worker-build`
  runs `wasm-opt` afterwards and strips properly, so the setting is left off with a comment
  in `Cargo.toml` explaining why. This costs an hour to rediscover; it is written down.
- **D1 rejects JS bigints.** i64 bind parameters must cross as f64
  (`D1_TYPE_ERROR: Type 'bigint' not supported`). Exact for ids below 2^53, which is far past
  what a 500 MB database can hold.
- **`axum` works on Workers** via `worker` 0.8's `http` feature, with `#[worker::send]` on
  handlers to satisfy axum's `Send` bound. M2's assumption holds.

## Not covered by this spike

Deliberately out of scope, and still open:

- Real Cloudflare deployment. Everything here is `wrangler dev --local`; no measurement
  against production D1, real network latency, or actual `rows_read` billing.
- Writes, auth, sessions. Read path only.
- R2 baking, cache versioning, Durable Object promotion (M6).
- FTS5 on D1 (open question #4) — untested.
- Concurrency and cold-start behaviour under load.
