//! Cloudflare Workers entrypoint for notespace.
//!
//! M0 scope (DESIGN.md §8): serve a 200-post thread page from D1 and prove it fits inside
//! the free-tier CPU, size and query budgets. Auth, writes and baking are M2/M6.

mod ids;
#[cfg(feature = "kdf-subtle")]
mod subtle_kdf;
mod store;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use notespace_core::conformance::{run_all, Fixture};
use notespace_core::id::PublicId;
use notespace_core::path::Path as TreePath;
use notespace_core::store::{Page, Store, StoreError, StoreResult};
use serde::Deserialize;
use store::D1Store;
use tower_service::Service;
use worker::{event, Context, Env, HttpRequest, Result as WorkerResult};

/// Name of the D1 binding in wrangler.toml.
/// Must match the `binding` name in `wrangler.toml` (and any binding configured in the
/// dashboard). A mismatch surfaces at runtime as "no D1 binding", never at build time.
const DB_BINDING: &str = "DATABASE";

/// Posts per page. 200 is the number DESIGN.md §8 names as the spike target: if a page this
/// size does not fit the budget, the free-tier premise fails.
const PAGE_SIZE: u32 = 200;

#[derive(Deserialize, Default)]
struct PageQuery {
    /// Opaque cursor: the materialized path to resume after.
    after: Option<String>,
}

/// Which thread `/__conformance` should exercise.
#[derive(Deserialize, Default)]
struct ConformanceQuery {
    thread: Option<String>,
}

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> WorkerResult<Response> {
    // Without this a wasm panic surfaces as an opaque 1101 with no stack.
    console_error_panic_hook::set_once();
    Ok(router(env).call(req).await?)
}

fn router(env: Env) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/t/{id}", get(thread_page))
        .route("/t/{id}/{slug}", get(thread_page_slug))
        .route("/p/{id}", get(post_permalink))
        // GET runs the read-only checks; POST adds the write checks, which mutate the thread.
        // A GET that appends posts would be wrong regardless of how convenient it is.
        .route(
            "/__conformance",
            get(conformance).post(conformance_with_writes),
        )
        .with_state(env)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn thread_page_slug(
    state: State<Env>,
    UrlPath((id, slug)): UrlPath<(String, String)>,
    query: Query<PageQuery>,
) -> Response {
    // The slug is decorative: it exists for readability and for search engines, and is never
    // used to resolve the thread. Retitling a thread therefore cannot break its links.
    render_thread(state, id, Some(slug), query).await
}

async fn thread_page(
    state: State<Env>,
    UrlPath(id): UrlPath<String>,
    query: Query<PageQuery>,
) -> Response {
    render_thread(state, id, None, query).await
}

/// Runs the shared conformance suite against D1 and reports it as plain text.
///
/// The other half of `crates/store-sqlite/tests/conformance.rs`: the same
/// [`notespace_core::conformance::run_all`], the same checks, a different adapter. A `#[test]`
/// cannot reach D1 — there is no Worker runtime in `cargo test` — so the suite is exposed as a
/// route and run against a deployed instance instead.
///
/// Takes the thread to exercise as `?thread=<public id>`, rather than assuming an id the seed
/// generator happened to pick. An earlier revision hard-coded one and 503'd against a perfectly
/// good database — a fixture constant duplicated across two crates is exactly the kind of drift
/// this suite exists to catch, so it should not have one.
///
/// Reads only that thread, and writes nothing.
#[worker::send]
async fn conformance(state: State<Env>, q: Query<ConformanceQuery>) -> Response {
    run_conformance(state, q, false).await
}

/// The full suite, write checks included. `POST` because it appends posts to the thread.
#[worker::send]
async fn conformance_with_writes(state: State<Env>, q: Query<ConformanceQuery>) -> Response {
    run_conformance(state, q, true).await
}

