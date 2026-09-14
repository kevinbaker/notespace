# Deploying notespace

Three ways to run it, in order of how finished they are:

| target | status at v0.1.0 | what it costs | what it holds |
|---|---|---|---|
| [Cloudflare, free plan](#cloudflare-free-plan) | **This is the release.** `dev.notespace.org` runs on it. | $0 | a small community: ~30k pageviews a day, ~500 AI classifications a day |
| [Cloudflare, Workers Paid](#cloudflare-workers-paid) | Same code, more headroom, two settings to change. | $5/month plus usage | no request cap; OWASP-grade password hashing; Cloudflare's own mail |
| [Self-hosted on SQLite](#self-hosted-sqlite) | **Not yet.** The storage adapter exists and passes the same tests as D1; the HTTP server around it does not. | — | — |

Everything below assumes a machine with Rust, Node and a logged-in `wrangler`. Nothing here
works from Cloudflare's dashboard build, which ships Node but not `cargo` — see
[Why the dashboard build fails](#why-the-dashboard-build-fails).

## Cloudflare, free plan

### What the free plan means for a forum

The free plan's limits, and what notespace spends against each (measured, not estimated; see
[M0-findings.md](M0-findings.md) and the `Server-Timing` header on any page):

| limit | free plan | a notespace pageview costs | so a day holds |
|---|---|---|---|
| Worker requests | 100,000 / day | 1 (the stylesheet, fonts and script are static assets: **free and uncounted**) | ~100k pageviews, minus writes and API calls |
| Worker CPU | 10 ms / request | 0.05 ms for a 200-post thread | not the constraint |
| D1 rows read | 5,000,000 / day | ~2 per post on the page, 404 for a 200-post page; a cache hit reads 1 | 12k cold 200-post pages, or far more cached ones |
| D1 rows written | 100,000 / day | a reply is two to four | ~25k replies |
| Queue operations | 10,000 / day | ~3 per post held for moderation | ~3k held posts; the cron sweep covers a dead queue |
| Workers AI | 10,000 neurons / day | ~20 per classification (Llama 3.1 8B, ~600 tokens in) | ~500 classifications; past that, posts wait for a human |
| Cron triggers | 5 / account | 1 | — |
| Static assets | 20,000 files, 25 MiB each | 10 files | — |

The binding constraint is requests per day. Edge caching (`cache::put` in the Worker) means a
popular thread costs one Worker request per reader but no D1 for most of them.

**Mail is the one thing the free plan does not include.** Cloudflare's own Email Service is
paid-plan only. On the free plan, verification and reset mail go through Resend, Postmark,
SendGrid, Mailgun or Brevo (all have free tiers), or nowhere — the site works without mail; it
just cannot reset a forgotten password. Sign-in through Google or GitHub needs no mail at all,
and is the recommended default.

### 1. Clone and create the resources

```bash
git clone https://github.com/kevinbaker/notespace && cd notespace
rustup target add wasm32-unknown-unknown
cargo install worker-build
npm install                                    # wrangler

npx wrangler login
npx wrangler d1 create notespace               # prints a database_id
npx wrangler queues create notespace-moderation
```

### 2. Edit `wrangler.toml`

Five things are yours to set; everything else can stay:

```toml
name = "notespace"                             # the Worker's name; must match the one your
                                               # domain is attached to, or a second Worker appears
[[d1_databases]]
database_name = "notespace"
database_id = "<the id d1 create printed>"     # an identifier, not a secret; commit it

[vars]
MODERATORS = "yourname"                        # the account that is admin before anyone has the role
SITE_NAME = "Your forum"                       # in mail and page titles
SITE_THEME = """                               # optional; see "The look" below
"""
```

The `[[send_email]]` block can stay — without a paid plan it refuses every send, is logged, and
the flow carries on — or be deleted. The `[ai]` block enables the classifier; without it,
moderation holds posts for a human with no model in the loop.

### 3. Secrets

```bash
openssl rand -hex 32 | npx wrangler secret put CSRF_KEY      # required
npx wrangler secret put SIGNUP_CODE                          # optional: invite-only registration
```

### 4. Schema and first deploy

```bash
npx wrangler d1 migrations apply notespace --remote          # --remote or you migrate a local copy
npx wrangler deploy
```

`deploy` runs `worker-build` through the `[build]` command, uploads the static assets from
`crates/render/public`, and attaches the cron. The Worker answers on `<name>.<account>.workers.dev`
unless `workers_dev = false` (it is, in the committed file: set it `true` until you have a
domain, or attach one under the Worker's Settings > Domains & Routes).

Then turn on **SSL/TLS > Edge Certificates > Always Use HTTPS** for the zone. The Worker also
redirects http to https itself and sends HSTS, so nothing depends on this, but it saves a
round trip.

### 5. Sign-in

Choose one or both. With neither, the site is read-only.

**Google / GitHub (recommended).** Register an OAuth client with each provider you want:

- Google: console.cloud.google.com/apis/credentials → Create credentials → OAuth client ID →
  Web application. Authorised redirect URI: `https://your.domain/auth/google/callback`. The
  consent screen needs configuring once ("External", scopes `email`, `profile`, `openid`).
- GitHub: github.com/settings/developers → OAuth Apps → New OAuth App. Authorization callback
  URL: `https://your.domain/auth/github/callback`.

```toml
OIDC_GOOGLE_CLIENT_ID = "1234-abc.apps.googleusercontent.com"
OIDC_GITHUB_CLIENT_ID = "Iv1.abcdef"
```

```bash
npx wrangler secret put OIDC_GOOGLE_CLIENT_SECRET
npx wrangler secret put OIDC_GITHUB_CLIENT_SECRET
npx wrangler deploy
```

A provider with an id but no secret (or the reverse) is logged and not offered. The redirect URI
is derived from the request's host, or from `BASE_URL`, and must match the provider's record to
the character. The first sign-in asks for a username; the account has no password, so no mail is
needed for it.

**Passwords.** Compiled in as a cargo feature, off by default, because OWASP-grade hashing needs
~57 ms of CPU and the free plan allows 10. Enabling it anyway runs Argon2id at reduced parameters
(4 MiB, t=1, ~4 ms) and makes a **pepper mandatory** — a server-side secret mixed into every
hash, which is what keeps a leaked database uncrackable at those parameters:

```bash
printf '1=%s' "$(openssl rand -hex 32)" | npx wrangler secret put PASSWORD_PEPPER
NOTESPACE_FEATURES=password npx wrangler deploy
```

`PASSWORD_PEPPER` is a list, `1=<secret>;2=<secret>;…`, highest id current. To rotate, append
an entry; every stored hash names the pepper that made it, and logins under an older one are
rewritten under the current one as accounts come back. A malformed list refuses password login
outright (503 on anything touching a password; reading is unaffected) rather than loading part of
it. The feature has to be in the environment of every `deploy` and `dev`, since wrangler runs the
build itself. The pepper is not auto-generated because a Worker has nowhere safe to keep one.

### 6. Mail (optional; password reset needs it)

```toml
MAIL_PROVIDER = "resend"        # resend | postmark | sendgrid | mailgun | brevo | cloudflare_api
EMAIL_FROM = "Your forum <no-reply@your.domain>"
```

```bash
npx wrangler secret put MAIL_API_KEY
```

The sender must be on a domain *that provider* has verified; otherwise every send is a 4xx,
logged, and the flow carries on. Mailgun reads `MAILGUN_DOMAIN` and `MAILGUN_REGION = "eu"`;
Postmark `POSTMARK_STREAM`; `cloudflare_api` `CF_ACCOUNT_ID` and a token with the Email Sending
permission. `REQUIRE_EMAIL = "true"` makes an address mandatory at signup; `BASE_URL` overrides
the request's host in links.

Until a provider is configured, signup says the address is saved but unconfirmed, and `/forgot`
answers "if that address belongs to a confirmed account, a link is on its way" while the log
says it was not.

### 7. Content and moderation

Sign in as the `MODERATORS` account. `/admin/spaces` creates spaces; the site starts with none.
Each space's form sets its nesting depth (0 is a flat board), ranking, moderation policy and
theme.

Moderation defaults are conservative: every post from an account younger than 72 hours is held
for the classifier, and a classifier that is unreachable holds the post for a human. For a beta
where every account is new, lower `new_account_hours` on each space or every first post waits.
The review queue is `/mod/queue`; the public log is `/modlog`.

To fill the database with real threads rather than starting empty,
[HN-IMPORT.md](HN-IMPORT.md) loads a window of Hacker News through the real write path.

### The look

The default is a small, dense, one-accent design in Noto Sans. `SITE_THEME` in `wrangler.toml`
restyles the whole site without touching CSS: one `name: value` per line for the stylesheet's
tokens (`bg`, `bg-2`, `fg`, `fg-2`, `line`, `accent`, `on-accent`, `danger`, `warn`, `font`,
`mono`, `size`, `lh`, `radius`, `measure`, `indent`), `light.` or `dark.` in front of a name to
set it for one colour scheme, and after a line reading `---`, the site's own CSS:

```toml
SITE_THEME = """
accent: #1d4ed8
dark.accent: #7aa2ff
font: "IBM Plex Sans", system-ui, sans-serif
---
.site{border-bottom:3px solid var(--accent)}
"""
```

IBM Plex Sans ships alongside Noto and costs nothing unless a theme names it. Each space has the
same two fields in its admin form, layered over the site's. A theme that does not validate is
logged and ignored (site) or refused with the line number (space).

### Running a closed beta

Three layers, heaviest first; they stack.

1. **Cloudflare Access on the hostname.** Zero Trust > Access > Applications, policy "Emails:
   the testers" or an email domain, login by one-time PIN. Nothing outside the list reaches the
   Worker. Free for up to 50 users, removed in one click when the site goes public.
2. **WAF rate-limiting rules** (Security > WAF > Rate limiting rules), keyed on IP:

   | rule | expression | limit |
   |---|---|---|
   | auth forms | `http.request.method eq "POST" and http.request.uri.path in {"/register" "/login" "/forgot" "/reset"}` | 10 / 10 min, block 1 h |
   | writes | `http.request.method eq "POST" and (http.request.uri.path matches "^/t/.*/reply$" or http.request.uri.path matches "^/s/.*/new$" or http.request.uri.path matches "^/p/.*/(edit\|delete\|report\|appeal)$")` | 20 / 10 min |
   | everything | `http.host eq "your.domain"` | 300 / min |

   Plus Bot Fight Mode under Security > Bots. The Worker has its own per-account rate limits;
   these are the layer in front of it.
3. **An invite code.** `SIGNUP_CODE` makes `/register` ask for it; the site stays readable by
   anyone and writable by people who were told the word. `wrangler secret delete SIGNUP_CODE`
   opens registration.

### Watching it

Every page carries a `Server-Timing` header:

```
server-timing: d1;desc="statements=2", d1_rows;desc="rows_read=403", d1_query;dur=1.8, cache;desc="miss"
```

`statements` should be 2 on a thread page; `rows_read` is what counts against the 5M/day;
`dur` is the D1 round trip. `npx wrangler tail` streams requests and the Worker's log lines,
including the startup posture ("password login is enabled, pepper configured", or why it is
not). CPU time is in the dashboard under the Worker's metrics.

## Cloudflare, Workers Paid

$5/month. The same code and steps; what changes:

| | free | paid |
|---|---|---|
| Worker requests | 100k / day | 10M / month included, then $0.30/M |
| CPU per request | 10 ms | 30 s default, configurable to 5 min |
| D1 reads / writes | 5M / 100k per day | 25B / 50M per month included |
| Queue operations | 10k / day | 1M / month included |
| Workers AI | 10k neurons / day | $0.011 per 1,000 neurons |
| Email Service | not available | available (beta) |

Two things worth changing once on it:

**OWASP-grade password hashing.** Raise the CPU limit and switch the scheme:

```toml
[limits]
cpu_ms = 100

[vars]
PASSWORD_SCHEME = "owasp"      # Argon2id 19 MiB, t=2, ~57 ms; the pepper stays mandatory
```

Accounts hashed under `constrained` keep working — each record carries its own parameters — and
are re-hashed under the stronger ones as their owners sign in. The startup log stops warning
about parameters. (`client-argon`, the third scheme, expects a browser-side derivation that no
client ships yet; leave it.)

**Cloudflare's own mail.** With the `[[send_email]]` binding, no API key: onboard the sending
domain once, which adds its SPF and DKIM records —

```bash
npx wrangler email sending enable your.domain
npx wrangler email sending dns get your.domain
```

— and leave `MAIL_PROVIDER` empty. If `enable` answers `Unauthorized`, the OAuth token predates
the email scope: `wrangler logout` and `login` again. Under `wrangler dev` the binding writes each
message to `.wrangler/tmp/email/` instead of sending, which is a convenient way to read a
verification link.

A paid plan also makes the Anthropic or OpenRouter classifiers (`MOD_PROVIDER`, with
`ANTHROPIC_API_KEY` or `OPENROUTER_API_KEY` as secrets) a matter of taste rather than budget;
their prompts and the evaluation behind the defaults are in [DECISIONS.md](../DECISIONS.md).

## Self-hosted, SQLite

**Not shipped in v0.1.0.** The design has always had two targets, and the storage half of the
second one is done: `crates/store-sqlite` implements the same `Store` trait as the D1 adapter,
passes the same conformance suite (`cargo test`), the migrations are dialect-identical, and the
seed and Hacker News importers write SQLite files today. What does not exist is the server
around it — the Cloudflare Worker's handlers are written against `worker::Env`, D1 and the
Cache API.

What a `crates/server` binary needs, in the order it would be built:

1. axum on tokio, mounting the same routes over `SqliteStore`, with an in-process cache in
   place of the Cache API and `include_bytes!` in place of static assets;
2. secrets from the environment or a file, and a pepper generated on first run (a binary has a
   disk; a Worker does not, which is why the Worker never generates one);
3. `Scheme::OWASP` for passwords, unchanged — there is no 10 ms limit;
4. a classifier over HTTP (Anthropic or OpenRouter; the providers exist) and a mail provider
   over HTTP (all five exist), in place of the AI and Email bindings;
5. a background task for the moderation sweep, in place of the cron trigger.

Steps 3–5 are wiring; 1 and 2 are the work. This is milestone M5 in [DESIGN.md](../DESIGN.md).
Until then, the SQLite adapter is how the test suite and the import tooling run, not how the
site does.

## Why the dashboard build fails

Cloudflare's build image ships Node, not `rustup`, `cargo` or `worker-build`, so the `[build]`
command cannot run there. A dashboard-driven build fails and the Worker keeps serving whatever
it had — a freshly created Worker still returns its placeholder after a failed build. Build from
a machine with Rust, or from CI that installs it and runs `wrangler deploy`.

## Gotchas

- **`name` must match the Worker that owns the route.** Deploying under a different name silently
  creates a second Worker and leaves the routed one on its placeholder.
- **Bindings come from `wrangler.toml`, not the dashboard**, and replace what the dashboard has
  on every deploy. The D1 binding is `DATABASE`; it must match `DB_BINDING` in
  `crates/worker/src/lib.rs`, and a mismatch fails at runtime ("no D1 binding"), not at build.
- **Undeclared routes survive a deploy.** A domain attached in the dashboard is left alone unless
  `wrangler.toml` declares routes; a `custom_domain` that already exists elsewhere makes the
  deploy fail rather than adopt it.
- **`wrangler dev` rebuilds on every change with whatever `NOTESPACE_FEATURES` was in its
  environment when it started.** A bundle built by hand with other features is replaced at the
  first edit.
- **`wrangler dev` calls the real Workers AI and spends real neurons**, local or not.
- **Resetting the local D1 under a running `wrangler dev`** (`rm -rf .wrangler/state/v3/d1`)
  leaves that server holding a deleted database; every query 500s until it is restarted.
- **Static assets are served before the Worker**, so a request for a path that exists under
  `crates/render/public` never reaches your routes. Nothing in the app depends on that today;
  keep new asset names out of the URL space (`/static/…`).
- **Local secrets** go in `.dev.vars` (gitignored): `CSRF_KEY`, `PASSWORD_PEPPER`, `MAIL_API_KEY`,
  the OIDC secrets. Under `wrangler dev` the OIDC callback is
  `https://127.0.0.1:8788/auth/<provider>/callback`, which Google and GitHub both accept for a
  test client.
