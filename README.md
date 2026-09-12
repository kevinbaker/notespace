# notespace

A configurable, AI-moderated, threaded forum in Rust that deploys to a free Cloudflare account
*or* runs as a single self-hosted binary.

> **Status: read path, auth, writing and moderation work; no deployment story yet.** The
> free-tier premise is measured and confirmed. See [DESIGN.md](DESIGN.md) for where this is
> going, [IMPLEMENTATION.md](IMPLEMENTATION.md) for what exists, and
> [docs/M0-findings.md](docs/M0-findings.md) for what has been proven by measurement.

## What exists today

A Cloudflare Worker that serves a threaded forum from D1, with accounts, replies, and an
AI-triaged moderation pipeline:

- **`crates/core`** — domain model, the `Store` trait, and materialized tree paths. No I/O, no
  target awareness, property-tested.
- **`crates/render`** — markdown → sanitized HTML (pulldown-cmark + ammonia) at write time, and
  maud page templates for the read path.
- **`crates/worker`** — axum on Workers, wired to a D1-backed `Store`; also the moderation
  queue consumer and cron sweep.
- **`crates/store-sqlite`** — the native adapter, and where the shared conformance suite and the
  moderation pipeline run under plain `cargo test`.
- **`crates/seed`** — deterministic seed/fixture generator that renders through the real write path.
- **`crates/hn-import`** — turns a Hacker News dump into seed SQL through that same write path.
- **`crates/bench-wasm`** — exposes the render paths to a Node harness so CPU can be measured in
  real wasm.

## The M0 result

| Budget (Cloudflare free plan) | Limit | Measured |
|---|---|---|
| Worker CPU / request | 10 ms | **0.048 ms** (200-post page) |
| Worker script size | 3 MB | **139.6 KB** gzipped (M0, read path only); **746 KB** today |
| D1 queries / invocation | 50 | **2**, one batched round trip |
| D1 rows read / page | 5M/day | **404** (production) |
| D1 round trip | — | **2.52 ms** p50, 6.24 ms p99 (production) |

CPU was the thing M0 existed to de-risk, and it turned out to have ~90x headroom at p99. For a
small forum the binding constraint is the **100k requests/day** cap, not CPU or D1.

Deployed at `dev.notespace.org`, and the D1 figures above are from there rather than from local
emulation. `rows_read` came out identical in both, so the emulator was not merely close.

Still unmeasured: billed CPU in production, which appears in the Worker's dashboard metrics
rather than in a response header.

## Moderation

Every reply goes through free heuristics inline (account age, link count, duplicate body,
blocklist, text addressed to the model). Held posts are written as `pending`, queued, and
classified by a model behind a trait — Workers AI's Llama 3.1 8B by default; Llama Guard,
Anthropic, or anything OpenAI-compatible (OpenRouter) by configuration; optionally a Guard
layered in front of an instruct model (`MOD_PROVIDER = "openrouter_guard+openrouter"`). A confident clean verdict publishes; a confident flag for a severe
category hides pending review; everything else waits for a human at `/mod/queue`. Readers
report from `/p/{id}/report`; enough reports pull a post back into the queue. Authors of hidden
posts appeal from `/p/{id}/appeal`. Every action, by person, rule or model, is in the public
log at `/modlog`. Reviewer agreement with the model tunes its thresholds per space.

The model triages; it never has the last word. Details in DESIGN.md §5, reasoning — including
what the live evaluation found — in DECISIONS.md.

```sh
# Everything except the live model calls.
cargo test

# The prompt against a real model, over crates/core/tests/fixtures/moderation_corpus.json.
CF_ACCOUNT_ID=... CF_API_TOKEN=... cargo test -p notespace-core --test live_classifier -- --ignored
ANTHROPIC_API_KEY=...             cargo test -p notespace-core --test live_classifier -- --ignored
ORKEY=...                         cargo test -p notespace-core --test live_classifier -- --ignored

# Both tiers over a sample of real Hacker News comments (after ./scripts/hn-import.sh has run).
ORKEY=... cargo run -p notespace-core --example hn_moderate -- target/hn/hn-7d.ndjson \
    --n 30 --guard meta-llama/llama-guard-4-12b --model openai/gpt-4o-mini
```

What the HN sample showed (DECISIONS.md has the numbers): the 8B default over-flags ordinary
comments and misreads quoted insults as the poster's own; `gpt-4o-mini` behind Llama Guard 4
published 27 of 30 random comments and hid none, and costs about $0.25 per thousand posts.

To make someone a moderator, set `MODERATORS = "alice,bob"` in `wrangler.toml` or
`UPDATE user SET role = 'moderator' WHERE name = 'alice'`. The queue needs creating once:
`npx wrangler queues create notespace-moderation`.

## Running it

Prerequisites: a Rust toolchain with the `wasm32-unknown-unknown` target, Node 18+, and
`cargo install worker-build`.

```sh
rustup target add wasm32-unknown-unknown
cargo install worker-build

# Unit tests (core + render). No wasm toolchain needed.
cargo test

# Build, seed a local D1, check the query plan, and run the CPU benchmarks.
./scripts/spike.sh

# Serve it.
npx wrangler dev --local
curl http://127.0.0.1:8787/t/1
```

`./scripts/spike.sh bench` runs just the CPU benchmarks — no wrangler, no D1.

### Real content

The generated seed exercises the renderer but reads like generated text. To fill the database
with actual threads instead:

```sh
./scripts/hn-import.sh --days 7      # ~2,300 threads, ~52,000 posts, about 15 minutes
npx wrangler dev --local
```

It downloads a window of Hacker News and renders it through the same write path, so `body_html`
is what a live post would have stored. See [docs/HN-IMPORT.md](docs/HN-IMPORT.md) for the field
mapping, the BigQuery alternative, and what it costs against a deployed D1.

### Deploying

Not a one-liner yet (that is M5). `wrangler.toml` targets the `notespace-dev` Worker and
database; a fresh instance needs its own `wrangler d1 create`, `wrangler queues create
notespace-moderation`, `wrangler secret put CSRF_KEY`, and the returned ids pasted in. There is
no email, so no password reset; and no user-facing way to grant the moderator role.

## Layout

```
crates/core/      domain model, Store trait, materialized paths, moderation  (pure, no I/O)
crates/render/    markdown -> sanitized HTML, page templates
crates/store-sqlite/ native SQLite adapter; conformance and pipeline tests
crates/worker/    Cloudflare Workers entrypoint + D1 adapter + queue/cron consumers
crates/seed/      deterministic seed + fixture generator
crates/hn-import/ Hacker News -> seed SQL, through the real write path
crates/bench-wasm/ CPU harness, run under Node
migrations/       plain SQL, forward-only, dialect-identical for D1 and native SQLite
scripts/          spike.sh reproduces every number in docs/M0-findings.md
                  hn-import.sh loads a window of Hacker News into the database
```

## Conventions

Set out in DESIGN.md §9, and they are load-bearing rather than stylistic:

- Every dependency must compile to `wasm32-unknown-unknown`. Check before adding.
- `core/` stays free of I/O so it is testable without a database or a network.
- No `unwrap()` outside tests. The wasm target panics badly.
- Any change adding a per-request D1 query or DO call needs justifying against the budget
  table in DESIGN.md §3.2.
- Migrations are plain SQL, forward-only, numbered.

## License

MIT — see [LICENSE](LICENSE).
