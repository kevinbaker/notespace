# notespace — Design Document

> A configurable, AI-moderated, threaded forum in Rust that deploys to a free Cloudflare
> account *or* runs as a single self-hosted binary.
>
> notespace.org · dev instance: dev.notespace.org

Status: draft v0.2 · M0 spike complete, measurements folded in · Target: greenfield implementation
Audience: implementers (human and agentic). This file is intended to be read by Claude Code as
project context. Keep it updated; it is the source of truth for architecture decisions.

**Changes in v0.2** (all driven by the M0 spike — see `docs/M0-findings.md` for the numbers):
§3.2 budget table annotated with measured values · §4 path encoding changed from decimal-6 to
base32-4 · §4 new subsection on the id scheme · §8 M0 marked complete · §10 open questions 1, 2
and 7 closed.

---

## 1. Why this exists

The forum landscape is stagnant. Discourse is powerful but demands Postgres, Redis, Sidekiq and a
~2GB RAM floor. Flarum has spent roughly three years in the 2.0 release-candidate cycle. phpBB,
MyBB and SMF are mature but dated. Lemmy is the only serious Rust discussion platform, but it is a
federated link aggregator built as a 40-crate workspace, not a general-purpose board. Recent
innovation is happening in Go and TypeScript (Storyden, Atrium, various single-binary community
platforms).

Nobody has built a forum that is simultaneously:

1. **Threaded and configurable** enough to impersonate a classic bulletin board, Hacker News,
   Slashdot, or a Reddit-style community.
2. **Deployable to a free tier** with no server to manage.
3. **AI-moderated as a first-class primitive**, not a bolted-on plugin.

That is the gap.

### Goals

- Single codebase, two deploy targets: Cloudflare Workers (free tier viable) and a self-hosted
  static binary.
- Server-rendered HTML. Fast on bad phones. Works without JavaScript for reading.
- Moderation that scales down to a solo admin running a 200-person community.
- Presets that reconfigure the forum's personality without forking the code.

### Non-goals (v1)

Federation/ActivityPub · real-time chat as a primary mode · plugin marketplace · WYSIWYG editor ·
mobile native apps · multi-tenancy in a single instance.

Say no to these loudly and early. Every one of them has killed a forum project.

---

## 2. Design principles

**Eight orthogonal primitives, not forty features.** Every feature harvested from Discord, Reddit,
HN, Slashdot, XenForo and Discourse maps onto one of these. If a proposed feature does not, either
it is genuinely new (rare) or the model is wrong (more likely). Interrogate it.

| # | Primitive | Subsumes |
|---|---|---|
| 1 | **Space** — board/sub/channel, owns permissions + ranking fn | phpBB forums, subreddits, Discord channels, Discourse categories |
| 2 | **Thread** — typed: discussion, link, question, poll, announcement | HN submissions, Reddit posts, SE questions, phpBB topics |
| 3 | **Post** — optional parent; depth cap is per-space config | Flat boards (cap 0), Reddit/Slashdot trees (uncapped) |
| 4 | **Signal** — typed, weighted reaction with optional reason tag | HN upvote, Slashdot labeled mod, Reddit up/down, Discourse likes, Discord reactions, XF ratings, **reports** |
| 5 | **Ranking function** — pluggable per space | Bump order, HN gravity, Wilson/best, score+threshold |
| 6 | **Capability** — granted by role *or* earned by trust | HN karma gates, Discourse trust levels, XF groups, Discord roles |
| 7 | **Action log entry** — immutable, optionally public | Lemmy modlog, Discord audit log, XF warning points, appeals |
| 8 | **Rule** — condition → action, evaluated on write | AutoModerator, XF spam cleaner, Discord AutoMod, **AI moderation** |

Two consequences worth internalising:

- A **report is a Signal**, not a separate table. Negative-weight type, routed to the review queue.
- **AI moderation is a Rule** whose predicate happens to call an LLM. There is no "AI subsystem".

**Metamoderation is the differentiator.** Slashdot's real insight was that moderator quality is
itself scoreable. Applied to AI: humans rate the model's calls, and those ratings tune per-space
thresholds over time. This is the answer to "why should I trust your AI mod?" — build it in v1,
not v3.

---

## 3. Architecture

### 3.1 Crate layout

