//! Cloudflare Workers entrypoint for notespace.

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
use notespace_core::csrf;
use notespace_core::id::PublicId;
#[cfg(feature = "password")]
use notespace_core::login;
use notespace_core::path::Path as TreePath;
use notespace_core::session::SessionToken;
use notespace_core::store::{Page, Store, StoreError, StoreResult};
use serde::Deserialize;
use store::D1Store;
use tower_service::Service;
use worker::{event, Context, Env, HttpRequest, Result as WorkerResult};

/// Kept in step with wrangler.toml by `the_d1_binding_name_matches_wrangler_toml`.
const DB_BINDING: &str = "DATABASE";

/// Posts per page.
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
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/t/{id}", get(thread_page))
        .route("/t/{id}/{slug}", get(thread_page_slug))
        .route("/p/{id}", get(post_permalink))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
        .route("/register", get(register_form).post(register_submit))
        .route("/t/{id}/reply", get(reply_form).post(reply_submit))
        // GET runs the read-only checks; POST adds the write checks, which mutate the thread.
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
    // The slug is decorative and never resolves the thread, so retitling cannot break links.
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

/// The CSRF token binds to a short-lived anonymous cookie, since there is no session yet.
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

    // Reuse an existing anonymous cookie, so a reload does not invalidate an open form.
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
            // Carries a token bound to one visitor.
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

/// Redirects on every outcome, so a reload cannot resubmit the attempt.
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
    // No anonymous cookie means nothing to bind against, which is a failure not a skip.
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

/// `CF-Connecting-IP` is set by the edge and unspoofable; `X-Forwarded-For` is not consulted.
fn client_address(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

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

/// `POST` only, so a prefetched image cannot sign a reader out.
#[worker::send]
async fn logout(State(env): State<Env>, headers: axum::http::HeaderMap) -> Response {
    if let (Ok(db), Some(raw)) = (
        env.d1(DB_BINDING),
        cookie::get(cookie_header(&headers), cookie::SESSION),
    ) {
        if let Some(token) = SessionToken::parse(&raw) {
            // Still clear the cookie, so a failed write cannot leave a live session behind.
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

/// Runs the shared conformance suite against D1 and reports it as plain text. Exposed as a
/// route because `cargo test` has no Worker runtime to reach D1 from. Reads only, writes nothing.
#[worker::send]
async fn conformance(state: State<Env>, q: Query<ConformanceQuery>) -> Response {
    run_conformance(state, q, false).await
}

/// The full suite, write checks included; `POST` because it appends posts.
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
    // Well-formed but absent: the thread's own timestamp, with randomness nothing generates.
    let Ok(absent) = PublicId::new(thread.timestamp_ms(), 0xDEAD_BEEF) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "fixture id out of range");
    };

    // From the database, not a constant: a duplicated fixture is the drift this suite catches.
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
        // A user that certainly exists.
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

/// Generic over [`Store`], so anything the handler does the native adapter can do too.
async fn fetch_page<S: Store>(
    store: &S,
    thread: &PublicId,
    page: &Page,
) -> StoreResult<notespace_core::ThreadPage> {
    store.thread_page(thread, page).await
}

async fn locate<S: Store>(store: &S, post: &PublicId) -> StoreResult<notespace_core::PostLocation> {
    store.locate_post(post, PAGE_SIZE).await
}

/// A durable permalink: resolves where the post lives now, so splitting or merging a thread
/// cannot break the link. 302 rather than 301, because the target moves with the post.
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
    let store = D1Store::new(db);
    match locate::<D1Store>(&store, &post_id).await {
        Ok(loc) => {
            // Without the cursor a post past the first page anchors to an id not in the document.
            let target = match &loc.cursor {
                Some(cursor) => format!(
                    "/t/{}?after={}#p{post_id}",
                    loc.thread,
                    urlencoding(cursor.as_str())
                ),
                None => format!("/t/{}#p{post_id}", loc.thread),
            };
            let mut resp = (StatusCode::FOUND, [(header::LOCATION, target)]).into_response();
            if let Ok(v) = store.last_stats().server_timing().parse() {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static("server-timing"), v);
            }
            resp
        }
        Err(StoreError::NotFound) => error(StatusCode::NOT_FOUND, "no such post"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `#[worker::send]` satisfies axum's `Send` bound; Workers are single-threaded, so it is sound.
#[worker::send]
async fn render_thread(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    id: String,
    slug: Option<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let thread_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };

    // Canonicalize the spelling, so a thread has one cacheable URL rather than one per variant.
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

    // Rejecting beats resetting to page 1, which would turn a typo into a full-thread read.
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

    // Every write bumps the version, so a key built from it is invalidated by the write itself.
    // A failed read falls through uncached; the query below decides whether the thread exists.
    let version = store.thread_version(&thread_id).await.ok().flatten();
    // Counted even on a hit: the version row is the standing cost of the cache key.
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
            let stats = store.last_stats();
            let timing = format!("{}, cache;desc=\"miss\"", stats.server_timing());

            // Cloudflare does not cache a Worker's own response, so `s-maxage` is inert without this.
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
    // The message goes to the log, not the body, so errors cannot leak schema details.
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

// ---------------------------------------------------------------------------
// The write path
// ---------------------------------------------------------------------------

/// Query parameters the reply form accepts.
#[derive(Deserialize, Default)]
struct ReplyQuery {
    /// Public id of the post being replied to. Absent posts at top level.
    parent: Option<String>,
    error: Option<String>,
}

/// `None` covers every way of not being signed in, without distinguishing them.
async fn current_user(
    store: &D1Store,
    headers: &axum::http::HeaderMap,
) -> Option<notespace_core::model::User> {
    let raw = cookie::get(cookie_header(headers), cookie::SESSION)?;
    let token = SessionToken::parse(&raw)?;
    let now = worker::Date::now().as_millis() as i64;
    let auth = store.lookup_session(&token.hash(), now).await.ok()??;
    auth.user.state.can_act().then_some(auth.user)
}

/// Ask for sign-in, preserving where they were trying to go.
fn needs_sign_in(next: &str) -> Response {
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            format!("/login?next={}", urlencoding(next)),
        )],
    )
        .into_response()
}

