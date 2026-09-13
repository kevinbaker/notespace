# notespace — Design Document

> A configurable, AI-moderated, threaded forum in Rust that deploys to a free Cloudflare
> account *or* runs as a single self-hosted binary.
>
> notespace.org · dev instance: dev.notespace.org

Status: draft v0.2 · M0 spike complete, measurements folded in · Target: greenfield implementation
Audience: implementers (human and agentic). This file is intended to be read by Claude Code as
project context. Keep it updated; it is the source of truth for architecture decisions.

**The three documents, and which one to open.** This file says what the system should be.
[`IMPLEMENTATION.md`](IMPLEMENTATION.md) maps each section below to the code that implements it,
and records what is not built yet. [`DECISIONS.md`](DECISIONS.md) holds the reasoning behind the
code — measurements, alternatives tried and rejected, bugs that shaped a design.

References run in one direction only: these documents point into the code, and the code points
at neither. A source file that needs a second document open to be read will drift from it.

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

### 4.4 Three different things were called "slug"

They are not the same, and one type for all three was wrong for all three:

| | resolves the thing? | mutable? | unique? | type |
|---|---|---|---|---|
| thread title text, `/t/{id}/{slug}` | **no** — decorative | freely | never | *none needed* |
| username, `/u/testuser` | yes | **never** | globally | `core::username::Username` |
| space name, `/s/sports/hockey` | yes | yes, with a redirect | per parent | `core::space_key::SpaceKey` |

The first is not modelled at all. `/t/{id}/anything-at-all` serves the same thread because the id
resolves it; the trailing text is for readers and search engines. Retitling cannot break a link.

The other two resolve, so they are validated — but their policies differ enough that they are
**separate types with separate reserved lists**. They share only the character rules, and only
because duplicating a charset check is how two of them drift apart.

Normalization is **case, and only case**. `test-user` and `testuser` are two different names, as
they are on GitHub. The stored text is already canonical, so there is no `*_canonical` column
anywhere. The defence that remains is the ASCII restriction — a rejection rather than a fold,
which kills every Cyrillic and Greek homoglyph outright, those being the invisible ones.

### 4.5 Usernames are permanent

There is no rename operation. Someone who wants a different name signs up again.

A renameable username is a reusable one, and a reusable one is an impersonation vector: every old
link, quote and `@mention` naming `alice` silently starts pointing at whoever claims it next. A
forum is an archive — a thread from four years ago is still cited, and its attributions must keep
meaning what they meant. Making the name permanent removes the whole class, along with the
redirect table, reclaim-window policy and tombstones that would otherwise be needed to contain it.

The name therefore stays taken after the account is gone: deletion sets `user.state` rather than
removing the row. An old `/u/` link to a deleted account 404s. It never resolves to a different
person.

### 4.6 Spaces are hierarchical, and rename via one nullable column

`/s/sports/hockey`, unique **per parent**, so `sports/general` and `music/general` coexist.
Resolution is one indexed lookup on a materialized `space.path` — the same trick `post.path`
uses. Measured at depth 3:

| resolution strategy | time | D1 queries |
|---|---|---|
| one query per level (naive walk) | 32.9 µs | **3** — scales with depth |
| **materialized path** | **1.6 µs** | **1** |
| flat global name | 1.7 µs | 1 |
| load whole tree, resolve in memory | 79.6 µs | 1 |

Per-parent uniqueness therefore costs nothing versus a flat namespace. Nesting caps at 3 levels.
Subtree listing is a range scan, not a recursive CTE: **294 µs against 2634 µs**, and the CTE
builds throwaway indexes at runtime.

Spaces *do* rename, unlike usernames — a space is a place, not an identity, and no post is
attributed to one in a way a rename could falsify. What that needs is a redirect, and it is **one
nullable `moved_to_id`**, not a history table. Renaming rewrites the row's path; if the old URL
should keep working, a tombstone row is left behind pointing at the new one. The lookup that
resolves any path already finds it, so a redirect costs **zero extra queries** — and an instance
that does not care lets old paths 404 and stores nothing.

#### Stored paths carry a trailing separator

Not cosmetic. A key may contain `-` (0x2D), which sorts **below** `/` (0x2F), so the obvious range
over untrailed paths silently swallows siblings:

```
['sports', 'sports0')    ->  sports, sports-betting, sports/hockey   WRONG
['sports/', 'sports0')   ->  sports/, sports/hockey/                 right
```

#### There is no `space.slug` column

The key is the last segment of the path; storing it twice invites drift. An inline
`slug TEXT UNIQUE` would also be *globally* unique, which directly contradicts the per-parent
rule — verified: it rejects `music/general` once `sports/general` exists, and because SQLite
builds an unnameable auto-index for an inline `UNIQUE`, it cannot be dropped later without
rebuilding the table.

