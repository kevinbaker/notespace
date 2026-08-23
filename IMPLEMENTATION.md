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

Not yet served: there are no `/s/{path}` or `/u/{name}` routes. The types exist and are tested;
nothing routes to them.

## §4.7 Post public ids

`Post.public_id` in `crates/core/src/model.rs`, `migrations/0004_post_public_id.sql`, and the
`/p/{id}` route in `crates/worker/src/lib.rs`. Resolution is `Store::locate_post`, which also
returns the page cursor that makes the post's anchor reachable; the arithmetic is
`store::page_cursor_offset`.

## §4.8 D1 working set, R2 corpus

Not implemented. `migrations/0004_post_public_id.sql` carries the note about what archiving will
need.

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
change on nearly every write.

Two dangling links in the UI: `/s/{path}` in the thread header and `/u/{name}` on every byline
both 404, because neither route exists.

## §5 Moderation pipeline

Not implemented. `PostState::Pending` in `crates/core/src/model.rs` is the only part present.

## §6 Presets

Partially present as data: `Space.depth_cap` and `Space.ranking` in `crates/core/src/model.rs`.
There is no preset table or selector yet.

## §7 Routes

`router()` in `crates/worker/src/lib.rs` is the live list. Currently: `/`, `/healthz`, `/t/{id}`,
`/t/{id}/{slug}`, `/p/{id}`, `/t/{id}/reply`, `/login`, `/logout`, `/register`, `/__conformance`.

`SpaceKey::RESERVED` and `Username::RESERVED` are hand-maintained and should be derived from this
router instead.

## §8 Milestones

M0 and the read path are done and measured. M1's `Store` seam is done. M2 is partial: login and
logout exist; registration, the reply form, edit/delete and RSS do not. `crates/server` (M5) does
not exist.

---

## Things that exist in code but not in the design

- `crates/store-sqlite` — the second adapter, added to make §3.1 checkable.
- `crates/bench-wasm` — CPU measurement under the engine that actually runs the code.
- `crates/seed` — fixture generation, including `user <name> <password> <pepper-spec>`.
- `scripts/spike.sh` — builds the bench, resets local D1, applies migrations, runs the suites.
