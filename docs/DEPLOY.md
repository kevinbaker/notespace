# Deploying the M0 spike

Run these from a machine with Rust and a logged-in `wrangler`. Nothing here works from
Cloudflare's dashboard build — see [Why the dashboard build fails](#why-the-dashboard-build-fails).

## One-time

```bash
rustup target add wasm32-unknown-unknown
cargo install worker-build
wrangler login
```

Check it the way wrangler will, not the way your shell will:

```bash
/bin/sh -c 'worker-build --version'      # fails? see below -- wrangler.toml handles it
```

Wrangler runs the build through `/bin/sh`, which does not source your shell profile, so
`~/.cargo/bin` is typically missing from its PATH even though your interactive shell has it.
That produces `worker-build: not found` and exit 127 from a correctly installed tool.
`wrangler.toml` prepends `$HOME/.cargo/bin` to PATH in the build command to cover this. If
`cargo install worker-build` put the binary somewhere else, point that line at it.

The database id is already in `wrangler.toml` (`wrangler d1 info notespace-dev` prints it if it
ever needs re-checking). It is an identifier, not a secret, and belongs in version control.
**The bindings in
`wrangler.toml` replace whatever the dashboard has when you deploy**, so a wrong or missing id
here points the deployed Worker at the wrong database — every page 500s with the route and
binding both looking correct in the UI.

## Deploy

```bash
# 1. Schema, against the REAL database. --remote is the whole point; without it you migrate
#    the local SQLite copy and the deployed Worker sees an empty database.
wrangler d1 migrations apply notespace-dev --remote

# 2. Seed one 200-post thread so there is something to measure.
cargo run -q -p notespace-seed -- 200 sql mixed > seed.sql
wrangler d1 execute notespace-dev --remote --file=seed.sql

# 3. Build and ship.
wrangler deploy
```

Then:

```bash
curl -sD- https://dev.notespace.org/t/06a1yabw03jnhej1 -o /dev/null | grep -i server-timing
```

## What to look for

The `Server-Timing` header carries the numbers M0 could not measure locally:

```
server-timing: d1;desc="statements=2", d1_rows;desc="rows_read=403", d1_query;dur=1.8
```

| field | why it matters |
|---|---|
| `statements` | Must be **2**. The free tier allows 50 per invocation, and the read path's whole design is not going per-post. |
| `rows_read` | Free tier is 5M/day. Locally this reads **404** for a 200-post page, which matches the ~403 `docs/M0-findings.md` derived from the query plan. |
| `dur` | The D1 round trip. Locally ~2 ms against an in-process SQLite; in production this crosses a network, and it is the number that decides whether the design is actually viable. |

Miniflare's D1 does populate all three locally — worth knowing, because it means the local
header is a real check of the wiring rather than a stub. What local mode cannot tell you is the
production `dur`, since there is no network in the path, and whether Cloudflare's `rows_read`
accounting matches the emulator's. Those are what the deploy is for.

Worker CPU time is in the dashboard under the Worker's metrics, or via `wrangler tail`.

## Why the dashboard build fails

Cloudflare's build image ships Node, not `rustup`, `cargo` or `worker-build`, so the `[build]`
command in `wrangler.toml` cannot run there. A dashboard-driven build fails and the Worker keeps
serving whatever it had — which is why a freshly created Worker still returns "Hello World"
after a failed build. There is no binding or route that changes this: the placeholder is the
deployed code until a successful deploy replaces it.

Building in CI is possible (install Rust, then `wrangler deploy`), and is the right answer once
this is worth automating. It is not worth automating yet.

## Gotchas

- **`name` must match the Worker that owns the route.** `wrangler.toml` says `notespace-dev`
  because that is the Worker attached to `dev.notespace.org`. Deploying under a different name
  silently creates a *second* Worker and leaves the routed one on its placeholder.
- **The binding name is `DATABASE`.** It must match `DB_BINDING` in
  `crates/worker/src/lib.rs`. A mismatch fails at runtime with "no D1 binding", not at build,
  so it looks correct everywhere until a page is requested.
- **Bindings come from `wrangler.toml`, not the dashboard.** Deploying with wrangler applies the
  D1 binding declared here. Adding one by hand in the dashboard is unnecessary and will be
  overwritten.
- **Undeclared routes survive.** `wrangler.toml` deliberately does not declare
  `dev.notespace.org`, so the dashboard-attached route is left alone by a deploy.