Uniqueness is indexed on the full path rather than `(parent_id, key)` for a related reason:
SQLite treats NULLs as **distinct** in a unique index, so the latter allows two top-level spaces
with the same key. Also verified rather than assumed.


### 4.7 Posts get a public id too

Every post carries one, at the same 16-character width threads use.

A post's other address is `(thread_id, path)` — and both halves encode *which thread*. Splitting
and merging threads is routine moderation, so a permalink built on either breaks the moment a
moderator acts. A global id survives the move. Same argument as a thread id surviving a retitle,
one level down.

`GET /p/{id}` resolves where the post lives *now* (one query) and 302s to that thread page,
anchored. 302 rather than 301 because the target legitimately changes when a post moves.

**Known limitation:** the redirect lands on page 1 and relies on the fragment, so on a thread
longer than one page the reader arrives at the top. The obvious shortcut — deriving a cursor by
truncating the post's path — is wrong, because an earlier sibling with a large subtree can still
push the post off the page. Proper "which page contains this path" belongs with pagination in M2.

Measured cost, at 200k posts: **+46 B per post**, ~17 B in the row and ~29 B in the unique
index. Against a row that still holds its body that is +2.6%; against a metadata-only row it is
+53%. Read path is unaffected — `rows_read` for a 200-post page stays at 404 either way, since
nothing on the read path consults the index.

Not nullable-and-promoted. Minting on first use would save ~50 B on posts nobody links to, at
the cost of a write on the read path and a race between two simultaneous linkers — and the
saving disappears entirely under §4.8 anyway.

An earlier revision of this document argued *against* post ids, on the grounds that 5M posts
would cost 135 MB of a 500 MB ceiling. The arithmetic was right and the premise was wrong: 5M
posts of body text alone is 5 GB, ten times over the ceiling, so that scenario cannot occur.

### 4.8 D1 holds the working set, R2 holds the corpus

The above only works because D1 stops being where everything lives.

Rendered HTML is already baked to R2 (§3.3). Extending that to whole archived threads — rather
than only their bodies — changes the capacity picture entirely:

| model | B/post | posts on the free tier | limit |
|---|---|---|---|
| D1 only, bodies inline | 1077 | 486,804 | 500 MB D1 |
| R2 archive, html + markdown | 1112 | 9,651,612 | 10 GB R2 |
| **R2 archive, gzipped** | **441** | **24,355,895** | 10 GB R2 |

**50x the corpus.** Forum HTML compresses extraordinarily well — the measured 200-post page goes
from 145,926 to 8,720 bytes, **16.3x** — because it is mostly repeated markup.

An archived thread keeps a stub row in D1 so it still appears in listings and search. Stubs are
cheap: 5M posts' worth is ~24 MB, under 5% of the D1 ceiling.

The read path for an archived thread is *cheaper* than for a live one, not more expensive: if
the R2 key is derived from the thread's public id, resolving one is Cache API → R2, with **zero
D1 queries**. That is the strongest argument for public ids doing double duty as storage keys.

What this costs, and what still has to be designed (M6):

- **Search.** FTS5 lives in D1. Archived bodies are not in it unless an index is kept behind.
- **Replies to an archived thread** need rehydration, or the thread is closed on archive.
- **R2 class-A operations** are 1M/month free; one write per bake is comfortable, but a
  rebake-everything migration is not.


### 4.9 Sessions live in D1

Cloudflare's own guidance says otherwise, so the reasoning matters more than the conclusion.

> "We recommend using Workers KV for storing session data, credentials (API keys), and/or
> configuration data." — [Choosing a data or storage product](https://developers.cloudflare.com/workers/platform/storage-options/)

That recommendation comes with a stated premise: such data is "read at high rates (thousands of
RPS or more), are not typically modified ... and do not need to be immediately consistent."
Two of those three are false here, and the third is fatal.

**Write budget.** KV's free plan allows **1,000 writes/day**; D1's allows **100,000 rows
written/day**. Every login is a write, and sliding expiry would make every *request* a write.
A hundredfold difference in the wrong direction, against a Worker budget of 100k requests/day.

| | KV free | D1 free |
|---|---|---|
| writes/day | **1,000** | 100,000 rows |
| reads/day | 100,000 | 5,000,000 rows |
| consistency | eventual, "up to 60 seconds or more" | immediate |

**Revocation.** KV is eventually consistent: a change "may take up to 60 seconds or more to be
visible in other global network locations", and negative lookups are cached the same way. A
logout, a ban, or a password change would therefore keep working for a minute somewhere in the
world. For a forum whose pitch is moderation (§5), a banned account that can keep posting for
another minute is not an acceptable default.

