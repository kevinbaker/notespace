# Implementation map

Where each part of `DESIGN.md` actually lives. References point **into** the code; the code does
not point back out. Symbols are named rather than line numbers, so this survives edits.

Companion documents: `DESIGN.md` is what the system should be, `DECISIONS.md` is why the code
took the shape it did, and this file is the index between them.

---

## §3.1 Crate layout and the `Store` seam

| what | where |
|---|---|
| The trait, and the per-method query budgets | `crates/core/src/store.rs` — `Store` |
| Shared SQL, verbatim on both targets | `crates/core/src/sql.rs` |
| D1 adapter | `crates/worker/src/store.rs` — `D1Store` |
| Native SQLite adapter | `crates/store-sqlite/src/lib.rs` — `SqliteStore` |
| One suite run against both | `crates/core/src/conformance.rs` — `run_all` |
| …natively | `crates/store-sqlite/tests/conformance.rs` |
| …against real D1 | route `/__conformance` in `crates/worker/src/lib.rs` |

The migration list has a completeness guard in `crates/store-sqlite/tests/conformance.rs` —
`migration_list_is_complete`.

## §3.2 Target constraints

| what | where |
|---|---|
| No clock, no RNG in `core` | every constructor takes them as parameters; see `PublicId::new` |
| Platform clock and CSPRNG | `crates/worker/src/ids.rs` |
| Per-request query telemetry | `crates/worker/src/store.rs` — `QueryStats`, emitted as `Server-Timing` |
| CPU benchmarks under V8 | `crates/bench-wasm/`, driven by `scripts/kdf-bench.mjs` and `scripts/kdf-sweep.mjs` |

## §3.3 Read path — bake, don't render

| what | where |
|---|---|
| Write-time rendering | `crates/render/src/markdown.rs` |
| Read-time assembly | `crates/render/src/page.rs` — `thread_page` |
| The user-agnostic invariant | `baked_page_contains_no_viewer_identity` in `page.rs` |
| Cache headers and edge caching | `crates/worker/src/cache.rs`; key logic in `crates/core/src/cache_key.rs` |

Timestamps are rendered by `crates/render/src/time.rs` — `stamp`, UTC, either unit. Error
responses other than 400 are `notespace_render::error_page`; unknown routes hit the router's
fallback; `/favicon.ico` is a 204 and every page declares `href="data:,"` so browsers do not
ask.

## §3.5 What the client may and may not do

`SanitizedHtml` in `crates/core/src/model.rs` is the type that enforces it — the only
constructor runs the sanitizer, and `Post.body_html` accepts nothing else.

## §4 Data model

| what | where |
|---|---|
| Domain types | `crates/core/src/model.rs` |
| Schema | `migrations/0001_init.sql` onward |

## §4.1 Path encoding

`crates/core/src/path.rs` — `Path`, `SEGMENT_WIDTH`, `ALPHABET`. The ordering invariant is
property-tested in the same module.

## §4.2 / §4.3 Id scheme and widening

`crates/core/src/id.rs` — `PublicId`. Conversions to and from ULID/UUIDv7 are `from_ulid`,
`from_uuid`, `to_ulid`, `to_uuid`; the `u128` bridge is `to_u128` / `from_u128`. Widening is
`with_width`, and prefix compatibility is pinned by
`wider_ids_keep_the_narrow_one_as_a_literal_prefix`.

## §4.4 / §4.5 / §4.6 Names, usernames, spaces

| what | where |
|---|---|
| Shared validation and case-only normalization | `crates/core/src/naming.rs` |
| Usernames, permanent, with their own reserved list | `crates/core/src/username.rs` |
| Space keys and paths, unique per parent | `crates/core/src/space_key.rs` |
| Schema | `migrations/0003_space_paths_and_names.sql` |

| Space page and new-thread form | `/s/{*path}` in `crates/worker/src/posting.rs` — `space`, `space_post` |
| Profile | `/u/{name}` — `posting::profile`; `Store::user_profile`; `crates/render/src/profile.rs` |

