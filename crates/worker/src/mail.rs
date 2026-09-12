//! Outbound mail on the Worker: Resend over its HTTP API, since a Worker has no SMTP. Request
//! and response shapes come from `core`; this file only carries bytes.

use notespace_core::email::{self, Links, MailError, Mailer, Message};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

/// Kept in step with wrangler.toml by `the_mail_binding_names_match_wrangler_toml`.
pub const API_KEY_SECRET: &str = "RESEND_API_KEY";
/// `"notespace <no-reply@notespace.org>"` or a bare address, on a domain verified with Resend.
pub const FROM_VAR: &str = "EMAIL_FROM";
/// Where links in mail point, `https://forum.example`. Defaults to the request's host.
pub const BASE_URL_VAR: &str = "BASE_URL";
/// What the mail calls the site.
pub const SITE_NAME_VAR: &str = "SITE_NAME";
/// `"true"` makes an address mandatory at signup. Only read with the `password` feature; an
/// external identity provider supplies its own.
#[cfg_attr(not(feature = "password"), allow(dead_code))]
pub const REQUIRE_EMAIL_VAR: &str = "REQUIRE_EMAIL";

pub struct Resend {
    key: String,
    from: String,
}

#[async_trait::async_trait(?Send)]
impl Mailer for Resend {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let transport = |e: worker::Error| MailError::Transport(e.to_string());
        let h = Headers::new();
        for (k, v) in email::resend_headers(&self.key) {
            h.set(k, &v).map_err(transport)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(h)
            .with_body(Some(
                email::resend_request(&self.from, message)
                    .to_string()
                    .into(),
            ));
        let req = Request::new_with_init(email::RESEND_URL, &init).map_err(transport)?;
        let mut resp = Fetch::Request(req).send().await.map_err(transport)?;
        let status = resp.status_code();
        let body: serde_json::Value = resp
            .json()
            .await
            .unwrap_or_else(|e| serde_json::json!({ "message": e.to_string() }));
        email::resend_parse(status, &body)
    }
}

/// Either Resend or nothing; the flows do not care which.
pub enum MaybeMailer {
    Real(Resend),
    None,
}

impl MaybeMailer {
    /// Both the key and a sender are needed; one without the other is treated as unconfigured
    /// and logged once, rather than failing every signup with a half-configured provider.
    pub fn from_env(env: &Env) -> Self {
        let key = env.secret(API_KEY_SECRET).ok().map(|s| s.to_string());
        let from = env.var(FROM_VAR).ok().map(|v| v.to_string());
        match (key, from) {
            (Some(key), Some(from)) if !key.is_empty() && !from.trim().is_empty() => {
                MaybeMailer::Real(Resend { key, from })
            }
            (Some(key), _) if !key.is_empty() => {
                worker::console_log!(
                    "mail: {API_KEY_SECRET} is set but {FROM_VAR} is empty; mail is off"
                );
                MaybeMailer::None
            }
            _ => MaybeMailer::None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Mailer for MaybeMailer {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        match self {
            MaybeMailer::Real(r) => r.send(message).await,
            MaybeMailer::None => Err(MailError::NotConfigured),
        }
    }
}

/// Where links point and what the site is called. `BASE_URL` wins; otherwise the request's
/// own host, which is right for every deployment with one hostname.
pub struct LinkConfig {
    base_url: String,
    site_name: String,
}

impl LinkConfig {
    pub fn resolve(env: &Env, host: Option<&str>) -> Self {
        let base_url = env
            .var(BASE_URL_VAR)
            .ok()
            .map(|v| v.to_string().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| host.map(|h| format!("https://{h}")))
            .unwrap_or_else(|| "https://localhost".into());
        let site_name = env
            .var(SITE_NAME_VAR)
            .ok()
            .map(|v| v.to_string())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "notespace".into());
        LinkConfig {
            base_url,
            site_name,
        }
    }

    pub fn links(&self) -> Links<'_> {
        Links {
            base_url: &self.base_url,
            site_name: &self.site_name,
        }
    }
}

#[cfg_attr(not(feature = "password"), allow(dead_code))]
pub fn require_email(env: &Env) -> bool {
    env.var(REQUIRE_EMAIL_VAR)
        .map(|v| {
            matches!(
                v.to_string().trim().to_lowercase().as_str(),
                "true" | "1" | "yes"
            )
        })
        .unwrap_or(false)
}
