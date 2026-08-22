//! Edge caching for the baked read path.
//!
//! A Worker's response is returned straight to the client: Cloudflare's CDN does not cache it,
//! so `s-maxage` on its own does nothing here. Storing the response through the Cache API is
//! what makes the header real, and what lets one render serve every reader for its TTL.
//!
//! Only safe because the baked page is user-agnostic — no viewer identity, no `Set-Cookie`.
//! `baked_page_contains_no_viewer_identity` in `notespace_render` is the test that keeps it so.
//! Anything that starts varying by viewer must stop being cached here first.

use axum::http::header;
use axum::response::Response;
use worker::{Cache, Headers, Response as WorkerResponse};

pub use notespace_core::cache_key::thread_key;

/// Headers every baked thread page carries.
///
/// One list, used to build both the live response and the copy that goes into the cache, so the
/// two cannot drift into disagreeing about how long the page may be held.
pub const PAGE_HEADERS: [(&str, &str); 5] = [
    ("content-type", "text/html; charset=utf-8"),
    // `s-maxage` is the edge TTL the Cache API reads; `max-age=0` keeps browsers revalidating.
    ("cache-control", "public, max-age=0, s-maxage=60"),
    // Server-rendered HTML with no inline script; blocks stored-XSS payloads that would
    // survive a future sanitizer regression.
    (
        "content-security-policy",
        "default-src 'self'; img-src https: data:; style-src 'unsafe-inline'; script-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
];

/// Look for a stored copy.
///
/// The stored response carries the `Server-Timing` of the render that produced it. Serving that
/// verbatim would report D1 work this request did not do, so it is replaced on the way out.
pub async fn get(key: &str) -> Option<Response> {
    let hit = Cache::default().get(key, false).await.ok()??;
    let mut resp: Response = hit.into();
    if let Ok(v) = "cache;desc=\"hit\"".parse() {
        resp.headers_mut()
            .insert(header::HeaderName::from_static("server-timing"), v);
    }
    Some(resp)
}

/// Store a rendered page.
///
/// Errors are swallowed deliberately: a cache that will not accept a write is a performance
/// problem, and turning it into a failed pageview would make it an availability one.
pub async fn put(key: &str, html: &str, server_timing: &str) {
    let headers = Headers::new();
    for (k, v) in PAGE_HEADERS {
        if headers.set(k, v).is_err() {
            return;
        }
    }
    let _ = headers.set("server-timing", server_timing);
    let stored = WorkerResponse::builder()
        .with_status(200)
        .with_headers(headers)
        .from_html(html);
    if let Ok(resp) = stored {
        let _ = Cache::default().put(key, resp).await;
    }
}