A space lists its own threads and every subspace's, through `Store::space_threads` and the
`SpacePath::subtree_range` scan; `migrations/0009_email_and_spaces.sql` backfills
`thread.space_path` so seeded rows are included.

## §4.7 Post public ids

`Post.public_id` in `crates/core/src/model.rs`, `migrations/0004_post_public_id.sql`, and the
`/p/{id}` route in `crates/worker/src/lib.rs`. Resolution is `Store::locate_post`, which also
returns the page cursor that makes the post's anchor reachable; the arithmetic is
`store::page_cursor_offset`.

## §4.8 D1 working set, R2 corpus

Not implemented. `migrations/0004_post_public_id.sql` carries the note about what archiving will
need.

## Email and account self-service

| what | where |
|---|---|
| Addresses, tokens, messages, the `Mailer` seam | `crates/core/src/email/mod.rs` |
| Every provider's request and response shape, as data | `crates/core/src/email/providers.rs` — `Provider`, `Sender` |
| Verification, address change, reset request/completion, password change | `crates/core/src/account.rs` — `send_verification`, `change_email`, `confirm_email`, `request_reset`, `complete_reset`, `change_password` |
| Signup's email field and verification mail | `crates/core/src/register.rs` — `Signup.email`, `RegisterConfig.require_email` |
| The HTTP transport, the Cloudflare binding, provider resolution, link base URL | `crates/worker/src/mail.rs` — `Http`, `CloudflareBinding`, `MaybeMailer::from_env`, `LinkConfig` |
| Handlers | `crates/worker/src/account.rs` — `/settings`, `/settings/{email,password,sessions}`, `/verify`, `/forgot`, `/reset` |
| Pages | `crates/render/src/account.rs` |
| Store methods | `Store::account` through `Store::retire_email_tokens` |
| Schema | `migrations/0009_email_and_spaces.sql` — `user.email`, `user.email_verified_at`, `email_token` |
| Flows end to end with a recording mailer | `crates/store-sqlite/tests/account.rs` |

`MAIL_PROVIDER` selects `cloudflare` (the `[[send_email]]` binding, the default when it
exists), `cloudflare_api`, `resend`, `postmark`, `sendgrid`, `mailgun` or `brevo`; every one
needs `EMAIL_FROM`, the HTTP ones need the `MAIL_API_KEY` secret. `SIGNUP_CODE`, a secret, closes registration: `register::invite_ok` is the constant-time
check, called before the limiter. Mail is off, and every flow
says so, whenever that is incomplete. Password recovery needs the `password` feature;
verification does not. The names are checked against `wrangler.toml` by
`the_mail_names_match_wrangler_toml`.

## §4.9 Sessions

| what | where |
|---|---|
| Token, hash, expiry policy | `crates/core/src/session.rs` |
| Schema | `migrations/0005_session.sql` |
| SQL | `crates/core/src/sql.rs` — `INSERT_SESSION` and neighbours |
| Cookie names and the `__Host-` prefix | `crates/core/src/cookie.rs` |

## §4.10 Password hashing

| what | where |
|---|---|
| Schemes, params, pepper set, hash/verify | `crates/core/src/password.rs` |
| The flow, and the order that is the security property | `crates/core/src/login.rs` |
| Reading the deployment's posture from bindings | `crates/worker/src/auth_config.rs` |
| Startup warnings | `crates/worker/src/startup.rs` |
| Handlers | `login_form`, `login_submit`, `logout` in `crates/worker/src/lib.rs` |
| Form | `crates/render/src/auth.rs` |
| Schema | `migrations/0007_user_password.sql` |
| The PBKDF2 size comparison | `crates/worker/src/subtle_kdf.rs` |

`Scheme::CLIENT_ARGON` has no browser client. The server half is complete and tested; nothing in
the tree performs the client-side derivation.