```
notespace/
├── crates/
│   ├── core/          # domain types, no I/O, no wasm/native awareness. Pure logic.
│   │   ├── model.rs   # Space, Thread, Post, Signal, Capability, ActionLog, Rule
│   │   ├── path.rs    # materialized tree paths            [M0: built]
│   │   ├── rank.rs    # ranking functions (pure, unit-testable)
│   │   ├── trust.rs   # capability resolution
│   │   └── store.rs   # trait Store — THE seam             [M0: built, thread_page only]
│   ├── render/        # markdown -> sanitized HTML, page templates (maud)   [M0: built]
│   ├── rules/         # rule engine + predicates (incl. LLM predicate behind a trait)
│   ├── web/           # axum router, handlers, session mw — generic over Store
│   ├── store-d1/      # wasm adapter                       [M0: lives in worker/, to extract]
│   ├── store-sqlite/  # native adapter: rusqlite or sqlx-sqlite
│   ├── worker/        # cdylib. Cloudflare entrypoint. Wires web + store-d1.  [M0: built]
│   ├── server/        # bin. Native entrypoint. Wires web + store-sqlite + embedded assets.
│   ├── seed/          # native bin. Deterministic seed/fixture generator.     [M0: built]
│   └── bench-wasm/    # cdylib. Exposes render paths to a Node CPU harness.   [M0: built]
├── migrations/        # plain .sql, dialect-identical for both targets
├── scripts/           # spike.sh — reproduces the M0 measurements
└── docs/              # M0-findings.md
```

M0 deliberately put the D1 `Store` implementation inside `crates/worker/` rather than a
separate `store-d1` crate. One implementation does not need a crate boundary to prove the
seam works; extract it in M1 when `store-sqlite` arrives and there is a shared conformance
suite to run against both.

**The `Store` trait is the most important design decision in this document.** Everything above it is
target-agnostic. Get it wrong and the dual-target promise dies.

```rust
#[async_trait(?Send)]  // ?Send is required: wasm futures are not Send
pub trait Store {
    async fn thread_page(&self, thread: ThreadId, page: Page) -> Result<ThreadPage>;
    async fn insert_post(&self, post: NewPost) -> Result<Post>;
    async fn apply_signal(&self, sig: NewSignal) -> Result<SignalTally>;
    // ...
}
```

Constraints on every implementation:
- Identical SQL dialect (SQLite) on both sides. No Postgres-isms, ever.
- **No N+1.** D1 free tier allows only 50 queries per Worker invocation. A thread page must be
  1-3 queries, not one-per-post.
