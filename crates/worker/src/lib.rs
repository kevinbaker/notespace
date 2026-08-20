//! Cloudflare Workers entrypoint for notespace.
//!
//! M0 scope (DESIGN.md §8): serve a 200-post thread page from D1 and prove it fits inside
//! the free-tier CPU, size and query budgets. Auth, writes and baking are M2/M6.

mod store;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use notespace_core::path::Path as TreePath;
use notespace_core::store::{Page, StoreError};
use serde::Deserialize;
use store::D1Store;
use tower_service::Service;
use worker::{event, Context, Env, HttpRequest, Result as WorkerResult};

/// Name of the D1 binding in wrangler.toml.
const DB_BINDING: &str = "DB";

/// Posts per page. 200 is the number DESIGN.md §8 names as the spike target: if a page this
/// size does not fit the budget, the free-tier premise fails.
const PAGE_SIZE: u32 = 200;

#[derive(Deserialize, Default)]
struct PageQuery {
    /// Opaque cursor: the materialized path to resume after.
    after: Option<String>,
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
        .with_state(env)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn thread_page_slug(
    state: State<Env>,
    UrlPath((id, _slug)): UrlPath<(i64, String)>,
    query: Query<PageQuery>,
) -> Response {
    thread_page(state, UrlPath(id), query).await
}

/// `#[worker::send]` wraps the future so axum's `Send` bound is satisfied. Workers are
/// single-threaded, so this is sound here and is the pattern workers-rs prescribes.
#[worker::send]
async fn thread_page(
    State(env): State<Env>,
    UrlPath(id): UrlPath<i64>,
    Query(q): Query<PageQuery>,
) -> Response {
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

    match store.thread_page_with_space(id, page).await {
        Ok((space, page)) => {
            let html = notespace_render::thread_page(&space, &page).into_string();
            (
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
                .into_response()
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
