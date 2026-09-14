# Changelog

## 0.1.0 — 2026-09-13

First release. Suitable for a limited beta on a free Cloudflare account; see
[docs/DEPLOY.md](docs/DEPLOY.md).

### What is in it

- **Reading.** Spaces (nested, with per-space nesting depth and ranking), threads, replies as a
  tree, permalinks that land on the right page, per-thread RSS, member profiles. Pages are
  baked once per version and served from the edge cache; a 200-post thread costs 0.05 ms of
  CPU and two D1 statements.
- **Accounts.** Sign in with Google or GitHub, or with a local password (a cargo feature, with
  a mandatory pepper). Email verification and password reset by mail through Cloudflare's
  Email Service or Resend, Postmark, SendGrid, Mailgun or Brevo. Sessions, "sign out
  everywhere", CSRF on every form, `__Host-` cookies, HSTS, http→https.
- **Writing.** New threads, replies with the parent shown and quoting, edit and delete (as a
  tombstone), report, appeal. Per-account rate limits. Markdown rendered and sanitised once, at
  write time.
- **Moderation.** Free heuristics on every post (account age, links, duplicates, blocklist,
  text addressed to the model), then a classifier — Workers AI Llama 3.1 8B by default; Llama
  Guard, Anthropic or anything OpenAI-compatible by configuration — behind a queue with a cron
  sweep. Review queue, appeals, a public log of every action, and per-space metamoderation that
  tunes the model's thresholds from reviewer agreement.
- **Admin.** Dashboard, users (roles, bans), spaces (create, configure policy and theme),
  threads (pin, lock, move, hide), the full action log.
- **The look.** One stylesheet, small dense type in Noto Sans, one green accent. Every colour,
  size and face is a token; the site (`SITE_THEME`) and each space override tokens and add CSS
  of their own, served as versioned immutable stylesheets. Light and dark.
- **Tooling.** A deterministic seed, a Hacker News importer that runs real threads through the
  real write path, a wasm CPU benchmark harness, a page preview that needs no database, and a
  conformance suite that both storage adapters pass.

### Known limitations — read before a beta

- **No self-hosted server yet.** The SQLite storage adapter passes the same tests as D1, but
  the HTTP server around it is not built. Cloudflare is the only deployment target in this
  release.
- **Mail on the free plan needs an outside provider.** Cloudflare's Email Service is paid-plan
  only. Without any provider the site runs, but a forgotten password cannot be reset — one
  reason Google/GitHub sign-in is the recommended default.
- **Password hashing on the free plan is below OWASP's minimum** (4 MiB Argon2id against a
  10 ms CPU budget). The pepper is the mitigation. `PASSWORD_SCHEME=owasp` on a paid plan is the
  fix.
- **The classifier budget is ~500 posts a day** on the free plan's Workers AI allowance, and
  the default 8B model over-flags ordinary comments (DECISIONS.md has the evaluation). Past the
  budget, held posts wait for a moderator. For a beta where every account is new, lower each
  space's `new_account_hours` or expect to clear the queue by hand.
- **No search, no notifications, no mentions.** Reading is by browsing and RSS.
- **No self-service account deletion.** A moderator can ban or delete an account; a member
  cannot delete their own from `/settings`. Anyone running a beta in a jurisdiction that
  requires it should be ready to do this on request.
- **Themes are edited by site admins only.** A space's custom CSS may use `url()`, which is a
  per-reader IP disclosure to whoever hosts the image; that is acceptable for site admins and
  would need revisiting before space-level moderators can edit themes.
- **The header personalises by script.** Without JavaScript every page shows "sign in ·
  register" regardless; the pages themselves are shared byte-for-byte by design.
- **Free-plan capacity is about 100k Worker requests a day**, static assets excluded. A
  popular thread costs one request per reader, D1 nothing; a signed-in reader costs two.
- **Backups are D1's.** `wrangler d1 export` for a copy; D1 Time Travel for point-in-time
  restore. Nothing in the app does this for you.
- **Migrations are forward-only.** There is no down migration and no in-app migration runner;
  `wrangler d1 migrations apply --remote` is the step, before `deploy`.
