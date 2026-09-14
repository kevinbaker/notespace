//! What the web layer needs from whatever is running it. A Cloudflare Worker answers these with
//! bindings; the native server with a SQLite file, `reqwest` and the process environment. The
//! handlers see only this trait.
//!
//! Everything a handler awaits comes through here or through [`Platform::Store`], and none of
//! it is required to be `Send`: `#[handler]` takes care of axum. A platform is cloned per
//! request by axum, so it should be a handle, not the resources themselves.

use async_trait::async_trait;
use notespace_core::email::providers::Sender;
use notespace_core::email::{MailError, Message};
use notespace_core::moderation::heuristics::Reason;
use notespace_core::oidc::Jwk;
use notespace_core::store::Store;
use serde::{Deserialize, Serialize};

/// One outbound HTTP call: a classifier, a mail provider, an OIDC token endpoint.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        HttpRequest {
            method: "GET",
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>, content_type: &str, body: String) -> Self {
        HttpRequest {
            method: "POST",
            url: url.into(),
            headers: vec![("Content-Type".into(), content_type.into())],
            body: Some(body),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Status and body; the caller's provider code decides what they mean.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// A baked page or feed in the edge cache: the bytes and what they are.
#[derive(Debug, Clone)]
pub struct Cached {
    pub body: String,
    pub content_type: String,
}

/// One moderation job: which post, and what Tier 0 held it for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub post: String,
    #[serde(default)]
    pub reasons: Vec<Reason>,
}

pub use notespace_core::store::{Instrumented, QueryStats};

#[async_trait(?Send)]
pub trait Platform: Clone + Send + Sync + 'static {
    type Store: Store + Instrumented;

    fn store(&self) -> &Self::Store;

    /// Wall clock, unix milliseconds.
    fn now_ms(&self) -> i64;

    /// A configuration value. `None` when unset or blank.
    fn var(&self, name: &str) -> Option<String>;

    /// A secret. On a Worker these are a separate kind of binding; natively they are variables
    /// like any other, which is why the default falls through.
    fn secret(&self, name: &str) -> Option<String> {
        self.var(name)
    }

    fn log(&self, line: &str) {
        crate::log(line)
    }

    fn log_error(&self, line: &str) {
        crate::log_error(line)
    }

    async fn http(&self, req: HttpRequest) -> Result<HttpResponse, String>;

    /// The edge cache for baked pages. Keys carry the content version, so a stale entry is
    /// garbage rather than wrong; a platform may drop entries whenever it likes.
    async fn cache_get(&self, key: &str) -> Option<Cached>;
    async fn cache_put(&self, key: &str, cached: Cached);

    /// Hand a held post to whatever classifies it. `Ok(false)` means there is no queue and the
    /// sweep will pick the post up; `Err` means there is one and it refused.
    async fn enqueue(&self, job: Job) -> Result<bool, String>;

    /// Workers AI, or nothing: `None` means classifiers that need it are not available here.
    async fn ai_run(
        &self,
        model: &str,
        input: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>>;

    fn has_ai(&self) -> bool;

    /// A mail transport that is a platform binding rather than an HTTP API (Cloudflare's Email
    /// Service). `None` when the platform has none.
    async fn send_email(&self, from: &Sender, message: &Message) -> Option<Result<(), MailError>>;

    fn has_email_binding(&self) -> bool;

    /// RS256 over a JWK, for OIDC id tokens. The Worker uses `crypto.subtle`; native code uses
    /// the `rsa` crate. Either way the JWT parsing around it is `core`'s.
    async fn verify_rs256(
        &self,
        key: &Jwk,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<bool, String>;
}
