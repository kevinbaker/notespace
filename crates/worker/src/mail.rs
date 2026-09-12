//! Outbound mail on the Worker. A Worker has no SMTP, so mail is either Cloudflare's own
//! Email Service through a binding or one of several providers over HTTPS. The request and
//! response shapes come from `core::email::providers`; this file only carries bytes.

use notespace_core::email::providers::{Body, OutboundRequest, Provider, Sender};
use notespace_core::email::{Links, MailError, Mailer, Message};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit};

/// Kept in step with wrangler.toml by `the_mail_names_match_wrangler_toml`.
pub const EMAIL_BINDING: &str = "EMAIL";
/// `cloudflare` (default when the `EMAIL` binding exists) | `cloudflare_api` | `resend` |
/// `postmark` | `sendgrid` | `mailgun` | `brevo` | `off`.
pub const PROVIDER_VAR: &str = "MAIL_PROVIDER";
/// The key or token for every HTTP provider. `RESEND_API_KEY` still works for Resend.
pub const API_KEY_SECRET: &str = "MAIL_API_KEY";
pub const RESEND_KEY_SECRET: &str = "RESEND_API_KEY";
/// `"notespace <no-reply@notespace.org>"` or a bare address, on a domain the provider has
/// verified. Required by every provider.
pub const FROM_VAR: &str = "EMAIL_FROM";
/// Where links in mail point, `https://forum.example`. Defaults to the request's host.
pub const BASE_URL_VAR: &str = "BASE_URL";
/// What the mail calls the site.
pub const SITE_NAME_VAR: &str = "SITE_NAME";
/// `"true"` makes an address mandatory at signup. Only read with the `password` feature; an
/// external identity provider supplies its own.
#[cfg_attr(not(feature = "password"), allow(dead_code))]
pub const REQUIRE_EMAIL_VAR: &str = "REQUIRE_EMAIL";
/// `cloudflare_api` only: the account whose Email Service sends.
pub const CF_ACCOUNT_VAR: &str = "CF_ACCOUNT_ID";
/// `mailgun` only: `"eu"` for an EU-region account; the sending domain defaults to the
/// sender's.
pub const MAILGUN_REGION_VAR: &str = "MAILGUN_REGION";
pub const MAILGUN_DOMAIN_VAR: &str = "MAILGUN_DOMAIN";
/// `postmark` only: a message stream other than the server default.
pub const POSTMARK_STREAM_VAR: &str = "POSTMARK_STREAM";

/// A provider spoken to over HTTPS.
pub struct Http {
    provider: Provider,
    key: String,
    from: Sender,
}

#[async_trait::async_trait(?Send)]
impl Mailer for Http {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        let (status, body) = post(self.provider.request(&self.key, &self.from, message)).await?;
        self.provider.parse_response(status, &body)
    }
}

/// One POST; the status and body come back for the provider to judge.
async fn post(req: OutboundRequest) -> Result<(u16, String), MailError> {
    let transport = |e: worker::Error| MailError::Transport(e.to_string());
    let h = Headers::new();
    for (k, v) in &req.headers {
        h.set(k, v).map_err(transport)?;
    }
    let body = match req.body {
        Body::Json(v) => {
            h.set("Content-Type", "application/json")
                .map_err(transport)?;
            v.to_string()
        }
        Body::Form(fields) => {
            h.set("Content-Type", "application/x-www-form-urlencoded")
                .map_err(transport)?;
            let mut enc = form_urlencoded::Serializer::new(String::new());
            for (k, v) in &fields {
                enc.append_pair(k, v);
            }
            enc.finish()
        }
    };
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(h)
        .with_body(Some(body.into()));
    let request = Request::new_with_init(&req.url, &init).map_err(transport)?;
    let mut resp = Fetch::Request(request).send().await.map_err(transport)?;
    let status = resp.status_code();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

/// Cloudflare Email Service through the `[[send_email]]` binding: no key, no HTTP, and the
/// sending domain has to be onboarded (`wrangler email sending enable <domain>`).
pub struct CloudflareBinding {
    binding: worker::email::SendEmail,
    from: Sender,
}

#[async_trait::async_trait(?Send)]
impl Mailer for CloudflareBinding {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        use worker::email::{EmailAddress, SendEmailBuilder};
        let from = EmailAddress::new(
            self.from.name.as_deref().unwrap_or(""),
            self.from.address.as_str(),
        );
        let request = SendEmailBuilder::builder_with_email_address_and_str(
            &from,
            message.to.as_str(),
            &message.subject,
        )
        .text(&message.text)
        .build();
        match self.binding.send_with_builder(&request).await {
            Ok(_) => Ok(()),
            // The runtime throws with an `E_*` code and a message; both are the reason.
            Err(e) => {
                let code = worker::js_sys::Reflect::get(&e, &"code".into())
                    .ok()
                    .and_then(|c| c.as_string())
                    .unwrap_or_default();
                let message = String::from(e.message());
                Err(MailError::Rejected(
                    format!("cloudflare: {code} {message}").trim().to_string(),
                ))
            }
        }
    }
}

