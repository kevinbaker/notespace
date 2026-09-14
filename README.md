# notespace

A configurable, AI-moderated, threaded forum in Rust. One codebase, two ways to run it: a single
binary on a machine with a SQLite file, or a Worker on a free Cloudflare account.

> **v0.1.0 — first release, suitable for a limited beta.** Reading, accounts (Google, GitHub or
> local passwords), posting, editing, per-space themes and an AI-triaged moderation pipeline
> all work, on both targets; `dev.notespace.org` is the Worker. [docs/DEPLOY.md](docs/DEPLOY.md)
> stands up either: `cargo build --release -p notespace-server` and run it, or the free plan,
> or Workers Paid.
> [docs/ADMIN.md](docs/ADMIN.md) is for whoever runs one: roles, spaces, the moderation policy
> and the queue. [CHANGELOG.md](CHANGELOG.md) lists what is in and what is known to be missing.
> [DESIGN.md](DESIGN.md) is where this is going; [IMPLEMENTATION.md](IMPLEMENTATION.md) maps
> the design to the code; [DECISIONS.md](DECISIONS.md) says why.

## What exists

A threaded forum with spaces, accounts, threads, replies, editing, profiles, RSS, themes, and an
AI-triaged moderation pipeline, as a Worker on D1 or a binary on SQLite:

- **`crates/core`** — domain model, the `Store` trait, and materialized tree paths. No I/O, no
  target awareness, property-tested.
- **`crates/render`** — markdown → sanitized HTML (pulldown-cmark + ammonia) at write time, maud
  page templates for the read path, and the one stylesheet and fonts under `public/`.
- **`crates/app`** — every route and handler, written against a `Platform` trait (a store, a
  clock, an HTTP client, a cache, a queue, configuration) rather than any runtime.
- **`crates/worker`** — the `Platform` made of Cloudflare bindings: D1, Workers AI, Queues, the
  Email Service, `crypto.subtle`; plus the queue consumer and cron sweep.
- **`crates/server`** — the `Platform` made of a SQLite file, `reqwest`, the environment and an
  in-memory cache: one binary, one thread, secrets generated on first run.
- **`crates/store-sqlite`** — the SQLite `Store`, and where the shared conformance suite and the
  moderation pipeline run under plain `cargo test`.
- **`crates/seed`** — deterministic seed/fixture generator that renders through the real write path.
- **`crates/hn-import`** — turns a Hacker News dump into seed SQL through that same write path.
- **`crates/bench-wasm`** — exposes the render paths to a Node harness so CPU can be measured in
  real wasm.

Not federated, and [docs/FEDERATION.md](docs/FEDERATION.md) says what that would take.

## The numbers

| Budget (Cloudflare free plan) | Limit | Measured |
|---|---|---|
| Worker CPU / request | 10 ms | **0.048 ms** (200-post page) |
| Worker script size | 64 MiB | **950 KB** gzipped with password login; the fonts and stylesheet are static assets, not script |
| D1 queries / invocation | 50 | **2**, one batched round trip |
| D1 rows read / page | 5M/day | **404** (production) |
| D1 round trip | — | **2.52 ms** p50, 6.24 ms p99 (production) |

CPU was the thing the first spike existed to de-risk ([docs/M0-findings.md](docs/M0-findings.md)),
and it has ~90x headroom at p99. For a small forum the binding constraint is the **100k
requests/day** cap, not CPU or D1 — and static assets do not count against it. The D1 figures
are from production, not emulation; `rows_read` came out identical in both.

## Accounts, posting, email

Sign in with Google or GitHub (`OIDC_<PROVIDER>_CLIENT_ID` plus a secret; the first sign-in
picks a username, and the account has no password), or with a local password when the Worker
is built with `NOTESPACE_FEATURES=password`. A build without that feature is the free-plan
shape: no password hashing, no reset mail, sign-in is the provider buttons alone.

With local passwords, registration takes a username, a password, and an optional email address; the address is stored
unconfirmed and a link is mailed. Following the link lands on a page with a button, because mail
scanners follow links. An address becomes unique only once confirmed, so nobody can squat on
yours by typing it into the form. `/settings` changes the address or the password (ending every
other session) and `/forgot` mails a reset link -- only to a confirmed address, and with the same
reply whether or not the address is known.

Anyone signed in can start a thread from a space page (`/s/general/new`), reply, edit their own
posts, and delete them, which leaves a `[deleted]` marker so replies keep their context. Edited
text goes back through the same Tier 0 heuristics as a new post. Every thread has a feed at
`/t/{id}.rss`, and `/u/{name}` lists a member's recent posts.

Mail goes through whichever provider `MAIL_PROVIDER` names. The default is Cloudflare's own
Email Service through the `[[send_email]]` binding (Workers Paid) -- no key, one command to
onboard the domain:

```sh
npx wrangler email sending enable your.domain      # adds SPF and DKIM; once
# and in wrangler.toml [vars]:
#   EMAIL_FROM = "notespace <no-reply@your.domain>"
#   SITE_NAME  = "notespace"
#   REQUIRE_EMAIL = "false"                          # "true" to make it mandatory
```

Or any of `resend`, `postmark`, `sendgrid`, `mailgun`, `brevo`, or `cloudflare_api` (the same
service over REST), each with `wrangler secret put MAIL_API_KEY` and a sender on a domain that
provider has verified. Each provider's request and response shape is a unit-tested table in
`crates/core/src/email/providers.rs`, so adding one is a few lines and no network.

With anything missing, mail is simply off: accounts still work, addresses stay unconfirmed, and
`/forgot` accepts requests it cannot fulfil (and logs that it could not).

To close registration for a test, `wrangler secret put SIGNUP_CODE`; `/register` then asks for
the code. See [docs/DEPLOY.md](docs/DEPLOY.md#running-a-closed-test) for the heavier gates
(Cloudflare Access, WAF rate limits).

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

To make the first admin, set `MODERATORS = "alice"` in `wrangler.toml`; from `/admin` an admin
grants roles, creates and configures spaces (including each space's moderation policy), edits
and moves threads, hides posts, and bans accounts. Moderators get everything but roles and
spaces. The queue needs creating once:
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

[docs/DEPLOY.md](docs/DEPLOY.md). Self-hosted: `cargo build --release -p notespace-server`, run
`notespace` with `MODERATORS` set, put Caddy in front. Cloudflare: `wrangler d1 create`,
`wrangler queues create`, five lines in `wrangler.toml`, one secret, and `wrangler deploy`. Sign-in through
Google or GitHub is the recommended shape; local passwords are a cargo feature
(`NOTESPACE_FEATURES=password`) with a mandatory pepper. The look is `SITE_THEME` in
`wrangler.toml`; each space has its own on top.

## Layout

```
crates/core/      domain model, Store trait, materialized paths, moderation  (pure, no I/O)
crates/render/    markdown -> sanitized HTML, page templates, public/ (stylesheet, fonts, scripts)
crates/app/       routes and handlers over a Platform trait; app-macros/ is its #[handler]
crates/server/    the binary: Platform on SQLite + reqwest + env, tokio current-thread
crates/worker/    Cloudflare Workers entrypoint: Platform on bindings + D1 adapter + consumers
crates/store-sqlite/ the SQLite Store; conformance and pipeline tests
crates/seed/      deterministic seed + fixture generator
crates/hn-import/ Hacker News -> seed SQL, through the real write path
crates/bench-wasm/ CPU harness, run under Node
migrations/       plain SQL, forward-only, dialect-identical for D1 and native SQLite
scripts/          spike.sh reproduces the numbers in docs/M0-findings.md
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
