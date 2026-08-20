# notespace

A configurable, AI-moderated, threaded forum in Rust that deploys to a free Cloudflare account
*or* runs as a single self-hosted binary.

> **Status: M0 spike complete.** The read path works end to end and the free-tier premise is
> measured and confirmed. There is no auth, no writing, and no deployment story yet. See
> [DESIGN.md](DESIGN.md) for where this is going and [docs/M0-findings.md](docs/M0-findings.md)
> for what has actually been proven.

## What exists today

A Cloudflare Worker that serves a threaded, 200-post forum page from D1:

- **`crates/core`** — domain model, the `Store` trait, and materialized tree paths. No I/O, no
  target awareness, property-tested.
- **`crates/render`** — markdown → sanitized HTML (pulldown-cmark + ammonia) at write time, and
  maud page templates for the read path.
- **`crates/worker`** — axum on Workers, wired to a D1-backed `Store`.
- **`crates/seed`** — deterministic seed/fixture generator that renders through the real write path.
- **`crates/bench-wasm`** — exposes the render paths to a Node harness so CPU can be measured in
  real wasm.

## The M0 result

| Budget (Cloudflare free plan) | Limit | Measured |
|---|---|---|
| Worker CPU / request | 10 ms | **0.048 ms** (200-post page) |
| Worker script size | 3 MB | **139.6 KB** gzipped |
| D1 queries / invocation | 50 | **2**, one batched round trip |

CPU was the thing M0 existed to de-risk, and it turned out to have ~90x headroom at p99. For a
small forum the binding constraint is the **100k requests/day** cap, not CPU or D1.

Caveat worth reading before relying on this: all of it is `wrangler dev --local`. Nothing has
been deployed to real Cloudflare, and `rows_read` — which D1 reports only in production — is
derived from the query plan rather than observed.

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

### Deploying

Not ready. `wrangler.toml` carries a placeholder `database_id`; a real deploy needs
`wrangler d1 create notespace` and the returned id pasted in. Auth and writes do not exist yet,
so anything deployed today is a public read-only demo.

## Layout

```
crates/core/      domain model, Store trait, materialized paths   (pure, no I/O)
crates/render/    markdown -> sanitized HTML, page templates
crates/worker/    Cloudflare Workers entrypoint + D1 adapter
crates/seed/      deterministic seed + fixture generator
crates/bench-wasm/ CPU harness, run under Node
migrations/       plain SQL, forward-only, dialect-identical for D1 and native SQLite
scripts/          spike.sh reproduces every number in docs/M0-findings.md
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