/// Whichever the deployment configured, or nothing.
pub enum MaybeMailer {
    Binding(CloudflareBinding),
    Http(Http),
    None,
}

impl MaybeMailer {
    /// A half-configured provider is "no mail", logged, rather than a failure on every signup.
    pub fn from_env(env: &Env) -> Self {
        match resolve(env) {
            Ok(m) => m,
            Err(why) => {
                worker::console_log!("mail: {why}; mail is off");
                MaybeMailer::None
            }
        }
    }
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|v| v.to_string())
        .filter(|v| !v.trim().is_empty())
}

fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name)
        .ok()
        .map(|s| s.to_string())
        .filter(|v| !v.is_empty())
}

fn resolve(env: &Env) -> Result<MaybeMailer, String> {
    let provider = var(env, PROVIDER_VAR).unwrap_or_default();
    let binding = env.send_email(EMAIL_BINDING).ok();
    let resend_key = secret(env, RESEND_KEY_SECRET);

    // Absent: the binding if it is there, otherwise a Resend key if one is, otherwise off.
    let provider = match provider.trim() {
        "" if binding.is_some() => "cloudflare",
        "" if resend_key.is_some() => "resend",
        "" => return Ok(MaybeMailer::None),
        p => p,
    };
    if matches!(provider, "off" | "none") {
        return Ok(MaybeMailer::None);
    }
    let from = var(env, FROM_VAR)
        .ok_or_else(|| format!("{PROVIDER_VAR}={provider} needs {FROM_VAR}"))
        .and_then(|f| {
            Sender::parse(&f).map_err(|e| format!("{FROM_VAR}={f:?} is not a sender: {e}"))
        })?;

    if provider == "cloudflare" {
        let binding = binding.ok_or_else(|| {
            format!(
                "{PROVIDER_VAR}=cloudflare needs a [[send_email]] binding named {EMAIL_BINDING}"
            )
        })?;
        return Ok(MaybeMailer::Binding(CloudflareBinding { binding, from }));
    }

    let key = secret(env, API_KEY_SECRET)
        .or_else(|| (provider == "resend").then_some(resend_key).flatten())
        .ok_or_else(|| format!("{PROVIDER_VAR}={provider} needs the {API_KEY_SECRET} secret"))?;
    let provider = match provider {
        "cloudflare_api" => Provider::Cloudflare {
            account_id: var(env, CF_ACCOUNT_VAR)
                .ok_or_else(|| format!("{PROVIDER_VAR}=cloudflare_api needs {CF_ACCOUNT_VAR}"))?,
        },
        "resend" => Provider::Resend,
        "postmark" => Provider::Postmark {
            stream: var(env, POSTMARK_STREAM_VAR),
        },
        "sendgrid" => Provider::SendGrid,
        "mailgun" => Provider::Mailgun {
            domain: var(env, MAILGUN_DOMAIN_VAR).unwrap_or_else(|| from.domain().to_string()),
            eu: var(env, MAILGUN_REGION_VAR)
                .map(|r| r.trim().eq_ignore_ascii_case("eu"))
                .unwrap_or(false),
        },
        "brevo" => Provider::Brevo,
        other => return Err(format!("{PROVIDER_VAR}={other:?} is not a known provider")),
    };
    Ok(MaybeMailer::Http(Http {
        provider,
        key,
        from,
    }))
}

#[async_trait::async_trait(?Send)]
impl Mailer for MaybeMailer {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        match self {
            MaybeMailer::Binding(b) => b.send(message).await,
            MaybeMailer::Http(h) => h.send(message).await,
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
        let base_url = var(env, BASE_URL_VAR)
            .map(|v| v.trim_end_matches('/').to_string())
            .or_else(|| host.map(|h| format!("https://{h}")))
            .unwrap_or_else(|| "https://localhost".into());
        let site_name = var(env, SITE_NAME_VAR).unwrap_or_else(|| "notespace".into());
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
    var(env, REQUIRE_EMAIL_VAR)
        .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}