## §4.11 CSRF

`crates/core/src/csrf.rs` — `CsrfKey::mint`, `mint_for_session`, `verify`. The anonymous binding
cookie used before a session exists is `cookie::ANON`.

## §4.12 Rate limiting

`crates/core/src/ratelimit.rs` — `Limit`, `Attempts`, `AttemptKeys`. Schema in
`migrations/0006_login_attempt.sql`. Called from `login::attempt` before any hashing.

## The write path

| what | where |
|---|---|
| Validation, rate limiting, collision retry | `crates/core/src/reply.rs` — `post`, `check_body` |
| Handlers | `reply_form`, `reply_submit` in `crates/worker/src/lib.rs` |
| Form | `crates/render/src/auth.rs` — `reply_page` |
| Session → user | `current_user` in `crates/worker/src/lib.rs` |

The form is a separate uncached page, not part of the baked thread.

| Starting a thread | `crates/core/src/compose.rs` — `create`, `check`; `Store::create_thread`, `Store::space_context` |
| Editing and deleting | `crates/core/src/edit.rs` — `edit`, `delete`; `Store::update_post_body` |
| Handlers | `crates/worker/src/posting.rs` — `compose_form`, `compose_submit`, `edit_form`, `edit_submit`, `delete_submit` |
| Forms | `crates/render/src/compose.rs` |
| RSS | `crates/render/src/feed.rs` — `thread_rss`; `posting::feed`, reached from `/t/{id}.rss` |
| Against a real store | `crates/store-sqlite/tests/posting.rs` |

Tier 0 is shared: `reply::triage` runs for a reply, a new thread (title included), and an edit.

Not built: the progressive enhancement described in DESIGN.md §7.1 that opens the form in place
when JS is available. There is no `/api/me/thread/{id}` endpoint yet, which is where the CSRF
token for that path has to come from.

## Registration and the index

| what | where |
|---|---|
| Signup flow | `crates/core/src/register.rs` — `signup`, `check` |
| Handlers | `register_form`, `register_submit` in `crates/worker/src/lib.rs` |
| Form | `crates/render/src/auth.rs` — `register_page` |
| Thread index | `crates/render/src/index.rs`; `Store::recent_threads`; `sql::RECENT_THREADS` |
| Index route | `/` in `crates/worker/src/lib.rs` |

The index is uncached: every reply bumps a thread and reorders the list, so a version key would
change on nearly every write. It also lists the top-level spaces (`Store::spaces_under`).

## §5 Moderation pipeline

| what | where |
|---|---|
| Tier 0 heuristics | `crates/core/src/moderation/heuristics.rs` — `triage`, `MANIPULATION_MARKERS` |
| Where Tier 0 runs on the write path | `crates/core/src/reply.rs` — `post`, between the limiter and the insert |
| Per-space policy, thresholds, metamoderation | `crates/core/src/moderation/policy.rs` — `ModerationPolicy::from_config`, `effective`, `decide` |
| The classifier trait, prompt and parser | `crates/core/src/moderation/classify.rs` — `Classifier`, `system_prompt`, `user_message`, `parse_verdict`, `PROMPT_VERSION` |
| Provider request/response shapes | `crates/core/src/moderation/providers.rs` — `workers_ai_*`, `llama_guard_*`, `anthropic_*`, `openai_compatible_*` |
| Layering a safety model in front | `crates/core/src/moderation/layers.rs` — `Layered`, `combine`, `guard_visible` |
| The steps: hold, classify, sweep, report, review, appeal | `crates/core/src/moderation/pipeline.rs` |
| Categories, actors, log and queue row types | `crates/core/src/moderation/mod.rs` |
| Store methods | `Store::write_context` through `Store::agreement` in `crates/core/src/store.rs`; SQL under "Moderation" in `sql.rs` |
| Schema | `migrations/0008_moderation.sql` — `signal`, `action_log`, `review_item`, `user.role` |
| Transport on the Worker | `crates/worker/src/moderation.rs` — `resolve` (reads `MOD_PROVIDER`, `a+b` for layers), `WorkersAi`, `Anthropic`, `OpenAiCompatible`, `QueueProducer` |
| Queue consumer and cron sweep | `queue` and `scheduled` events in `crates/worker/src/lib.rs` |
| Handlers | `report_form`/`report_submit`, `appeal_form`/`appeal_submit`, `mod_queue`, `mod_review`, `modlog`, `held_notice` in `crates/worker/src/lib.rs` |
| Pages | `crates/render/src/moderation.rs`; `held_page` in `crates/render/src/auth.rs` |
| Bindings and vars | `[ai]`, `[[queues.*]]`, `[triggers]`, `[vars]` in `wrangler.toml`; guarded by `the_moderation_binding_names_match_wrangler_toml` |

