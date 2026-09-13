//! Cloudflare Workers entrypoint for notespace.

mod account;
mod admin;
#[cfg(feature = "password")]
mod auth_config;
mod cache;
mod ids;
mod mail;
mod moderation;
mod posting;
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
use notespace_core::model::ThreadState;
use notespace_core::path::Path as TreePath;
use notespace_core::session::SessionToken;
use notespace_core::store::{Page, Store, StoreError, StoreResult};
use serde::Deserialize;
use store::D1Store;
use tower_service::Service;
use worker::{event, Context, Env, HttpRequest, MessageBatch, MessageExt, Result as WorkerResult};

/// Kept in step with wrangler.toml by `the_d1_binding_name_matches_wrangler_toml`.
pub(crate) const DB_BINDING: &str = "DATABASE";

/// Posts per page.
pub(crate) const PAGE_SIZE: u32 = 200;

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
        .route("/favicon.ico", get(favicon))
        .route("/static/reply.js", get(reply_script))
        .route("/t/{id}", get(thread_page))
        .route("/t/{id}/{slug}", get(thread_page_slug))
        .route("/p/{id}", get(post_permalink))
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout))
        .route("/register", get(register_form).post(register_submit))
        .route("/t/{id}/reply", get(reply_form).post(reply_submit))
        .route("/t/{id}/held/{post}", get(held_notice))
        // `/s/{path}` and `/s/{path}/new` share one catch-all; the handler splits them.
        .route("/s/{*path}", get(posting::space).post(posting::space_post))
        .route("/u/{name}", get(posting::profile))
        .route(
            "/p/{id}/edit",
            get(posting::edit_form).post(posting::edit_submit),
        )
        .route("/p/{id}/delete", post(posting::delete_submit))
        // Account self-service.
        .route("/settings", get(account::settings))
        .route("/settings/email", post(account::change_email))
        .route("/settings/password", post(account::change_password))
        .route("/settings/sessions", post(account::end_other_sessions))
        .route(
            "/verify",
            get(account::verify_form).post(account::verify_submit),
        )
        .route(
            "/forgot",
            get(account::forgot_form).post(account::forgot_submit),
        )
        .route(
            "/reset",
            get(account::reset_form).post(account::reset_submit),
        )
        // Moderation. Forms are separate uncached pages for the same reason the reply form is.
        .route("/p/{id}/report", get(report_form).post(report_submit))
        .route("/p/{id}/appeal", get(appeal_form).post(appeal_submit))
        .route("/admin", get(admin::dashboard))
        .route(
            "/admin/thread/{id}",
            get(admin::thread_form).post(admin::thread_submit),
        )
        .route("/admin/post/{id}/state", post(admin::post_state))
        .route("/admin/users", get(admin::users))
        .route("/admin/user/{name}", get(admin::user))
        .route("/admin/user/{name}/state", post(admin::user_state))
        .route("/admin/user/{name}/role", post(admin::user_role))
        .route(
            "/admin/spaces",
            get(admin::spaces).post(admin::space_create),
        )
        .route(
            "/admin/space/{id}",
            get(admin::space_form_page).post(admin::space_submit),
        )
        .route("/admin/log", get(admin::log))
        .route("/mod/queue", get(mod_queue))
        .route("/mod/review/{id}", post(mod_review))
        .route("/modlog", get(modlog))
        // GET runs the read-only checks; POST adds the write checks, which mutate the thread.
        .route(
            "/__conformance",
            get(conformance).post(conformance_with_writes),
        )
        .fallback(not_found)
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
    // `/t/{id}.rss` is the same thread as a feed.
    if let Some(id) = id.strip_suffix(".rss") {
        return posting::feed(state, headers, id.to_string()).await;
    }
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
    // Already signed in: the account page answers "who am I" better than a form would.
    if let Ok(db) = env.d1(DB_BINDING) {
        if current_user(&D1Store::new(db), &headers).await.is_some() {
            return see_other("/settings".into());
        }
    }
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };

    let (token, set_anon) = match anon_token(&key, &headers) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let next = q.next.as_deref().and_then(cookie::safe_next);
    let notice = q.done.as_deref().and_then(|d| match d {
        "reset" => Some(notespace_render::auth::LoginNotice::PasswordReset {
            username: q.name.clone().unwrap_or_default(),
        }),
        _ => None,
    });
    let body =
        notespace_render::auth::login_page(&token, next, q.error.and_then(parse_error), notice);
    anon_page(body, set_anon)
}