/// Percent-encode the few characters that would break out of a query parameter.
fn urlencoding(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// On its own uncached page: the baked thread is shared byte-for-byte, so a per-visitor CSRF
/// token cannot live in it.
#[worker::send]
async fn reply_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<ReplyQuery>,
) -> Response {
    let thread_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };
    let canonical = thread_id.encode();
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);

    let Some(_user) = current_user(&store, &headers).await else {
        return needs_sign_in(&format!("/t/{canonical}/reply"));
    };
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    // Bound to the session cookie, so one visitor's token is useless to another.
    let Some(session) = cookie::get(cookie_header(&headers), cookie::SESSION) else {
        return needs_sign_in(&format!("/t/{canonical}/reply"));
    };
    let token = key.mint(
        &session,
        worker::Date::now().as_millis() as i64,
        csrf::DEFAULT_LIFETIME_MS,
    );

    let body = notespace_render::auth::reply_page(
        token.as_str(),
        &canonical,
        q.parent.as_deref(),
        "",
        q.error.and_then(parse_reply_error),
    );
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // Carries a token bound to one visitor.
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(body.into_string()),
    )
        .into_response()
}

/// Turn `?error=` back into something to show. Only values this handler itself emits.
fn parse_reply_error(code: String) -> Option<notespace_render::auth::ReplyError> {
    use notespace_render::auth::ReplyError;
    match code.as_str() {
        "empty" => Some(ReplyError::Empty),
        "expired" => Some(ReplyError::Expired),
        "contended" => Some(ReplyError::Contended),
        other => {
            if let Some(n) = other.strip_prefix("long-") {
                return n.parse().ok().map(|max| ReplyError::TooLong { max });
            }
            if let Some(n) = other.strip_prefix("deep-") {
                return n.parse().ok().map(|cap| ReplyError::TooDeep { cap });
            }
            other
                .strip_prefix("wait-")
                .and_then(|s| s.parse().ok())
                .map(|retry_after_secs| ReplyError::RateLimited { retry_after_secs })
        }
    }
}

