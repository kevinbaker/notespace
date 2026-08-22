//! Cloudflare Workers entrypoint for notespace.
//!
//! M0 scope: serve a 200-post thread page from D1 and prove it fits inside the free-tier CPU,
//! size and query budgets. Auth, writes and baking are M2/M6.

#[cfg(feature = "password")]
mod auth_config;
mod cache;
mod ids;
mod startup;
mod store;
#[cfg(feature = "kdf-subtle")]
mod subtle_kdf;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use notespace_core::conformance::{run_all, Fixture};
use notespace_core::cookie;
use notespace_core::id::PublicId;
use notespace_core::path::Path as TreePath;
use notespace_core::session::SessionToken;
use notespace_core::store::{Page, Store, StoreError, StoreResult};
#[cfg(feature = "password")]
use notespace_core::{csrf, login};
use serde::Deserialize;
use store::D1Store;
use tower_service::Service;
use worker::{event, Context, Env, HttpRequest, Result as WorkerResult};

/// Name of the D1 binding in wrangler.toml.
/// Must match the `binding` name in `wrangler.toml` (and any binding configured in the
/// dashboard). A mismatch surfaces at runtime as "no D1 binding", never at build time.
const DB_BINDING: &str = "DATABASE";

/// Posts per page. 200 is the spike target: if a page this size does not fit the budget, the
/// free-tier premise fails.
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
    startup::report_once(&posture(&env));
    Ok(router(env).call(req).await?)
}

/// What this deployment is actually running, read from bindings rather than assumed.
/// Password login compiled out.
#[cfg(not(feature = "password"))]
fn posture(_env: &Env) -> startup::Posture {
    startup::Posture::External
}

#[cfg(feature = "password")]
fn posture(env: &Env) -> startup::Posture {
    match auth_config::AuthConfig::resolve(env) {
        auth_config::AuthConfig::External => startup::Posture::External,
        auth_config::AuthConfig::Refused(why) => startup::Posture::Refused(why),
        auth_config::AuthConfig::Passwords { scheme, .. } if scheme.client_params().is_some() => {
            startup::Posture::PasswordsClientArgon
        }
        auth_config::AuthConfig::Passwords { .. } => startup::Posture::PasswordsWithPepper,
    }
}

fn router(env: Env) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/t/{id}", get(thread_page))
        .route("/t/{id}/{slug}", get(thread_page_slug))
        .route("/p/{id}", get(post_permalink))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
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
    headers: axum::http::HeaderMap,
    UrlPath((id, slug)): UrlPath<(String, String)>,
    query: Query<PageQuery>,
) -> Response {
    // The slug is decorative: it exists for readability and for search engines, and is never
    // used to resolve the thread. Retitling a thread therefore cannot break its links.
    render_thread(state, headers, id, Some(slug), query).await
}

async fn thread_page(
    state: State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    query: Query<PageQuery>,
) -> Response {
    render_thread(state, headers, id, None, query).await
}

