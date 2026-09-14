//! The edge cache for the baked read path, in terms of the platform. Only sound while a cached
//! page carries no viewer identity.

use crate::platform::{Cached, Platform};
use axum::http::header;
use axum::response::{IntoResponse, Response};

pub use notespace_core::cache_key::thread_key;

/// Used for both the live response and the cached copy, so the two cannot disagree.
pub const PAGE_HEADERS: [(&str, &str); 5] = [
    ("content-type", "text/html; charset=utf-8"),
    // Retention, not correctness: the version key is what invalidates.
    ("cache-control", "public, max-age=0, s-maxage=3600"),
    (
        "content-security-policy",
        "default-src 'self'; img-src https: data:; style-src 'self' 'unsafe-inline'; script-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
];

/// A page response with the shared headers and a `Server-Timing`.
pub fn page_response(body: String, content_type: &str, server_timing: &str) -> Response {
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    for (name, value) in PAGE_HEADERS {
        let value = if name == "content-type" {
            content_type
        } else {
            value
        };
        if let (Ok(n), Ok(v)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            value.parse(),
        ) {
            h.insert(n, v);
        }
    }
    if let Ok(v) = server_timing.parse() {
        h.insert(header::HeaderName::from_static("server-timing"), v);
    }
    resp
}

/// `spent` is the version lookup that produced the key; the stored copy's own timing described
/// a different request, so the header is rebuilt.
pub async fn get<P: Platform>(p: &P, key: &str, spent: &str) -> Option<Response> {
    let hit = p.cache_get(key).await?;
    Some(page_response(
        hit.body,
        &hit.content_type,
        &format!("{spent}, cache;desc=\"hit\""),
    ))
}

pub async fn put<P: Platform>(p: &P, key: &str, body: &str, content_type: &str) {
    p.cache_put(
        key,
        Cached {
            body: body.to_string(),
            content_type: content_type.to_string(),
        },
    )
    .await
}
