//! Edge caching for the baked read path.
//!
//! Cloudflare does not cache a Worker's own response, so `s-maxage` alone does nothing — the
//! Cache API call here is what makes it real. Entries are keyed by `cache_version`, so a write
//! invalidates them in the statement that stores the post.
//!
//! Only sound while the page carries no viewer identity.

use axum::http::header;
use axum::response::Response;
use worker::{Cache, Headers, Response as WorkerResponse};

pub use notespace_core::cache_key::thread_key;

/// Used for both the live response and the cached copy, so the two cannot disagree.
pub const PAGE_HEADERS: [(&str, &str); 5] = [
    ("content-type", "text/html; charset=utf-8"),
    // Retention, not correctness: the version key is what invalidates.
    ("cache-control", "public, max-age=0, s-maxage=3600"),
    (
        "content-security-policy",
        "default-src 'self'; img-src https: data:; style-src 'unsafe-inline'; script-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
];

/// `spent` is the version lookup that produced the key; the stored response's own
/// `Server-Timing` describes a different request, so the header is rebuilt.
pub async fn get(key: &str, spent: &str) -> Option<Response> {
    let hit = Cache::default().get(key, false).await.ok()??;
    let mut resp: Response = hit.into();
    if let Ok(v) = format!("{spent}, cache;desc=\"hit\"").parse() {
        resp.headers_mut()
            .insert(header::HeaderName::from_static("server-timing"), v);
    }
    Some(resp)
}

/// Errors are swallowed: a cache that will not accept a write should not fail the pageview.
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