/// A CSRF token for a visitor with no session, bound to a short-lived anonymous cookie. Reuses
/// an existing cookie, so a reload does not invalidate an open form. The cookie to set, if any,
/// comes back alongside.
pub(crate) fn anon_token(
    key: &csrf::CsrfKey,
    headers: &axum::http::HeaderMap,
) -> Result<(String, Option<String>), String> {
    let (anon, set_anon) = match cookie::get(cookie_header(headers), cookie::ANON) {
        Some(existing) => (existing, None),
        None => {
            let v = ids::random_hex()?;
            let c = cookie::set(cookie::ANON, &v, 60 * 60);
            (v, Some(c))
        }
    };
    let token = key.mint(&anon, now_ms(), csrf::DEFAULT_LIFETIME_MS);
    Ok((token.as_str().to_string(), set_anon))
}

/// The anonymous cookie's value, for verifying a token minted by [`anon_token`].
pub(crate) fn anon_binding(headers: &axum::http::HeaderMap) -> String {
    // No anonymous cookie means nothing to bind against, which is a failure not a skip.
    cookie::get(cookie_header(headers), cookie::ANON).unwrap_or_default()
}

/// An uncached page carrying a per-visitor token, setting the anonymous cookie if needed.
pub(crate) fn anon_page(body: maud::Markup, set_anon: Option<String>) -> Response {
    let mut resp = uncached_html(body);
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
    if key.verify(&token, &anon_binding(&headers), now).is_err() {
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
pub(crate) fn cookie_header(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
}

/// `CF-Connecting-IP` is set by the edge and unspoofable; `X-Forwarded-For` is not consulted.
pub(crate) fn client_address(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

pub(crate) fn csrf_key(env: &Env) -> Option<csrf::CsrfKey> {
    csrf::CsrfKey::new(env.secret("CSRF_KEY").ok()?.to_string().as_bytes()).ok()
}

/// Query parameters the login form accepts.
#[cfg(feature = "password")]
#[derive(Deserialize, Default)]
struct LoginQuery {
    next: Option<String>,
    error: Option<String>,
    done: Option<String>,
    /// Whose password `done=reset` was about.
    name: Option<String>,
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
        match (0..Fixture::WRITABLE)
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
                cache::thread_key(
                    host,
                    &canonical,
                    v,
                    notespace_render::BAKE_REVISION,
                    after.as_ref().map(|p| p.as_str()),
                )
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
        // Not cached either: the 404 must lift the moment the thread is restored.
        Ok(page)
            if matches!(
                page.thread.state,
                ThreadState::Hidden | ThreadState::Deleted
            ) =>
        {
            error(StatusCode::NOT_FOUND, "thread is hidden")
        }
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

pub(crate) fn error(code: StatusCode, msg: &str) -> Response {
    worker::console_log!("notespace error {}: {}", code.as_u16(), msg);
    // A 400 says what was wrong with the request, as text. Anything else says only its
    // status -- the message goes to the log, so errors cannot leak schema details -- on a
    // page with a way home.
    if code == StatusCode::BAD_REQUEST {
        return (
            code,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            msg.to_owned(),
        )
            .into_response();
    }
    let reason = code.canonical_reason().unwrap_or("error");
    (
        code,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(notespace_render::error_page(code.as_u16(), reason).into_string()),
    )
        .into_response()
}

/// Unknown routes. Usernames and space keys reserve the words that are routes, so nothing
/// here can shadow a page that should exist.
async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such route")
}

/// Every page suppresses the favicon request with `<link rel="icon" href="data:,">`; this
/// covers clients that ask anyway, without spending a request on it every time.
async fn favicon() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(header::CACHE_CONTROL, "public, max-age=604800")],
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
    /// Prefill the draft with the parent quoted: the no-JS half of "quote".
    quote: Option<String>,
}

/// `None` covers every way of not being signed in, without distinguishing them.
async fn current_user(
    store: &D1Store,
    headers: &axum::http::HeaderMap,
) -> Option<notespace_core::model::User> {
    current_auth(store, headers).await.map(|a| a.user)
}

async fn current_auth(
    store: &D1Store,
    headers: &axum::http::HeaderMap,
) -> Option<notespace_core::store::Authenticated> {
    let raw = cookie::get(cookie_header(headers), cookie::SESSION)?;
    let token = SessionToken::parse(&raw)?;
    let now = worker::Date::now().as_millis() as i64;
    let auth = store.lookup_session(&token.hash(), now).await.ok()??;
    auth.user.state.can_act().then_some(auth)
}

/// Ask for sign-in, preserving where they were trying to go.
pub(crate) fn needs_sign_in(next: &str) -> Response {
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
pub(crate) fn urlencoding(s: &str) -> String {
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

    let parent = match q.parent.as_deref().filter(|p| !p.is_empty()) {
        Some(raw) => match PublicId::parse(raw) {
            Ok(p) => Some(p),
            Err(_) => return error(StatusCode::BAD_REQUEST, "bad parent id"),
        },
        None => None,
    };
    let (title, parent_post) = match reply_target(&store, &thread_id, parent.as_ref()).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let draft = match (&parent_post, q.quote.is_some()) {
        (Some(p), true) => notespace_render::auth::quoted(p.body_md.as_deref().unwrap_or("")),
        _ => String::new(),
    };
    let target = notespace_render::auth::ReplyTarget {
        thread_title: &title,
        parent: parent_post
            .as_ref()
            .map(|p| notespace_render::auth::ParentPost {
                public_id: p.public_id.as_str(),
                author_name: &p.author_name,
                body_html: &p.body_html,
            }),
    };
    uncached_html(notespace_render::auth::reply_page(
        token.as_str(),
        &canonical,
        &target,
        &draft,
        q.error.and_then(parse_reply_error),
    ))
}

/// The thread's title and, for a nested reply, the parent post -- which has to be in this
/// thread, or the form would show one thread's post above a reply into another.
async fn reply_target(
    store: &D1Store,
    thread: &PublicId,
    parent: Option<&PublicId>,
) -> Result<(String, Option<notespace_core::model::Post>), Response> {
    let (_, head) = match store.thread_head(thread).await {
        Ok(t) => t,
        Err(StoreError::NotFound) => return Err(error(StatusCode::NOT_FOUND, "no such thread")),
        Err(e) => return Err(error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    };
    let Some(parent) = parent else {
        return Ok((head.title, None));
    };
    match store.post_by_id(parent).await {
        Ok((post, in_thread)) if &in_thread == thread => Ok((head.title, Some(post))),
        Ok(_) | Err(StoreError::NotFound) => Err(error(StatusCode::NOT_FOUND, "no such post")),
        Err(e) => Err(error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    }
}

/// The reply form's enhancement, served as a file rather than inlined: the CSP is
/// `script-src 'self'` and stays that way.
async fn reply_script() -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("../static/reply.js"),
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
        "locked" => Some(ReplyError::Locked),
        "duplicate" => Some(ReplyError::Duplicate),
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
    let parent = match form_parent.as_str() {
        "" => None,
        raw => match PublicId::parse(raw) {
            Ok(p) => Some(p),
            Err(_) => return error(StatusCode::BAD_REQUEST, "bad parent id"),
        },
    };
    let (title, parent_post) = match reply_target(&store, &thread_id, parent.as_ref()).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    // A rejection re-renders the form with the draft in it, under a fresh token, rather than
    // redirecting and losing what was typed.
    let back = |e: &str| -> Response {
        let fresh = key.mint(&session, now, csrf::DEFAULT_LIFETIME_MS);
        let target = notespace_render::auth::ReplyTarget {
            thread_title: &title,
            parent: parent_post
                .as_ref()
                .map(|p| notespace_render::auth::ParentPost {
                    public_id: p.public_id.as_str(),
                    author_name: &p.author_name,
                    body_html: &p.body_html,
                }),
        };
        uncached_html(notespace_render::auth::reply_page(
            fresh.as_str(),
            &canonical,
            &target,
            &form_body,
            parse_reply_error(e.to_string()),
        ))
    };
    if key.verify(&form_csrf, &session, now).is_err() {
        return back("expired");
    }

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

    let queue = moderation::MaybeQueue::from_env(&env);
    match reply::post(&store, &queue, &cfg, attempt).await {
        Ok(Outcome::Posted(post)) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("/p/{}", post.public_id))],
        )
            .into_response(),
        // The permalink would show "[awaiting review]" with no explanation.
        Ok(Outcome::Held(post)) => (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                format!("/t/{canonical}/held/{}", post.public_id),
            )],
        )
            .into_response(),
        Ok(Outcome::RateLimited { retry_after_secs }) => back(&format!("wait-{retry_after_secs}")),
        Ok(Outcome::Rejected(Rejected::Empty)) => back("empty"),
        Ok(Outcome::Rejected(Rejected::TooLong { max, .. })) => back(&format!("long-{max}")),
        Ok(Outcome::Rejected(Rejected::TooDeep { cap })) => back(&format!("deep-{cap}")),
        Ok(Outcome::Rejected(Rejected::Contended)) => back("contended"),
        Ok(Outcome::Rejected(Rejected::Locked)) => back("locked"),
        Ok(Outcome::Rejected(Rejected::Duplicate)) => back("duplicate"),
        Ok(Outcome::Rejected(Rejected::NotFound)) => error(StatusCode::NOT_FOUND, "no such thread"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Told rather than shown: the thread page renders the held post as a tombstone.
#[worker::send]
async fn held_notice(UrlPath((thread, post)): UrlPath<(String, String)>) -> Response {
    let (Ok(thread), Ok(post)) = (PublicId::parse(&thread), PublicId::parse(&post)) else {
        return error(StatusCode::BAD_REQUEST, "bad id");
    };
    uncached_html(notespace_render::auth::held_page(
        &thread.encode(),
        &post.encode(),
    ))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Query parameters the signup form accepts.
#[cfg(feature = "password")]
#[derive(Deserialize, Default)]
struct RegisterQuery {
    name: Option<String>,
    email: Option<String>,
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
    let (token, set_anon) = match anon_token(&key, &headers) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let body = notespace_render::auth::register_page(
        &token,
        q.name.as_deref().unwrap_or(""),
        q.email.as_deref().unwrap_or(""),
        mail::require_email(&env),
        signup_code(&env).is_some(),
        q.error.and_then(parse_register_error),
    );
    anon_page(body, set_anon)
}

/// The invite code registration requires, if the deployment set one. A secret rather than a
/// var so it is not in version control; `SIGNUP_CODE` unset means open registration.
#[cfg(feature = "password")]
fn signup_code(env: &Env) -> Option<String> {
    env.secret(SIGNUP_CODE_SECRET)
        .ok()
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(feature = "password")]
pub(crate) const SIGNUP_CODE_SECRET: &str = "SIGNUP_CODE";

/// Turn `?error=` back into something to show. Only values this handler itself emits.
#[cfg(feature = "password")]
fn parse_register_error(code: String) -> Option<notespace_render::auth::RegisterError> {
    use notespace_render::auth::RegisterError;
    match code.as_str() {
        "taken" => Some(RegisterError::Taken),
        "expired" => Some(RegisterError::Expired),
        "invite" => Some(RegisterError::BadInvite),
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
            if let Some(why) = other.strip_prefix("email-") {
                return Some(RegisterError::BadEmail(why.to_string()));
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
    let mut form_email = String::new();
    let mut form_csrf = String::new();
    let mut form_invite = String::new();
    for (k, v) in form_urlencoded::parse(body.as_bytes()) {
        match k.as_ref() {
            "username" => form_name = v.into_owned(),
            "password" => form_pw = v.into_owned(),
            "email" => form_email = v.into_owned(),
            "csrf" => form_csrf = v.into_owned(),
            "invite" => form_invite = v.into_owned(),
            _ => {}
        }
    }

    // The name and address are echoed back on rejection; the password never is.
    let back = |e: &str| -> Response {
        (
            StatusCode::SEE_OTHER,
            [(
                header::LOCATION,
                format!(
                    "/register?name={}&email={}&error={}",
                    urlencoding(&form_name),
                    urlencoding(&form_email),
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
    let now = worker::Date::now().as_millis() as i64;
    if key
        .verify(&form_csrf, &anon_binding(&headers), now)
        .is_err()
    {
        return back("expired");
    }
    // Before the limiter and the hash: a wrong code should cost nothing.
    if !register::invite_ok(signup_code(&env).as_deref(), &form_invite) {
        return back("invite");
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

    let verify_token = match ids::random_email_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let cfg = RegisterConfig {
        scheme,
        peppers,
        sessions: notespace_core::session::SessionPolicy::default(),
        // Ten an hour: a shared office address should be able to sign up a team.
        per_client: notespace_core::ratelimit::Limit {
            max: 10,
            window_ms: 60 * 60_000,
        },
        require_email: mail::require_email(&env),
    };
    let attempt = Signup {
        username: &form_name,
        password: &form_pw,
        email: &form_email,
        client: &client_address(&headers),
        token: token.clone(),
        salt: &salt,
        verify_token,
        now,
    };
    let mailer = mail::MaybeMailer::from_env(&env);
    let link_cfg = mail::LinkConfig::resolve(&env, host(&headers));

    match register::signup(&store, &mailer, &link_cfg.links(), &cfg, attempt).await {
        Ok(Outcome::Created { verification, .. }) => {
            // Land where the verification status is visible, if there is one to show.
            let target = match verification {
                Some(d) => format!("/settings?did={}", account::delivery_code(&d)),
                None => "/".to_string(),
            };
            (
                StatusCode::SEE_OTHER,
                [
                    (header::LOCATION, target),
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
                .into_response()
        }
        Ok(Outcome::RateLimited { retry_after_secs }) => back(&format!("wait-{retry_after_secs}")),
        Ok(Outcome::Rejected(Rejected::Taken)) => back("taken"),
        Ok(Outcome::Rejected(Rejected::ShortPassword { min })) => back(&format!("short-{min}")),
        Ok(Outcome::Rejected(Rejected::BadName(why))) => back(&format!("name-{why}")),
        Ok(Outcome::Rejected(Rejected::BadEmail(why))) => back(&format!("email-{why}")),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The request's `Host`, for links in mail and cache keys.
pub(crate) fn host(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|h| h.to_str().ok())
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
    let spaces = match store.spaces_under(None).await {
        Ok(s) => s,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let stats = store.last_stats();
    match store.recent_threads(INDEX_LIMIT).await {
        Ok(threads) => {
            let html = notespace_render::index::index_page(&spaces, &threads).into_string();
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
            if let Ok(v) = stats.plus(store.last_stats()).server_timing().parse() {
                resp.headers_mut()
                    .insert(header::HeaderName::from_static("server-timing"), v);
            }
            resp
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------

/// Queue items shown per page.
const QUEUE_LIMIT: u32 = 50;
/// Log rows shown.
const MODLOG_LIMIT: u32 = 100;
/// Posts the cron sweep classifies per run. Bounded by the model budget, not by D1.
const SWEEP_LIMIT: u32 = 10;

pub(crate) fn now_ms() -> i64 {
    worker::Date::now().as_millis() as i64
}

pub(crate) fn uncached_html(body: maud::Markup) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(body.into_string()),
    )
        .into_response()
}

pub(crate) fn see_other(target: String) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, target)]).into_response()
}

/// A moderator by role, or listed in the `MODERATORS` variable -- the bootstrap path for the
/// first admin, who has nobody to grant them the role.
pub(crate) fn can_moderate(env: &Env, user: &notespace_core::model::User) -> bool {
    if user.role.can_moderate() {
        return true;
    }
    env.var("MODERATORS")
        .map(|v| v.to_string())
        .unwrap_or_default()
        .split(',')
        .map(|n| n.trim().to_lowercase())
        .any(|n| !n.is_empty() && n == user.name)
}

/// The signed-in user, the store, and a CSRF token bound to their session -- what every
/// moderation form needs. `Err` is the response to send instead.
pub(crate) struct Signed {
    pub(crate) store: D1Store,
    pub(crate) user: notespace_core::model::User,
    /// The session row, for flows that end every session but this one.
    pub(crate) auth: notespace_core::session::Session,
    session: String,
    key: csrf::CsrfKey,
}

pub(crate) async fn signed_in(
    env: &Env,
    headers: &axum::http::HeaderMap,
    next: &str,
) -> Result<Signed, Response> {
    let Ok(db) = env.d1(DB_BINDING) else {
        return Err(error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding"));
    };
    let store = D1Store::new(db);
    let Some(auth) = current_auth(&store, headers).await else {
        return Err(needs_sign_in(next));
    };
    let Some(key) = csrf_key(env) else {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        ));
    };
    let Some(session) = cookie::get(cookie_header(headers), cookie::SESSION) else {
        return Err(needs_sign_in(next));
    };
    Ok(Signed {
        store,
        user: auth.user,
        auth: auth.session,
        session,
        key,
    })
}

impl Signed {
    pub(crate) fn mint(&self) -> String {
        self.key
            .mint(&self.session, now_ms(), csrf::DEFAULT_LIFETIME_MS)
            .as_str()
            .to_string()
    }
    pub(crate) fn verify(&self, token: &str) -> bool {
        self.key.verify(token, &self.session, now_ms()).is_ok()
    }
}

pub(crate) fn form_fields(body: &str) -> std::collections::HashMap<String, String> {
    form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[derive(Deserialize, Default)]
struct ReportQuery {
    error: Option<String>,
    done: Option<String>,
}

#[worker::send]
async fn report_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<ReportQuery>,
) -> Response {
    use notespace_render::moderation::{report_page, ReportDone, ReportError};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/report")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let error = q.error.as_deref().and_then(|e| match e {
        "expired" => Some(ReportError::Expired),
        "own" => Some(ReportError::OwnPost),
        "gone" => Some(ReportError::Gone),
        _ => None,
    });
    let done = q.done.as_deref().and_then(|d| match d {
        "recorded" => Some(ReportDone::Recorded),
        "already" => Some(ReportDone::AlreadyReported),
        "held" => Some(ReportDone::Held),
        _ => None,
    });
    uncached_html(report_page(&signed.mint(), &canonical, error, done))
}

#[worker::send]
async fn report_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    use notespace_core::moderation::pipeline::{self, ReportOutcome};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let back = |q: &str| see_other(format!("/p/{canonical}/report?{q}"));
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/report")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return back("error=expired");
    }
    let queue = moderation::MaybeQueue::from_env(&env);
    let reason = fields.get("reason").map(String::as_str);
    match pipeline::report(&signed.store, &queue, &post, &signed.user, reason, now_ms()).await {
        Ok(ReportOutcome::Recorded { .. }) => back("done=recorded"),
        Ok(ReportOutcome::AlreadyReported { .. }) => back("done=already"),
        Ok(ReportOutcome::Held { .. }) => back("done=held"),
        Ok(ReportOutcome::OwnPost) => back("error=own"),
        Ok(ReportOutcome::Gone) => back("error=gone"),
        Err(StoreError::NotFound) => error(StatusCode::NOT_FOUND, "no such post"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct AppealQuery {
    error: Option<String>,
    filed: Option<String>,
}

#[worker::send]
async fn appeal_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<AppealQuery>,
) -> Response {
    use notespace_render::moderation::{appeal_page, AppealError};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/appeal")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let error = q.error.as_deref().and_then(|e| match e {
        "expired" => Some(AppealError::Expired),
        "empty" => Some(AppealError::Empty),
        "notyours" => Some(AppealError::NotYours),
        "nothidden" => Some(AppealError::NotHidden),
        other => other
            .strip_prefix("long-")
            .and_then(|n| n.parse().ok())
            .map(|max| AppealError::TooLong { max }),
    });
    uncached_html(appeal_page(
        &signed.mint(),
        &canonical,
        error,
        q.filed.is_some(),
    ))
}

#[worker::send]
async fn appeal_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: String,
) -> Response {
    use notespace_core::moderation::pipeline::{self, AppealOutcome};
    let Ok(post) = PublicId::parse(&id) else {
        return error(StatusCode::BAD_REQUEST, "bad post id");
    };
    let canonical = post.encode();
    let back = |q: &str| see_other(format!("/p/{canonical}/appeal?{q}"));
    let signed = match signed_in(&env, &headers, &format!("/p/{canonical}/appeal")).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return back("error=expired");
    }
    let text = fields.get("text").map(String::as_str).unwrap_or("");
    match pipeline::appeal(&signed.store, &post, &signed.user, text, now_ms()).await {
        Ok(AppealOutcome::Filed) => back("filed=1"),
        Ok(AppealOutcome::Empty) => back("error=empty"),
        Ok(AppealOutcome::TooLong { max }) => back(&format!("error=long-{max}")),
        Ok(AppealOutcome::NotYours) => back("error=notyours"),
        Ok(AppealOutcome::NotHidden) => back("error=nothidden"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct QueueQuery {
    did: Option<String>,
}

/// Capability-gated. A 404 rather than a 403 for non-moderators: the page's existence is not
/// their business.
#[worker::send]
async fn mod_queue(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    Query(q): Query<QueueQuery>,
) -> Response {
    use notespace_render::moderation::{queue_page, QueueNotice};
    let signed = match signed_in(&env, &headers, "/mod/queue").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !can_moderate(&env, &signed.user) {
        return error(StatusCode::NOT_FOUND, "not found");
    }
    let items = match signed.store.open_reviews(QUEUE_LIMIT).await {
        Ok(items) => items,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let notice = q.did.as_deref().and_then(|d| match d {
        "approve" => Some(QueueNotice::Approved),
        "reject" => Some(QueueNotice::Rejected),
        "gone" => Some(QueueNotice::Gone),
        _ => None,
    });
    uncached_html(queue_page(&items, &signed.mint(), notice))
}

#[worker::send]
async fn mod_review(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    UrlPath(id): UrlPath<i64>,
    body: String,
) -> Response {
    use notespace_core::moderation::pipeline::{self, ReviewOutcome};
    use notespace_core::moderation::Resolution;
    let signed = match signed_in(&env, &headers, "/mod/queue").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !can_moderate(&env, &signed.user) {
        return error(StatusCode::NOT_FOUND, "not found");
    }
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return see_other("/mod/queue".into());
    }
    let Some(resolution) = fields.get("resolution").and_then(|r| Resolution::parse(r)) else {
        return error(
            StatusCode::BAD_REQUEST,
            "resolution must be approve or reject",
        );
    };
    // The role check above covers the `MODERATORS` bootstrap list; the pipeline checks the
    // role on the user record, so lift a listed user to moderator for this call.
    let mut reviewer = signed.user.clone();
    if !reviewer.role.can_moderate() {
        reviewer.role = notespace_core::model::Role::Admin;
    }
    match pipeline::review(&signed.store, id, &reviewer, resolution, now_ms()).await {
        Ok(ReviewOutcome::Resolved { .. }) => {
            see_other(format!("/mod/queue?did={}", resolution.as_str()))
        }
        Ok(ReviewOutcome::Gone) => see_other("/mod/queue?did=gone".into()),
        Ok(ReviewOutcome::Forbidden) => error(StatusCode::NOT_FOUND, "not found"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Public. Briefly cacheable: it only changes when someone acts.
#[worker::send]
async fn modlog(State(env): State<Env>) -> Response {
    let Ok(db) = env.d1(DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = D1Store::new(db);
    match store.public_log(MODLOG_LIMIT).await {
        Ok(entries) => {
            let html = notespace_render::moderation::modlog_page(&entries).into_string();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::CACHE_CONTROL, "public, max-age=0, s-maxage=30"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                Html(html),
            )
                .into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The queue consumer. A store failure retries the message; a classifier failure does not,
/// because the pipeline has already handed the post to a human.
#[event(queue)]
async fn queue(batch: MessageBatch<moderation::Job>, env: Env, _ctx: Context) -> WorkerResult<()> {
    use notespace_core::moderation::pipeline;
    console_error_panic_hook::set_once();
    let db = env.d1(DB_BINDING)?;
    let store = D1Store::new(db);
    let classifier = match moderation::resolve(&env) {
        Ok(Some(c)) => c,
        Ok(None) => {
            // Nothing to classify with. Leave the posts pending for a human; ack so the
            // messages do not churn.
            worker::console_log!(
                "moderation: no classifier configured; {} held posts await a human",
                batch.messages()?.len()
            );
            batch.ack_all();
            return Ok(());
        }
        Err(why) => {
            worker::console_log!("moderation: classifier misconfigured: {why}");
            batch.retry_all();
            return Ok(());
        }
    };
    for msg in batch.messages()? {
        let job = msg.body();
        let Ok(post) = PublicId::parse(&job.post) else {
            worker::console_log!("moderation: dropping message with bad id {:?}", job.post);
            msg.ack();
            continue;
        };
        match pipeline::process_post(&store, &classifier, &post, &job.reasons, now_ms()).await {
            Ok(outcome) => {
                worker::console_log!("moderation: {post} -> {outcome:?}");
                msg.ack();
            }
            Err(e) => {
                worker::console_log!("moderation: {post} store error, will retry: {e}");
                msg.retry();
            }
        }
    }
    Ok(())
}

/// The safety net. Anything still pending past the grace period, that no human has yet, is
/// classified here -- whether the queue is misconfigured, absent, or simply lost a message.
#[event(scheduled)]
async fn scheduled(_event: worker::ScheduledEvent, env: Env, _ctx: worker::ScheduleContext) {
    use notespace_core::moderation::pipeline;
    console_error_panic_hook::set_once();
    let Ok(db) = env.d1(DB_BINDING) else {
        worker::console_log!("sweep: no D1 binding");
        return;
    };
    let store = D1Store::new(db);
    let classifier = match moderation::resolve(&env) {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(why) => {
            worker::console_log!("sweep: classifier misconfigured: {why}");
            return;
        }
    };
    match pipeline::drain(&store, &classifier, now_ms(), SWEEP_LIMIT).await {
        Ok(results) => {
            for (post, r) in results {
                worker::console_log!("sweep: {post} -> {r:?}");
            }
        }
        Err(e) => worker::console_log!("sweep: {e}"),
    }
}