**Dual target.** There is no Workers KV in a self-hosted binary. Goal #1 is one codebase and two
deploy targets, and a KV session store means a second session implementation for the native
side — precisely the drift the `Store` seam exists to prevent. A `session` table is just SQL and
runs unmodified on both.

Cloudflare's recommendation is sound for what it describes: a read-mostly, revocation-tolerant
credential cache at thousands of RPS. This forum is none of those.

#### The design

- The cookie carries **256 bits from `crypto.getRandomValues`**, base64url. Not a JWT: there is
  no third party to convince, and a random token needs no signature scheme to get wrong.
- The table stores **only the SHA-256 of the token**. A leaked database — or a backup, or one of
  D1's seven days of Time Travel snapshots — then yields no usable sessions.
- `HttpOnly; Secure; SameSite=Lax; Path=/`. Lax rather than Strict so that following a link into
  the forum keeps you logged in.
- **Sliding expiry, refreshed at most once a day.** Naive sliding is a write per request, which
  is what makes session stores expensive. Refreshing only when the last one is a day old gives
  the same "never logged out" experience for roughly **one write per active user per day** —
  against D1's 100,000 rows/day, the difference between a few thousand pageviews and tens of
  thousands of daily users. Default lifetime 30 days.
- An **expired session is never refreshed**, so a request arriving a month late is a logout
  rather than a resurrection.
- Revocation is a `DELETE`, effective immediately.

#### Why a per-request lookup is affordable

§9 requires justifying any new per-request query. This one is cheap for a structural reason:
baked pages are user-agnostic (§3.3), so a cached read consults no session at all.
Personalisation is a separate small fetch, and writes need a session anyway. When a lookup is
needed it joins the existing `batch()` — 3 statements in one round trip rather than 2, against a
limit of 50 and a measured 2.52 ms.

#### Not yet decided

CSRF. `SameSite=Lax` covers cross-site form posts from other origins, but not everything, and
the reply form is a `POST`. Needs a token, decided when the form is built.


### 4.10 Password hashing does not fit a free Worker

Measured, because the collision is not obvious until you put numbers on it. Password hashing is
deliberately slow; the free plan allows **10 ms of CPU per request**. Wasm under V8,
`scripts/kdf-bench.mjs`:

| candidate | p50 | vs 10 ms |
|---|---|---|
| Argon2id, OWASP minimum (19 MiB, t=2) | 25.2 ms | **2.5x over** |
| Argon2id, RFC 9106 (64 MiB, t=3) | 134.9 ms | 13.5x over |
| PBKDF2-SHA256, OWASP minimum (600k) | 456.1 ms | 45.6x over |
| PBKDF2-SHA256 at workerd's 100k cap | 75.8 ms | 7.6x over |
| …the same via **native** WebCrypto | 12.7 ms | 1.3x over |