Tests, by layer:

| what | where |
|---|---|
| Heuristics, policy, prompt, parser, providers | unit tests in each `moderation/*.rs` |
| Pipeline end to end against SQLite with a scripted model | `crates/store-sqlite/tests/moderation.rs` |
| Store methods on both adapters | the moderation checks in `core::conformance::write_checks` |
| Live evaluation against real models | `crates/core/tests/live_classifier.rs` over `tests/fixtures/moderation_corpus.json`; `#[ignore]`, needs credentials |
| Sampling real Hacker News comments through both tiers | `crates/core/examples/hn_moderate.rs` — `--guard`, `--grep`, `--seed` |

Capability is `user.role` plus the `MODERATORS` variable (`can_moderate` in the worker), which
is how the first moderator comes to exist. There is no UI for granting the role; it is a SQL
update for now.

Not built: a general rule engine (the heuristics are fixed code with policy-supplied numbers),
reporter accuracy weighting, per-user notification that a post was held or hidden, the
author's view of their own pending post -- the baked page shows everyone the same tombstone --
and a pending state for a *thread*: a held first post leaves its title visible.

## §6 Presets

Partially present as data: `Space.depth_cap` and `Space.ranking` in `crates/core/src/model.rs`.
There is no preset table or selector yet.

## §7 Routes

`router()` in `crates/worker/src/lib.rs` is the live list. Currently: `/`, `/healthz`,
`/s/{path}`, `/s/{path}/new`, `/u/{name}`, `/t/{id}`, `/t/{id}.rss`, `/t/{id}/{slug}`, `/p/{id}`,
`/p/{id}/edit`, `/p/{id}/delete`, `/t/{id}/reply`, `/t/{id}/held/{post}`, `/p/{id}/report`,
`/p/{id}/appeal`, `/mod/queue`, `/mod/review/{id}`, `/modlog`, `/login`, `/logout`, `/register`,
`/settings`, `/settings/{email,password,sessions}`, `/verify`, `/forgot`, `/reset`,
`/__conformance`.

Not served from the sketch: `POST /p/{id}/signal` (votes), `/api/me/thread/{id}`,
`/uploads/sign`, and `/t/{id}/live`.

`SpaceKey::RESERVED` and `Username::RESERVED` are hand-maintained and should be derived from this
router instead.

## §8 Milestones

M0 and the read path are done and measured. M1's `Store` seam is done. M2 is done except for
htmx and voting: auth, sessions, registration with email, spaces, threads, nested replies,
edit/delete with tombstones, RSS. M4's moderation pipeline is in, ahead of M3, because the
write path needed it. `crates/server` (M5) does not exist.

---

## Things that exist in code but not in the design

- `crates/store-sqlite` — the second adapter, added to make §3.1 checkable.
- `crates/bench-wasm` — CPU measurement under the engine that actually runs the code.
- `crates/seed` — fixture generation, including `user <name> <password> <pepper-spec>`.
- `scripts/spike.sh` — builds the bench, resets local D1, applies migrations, runs the suites.