/// The login form.
///
/// A visitor here has no session, so the CSRF token binds to a short-lived anonymous cookie set
/// on this response. Without a binding the token is a signed constant and any visitor's token
/// works for any other.
#[cfg(feature = "password")]
#[worker::send]
async fn login_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    let cfg = auth_config::AuthConfig::resolve(&env);
    if let auth_config::AuthConfig::Refused(why) = &cfg {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")).into_response();
    }
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };

    // Reuse the visitor's anonymous cookie if they have one, so a reload does not invalidate a
    // form they already have open in another tab.
    let (anon, set_anon) = match cookie::get(cookie_header(&headers), cookie::ANON) {
        Some(existing) => (existing, None),
        None => match ids::random_hex() {
            Ok(v) => {
                let c = cookie::set(cookie::ANON, &v, 60 * 60);
                (v, Some(c))
            }
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        },
    };

    let token = key.mint(
        &anon,
        worker::Date::now().as_millis() as i64,
        csrf::DEFAULT_LIFETIME_MS,
    );
    let next = q.next.as_deref().and_then(cookie::safe_next);
    let body =
        notespace_render::auth::login_page(token.as_str(), next, q.error.and_then(parse_error));

    let mut resp = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
            // Never cached: it carries a token bound to one visitor.
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Html(body.into_string()),
    )
        .into_response();
    if let Some(c) = set_anon {
        if let Ok(v) = c.parse() {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

/// Turn `?error=` back into something to show. Only values this handler itself emits.
#[cfg(feature = "password")]
fn parse_error(code: String) -> Option<notespace_render::auth::LoginError> {
    use notespace_render::auth::LoginError;
    match code.as_str() {
        "rejected" => Some(LoginError::Rejected),
        "expired" => Some(LoginError::Expired),
        other => other
            .strip_prefix("wait-")
            .and_then(|s| s.parse().ok())
            .map(|retry_after_secs| LoginError::RateLimited { retry_after_secs }),
    }
}

/// Handle a submitted login.
///
/// Redirects on every outcome rather than rendering in place: a POST that renders leaves the
/// browser able to resubmit it, and resubmitting a login is a wasted attempt against the
/// visitor's own rate limit.
#[cfg(feature = "password")]
#[worker::send]
async fn login_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let cfg = match auth_config::AuthConfig::resolve(&env) {
        auth_config::AuthConfig::Refused(why) => {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")).into_response()
        }
        auth_config::AuthConfig::External => {
            return error(StatusCode::NOT_FOUND, "password login is disabled")
        }
        auth_config::AuthConfig::Passwords { peppers, scheme } => (peppers, scheme),
    };
    let (peppers, scheme) = cfg;

    let form = form_urlencoded::parse(body.as_bytes());
    let (mut username, mut password, mut token, mut next) =
        (String::new(), String::new(), String::new(), None);
    for (k, v) in form {
        match k.as_ref() {
            "username" => username = v.into_owned(),
            "password" => password = v.into_owned(),
            "csrf" => token = v.into_owned(),
            "next" => next = Some(v.into_owned()),
            _ => {}
        }
    }

    let now = worker::Date::now().as_millis() as i64;
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    // The token is bound to the anonymous cookie; without it there is nothing to check against,
    // which is itself a failure rather than a reason to skip the check.
    let anon = cookie::get(cookie_header(&headers), cookie::ANON).unwrap_or_default();
    if key.verify(&token, &anon, now).is_err() {
        return redirect_to_login("expired", next.as_deref());
    }

    let db = match env.d1(DB_BINDING) {
        Ok(db) => db,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("no D1: {e}")),
    };
    let store = D1Store::new(db);

    let session_token = match ids::random_session_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let login_cfg = login::LoginConfig {
        dummy_hash: login::LoginConfig::dummy_hash_for(scheme, &peppers),
        scheme,
        peppers,
        sessions: notespace_core::session::SessionPolicy::default(),
        per_identity: notespace_core::ratelimit::Limit::PER_IDENTITY,
        per_client: notespace_core::ratelimit::Limit::PER_CLIENT,
    };
    let attempt = login::Attempt {
        username: &username,
        password: &password,
        client: &client_address(&headers),
        token: session_token.clone(),
        now,
    };

    match login::attempt(&store, &login_cfg, attempt).await {
        Ok(login::Outcome::Success { session, .. }) => {
            let max_age = (session.expires_at - now) / 1000;
            let target = next.as_deref().and_then(cookie::safe_next).unwrap_or("/");
            (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, target.to_string()),
                    (
                        header::SET_COOKIE,
                        cookie::set(cookie::SESSION, &session_token.to_cookie_value(), max_age),
                    ),
                ],
            )
                .into_response()
        }
        Ok(login::Outcome::Rejected) => redirect_to_login("rejected", next.as_deref()),
        Ok(login::Outcome::RateLimited { retry_after_secs }) => {
            redirect_to_login(&format!("wait-{retry_after_secs}"), next.as_deref())
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(feature = "password")]
fn redirect_to_login(code: &str, next: Option<&str>) -> Response {
    let target = match next.and_then(cookie::safe_next) {
        Some(n) => format!("/login?error={code}&next={}", urlencode(n)),
        None => format!("/login?error={code}"),
    };
    (StatusCode::SEE_OTHER, [(header::LOCATION, target)]).into_response()
}

/// Percent-encode the few characters that matter in a query value.
#[cfg(feature = "password")]
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The raw `Cookie:` header, if present.
fn cookie_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
}

/// The address rate limiting counts against.
///
/// `CF-Connecting-IP` is set by Cloudflare's edge and cannot be spoofed by the client — unlike
/// `X-Forwarded-For`, which is why that one is not consulted.
#[cfg(feature = "password")]
fn client_address(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(feature = "password")]
fn csrf_key(env: &Env) -> Option<csrf::CsrfKey> {
    csrf::CsrfKey::new(env.secret("CSRF_KEY").ok()?.to_string().as_bytes()).ok()
}