- No connection pool on the wasm side (sqlx's pool needs a Tokio runtime that does not exist there).

### 3.2 Target constraints (Cloudflare free tier)

These numbers drive the architecture. Verify against live docs before relying on them.
The **Measured** column is from the M0 spike (`docs/M0-findings.md`); blank means untested.

| Constraint | Free | Measured (M0) | Consequence for us |
|---|---|---|---|
| Worker CPU / request | 10 ms | **0.048 ms** p50 / 200-post page | Render at write time, never at read time |
| Worker requests | 100k/day | — | Bake and cache aggressively |
| Worker script size | 3 MB | **139.6 KB** gzipped | Watch wasm bloat; audit every dependency |
| Subrequests | 50/request | — | No fan-out designs |
| D1 queries / invocation | 50 | **2**, one batched round trip | Materialized path, not recursive CTE walks |
| D1 database size | 500 MB | — | Archive strategy needed eventually |
| D1 rows read/written | 5M / 100k per day | ~403/page *(derived, not observed)* | Index everything; baked pages read zero rows |
| Durable Object requests | 100k/day (**separate** from Worker requests) | — | A DO hit per pageview halves the effective budget |
| DO duration | 13,000 GB-s/day | — | Use WebSocket Hibernation API |
| DO SQLite storage | Not billed on free plan | — | Cheap to use for hot state |
| R2 | 10 GB, 1M Class A (writes), 10M Class B (reads) | — | Direct-to-R2 uploads; baked pages live here |
| Workers AI | 10,000 neurons/day, shared across models | — | The binding constraint. Small models only. Heuristics first. |
| Queues | 10k ops/day, 24h retention | — | Async moderation pipeline |

**What M0 changed about this table.** CPU was assumed to be the risk; it is not, by two
orders of magnitude. For a small forum the binding constraint is **100k Worker requests/day**
(~50–65k pageviews once the personalisation fetch is counted), with D1 rows read a close
second on very long threads. Design effort is better spent on reducing *request count* than
on shaving CPU. Note that `rows_read` is reported only in production — the figure above is
derived from the query plan and still needs confirming against a real D1 instance.

Everything must compile to `wasm32-unknown-unknown`. **No Tokio, no `async_std`, no threads, no
`std::time::SystemTime`, no filesystem.** Audit dependencies for this before adding them. The
`time` crate needs its `wasm-bindgen` feature.

### 3.3 Read path — bake, don't render

Reads dominate a forum by two orders of magnitude. The read path must be nearly free.

```
WRITE:  post submitted
          -> sanitize + render markdown to HTML (once, ~sub-ms)
          -> persist markdown AND rendered HTML
          -> append rendered fragment to baked page object in R2/KV
          -> bump cache version pointer
          -> enqueue moderation job

READ:   GET /t/{id}
          -> Cache API hit -> return baked HTML. Zero D1 reads, ~zero CPU.
          -> miss -> fetch baked object from R2 -> return, populate cache
          -> no baked object -> render from D1, bake, return (cold path only)
```

Rules for this to work:

- **Baked HTML is user-agnostic.** No usernames, no vote state, no unread markers in the baked
  blob, or you lose all cache sharing. Personalisation is layered by one small JSON fetch
  (`GET /api/me/thread/{id}`) and a client-side patch, or by a signed cookie read at the edge.
- Threads are append-only in the common case, so appending a fragment beats regenerating a page.
- Edits and deletions invalidate by bumping a version integer held in KV or a DO, not by issuing
  cache purges.

### 3.4 Hot threads and Durable Objects

DO-per-thread is attractive but not free (see the request budget above). Use a promotion model:

- **Cold** (default, 95% of threads): D1 + baked R2 objects. No DO involvement at all.
- **Hot** (recent write activity above a threshold): promoted into a per-thread Durable Object
  holding a SQLite tail of recent posts, presence, sequence numbers, and hibernatable WebSockets
  for live updates. Demoted back to cold on an alarm after inactivity.

Note the 20:1 billing ratio on inbound WebSocket messages, and use `state.acceptWebSocket()` (the
hibernation API) rather than plain event listeners so idle rooms cost nothing.

### 3.5 What the client may and may not do

- **May**: render live markdown preview locally (free, instant), do optimistic UI, upload image
  bytes directly to R2 via a Worker-issued scoped URL (bytes never touch Worker CPU or the request
  body limit).
- **Must not**: submit HTML for storage. Client-generated HTML persisted and served to other users
  is stored XSS. The client is the attacker. Always sanitize server-side.
- Note that clients cannot reach a Durable Object directly — a Worker is always in the path. Good:
  there is always a chokepoint to sanitize at.
- Verify uploaded files by magic bytes server-side; never trust a declared content type.

---

## 4. Data model

Sketch, not final. SQLite dialect, valid on both targets.

```sql
CREATE TABLE space (
  id            INTEGER PRIMARY KEY,
  slug          TEXT NOT NULL UNIQUE,
  name          TEXT NOT NULL,
  parent_id     INTEGER REFERENCES space(id),
  ranking       TEXT NOT NULL DEFAULT 'bump',   -- bump|gravity|best|score_threshold
  depth_cap     INTEGER NOT NULL DEFAULT 8,     -- 0 = flat board
  config        TEXT NOT NULL DEFAULT '{}'      -- JSON: preset overrides
);

CREATE TABLE thread (
  id            INTEGER PRIMARY KEY,
  space_id      INTEGER NOT NULL REFERENCES space(id),
  kind          TEXT NOT NULL,                  -- discussion|link|question|poll|announcement
  title         TEXT NOT NULL,
  url           TEXT,                           -- link threads
  author_id     INTEGER NOT NULL REFERENCES user(id),
  created_at    INTEGER NOT NULL,
  bumped_at     INTEGER NOT NULL,
  score         REAL NOT NULL DEFAULT 0,
  rank          REAL NOT NULL DEFAULT 0,        -- denormalized, recomputed on write
  state         TEXT NOT NULL DEFAULT 'visible',-- visible|locked|pinned|hidden|deleted
  cache_version INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_thread_space_rank ON thread(space_id, state, rank DESC);

CREATE TABLE post (
  id            INTEGER PRIMARY KEY,
  thread_id     INTEGER NOT NULL REFERENCES thread(id),
  parent_id     INTEGER REFERENCES post(id),
  path          TEXT NOT NULL,        -- materialized path, e.g. '000C.0004.0001'  (base32)
  depth         INTEGER NOT NULL,
  author_id     INTEGER NOT NULL REFERENCES user(id),
  body_md       TEXT NOT NULL,        -- source of truth
  body_html     TEXT NOT NULL,        -- rendered + sanitized at write time
  created_at    INTEGER NOT NULL,
  edited_at     INTEGER,
  score         REAL NOT NULL DEFAULT 0,
  state         TEXT NOT NULL DEFAULT 'visible' -- visible|pending|hidden|deleted
);
CREATE INDEX idx_post_thread_path ON post(thread_id, path);
```

**The materialized `path` column is load-bearing.** One indexed range scan returns an entire
correctly-ordered thread page. Recursive CTEs would blow the 50-query and 10ms budgets. Pad
segments to fixed width so lexicographic order equals tree order.

### 4.1 Path encoding — base32, four digits per level

Segments are four Crockford-base32 digits (`0-9`, `A-Z` minus `I`/`L`/`O`/`U`), `.`-separated:
`000C.0004.0001`. M0 measured this against the decimal-6 scheme this document originally
sketched, and base32-4 **strictly dominates it** — 29% fewer bytes *and* slightly more capacity
(1,048,576 vs 1,000,000 siblings per level), and marginally faster to parse once a compile-time
decode table is used.

Depth costs storage, not CPU: across the same 200 posts, path text grows 14x from a flat board
to a 31-deep chain, while render time grows 24%. Since the `(thread_id, path)` index *is* the
read path, and the free D1 ceiling is 500 MB, the encoding is worth the 29%.

Base62 was measured and **rejected**. At four digits it stores the same four bytes as base32, so
its extra headroom buys nothing, and it costs case sensitivity — any URL normaliser, `NOCASE`
collation, or stray `to_lowercase()` would silently destroy the ordering invariant. Base32 is
closed under case folding. Do not "upgrade" to base62 later without re-reading this paragraph.

Ordering rests on three facts, all asserted in tests rather than assumed:

1. The alphabet is strictly ascending in ASCII, so bytewise order equals numeric order.
2. The separator `.` (`0x2E`) sorts below every alphabet byte (`0` is `0x30`), so a parent
   precedes its children and a subtree precedes the next sibling.
3. `/` (`0x2F`) sits between them, giving `<path>/` as a tight exclusive upper bound for a
   whole subtree — one range scan, no recursion.

`crates/core/src/path.rs` property-tests these (`lexicographic_order_equals_preorder`,
`subtree_range_is_exact`). Those tests are the guardrail on any future encoding change: they
were written against decimal-6 and passed unmodified against base32.

### 4.2 Id scheme — integer inside, opaque outside

**Path segments are per-level sibling ordinals, not ids.** The path scheme and the id scheme are
therefore independent, and can be decided separately.

Ids are two-tier:

- **Internal: `INTEGER PRIMARY KEY`.** In SQLite this *is* the rowid, so lookups need no
  secondary index. All foreign keys (`post.thread_id`, `post.author_id`, `signal.target_id`)
  stay integers. These appear on every row and in every index; widening them to 16-byte text
  is the most expensive change available against a 500 MB ceiling and buys nothing, because
  they are never user-visible.
- **Public: an opaque, time-sortable base32 `public_id`, `TEXT UNIQUE`, used only in URLs.**

M0 measured the tradeoff on 20k rows — 5000 point lookups, best of five:

| scheme | lookup | key storage (20k) |
|---|---|---|
| `INTEGER PRIMARY KEY` | 2 ms | ~60 KB |
| random text id (uuid, nanoid) | **4 ms** | 320 KB |
| time-sortable text id (ULID, KSUID) | 2 ms | 320 KB |

**Sortability matters more than the alphabet.** A time-prefixed id costs the same as an integer;
a random one costs 2x, because random keys scatter across the index instead of appending at its
right edge. If you add public ids, they must be time-sortable — a bare UUID or nanoid is the one
option measurably worse than plain integers.

Why have them at all: URLs stop being enumerable (`/t/1..N` otherwise reveals the forum's size
and lets a scraper walk every thread), and ids survive the phpBB/Discourse imports in M5, where
colliding integer sequences from separate source forums are otherwise a genuine problem. The
extra index probe is paid once per pageview on the thread lookup, not once per post — under
0.3% of a 200-post page's rows read.

Posts also need addressing (`POST /p/{id}/signal`, §7). Address them *within* their thread —
`/t/{thread_pub}/p/{n}` — so a global unique index over every post's public id is never needed.
At 5M posts such an index would cost ~135 MB, 27% of the free ceiling, for no benefit.

**Format: 16 lowercase Crockford base32 characters** — 48-bit unix-ms timestamp + 32 random bits.
A ULID truncated from 26 characters to 16. Implemented in `crates/core/src/id.rs`.

```
/t/06a1yabw03jnhej1/is-a-rust-forum-on-free-cloudflare-viable
```

Lowercase is canonical because these get typed and read aloud. The alphabet already omits
`I`, `L`, `O` and `U`; on input the parser also accepts either case, folds `I`/`l` to `1` and
`O` to `0`, and ignores grouping hyphens, so `06a1-YABW-O3jn-heJ1` resolves. Anything
non-canonical gets a 301 to the canonical spelling, so a thread has exactly one cacheable URL
rather than one per way of typing it.

32 random bits is ample because collisions are only possible *within* a millisecond: the
birthday bound is ~65,536 ids/ms for a 50% chance, against a forum's realistic ~0. And they are
caught, not silent — `public_id` is UNIQUE, so a collision is a failed insert to retry.

**Resolving a public id must not cost a round trip.** The URL carries `public_id` but posts key
off the integer `thread_id`, so the naive implementation looks up the thread, then queries posts
— two round trips, undoing §3.1's batching. Instead the posts query resolves it inline:

```sql
WHERE p.thread_id = (SELECT id FROM thread WHERE public_id = ?1) AND p.path > ?2
```

Measured plan: `SEARCH thread USING COVERING INDEX idx_thread_public_id`. The subquery is served
entirely from the index without touching the thread table, and both statements stay in one
`batch()`.

### 4.3 Widening the id later

If 32 random bits ever stops being enough, the id widens **while staying base32** — and this
works cleanly, provided one rule is followed:

> Keep the 48-bit timestamp in the **top** bits and append whole base32 characters at the
> **bottom**. Never re-align the payload.

Under that rule every shorter id is a *literal prefix* of its wider form, prefix order is time
order, and mixed-width ids sort correctly with no special handling:

```
same timestamp, three widths
  16 chars ( 80 bits, 32 random)   06a1yabw00000000
  20 chars (100 bits, 52 random)   06a1yabw000000000000
  26 chars (128 bits, 80 random)   06a1yabw0000000000000000000
```

26 characters is the ceiling: 130 bits of encoding space carrying a 128-bit payload, with the two
spare bits **reserved at the bottom and always zero**. That cap makes `PublicId::to_u128` total
and exact, and it means a full ULID or UUIDv7 imports without loss — at 26 characters the payload
is 48 bits of timestamp and 80 of randomness, exactly a ULID's budget.

Bottom is the only place the spare bits can go. Canonical ULID puts them at the top, which shifts
every character boundary and destroys the prefix relationship (1 character in common with a
16-character id, against 16 when bottom-aligned).

`from_ulid` / `from_uuid` / `to_ulid` / `to_uuid` cover the M5 import path. The *value* round-trips
exactly and an imported UUIDv7 keeps its real creation time; the *text* does not, since the id is
stored re-aligned. Exact value plus prefix-stable widening, rather than byte-identical text.

`crates/core/src/id.rs` already parses any width in 16-26 characters even though this build only
generates 16, so **a future instance can widen its generated ids with no change to the parser and
no stranded URLs**. Making the generated width a per-instance setting is therefore a small change:
generation picks a width, parsing already accepts them all. `mixed_width_ids_sort_by_creation_time`
covers the realistic rollout case where widths interleave mid-deploy rather than changing cleanly.

**The one thing to avoid is adopting a canonical 26-character ULID.** ULID packs 128 bits into
130 bits of base32 space, so it carries two leading padding bits, and that offset shifts every
character boundary. Ids in the two formats then share no prefix even for the same millisecond,
and a mixed set stops sorting by time:

```
  native   (16)   06a1yabw02f3eyds
  top-aligned     01jgfjjz00krvqkebz99y1a4hm   <- canonical ULID:  1 char in common
  bottom-aligned  06a1yabw02f3eydsfx57r58j6g   <- ours:            16 chars in common
```

That is a difference of encoding alignment, not of alphabet — both are base32. Widening our own
format is safe; swapping in someone else's 128-bit layout is not.

Worth knowing before treating even that as a hard constraint: **nothing currently reads
cross-format sort order.** Feeds order by `created_at`/`bumped_at`, never by id. What the time
prefix actually buys is index insert locality, and every candidate keeps that, because all of them
put the timestamp in the high bits. A hard switch to canonical ULID would still work
operationally; it would only forfeit a property nothing uses. The append rule is what keeps the
option open for free.

## 5. Moderation pipeline

Runs asynchronously off the request path. You do not have 10ms to spare for an LLM call.

```
post write
  -> Tier 0: free heuristics, inline (~0 cost)
       rate limit · link count · account age · duplicate content hash · blocklist
       -> most spam dies here, zero neurons spent
  -> if new user OR heuristic-flagged: state = 'pending', enqueue
  -> Queue consumer:
       Tier 1: small-model classifier (Workers AI, 8B class)
       -> verdict + confidence + categories -> action_log (actor_kind='model')
       -> confident-clean: publish · confident-bad: hide + review · unsure: review
  -> Human review queue: one-key approve/reject/escalate
  -> Metamoderation: reviewer agreement/disagreement feeds back into per-space thresholds
```

Non-negotiable properties:

- **The model triages; it never delivers a final verdict.** Auto-hide pending review, never
  auto-delete.
- Every model call is logged to `action_log` with its inputs, verdict and confidence.
- Public modlog by default (Lemmy got this right).
- Appeals path exists from day one.
- Provider is pluggable behind a trait: Workers AI by default, bring-your-own-key (Anthropic,
  OpenAI) via AI Gateway for anyone wanting better judgment. Never hard-code a vendor.

Budget reality check: 10,000 neurons/day shared across all models means an 8B classifier on short
posts is affordable for a small forum and a 70B one is not. Heuristics must catch the bulk.

---

## 6. Presets

A preset is a config bundle over the eight primitives. Ship these in v1:

| Preset | depth_cap | Signals | Ranking | Notes |
|---|---|---|---|---|
| Classic BB | 0 (flat) | none or like | bump | Paged, signatures, thread prefixes |
| Hacker News | unlimited | upvote only; downvote gated by trust | gravity decay | Flagging, `showdead`, reply delay on hot threads |
| Slashdot | unlimited | labeled mods, capped -1..5 | score + reader threshold | Metamod, friend/foe score adjustment |
| Reddit-ish | unlimited | up/down | hot / best / controversial | Per-space rules, flair |
| Q&A | 1 | upvote + accept | votes, accepted first | Accepted answer pinned |
| Team space | 2 | reactions | recency | Private, invite-only, no public modlog |

Presets must be expressible entirely as data. If a preset needs a code change, the primitive model
has a hole in it — fix the model, not the preset.

---

## 7. Routes (sketch)

```
GET  /                          space index
GET  /s/{slug}                  thread list (ranked)
GET  /t/{id}/{slug?}            thread page (baked)
GET  /t/{id}.rss                feed
POST /t/{id}/reply
POST /s/{slug}/new
POST /p/{id}/signal             vote / like / flag  (idempotent)
GET  /api/me/thread/{id}        personalization layer (vote state, unread)
GET  /u/{name}                  profile
GET  /modlog                    public action log
GET  /mod/queue                 review queue (capability-gated)
POST /uploads/sign              issue scoped R2 upload URL
WS   /t/{id}/live               hot threads only, via DO
```

Reading must work with JavaScript disabled. Voting and live updates may require it.

---

## 8. Milestones

**M0 — Spike. COMPLETE.** See `docs/M0-findings.md`; reproduce with `./scripts/spike.sh`.
Rendering a 200-post thread page from D1 costs **0.048 ms** of the 10 ms CPU budget (~1%), in a
**139.6 KB** gzipped bundle (4.5% of the 3 MB limit), using **2 D1 queries** in one batched round
trip. The free-tier premise holds comfortably; CPU was never the risk. Also settled: `ammonia`
compiles to wasm cheaply, `axum` works on Workers, and the binding constraint for a small forum
is the 100k requests/day cap, not CPU or D1.

Not covered, and still open: real Cloudflare deployment (everything was `wrangler dev --local`),
observed `rows_read`, writes, auth, and behaviour under concurrency.

**M1 — Core + schema.** Migrations, `trait Store`, both adapters passing an identical test suite.
Materialized path insert/query logic with property tests.
*(M0 delivered: migration 0001, `trait Store` with `thread_page`, the D1 adapter, and the path
property tests. Remaining: extract `store-d1`, add `store-sqlite`, write the shared conformance
suite, and extend `Store` past reads.)*

**M2 — MVP.** Axum on Workers (`worker` crate's `http` feature makes axum usable), maud templates,
pulldown-cmark + ammonia at write time, htmx for interactions. Auth, sessions, boards, threads,
nested replies, edit/delete with tombstones, RSS. No SPA.

**M3 — Community features.** FTS5 search, notifications, capabilities/trust, signals + ranking
functions, mod actions, public modlog.

**M4 — Moderation.** Rule engine, heuristics, Queue-based AI classification, review queue,
metamoderation.

**M5 — Deploy story.** `wrangler deploy` one-liner, Docker image, single static binary, seed
script, demo instance, import from phpBB/Discourse dumps.

**M6 — Bake + hot threads.** R2 baking, cache versioning, DO promotion, live updates.

---

## 9. Conventions for implementers

- **Every dependency must compile to `wasm32-unknown-unknown`.** Check before adding. This rules
  out most of the obvious crates. Prefer `getrandom` with the wasm feature, `time` with
  `wasm-bindgen`, avoid anything touching threads, sockets or the filesystem.
- Keep `core/` free of I/O so it is testable without a database or a network.
- Two test suites: one shared conformance suite run against both `Store` implementations, and pure
  unit tests for ranking/trust/path logic.
- No `unwrap()` outside tests. Errors are typed; the wasm target panics badly.
- Prefer one big query over three small ones — D1 counts queries, not rows returned.
- Any change that adds a per-request D1 query or per-request DO call needs justification against
  the budget table in §3.2.
- Migrations are plain SQL, forward-only, numbered.

## 10. Open questions

### Closed

1. **License — MIT.** Decided before the first commit. Chosen for adoption over the
   hosted-SaaS-fork protection AGPL would give; see `LICENSE`.
2. **Does `ammonia`/`html5ever` compile cleanly to wasm at an acceptable size? — Yes.** M0
   measured the whole Worker bundle, sanitizer included, at **139.6 KB gzipped**: 4.5% of the
   3 MB limit. No hand-rolled Markdown-AST sanitizer is needed. `crates/render/src/markdown.rs`
   uses `ammonia` with a tight allowlist and an XSS test suite.
7. **Project name — notespace.** notespace.org, with a dev instance at dev.notespace.org.

### Open

3. Auth: sessions in D1, in a DO, or signed stateless cookies? Stateless is cheapest but
   complicates revocation.
4. Search on the wasm target — does D1 expose FTS5? If not, an external index or a native-only
   feature flag is needed. **Untested in M0.**
5. Archive strategy when a D1 database approaches the 500 MB free ceiling.
6. Email: needed for notifications and password reset, but there is no SMTP from a Worker.
   Requires a third-party API, which is a dependency on someone else's free tier.

### Raised by M0

8. **Denormalise `author_name` onto `post`?** The `user` join roughly doubles rows read per
   thread page. Denormalising would halve it, at the cost of a rewrite on username change.
   Decide once `rows_read` has been observed on a real D1 instance rather than derived.
9. **Confirm the derived numbers in production.** `rows_read` is production-only, and all M0
   CPU figures come from wasm-under-Node as a proxy for wasm-under-workerd. The margins are
   large enough that the conclusion is safe, but the figures themselves are not yet firm.
10. **Post addressing.** `/p/{id}` (needs a global unique index over every post's public id) vs
    `/t/{thread_pub}/p/{n}` (needs none). §4.2 prefers the latter; settle it in M2.
