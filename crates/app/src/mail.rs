//! Outbound mail, in terms of the platform. The provider request and response shapes are
//! `core`'s; this file resolves configuration and carries bytes.

use crate::platform::{HttpRequest, Platform};
use notespace_core::email::providers::{Body, OutboundRequest, Provider, Sender};
use notespace_core::email::{Links, MailError, Mailer, Message};

/// The Cloudflare Email Service binding's name, when the platform has one.
pub const EMAIL_BINDING: &str = "EMAIL";
/// `cloudflare` (default when the binding exists), `resend` (default when `RESEND_API_KEY` is
/// set), `postmark`, `sendgrid`, `mailgun`, `brevo`, `cloudflare_api`, or `off`.
pub const PROVIDER_VAR: &str = "MAIL_PROVIDER";
pub const API_KEY_SECRET: &str = "MAIL_API_KEY";
pub const RESEND_KEY_SECRET: &str = "RESEND_API_KEY";
pub const FROM_VAR: &str = "EMAIL_FROM";
pub const BASE_URL_VAR: &str = "BASE_URL";
pub const SITE_NAME_VAR: &str = "SITE_NAME";
#[cfg_attr(not(feature = "password"), allow(dead_code))]
pub const REQUIRE_EMAIL_VAR: &str = "REQUIRE_EMAIL";
pub const CF_ACCOUNT_VAR: &str = "CF_ACCOUNT_ID";
pub const MAILGUN_REGION_VAR: &str = "MAILGUN_REGION";
pub const MAILGUN_DOMAIN_VAR: &str = "MAILGUN_DOMAIN";
pub const POSTMARK_STREAM_VAR: &str = "POSTMARK_STREAM";

/// A provider spoken to over HTTPS.
pub struct Http<P: Platform> {
    p: P,
    provider: Provider,
    key: String,
    from: Sender,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Mailer for Http<P> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let req = outbound(self.provider.request(&self.key, &self.from, message));
        let resp = self.p.http(req).await.map_err(MailError::Transport)?;
        self.provider.parse_response(resp.status, &resp.body)
    }
}

fn outbound(req: OutboundRequest) -> HttpRequest {
    let (content_type, body) = match req.body {
        Body::Json(v) => ("application/json", v.to_string()),
        Body::Form(fields) => {
            let mut enc = form_urlencoded::Serializer::new(String::new());
            for (k, v) in &fields {
                enc.append_pair(k, v);
            }
            ("application/x-www-form-urlencoded", enc.finish())
        }
    };
    let mut out = HttpRequest::post(req.url, content_type, body);
    for (k, v) in req.headers {
        out = out.header(k, v);
    }
    out
}

/// The platform's own transport, when it has one: Cloudflare's Email Service binding.
pub struct Binding<P: Platform> {
    p: P,
    from: Sender,
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Mailer for Binding<P> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        self.p
            .send_email(&self.from, message)
            .await
            .unwrap_or(Err(MailError::NotConfigured))
    }
}

/// Whatever the deployment configured, or nothing; the caller does not care which. Every
/// send through `None` is `MailError::NotConfigured`, which the flows treat as "mail is off".
pub enum MaybeMailer<P: Platform> {
    Binding(Binding<P>),
    Http(Http<P>),
    None,
}

impl<P: Platform> MaybeMailer<P> {
    pub fn resolve(p: &P) -> Self {
        match configured(p) {
            Ok(m) => m,
            Err(why) => {
                p.log(&format!("mail: {why}; mail is off"));
                MaybeMailer::None
            }
        }
    }
}

fn configured<P: Platform>(p: &P) -> Result<MaybeMailer<P>, String> {
    let provider = p.var(PROVIDER_VAR).unwrap_or_default();
    let binding = p.has_email_binding();
    let resend_key = p.secret(RESEND_KEY_SECRET);

    let provider = match provider.trim() {
        "" if binding => "cloudflare",
        "" if resend_key.is_some() => "resend",
        "" => return Ok(MaybeMailer::None),
        p => p,
    };
    if matches!(provider, "off" | "none") {
        return Ok(MaybeMailer::None);
    }
    let from = p
        .var(FROM_VAR)
        .ok_or_else(|| format!("{PROVIDER_VAR}={provider} needs {FROM_VAR}"))
        .and_then(|f| {
            Sender::parse(&f).map_err(|e| format!("{FROM_VAR}={f:?} is not a sender: {e}"))
        })?;

    if provider == "cloudflare" {
        if !binding {
            return Err(format!(
                "{PROVIDER_VAR}=cloudflare needs a [[send_email]] binding named {EMAIL_BINDING}"
            ));
        }
        return Ok(MaybeMailer::Binding(Binding { p: p.clone(), from }));
    }

    let key = p
        .secret(API_KEY_SECRET)
        .or_else(|| (provider == "resend").then_some(resend_key).flatten())
        .ok_or_else(|| format!("{PROVIDER_VAR}={provider} needs the {API_KEY_SECRET} secret"))?;
    let provider = match provider {
        "cloudflare_api" => Provider::Cloudflare {
            account_id: p
                .var(CF_ACCOUNT_VAR)
                .ok_or_else(|| format!("{PROVIDER_VAR}=cloudflare_api needs {CF_ACCOUNT_VAR}"))?,
        },
        "resend" => Provider::Resend,
        "postmark" => Provider::Postmark {
            stream: p.var(POSTMARK_STREAM_VAR),
        },
        "sendgrid" => Provider::SendGrid,
        "mailgun" => Provider::Mailgun {
            domain: p
                .var(MAILGUN_DOMAIN_VAR)
                .unwrap_or_else(|| from.domain().to_string()),
            eu: p
                .var(MAILGUN_REGION_VAR)
                .map(|r| r.trim().eq_ignore_ascii_case("eu"))
                .unwrap_or(false),
        },
        "brevo" => Provider::Brevo,
        other => return Err(format!("{PROVIDER_VAR}={other:?} is not a known provider")),
    };
    Ok(MaybeMailer::Http(Http {
        p: p.clone(),
        provider,
        key,
        from,
    }))
}

#[async_trait::async_trait(?Send)]
impl<P: Platform> Mailer for MaybeMailer<P> {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        match self {
            MaybeMailer::Binding(b) => b.send(message).await,
            MaybeMailer::Http(h) => h.send(message).await,
            MaybeMailer::None => Err(MailError::NotConfigured),
        }
    }
}

/// Where links in mail point, and what the site is called there.
pub struct LinkConfig {
    base_url: String,
    site_name: String,
}

impl LinkConfig {
    /// `BASE_URL` if set, else the request's host over https.
    pub fn resolve<P: Platform>(p: &P, host: Option<&str>) -> Self {
        let base_url = p
            .var(BASE_URL_VAR)
            .map(|v| v.trim_end_matches('/').to_string())
            .or_else(|| host.map(|h| format!("https://{h}")))
            .unwrap_or_else(|| "https://localhost".into());
        let site_name = p.var(SITE_NAME_VAR).unwrap_or_else(|| "notespace".into());
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

/// `REQUIRE_EMAIL`: whether signup insists on an address.
#[cfg_attr(not(feature = "password"), allow(dead_code))]
pub fn require_email<P: Platform>(p: &P) -> bool {
    p.var(REQUIRE_EMAIL_VAR)
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}