/// Redirects on every outcome, so a reload cannot post the reply twice.
#[worker::send]
async fn reply_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    use notespace_core::ratelimit::Limit;
    use notespace_core::reply::{self, Outcome, Rejected, Reply, ReplyConfig};

    let thread_id = match PublicId::parse(&id) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("bad thread id: {e}")),
    };
    let canonical = thread_id.encode();
    let back = |e: &str| -> Response {
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/t/{canonical}/reply?error={e}"))],
        )
            .into_response()
    };

    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);

    let Some(user) = current_user(&store, &headers).await else {
        return needs_sign_in(&format!("/t/{canonical}/reply"));
    };

    let mut form_body = String::new();
    let mut form_csrf = String::new();
    let mut form_parent = String::new();
    for (k, v) in form_urlencoded::parse(body.as_bytes()) {
        match k.as_ref() {
            "body" => form_body = v.into_owned(),
            "csrf" => form_csrf = v.into_owned(),
            "parent" => form_parent = v.into_owned(),
            _ => {}
        }
    }

    // CSRF before anything that costs.
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let session = cookie::get(cookie_header(&headers), cookie::SESSION).unwrap_or_default();
    let now = worker::Date::now().as_millis() as i64;
    if key.verify(&form_csrf, &session, now).is_err() {
        return back("expired");
    }

    let parent = match form_parent.as_str() {
        "" => None,
        raw => match PublicId::parse(raw) {
            Ok(p) => Some(p),
            Err(_) => return error(StatusCode::BAD_REQUEST, "bad parent id"),
        },
    };

    // The read path never renders, so an unrenderable body has to fail here.
    let html = notespace_core::model::SanitizedHtml::assert_sanitized(
        notespace_render::markdown_to_html(&form_body),
    );

    let ids = match ids::generate_many(reply::MAX_PATH_RETRIES as usize + 1) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let cfg = ReplyConfig {
        per_author: Limit {
            max: 10,
            window_ms: 5 * 60_000,
        },
        per_client: Limit {
            max: 30,
            window_ms: 5 * 60_000,
        },
    };
    let attempt = Reply {
        thread: thread_id,
        parent,
        author: user.id,
        body_md: &form_body,
        body_html: html,
        client: &client_address(&headers),
        ids: &ids,
        now,
    };

    match reply::post(&store, &cfg, attempt).await {
        Ok(Outcome::Posted(post)) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/p/{}", post.public_id))],
        )
            .into_response(),
        Ok(Outcome::RateLimited { retry_after_secs }) => back(&format!("wait-{retry_after_secs}")),
        Ok(Outcome::Rejected(Rejected::Empty)) => back("empty"),
        Ok(Outcome::Rejected(Rejected::TooLong { max, .. })) => back(&format!("long-{max}")),
        Ok(Outcome::Rejected(Rejected::TooDeep { cap })) => back(&format!("deep-{cap}")),
        Ok(Outcome::Rejected(Rejected::Contended)) => back("contended"),
        Ok(Outcome::Rejected(Rejected::NotFound)) => error(StatusCode::NOT_FOUND, "no such thread"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Query parameters the signup form accepts.
#[cfg(feature = "password")]
#[derive(Deserialize, Default)]
struct RegisterQuery {
    name: Option<String>,
    error: Option<String>,
}

/// The CSRF token binds to a short-lived anonymous cookie, since there is no session yet.
#[cfg(feature = "password")]
#[worker::send]
async fn register_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    Query(q): Query<RegisterQuery>,
) -> Response {
    if let auth_config::AuthConfig::Refused(why) = auth_config::AuthConfig::resolve(&env) {
        return (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")).into_response();
    }
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
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
    let body = notespace_render::auth::register_page(
        token.as_str(),
        q.name.as_deref().unwrap_or(""),
        q.error.and_then(parse_register_error),
    );
    let mut resp = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
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
fn parse_register_error(code: String) -> Option<notespace_render::auth::RegisterError> {
    use notespace_render::auth::RegisterError;
    match code.as_str() {
        "taken" => Some(RegisterError::Taken),
        "expired" => Some(RegisterError::Expired),
        other => {
            if let Some(n) = other.strip_prefix("short-") {
                return n
                    .parse()
                    .ok()
                    .map(|min| RegisterError::ShortPassword { min });
            }
            if let Some(why) = other.strip_prefix("name-") {
                return Some(RegisterError::BadName(why.to_string()));
            }
            other
                .strip_prefix("wait-")
                .and_then(|s| s.parse().ok())
                .map(|retry_after_secs| RegisterError::RateLimited { retry_after_secs })
        }
    }
}

/// Create an account, and sign in on success.
#[cfg(feature = "password")]
#[worker::send]
async fn register_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    use notespace_core::register::{self, Outcome, RegisterConfig, Rejected, Signup};

    let (peppers, scheme) = match auth_config::AuthConfig::resolve(&env) {
        auth_config::AuthConfig::Refused(why) => {
            return (StatusCode::SERVICE_UNAVAILABLE, format!("{why}\n")).into_response()
        }
        auth_config::AuthConfig::External => {
            return error(StatusCode::NOT_FOUND, "local accounts are disabled")
        }
        auth_config::AuthConfig::Passwords { peppers, scheme } => (peppers, scheme),
    };

    let mut form_name = String::new();
    let mut form_pw = String::new();
    let mut form_csrf = String::new();
    for (k, v) in form_urlencoded::parse(body.as_bytes()) {
        match k.as_ref() {
            "username" => form_name = v.into_owned(),
            "password" => form_pw = v.into_owned(),
            "csrf" => form_csrf = v.into_owned(),
            _ => {}
        }
    }

    // The name is echoed back on rejection; the password never is.
    let back = |e: &str| -> Response {
        (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                format!(
                    "/register?name={}&error={}",
                    urlencoding(&form_name),
                    urlencoding(e)
                ),
            )],
        )
            .into_response()
    };

    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let anon = cookie::get(cookie_header(&headers), cookie::ANON).unwrap_or_default();
    let now = worker::Date::now().as_millis() as i64;
    if key.verify(&form_csrf, &anon, now).is_err() {
        return back("expired");
    }

    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);

    let token = match ids::random_session_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let salt_bytes = match ids::random_hex() {
        Ok(h) => h,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let salt = match notespace_core::password::encode_salt(&salt_bytes.as_bytes()[..16]) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let cfg = RegisterConfig {
        scheme,
        peppers,
        sessions: notespace_core::session::SessionPolicy::default(),
        per_client: notespace_core::ratelimit::Limit {
            max: 3,
            window_ms: 60 * 60_000,
        },
    };
    let attempt = Signup {
        username: &form_name,
        password: &form_pw,
        client: &client_address(&headers),
        token: token.clone(),
        salt: &salt,
        now,
    };

    match register::signup(&store, &cfg, attempt).await {
        Ok(Outcome::Created { .. }) => (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, "/".to_string()),
                (
                    header::SET_COOKIE,
                    cookie::set(
                        cookie::SESSION,
                        &token.to_cookie_value(),
                        cfg.sessions.lifetime_ms / 1000,
                    ),
                ),
            ],
        )
            .into_response(),
        Ok(Outcome::RateLimited { retry_after_secs }) => back(&format!("wait-{retry_after_secs}")),
        Ok(Outcome::Rejected(Rejected::Taken)) => back("taken"),
        Ok(Outcome::Rejected(Rejected::ShortPassword { min })) => back(&format!("short-{min}")),
        Ok(Outcome::Rejected(Rejected::BadName(why))) => back(&format!("name-{why}")),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Local accounts compiled out: the endpoints do not exist.
#[cfg(not(feature = "password"))]
async fn register_form() -> Response {
    external_auth()
}
#[cfg(not(feature = "password"))]
async fn register_submit() -> Response {
    external_auth()
}

/// Threads shown on the index.
const INDEX_LIMIT: u32 = 50;

/// Uncached: every reply reorders this list, so a version key would change on nearly every write.
#[worker::send]
async fn index(State(env): State<Env>) -> Response {
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);
    match store.recent_threads(INDEX_LIMIT).await {
        Ok(threads) => {
            let html = notespace_render::index::index_page(&threads).into_string();
            let mut resp = (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "public, max-age=0, s-maxage=10"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
                ],
                Html(html),
            )
                .into_response();
            if let Ok(v) = store.last_stats().server_timing().parse() {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static("server-timing"), v);
            }
            resp
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