async fn run_conformance(
    State(env): State<Env>,
    Query(q): Query<ConformanceQuery>,
    writes: bool,
) -> Response {
    let db = match env.d1(DB_BINDING) {
        Ok(db) => db,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("no D1: {e}")),
    };
    let store = D1Store::new(db);

    let Some(raw) = q.thread.as_deref().filter(|s| !s.is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "pass ?thread=<public id> -- a seeded thread to run the suite against",
        );
    };
    let thread = match PublicId::parse(raw) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };
    // A well-formed id that is not in the database. Derived from the thread's own timestamp so
    // it stays plausible, with randomness no generated id would produce.
    let Ok(absent) = PublicId::new(thread.timestamp_ms(), 0xDEAD_BEEF) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "fixture id out of range");
    };

    // Post count and a real post path come from the database rather than from a constant: a
    // fixture duplicated across two crates is the drift this suite exists to catch.
    let probe = match store.thread_page(&thread, &Page::first(4)).await {
        Ok(p) => p,
        Err(e) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("thread {thread} not found: {e}"),
            )
        }
    };
    let Some(known) = probe.posts.get(3).or_else(|| probe.posts.last()) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "seeded thread has no posts",
        );
    };

    // Real generated ids, so the write checks exercise the same id path a real post takes.
    let writable = if writes {
        match (0..4)
            .map(|_| ids::generate())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(e) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("id generation: {e}"),
                )
            }
        }
    } else {
        Vec::new()
    };

    let fixture = Fixture {
        thread: thread.clone(),
        post_count: probe.thread.post_count,
        known_post: known.public_id.clone(),
        known_post_path: known.path.clone(),
        absent,
        writable,
        // A user that certainly exists: whoever started the thread.
        author_id: probe.thread.author_id,
    };
    let checks = run_all(&store, &fixture).await;
    let failed = checks.iter().filter(|c| !c.passed()).count();
    let mut body = format!(
        "notespace conformance suite -- D1 adapter\nthread {} / {} posts\nwrites: {}\n\n",
        fixture.thread,
        fixture.post_count,
        if writes { "yes (POST)" } else { "no (GET)" }
    );
    for c in &checks {
        match &c.failure {
            None => body.push_str(&format!("ok    {}\n", c.name)),
            Some(why) => body.push_str(&format!("FAIL  {}\n        {why}\n", c.name)),
        }
    }
    body.push_str(&format!(
        "\n{} passed, {failed} failed\n",
        checks.len() - failed
    ));
    let code = if failed == 0 {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        code,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Storage access goes through [`Store`], never through a concrete adapter.
///
/// These two functions exist to make that structural: they are generic, so anything they can do
/// the native adapter can do too. An earlier revision called inherent methods on `D1Store` while
/// the trait sat unused with a different signature -- the seam compiled and carried nothing.
async fn fetch_page<S: Store>(
    store: &S,
    thread: &PublicId,
    page: &Page,
) -> StoreResult<notespace_core::ThreadPage> {
    store.thread_page(thread, page).await
}

async fn locate<S: Store>(store: &S, post: &PublicId) -> StoreResult<notespace_core::PostLocation> {
    store.locate_post(post).await
}

/// A durable permalink to a single post.
///
/// Resolves where the post lives *now* and redirects to that thread page, anchored at the post.
/// The indirection is the point: splitting or merging threads moves a post between threads, and
/// a link that baked in the thread id would break. This one does not.
///
/// 302 rather than 301 -- the target legitimately changes when a post is moved, so this must
/// not be cached permanently by browsers.
#[worker::send]
async fn post_permalink(State(env): State<Env>, UrlPath(id): UrlPath<String>) -> Response {
    let post_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad post id: {e}")),
    };
    let db = match env.d1(DB_BINDING) {
        Ok(db) => db,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("no D1 binding: {e}"),
            )
        }
    };
    match locate::<D1Store>(&D1Store::new(db), &post_id).await {
        Ok(loc) => {
            // KNOWN LIMITATION: this lands on page 1 and relies on the fragment. For a thread
            // longer than one page the anchor will not be present and the reader arrives at
            // the top.
            //
            // Doing better needs "which page contains this path?", and the obvious shortcut --
            // deriving a cursor by truncating the post's path -- is wrong: an earlier sibling
            // with a large subtree can still push the post off the page. That belongs with
            // real pagination in M2, not a cursor trick that fails on exactly the deep threads
            // permalinks matter most for.
            let _ = &loc.path;
            let target = format!("/t/{}#p{post_id}", loc.thread);
            (StatusCode::FOUND, [(header::LOCATION, target)]).into_response()
        }
        Err(StoreError::NotFound) => error(StatusCode::NOT_FOUND, "no such post"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `#[worker::send]` wraps the future so axum's `Send` bound is satisfied. Workers are
/// single-threaded, so this is sound here and is the pattern workers-rs prescribes.
#[worker::send]
async fn render_thread(
    State(env): State<Env>,
    id: String,
    slug: Option<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    // Parsing is forgiving in the ways people actually mistype: either case, `I`/`l` for `1`,
    // `O` for `0`, and grouping hyphens (see `notespace_core::id`). It is strict otherwise.
    let thread_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };

    // Redirect any accepted-but-non-canonical spelling to the canonical lowercase form, so a
    // thread has exactly one cacheable URL instead of one per way of typing it. Without this,
    // `/t/ABCD...` and `/t/abcd...` would occupy separate cache entries for identical bytes.
    let canonical = thread_id.encode();
    if id != canonical {
        let location = match &slug {
            Some(s) => format!("/t/{canonical}/{s}"),
            None => format!("/t/{canonical}"),
        };
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, location)],
        )
            .into_response();
    }

    let db = match env.d1(DB_BINDING) {
        Ok(db) => db,
        Err(e) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("no D1 binding: {e}"),
            )
        }
    };

    // A malformed cursor is a client error, not a reason to scan from the top: silently
    // resetting to page 1 would turn a typo into a full-thread read.
    let after = match q.after.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match TreePath::parse(raw) {
            Ok(p) => Some(p),
            Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad cursor: {e}")),
        },
        None => None,
    };

    let store = D1Store::new(db);
    let page = Page {
        after,
        limit: PAGE_SIZE,
    };

    match fetch_page::<D1Store>(&store, &thread_id, &page).await {
        Ok(page) => {
            let html = notespace_render::thread_page(&page).into_string();
            // What D1 actually reported for this request. Empty-ish locally, real in
            // production -- see QueryStats.
            let stats = store.last_stats();
            let mut resp = (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    // The document is user-agnostic (DESIGN.md §3.3), so it is safe to
                    // share one cached copy across every reader. M6 replaces this with a
                    // baked R2 object keyed by the thread's cache_version.
                    (header::CACHE_CONTROL, "public, max-age=0, s-maxage=60"),
                    // Server-rendered HTML with no inline script; blocks stored-XSS
                    // payloads that survive a future sanitizer regression.
                    (
                        header::CONTENT_SECURITY_POLICY,
                        "default-src 'self'; img-src https: data:; style-src 'unsafe-inline'; \
                         script-src 'self'; frame-ancestors 'none'; base-uri 'none'",
                    ),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
                ],
                Html(html),
            )
                .into_response();
            // Inserted after building rather than in the array above, which is homogeneous
            // over &'static str while this value is per-request.
            if let Ok(v) = stats.server_timing().parse() {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static("server-timing"), v);
            }
            resp
        }
        Err(StoreError::NotFound) => error(StatusCode::NOT_FOUND, "no such thread"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn error(code: StatusCode, msg: &str) -> Response {
    worker::console_log!("notespace error {}: {}", code.as_u16(), msg);
    // The message goes to the log, not the body: internal errors must not leak schema
    // details to the public.
    let public = if code == StatusCode::BAD_REQUEST {
        msg
    } else {
        code.canonical_reason().unwrap_or("error")
    };
    (
        code,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        public.to_owned(),
    )
        .into_response()
}
