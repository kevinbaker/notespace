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

## Enabling password login (optional)

Off by default: authentication is expected to be external (OIDC), because OWASP-grade password
hashing does not fit the free plan's 10 ms CPU budget (DESIGN.md §4.10).

If you enable it anyway, a **pepper is mandatory** — it is what keeps a leaked database
uncrackable at the reduced Argon2 parameters this target can afford. Password login refuses to
start without one, and says so on every request that touches a password.

```bash
printf '1=%s' "$(openssl rand -hex 32)" | wrangler secret put PASSWORD_PEPPER
openssl rand -hex 32 | wrangler secret put CSRF_KEY
```

`PASSWORD_PEPPER` is a **list**: `1=<secret>;2=<secret>;…`, highest id current. One variable
rather than one per pepper, because a scan over numbered bindings cannot tell "id 3 was never
used" from "id 3 failed to load" — and silently holding fewer peppers than intended strands
accounts. Any parse error refuses login outright rather than loading part of the set.

Then build with the feature. The `[build]` command reads it from the environment, because
`wrangler dev` and `wrangler deploy` run the build themselves and would otherwise overwrite a
hand-built bundle with the default one:

```bash
NOTESPACE_FEATURES=password wrangler deploy
NOTESPACE_FEATURES=password wrangler dev --local
```

### Choosing where the hashing happens

Optional var `PASSWORD_SCHEME`, default `constrained`:

| value | meaning | server CPU (p95) |
|---|---|---|
| `constrained` | Server hashes the password at 4 MiB, t=1. Below OWASP; the pepper is what makes it tolerable. | 4.18 ms (42% of budget) |
| `client-argon` | Client runs OWASP-grade Argon2id and posts the 32-byte key as hex; the Worker only peppers it. | 1.25 ms (12%) |

An unrecognised value **refuses** login rather than falling back — an operator who asked for the
stronger scheme and silently got the weaker one is the failure this is guarding against.

**`client-argon` has no browser client yet.** The stock login form posts a plaintext password,
which that scheme rejects. Use it only if your client performs the derivation itself; the Worker
warns loudly at startup either way. Credentials written under the two schemes are not
interchangeable — client records are stored with a leading `c` and `verify` refuses a mismatch —
so switching schemes means existing accounts must set a new password, not merely log in again.

Create an account:

```bash
cargo run -p notespace-seed -- user alice "<password>" "<the same pepper spec>" \
  | wrangler d1 execute notespace-dev --remote --file=/dev/stdin
```

The pepper is deliberately **not** auto-generated. On a Worker there is nowhere safe to put one:
the only writable store is D1, and a pepper inside the database is not a pepper. A Worker also
cannot write its own secrets, and concurrent isolates would each generate a different value —
so a password hashed by one would fail against every other. A self-hosted binary has none of
those constraints and should generate one on first run; that is M5.

For local `wrangler dev`, put it in `.dev.vars` (gitignored):

```
PASSWORD_PEPPER = "<64 hex characters>"
```

**Rotating**: append an entry. `1=<old>;2=<new>` makes 2 current, and every hash keeps naming
the id that made it, so nothing already stored changes meaning. Logins under an older pepper are
rewritten under the current one as accounts come back. Nothing has to be retired on a deadline:
holding every pepper is free, because verification looks up exactly one.

## Enabling mail (optional, needs password login for reset)

Address verification, password reset and the "your password changed" notice go through Resend.
Nothing is sent unless both halves are configured:

```bash
wrangler secret put RESEND_API_KEY        # re_... from https://resend.com/api-keys
```

and in `wrangler.toml` under `[vars]`, a sender on a domain you have verified in the Resend
dashboard (an unverified domain is a 4xx from the API, logged, and the flow carries on without
the mail):

```toml
EMAIL_FROM = "notespace <no-reply@your.domain>"
SITE_NAME = "notespace"
REQUIRE_EMAIL = "false"   # "true" once mail works, if accounts must have an address
BASE_URL = ""             # links use the request's host unless set
```

For local `wrangler dev`, `RESEND_API_KEY` goes in `.dev.vars` like the pepper. Without it,
signup says the address is saved but unconfirmed, and `/forgot` still answers "if that address
belongs to a confirmed account, a link is on its way" -- the log says it was not.

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
- **`wrangler dev` rebuilds on every source change with the `[build]` command**, which means
  with whatever `NOTESPACE_FEATURES` was in its environment when it started. A bundle built by
  hand with other features is replaced the first time a file changes.
- **Resetting the local D1 under a running `wrangler dev`** (`rm -rf .wrangler/state/v3/d1`, as
  `scripts/spike.sh` does) leaves that server holding a deleted database: every query 500s
  until it is restarted.