The last row is the one that closes the door. `crypto.subtle` runs natively rather than in wasm
and is six times faster — but workerd caps PBKDF2 at 100,000 iterations to limit DoS
([workerd#1346](https://github.com/cloudflare/workerd/issues/1346), open since 2023), and OWASP
asks for 600,000. **The fastest legal configuration is simultaneously below the recommended
strength and over the CPU budget.**

Nor is Argon2 rescued by tuning. Measured at p95 over fifteen samples — a request that overruns
is a failed login, not a slow one, so the tail is what matters:

| Argon2id | p50 | **p95** | p95 vs budget |
|---|---|---|---|
| m=2 MiB, t=1 | 2.07 ms | 2.30 ms | 23% |
| **m=4 MiB, t=1** | **4.21 ms** | **4.33 ms** | **43% — chosen** |
| m=4 MiB, t=2 | 8.57 ms | 9.12 ms | 91%, tight |
| m=6 MiB, t=1 | 6.77 ms | 9.83 ms | 98%, tight |
| m=8 MiB, t=1 | 9.78 ms | 11.99 ms | **over** |
| m=19 MiB, t=2 | 53.45 ms | 56.77 ms | OWASP minimum, 5.7x over |

An earlier revision of this table claimed 8 MiB t=1 fitted at 5.98 ms. That came from a
three-sample run; fifteen samples put its p95 at 11.99 ms. `Params::CONSTRAINED` is 4 MiB, t=1,
which leaves the rest of the request — render, D1, session lookup — somewhere to live.

These are development-machine numbers. Cloudflare's hardware differs, and the headroom is worth
re-checking against a deployed instance.

So: **there is no OWASP-grade password login on a free-plan Worker.** That is a platform
property, not a code one, and the design should say so rather than quietly pick weak parameters.

What follows:

- **OIDC is the answer for the Workers deployment.** Verifying a signed assertion is a signature
  check — sub-millisecond — and the problem disappears. Already the intended direction.
- **The self-hosted target has no 10 ms limit** and uses `Params::OWASP` unchanged. The
  dual-target design turns out to matter here for a reason nobody planned.
- **`Scheme::CONSTRAINED`** exists for a free Worker that insists on passwords: 4 MiB, t=1. It
  is never the default, and the Worker logs a `console_error` at startup when it is in use. A
  weakened KDF nobody mentions is how it stays weakened.
- **`Scheme::CLIENT_ARGON`** takes the other way out: the scarce CPU is the *Worker's*, and the
  browser's is not scarce. The client runs OWASP-grade Argon2id and posts the 32-byte result;
  the Worker applies `Params::HANDOFF` (1 MiB, t=1 — **1.25 ms p95, 12% of budget**, against
  4.18 ms / 42% for `CONSTRAINED`) purely to pepper it. See §4.10.1.
- **A paid plan** raises the limit to 30 s, at which point all of this is moot.

##### 4.10.1 Client-side Argon2id

Selected with `PASSWORD_SCHEME=client-argon`. The honest trade:

| | |
|---|---|
| **Gained** | A stolen database costs a full 19 MiB Argon2id run *per guess* — the work factor no free-Worker server-side scheme can charge. Plus 3.3× of the request's CPU back. |
| **Lost** | Login requires a client that performs the derivation. The salt must be fetched before submit, so that endpoint has to answer for unknown accounts too or it becomes the enumeration oracle §4.11 exists to avoid. |
| **Unchanged** | Anyone reading the derived key in flight holds a password-equivalent — exactly their position with the password itself. |

The danger is not the cryptography, it is that a client record and a server record *look alike*:
both end in a cheap Argon2id PHC string. Cheap is sound over a key that cost 19 MiB to derive and
close to worthless over a plaintext password. So the two are made non-interchangeable twice over:
stored client records carry a `c` marker and `verify` refuses a mismatch rather than guessing,
and the hashed input is domain-separated so the digests cannot collide even if a marker were
forged. Both failures are closed — a mismatch rejects the login. Neither can silently downgrade.

`MIN_PASSWORD_CHARS` does not apply server-side under this scheme, because the server never sees
the password. Enforcing length becomes the client's job, and that is a real transfer of
responsibility rather than a detail.

**No browser client ships yet.** The scheme is usable today only by a caller that derives the key
itself; the stock login form posts a plaintext password, which this scheme rejects (closed, not
weakly accepted). The Worker says so at startup. Building the browser half is the remaining work.

Hashes are PHC strings — `$argon2id$v=19$m=19456,t=2,p=1$salt$hash` — so cost travels with the
hash and `verify` can report `YesRehash` to upgrade an account on its next login, the one moment
the plaintext is ever in hand.

#### Getting strength back: what works and what does not

| measure | against a leaked database | against online guessing | cost |
|---|---|---|---|
| **Pepper** | **decisive** — hashes are uncrackable without it | nothing | **0.3%** |
| Per-user salt | stops precomputation and batch cracking, not single-target | nothing | 0 (mandatory) |
| Minimum length | raises the floor a weak KDF would otherwise give away | helps | 0 |
| Rate limiting | nothing | **decisive** | not built |
| Client-side pre-hash | adds cost the server does not pay | nothing | needs JS |

The **pepper** is the one that changes the picture. It is Argon2's own `K` parameter, held
outside the database as a Worker secret, so a leaked database — SQL injection, an exposed
backup, one of D1's seven days of Time Travel snapshots — yields hashes that cannot be attacked
at *any* KDF cost. That is the threat reduced parameters actually expose, and peppering closes
it. Measured cost: **0.3%**, interleaved A/B (a naive sequential comparison first suggested 27%,
which was drift).

What it does not do: nothing against a full server compromise, where both leak together, and
nothing against online guessing. It is not a substitute for KDF cost either — an attacker
holding the pepper is back to attacking 4 MiB Argon2id.

A **salt** is not a strength measure and is worth separating from the pepper. It is per-user,
stored *with* the hash, and defeats rainbow tables and cracking many accounts at once. It adds
nothing against a single targeted password. Already mandatory via PHC.

#### Every pepper is kept, and the hash names which one

A pepper cannot be changed in place — the hashes depend on it and the plaintexts are gone — so
rotation means holding the old ones. The naive form of that is a list tried in turn, and it does
not survive the CPU budget: a **wrong** password tries every pepper, and at
`Params::CONSTRAINED` each attempt is 3.34 ms.

| peppers tried | CPU | |
|---|---|---|
| 1 | 3.34 ms | 33% |
| 2 | 6.68 ms | 67%, tight |
| **3** | **10.03 ms** | **over** |

A list therefore caps rotation at one generation, on precisely the path an attacker controls.

So the stored hash records which pepper made it — `<id>$<phc>`, e.g. `3$argon2id$v=19$…`, with
an empty prefix meaning unpeppered — and verification looks up exactly that one. **Cost is
constant however many are held**, which is what makes keeping all of them practical: no pepper
is retired on a deadline, and no account is stranded by a rotation that finished before its
owner came back. `PASSWORD_PEPPER` is current; `PASSWORD_PEPPER_<n>` are the historical ones.

Ids are permanent. Reusing one for a different secret strands every account hashed under the
old one, so a hash naming an absent pepper is an error rather than a failed login — the account
cannot be verified at all and needs a reset.

### 4.12 Rate limiting is the defence the KDF is not

A weak KDF is an *offline* problem, and the pepper answers it. Online guessing is a different
attack: the attacker pays nothing for a wrong guess, and **we** pay 3.34 ms of a 10 ms budget —
so unthrottled login is both a credential attack and a cheap way to exhaust the account's CPU.

The check therefore runs **before** the hash. A refused attempt costs a lookup, not a KDF.

Two buckets, because the attacks differ:

| bucket | attack | limit |
|---|---|---|
| per identity | one account, many passwords | 5 / 15 min |
| per client | one password, many accounts (credential stuffing) | 60 / 15 min |

The client bucket is twelve times looser because a client is not a person — an office, a campus
or a mobile carrier is one address. It still kills stuffing, which needs thousands of attempts
to be worth running. An attempt must pass both.

Identities are keyed by **name, not user id**: a login for an account that does not exist has no
id, and skipping the limiter for unknown names would make enumeration free. Names are lowercased
into the key, since case would otherwise multiply the limit.

Fixed windows, not sliding. Sliding needs a row per attempt; fixed needs one counter and one
instant. The approximation lets an attacker straddle a boundary for `2 * limit` in quick
succession — 10 instead of 5 — which changes nothing, and is not worth a row per attempt against
a 500 MB ceiling.

**A successful login clears its bucket.** A limiter that punishes success locks out the people
sharing an address correctly, and gets switched off.



#### What each costs in the bundle

Measured with each KDF actually reachable from a route, because unused code is dead-stripped —
an earlier revision of this section reported Argon2 as costing 176.5 KB, which was simply the
whole bundle, and the Argon2 in it was being stripped for never being called.

| variant | raw | gzipped | delta (gzipped) |
|---|---|---|---|
| baseline, no KDF | 1,421,837 | 421,789 | — |
| **+ `crypto.subtle` PBKDF2** | 1,427,085 | 423,802 | **+2.0 KB** |
| **+ Argon2id (Rust)** | 1,455,353 | 432,610 | **+10.6 KB** |

`crypto.subtle` costs almost nothing because it is a *platform* API: the cryptography is in the
runtime and only the wasm-bindgen glue is compiled in. It also needs no new dependencies —
`js-sys`, `wasm-bindgen` and `wasm-bindgen-futures` are already re-exported by `worker`. Argon2
is a Rust crate, so all of it is linked.

**Neither is a reason to choose.** Both are trivial against the 3 MB script limit; the decision
is made entirely by CPU. Both are behind cargo features (`password`, `kdf-subtle`), off by
default in the Worker, so an OIDC deployment carries neither.

One Cargo detail worth knowing, since it made the first three measurements identical: a member
crate **cannot** disable default features on an inherited workspace dependency. `default-features
= false` has to be set on the root `[workspace.dependencies]` entry, and Cargo warns that it is
"ignored" otherwise. The root now sets it and crates opt in with `features = ["password"]`.

### 4.11 CSRF tokens are signed, not stored

`<expiry>.<HMAC-SHA256(key, session_hash + expiry)>`, verified in constant time. Measured at
**1.78 µs** — 0.018% of the request budget — which is why nothing is stored. A CSRF token in the
database would cost a query on every form render and every submit, for a value that has to be
derivable anyway.

Bound to the **session**, not merely signed, so a token minted for one visitor cannot be
replayed by another. It carries its own expiry, so one scraped from a cached page stops working.

This is defence in depth rather than a replacement for `SameSite=Lax`: that attribute already
blocks cross-site form posts, but it does not cover a subdomain takeover and it is one browser
default away from being the only protection there is.


## 5. Moderation pipeline

Runs asynchronously off the request path. You do not have 10ms to spare for an LLM call.

```
post write
  -> Tier 0: free heuristics, inline (~0 cost)
       account age · link count · duplicate body · blocklist · text addressed to the model
       -> most posts publish here, zero neurons spent; moderators skip it entirely
  -> held: state = 'pending', action_log row (actor 'tier0'), enqueue {post, reasons}
  -> Queue consumer (or the cron sweep, for anything the queue lost):
       Tier 1: classifier behind a trait -- Workers AI 8B by default; Llama Guard, Anthropic
               or any OpenAI-compatible endpoint (OpenRouter) by configuration; optionally a
               safety model layered in front of an instruct model
       -> verdict {call, confidence, categories, rationale} -> action_log (actor 'model')
       -> confident clean: publish · confident bad + severe category: hide + review
          · anything else: stays pending, review
  -> Human review queue: approve / reject, one POST each, CSRF-bound
  -> Metamoderation: agreement between verdict and decision, per space, moves the
     thresholds the model is judged against
```

Reports are the other way in: a report is a Signal (`kind = 'report'`, weight −1, one per
reporter per post). At the space's threshold a visible post goes back to `pending`, opens a
review item, and is enqueued so the reviewer gets the model's opinion alongside the readers'.
Appeals reopen a resolved item with the author's text and the original verdict still attached.

Non-negotiable properties, all of them pinned by tests:

- **The model triages; it never delivers a final verdict.** Auto-hide pending review, never
  auto-delete. A human decision is final for that item; nothing automatic reopens it.
- **A flag hides only for a severe category.** Spam, harassment, hate, violence, sexual content,
  self-harm, illegal content and doxxing can hide a post pending review. `off_topic` and
  `other` can only hold it for a human, at any confidence: the live evaluation found an 8B model
  calling ordinary technical posts off-topic at confidence 1.0, and topicality is a judgement a
  human makes.
- **Text addressed to the model is never auto-published.** Tier 0 holds it on sight
  (`ignore previous instructions`, forged `</post>` tags, JSON aimed at the parser); the prompt
  fences the body and tells the model it is data; and a verdict carrying the `manipulation`
  category cannot publish however confident it is. Three layers, because the first two are
  heuristics and the third is the one that holds.
- **Every model call is logged** to `action_log` as an action by an actor of kind `model`, with
  the verdict, confidence, categories, rationale, model id and prompt version -- before it is
  acted on, so a crash leaves a verdict without an action rather than the reverse.
- **Public modlog by default** (Lemmy got this right): who or what did what to which post.
  Never the rationale, never who reported. Reports themselves are private rows.
- **Appeals path exists from day one.** The author of a hidden post gets a form; the item reopens
  with the verdict it appeals still on it.
- **Provider is pluggable behind a trait.** The prompt, the schema and the response parsing live
  in `core` and are tested without a network; a provider only shapes one vendor's request. The
  Worker configures it with `MOD_PROVIDER`. Never hard-code a vendor.
- **Classifiers compose.** `Layered` puts a safety classifier (Llama Guard) in front of an
  instruct model and combines the two verdicts by a pure rule: two flags corroborate into a
  stronger one; a flag one layer made and the other contradicted is scaled below the hide bar;
  and a Guard's `safe` is a veto on hiding for the categories it is trained on (hate, violence,
  sexual content, self-harm, illegal, doxxing) -- never for spam or harassment, which it cannot
  see. The Guard is a frontend, not a gate: the instruct model still reads every post.
- **The database is the source of truth; the Queue is an accelerator.** A pending post with no
  open review item is work, wherever the message went. A cron sweep every five minutes picks up
  whatever the queue did not deliver, so a lost message costs latency and nothing else. A native
  binary with no queue at all runs the same sweep on a timer.

**Budget reality check.** Measured against Workers AI pricing (September 2026): Llama 3.1 8B
costs 25.6 neurons per thousand input tokens and 75 per thousand output, so one classification
of a typical post -- ~600 tokens in, ~60 out -- is ~20 neurons, and the free 10,000 a day buys
**~500 classifications**. Llama Guard 3 8B is dearer per token (44 neurons/k in), ~380 a day. A
70B model is not on the table. Every classification is therefore of a *held* post, never a
routine one, and Tier 0 is what decides who pays -- a new-account window and a link cap are
the two rules that matter. Each queue message also costs ~3 of the day's 10,000 queue operations.

**Metamoderation, concretely.** Each resolved review item records the model's call and the
human's resolution. `clean`/approve and `flag`/reject are agreement; the other diagonal is
disagreement; `unsure` is neither. Per space, once ten items are resolved, a disagreement rate
above 30% raises the confidence a `clean` needs to publish by 0.15 and a `flag` needs to hide by
0.05; once thirty are resolved, a rate under 5% lowers the publish bar by 0.05, never below 0.5
and never touching the hide bar. The thresholds are read from `space.config` and adjusted at
decision time, so a change in reviewer behaviour takes effect on the next post. The
`prompt_version` logged with each verdict lets the statistics be read per prompt when the prompt
changes.

**What the model is told.** Space name, thread title, the author's account age in days, why
Tier 0 held the post, and the body inside `<post>` tags with any embedded closing tag defanged.
Not the username: a name is not evidence, and it is one more thing to leak. The space's own
rules, if it has written any, go into the system prompt -- with the instruction that they
describe the community's spirit and are not a list of permitted subjects, which the 8B model
needed telling twice.

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
GET  /s/{path}                  thread list (ranked), subspaces included
GET  /t/{id}/{slug?}            thread page (baked)
GET  /t/{id}.rss                feed
POST /t/{id}/reply
POST /s/{path}/new
POST /p/{id}/edit               author only; the edited body goes back through Tier 0
POST /p/{id}/delete             tombstone; author or moderator
POST /p/{id}/signal             vote / like  (idempotent)
GET  /p/{id}/report             report form; POST records a Signal of kind 'report'
GET  /p/{id}/appeal             appeal form for the author of a hidden post
GET  /api/me/thread/{id}        personalization layer (vote state, unread)
GET  /u/{name}                  profile
GET  /settings                  the account: email, password, sessions
GET  /verify?token=             confirm an address (the POST spends the token)
GET  /forgot, /reset?token=     password recovery, by mail
GET  /modlog                    public action log
GET  /mod/queue                 review queue (capability-gated)
POST /mod/review/{id}           approve or reject one item
GET  /admin                     dashboard; /admin/thread/{id}, /admin/users, /admin/spaces,
                                /admin/log — moderators; roles and spaces admin-only
POST /uploads/sign              issue scoped R2 upload URL
WS   /t/{id}/live               hot threads only, via DO
```

Reading must work with JavaScript disabled. Voting and live updates may require it.

### 7.1 Replying: JS-free by default, inlined when JS is available

The no-JS path is the real one, in the news.ycombinator sense — it is what ships, what is
tested, and what everything else degrades to. The baked thread page carries a plain `reply` link
to `/t/{id}/reply`, which serves a form on its own uncached page and redirects back on success.
Nothing about that path is a fallback bolted on afterwards.

Where JS is available, the link is progressively enhanced into a form that opens in place. Three
constraints shape how:

- **The token cannot be baked.** The thread page is shared byte-for-byte between readers, so the
  script has to fetch a CSRF token rather than read one out of the HTML. `GET /api/me/thread/{id}`
  is already the endpoint that layers per-viewer state (§3.3) over the shared page; the token
  goes with it, and no new round trip is introduced.
- **Anonymous readers fetch nothing.** No session cookie means no `/api/me` call, so the common
  case stays at exactly one request per pageview — which matters, because requests are the
  binding free-tier constraint.
- **The script is external.** The CSP is `script-src 'self'` with no `unsafe-inline`, and that
  does not get relaxed for convenience.

The enhancement must degrade by *removal*: with the script absent or failed, the link is still a
link. That rules out rendering a dead `<form>` server-side and animating it open, which is the
usual shortcut and the usual way the no-JS path quietly rots.

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
*(Delivered: all of it except htmx. Registration takes an optional email address and mails a
verification link; password reset works by mail; both through Resend behind a `Mailer` trait,
and both degrade to "no mail" rather than to failure. Space pages, profiles, new threads,
edit/delete and per-thread RSS are served. See §10 Q6.)*

**M3 — Community features.** FTS5 search, notifications, capabilities/trust, signals + ranking
functions, mod actions, public modlog.

**M4 — Moderation.** Rule engine, heuristics, Queue-based AI classification, review queue,
metamoderation.
*(Delivered ahead of M3, because the write path needed it: Tier 0 heuristics on every reply,
a pluggable classifier with Workers AI and Anthropic providers, the Queue consumer and cron
sweep, reports as Signals, the review queue, appeals, the public modlog, and per-space
metamoderation. See §5. Not yet: a general rule engine beyond the fixed heuristics, reporter
accuracy weighting, and editing/deleting one's own posts.)*

**M5 — Deploy story.** `wrangler deploy` one-liner, Docker image, single static binary, seed
script, demo instance, import from phpBB/Discourse dumps.

**M6 — Bake + hot threads.** R2 baking, cache versioning, DO promotion, live updates.

---

### 8.1 M1 status — the `Store` seam

`Store` is the trait everything above storage depends on, and for a while it was decorative: one
method, one implementation, **zero call sites**. The Worker used two inherent methods on
`D1Store` that were not on the trait at all, so the trait compiled and carried nothing.

Closed:

- The trait carries what the application actually uses — `thread_page` and `locate_post` — and
  handlers reach storage only through it.
- `ThreadPage` carries its `Space`, so the trait is one method returning one type rather than a
  tuple the render had to reassemble.
- The SQL lives once, in `core::sql`, and both adapters send the same statements. Sharing it
  makes "identical dialect on both targets" structural rather than aspirational.
- Enum string forms live once, in `core::model`, with a test asserting `as_str` agrees with
  serde. Two routes to one mapping was a live drift vector: nothing would have failed to compile
  if serde said `score_threshold` and a hand-written parser said `scorethreshold` — one target
  would just have ranked wrongly.
- A second implementation exists: `crates/store-sqlite`, native rusqlite.
- One conformance suite, `core::conformance`, runs against both. Natively via `cargo test`; against
  D1 via `GET /__conformance?thread=<id>`, because no `#[test]` can reach a Worker runtime.

Both adapters pass all ten checks. They cover what SQL does not enforce and where adapters
actually drift: cursor exclusivity, tree order, paging visiting every post exactly once, absent
rows being `NotFound` rather than an empty page, and the read path never loading `body_md`.

### 8.2 The write path, and what the second adapter caught

`insert_post` allocates a materialized path and appends. Four statements: resolve the thread,
resolve the parent, find the deepest path under it, then insert and bump the thread counters in
one batch.

Allocating a child ordinal is O(1), not a sibling scan. One backwards index walk gives the last
*descendant* of the parent in preorder, and truncating that to `parent.depth() + 1` gives the
last direct child. Counting children instead would be wrong rather than merely slower, because
tombstones stay in the table and a count reuses an occupied ordinal.

Allocation is read-then-write, so two replies to the same parent can compute the same ordinal.
`UNIQUE(thread_id, path)` catches the loser and it surfaces as `StoreError::Conflict` for the
caller to retry. Adapters must not silently pick another ordinal: a retry has to re-read the
parent anyway.

**Two real divergences between the targets, both found by running the suite against D1 rather
than reasoning about it:**

- **D1 rejects `bigint` bindings.** `i64::into::<JsValue>()` produces a JS `BigInt` and D1
  answers `D1_TYPE_ERROR: Type 'bigint' not supported`. Every integer bind must go through
  `f64`. rusqlite takes an `i64` without complaint, so the native adapter never sees this. Exact
  within 2^53, which covers row ids, depths and millisecond timestamps.
- **D1 has no `last_insert_rowid()`.** The row id has to be read from the batch result's own
  meta. The first implementation returned a placeholder `0`, which the suite caught immediately
  by comparing a child's `parent_id` against its parent's returned id.

Neither is exotic, and neither would have been found by a read-only suite — which is why the
write checks matter more than the read ones. Both adapters now pass all fifteen.

Also closed: DESIGN.md §9's "no system RNG on wasm". Public ids are generated from
`Date.now()` and `crypto.getRandomValues`, via `getrandom`'s `js` feature. Not `Math.random()`
— V8's PRNG is predictable from observed output, and a predictable id is an enumerable one.

Still open: `crates/server` does not exist, so the native adapter is exercised by tests rather
than shipping a binary. That is M5.


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

11. Federation (ActivityPub). Not planned; `docs/FEDERATION.md` is the primer and the cost
    estimate. The read path would be unaffected; the request budget, storage and moderation
    load would not be.

3. ~~Auth: sessions in D1, in a DO, or signed stateless cookies?~~ **Decided: D1.** See §4.9.
4. Search on the wasm target — does D1 expose FTS5? If not, an external index or a native-only
   feature flag is needed. **Untested in M0.**
5. Archive strategy when a D1 database approaches the 500 MB free ceiling.
6. ~~Email: needed for notifications and password reset, but there is no SMTP from a Worker.~~
   **Decided: Resend, behind a trait.** One JSON POST per message; the free tier (100/day,
   3,000/month) covers verification and reset for a small forum. The dependency on someone
   else's free tier is contained by `email::Mailer`: every flow works with `NoMailer`, and the
   native binary can put SMTP behind the same seam. Notifications are still M3.

### Raised by M0

8. **Denormalise `author_name` onto `post`?** The `user` join roughly doubles rows read per
   thread page. Denormalising would halve it, at the cost of a rewrite on username change.
   Decide once `rows_read` has been observed on a real D1 instance rather than derived.
9. **Confirm the derived numbers in production.** `rows_read` is production-only, and all M0
   CPU figures come from wasm-under-Node as a proxy for wasm-under-workerd. The margins are
   large enough that the conclusion is safe, but the figures themselves are not yet firm.
10. **Post addressing.** `/p/{id}` (needs a global unique index over every post's public id) vs
    `/t/{thread_pub}/p/{n}` (needs none). §4.2 prefers the latter; settle it in M2.