/// Query parameters the login form accepts.
#[cfg(feature = "password")]
#[derive(Deserialize, Default)]
struct LoginQuery {
    next: Option<String>,
    error: Option<String>,
}

/// Password login compiled out: the endpoints do not exist.
#[cfg(not(feature = "password"))]
async fn login_form() -> Response {
    external_auth()
}
#[cfg(not(feature = "password"))]
async fn login_submit() -> Response {
    external_auth()
}
#[cfg(not(feature = "password"))]
fn external_auth() -> Response {
    (
        StatusCode::NOT_FOUND,
        "local password login is disabled on this instance; authentication is external\n",
    )
        .into_response()
}

/// Sign out. `POST` only: a `GET /logout` is a one-pixel image away from being a denial of
/// service on every reader whose browser prefetches it.
#[worker::send]
async fn logout(State(env): State<Env>, headers: axum::http::HeaderMap) -> Response {
    if let (Ok(db), Some(raw)) = (
        env.d1(DB_BINDING),
        cookie::get(cookie_header(&headers), cookie::SESSION),
    ) {
        if let Some(token) = SessionToken::parse(&raw) {
            // A failure here still clears the cookie: the visitor asked to be signed out, and
            // leaving them holding a live cookie because a write failed is the wrong direction
            // to err in.
            let _ = D1Store::new(db).delete_session(&token.hash()).await;
        }
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/".to_string()),
            (header::SET_COOKIE, cookie::clear(cookie::SESSION)),
        ],
    )
        .into_response()
}

/// Runs the shared conformance suite against D1 and reports it as plain text.
///
/// The other half of `crates/store-sqlite/tests/conformance.rs`: the same
/// [`notespace_core::conformance::run_all`], the same checks, a different adapter. A `#[test]`
/// cannot reach D1 — there is no Worker runtime in `cargo test` — so the suite is exposed as a
/// route and run against a deployed instance instead.
///
/// Takes the thread to exercise as `?thread=<public id>` rather than assuming an id the seed
/// generator picked: a fixture constant duplicated across crates is the drift this suite is
/// meant to catch.
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
/// These two functions make that structural: they are generic, so anything they can do the
/// native adapter can do too.
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
    headers: axum::http::HeaderMap,
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

    // A malformed cursor is a client error, not a reason to scan from the top: silently
    // resetting to page 1 would turn a typo into a full-thread read.
    let after = match q.after.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => match TreePath::parse(raw) {
            Ok(p) => Some(p),
            Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad cursor: {e}")),
        },
        None => None,
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

    let store = D1Store::new(db);

    // One row, to learn the thread's bake version. Every write bumps it, so a key built from it
    // is invalidated by the write itself: a reply shows up on the very next request instead of
    // whenever a TTL happens to lapse. That one row is what buys the freshness -- the
    // alternative pays the page's full 404-row scan every time a TTL turns over.
    //
    // A failed or absent version read falls through uncached rather than failing the page; the
    // query below is what decides whether the thread exists.
    let version = store.thread_version(&thread_id).await.ok().flatten();
    // Counted even on a hit: this row is the standing cost of version-keyed caching, and a
    // Server-Timing that omitted it would understate D1 usage by one row per pageview.
    let lookup = store.last_stats().server_timing();
    let key = version.and_then(|v| {
        headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .and_then(|host| {
                cache::thread_key(host, &canonical, v, after.as_ref().map(|p| p.as_str()))
            })
    });
    if let Some(k) = &key {
        if let Some(hit) = cache::get(k, &lookup).await {
            return hit;
        }
    }

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
            let timing = format!("{}, cache;desc=\"miss\"", stats.server_timing());

            // Store before responding. The page is user-agnostic, so one render serves every
            // reader for the TTL; without this the `s-maxage` header above is inert, because
            // Cloudflare does not cache a Worker's own response.
            if let Some(k) = &key {
                cache::put(k, &html, &timing).await;
            }

            let mut resp = (StatusCode::OK, Html(html)).into_response();
            let h = resp.headers_mut();
            for (name, value) in cache::PAGE_HEADERS {
                if let (Ok(n), Ok(v)) = (
                    header::HeaderName::from_bytes(name.as_bytes()),
                    value.parse(),
                ) {
                    h.insert(n, v);
                }
            }
            if let Ok(v) = timing.parse() {
                h.insert(header::HeaderName::from_static("server-timing"), v);
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
