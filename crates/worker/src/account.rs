//! Account self-service: settings, address verification, and password recovery. The password
//! flows exist only with the `password` feature; verification works for any account.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use notespace_core::account::{self, ConfirmOutcome, Delivery};
use notespace_core::email::EmailToken;
use notespace_core::store::Store;
use notespace_render::account::{SettingsError, SettingsNotice, VerifyOutcome};
use serde::Deserialize;
use worker::Env;

use crate::{
    anon_binding, anon_page, anon_token, csrf_key, error, form_fields, host, ids, mail, now_ms,
    see_other, signed_in, uncached_html, urlencoding,
};

/// `?did=` value for how a verification mail went.
pub(crate) fn delivery_code(d: &Delivery) -> &'static str {
    match d {
        Delivery::Sent => "sent",
        Delivery::NotConfigured => "nomail",
        Delivery::Failed(_) => "failed",
    }
}

#[derive(Deserialize, Default)]
pub struct SettingsQuery {
    did: Option<String>,
    error: Option<String>,
}

fn settings_notice(q: &SettingsQuery) -> Option<SettingsNotice> {
    if let Some(did) = q.did.as_deref() {
        return match did {
            "sent" => Some(SettingsNotice::VerificationSent),
            "nomail" => Some(SettingsNotice::VerificationNotSent),
            "failed" => Some(SettingsNotice::VerificationFailed),
            "password" => Some(SettingsNotice::PasswordChanged),
            "sessions" => Some(SettingsNotice::SignedOutEverywhere),
            _ => None,
        };
    }
    let e = q.error.as_deref()?;
    let err = match e {
        "wrong" => SettingsError::WrongPassword,
        "nopassword" => SettingsError::NoPassword,
        "expired" => SettingsError::Expired,
        other => match other.strip_prefix("email-") {
            Some(why) => SettingsError::BadEmail(why.to_string()),
            None => SettingsError::ShortPassword {
                min: other.strip_prefix("short-")?.parse().ok()?,
            },
        },
    };
    Some(SettingsNotice::Error(err))
}

#[worker::send]
pub async fn settings(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    Query(q): Query<SettingsQuery>,
) -> Response {
    let signed = match signed_in(&env, &headers, "/settings").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let account = match signed.store.account(signed.user.id).await {
        Ok(Some(a)) => a,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such account"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let providers = signed
        .store
        .user_identities(signed.user.id)
        .await
        .unwrap_or_default();
    uncached_html(notespace_render::account::settings_page(
        &signed.mint(),
        &signed.user,
        &account,
        &providers,
        settings_notice(&q),
    ))
}

/// Also how "resend the link" works: re-saving the same address issues a fresh one.
#[worker::send]
pub async fn change_email(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let signed = match signed_in(&env, &headers, "/settings").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return see_other("/settings?error=expired".into());
    }
    let email = fields.get("email").map(String::as_str).unwrap_or("");
    let token = match ids::random_email_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let mailer = mail::MaybeMailer::from_env(&env);
    let link_cfg = mail::LinkConfig::resolve(&env, host(&headers));
    match account::change_email(
        &signed.store,
        &mailer,
        &link_cfg.links(),
        &signed.user,
        email,
        token,
        now_ms(),
    )
    .await
    {
        Ok(Ok(delivery)) => {
            if let Delivery::Failed(why) = &delivery {
                worker::console_log!(
                    "mail: verification to user {} failed: {why}",
                    signed.user.id
                );
            }
            see_other(format!("/settings?did={}", delivery_code(&delivery)))
        }
        Ok(Err(why)) => see_other(format!(
            "/settings?error=email-{}",
            urlencoding(&why.to_string())
        )),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Ends every session but the one asking.
#[worker::send]
pub async fn end_other_sessions(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let signed = match signed_in(&env, &headers, "/settings").await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let fields = form_fields(&body);
    if !signed.verify(fields.get("csrf").map(String::as_str).unwrap_or("")) {
        return see_other("/settings?error=expired".into());
    }
    if let Err(e) = signed.store.delete_user_sessions(signed.user.id).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    if let Err(e) = signed.store.create_session(&signed.auth).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    see_other("/settings?did=sessions".into())
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct TokenQuery {
    token: Option<String>,
    /// Read by the reset form only; verification renders its outcome in place.
    #[cfg_attr(not(feature = "password"), allow(dead_code))]
    error: Option<String>,
}

/// The link lands here and shows a button. Following a link must not spend it: mail scanners
/// follow links.
#[worker::send]
pub async fn verify_form(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let raw = q.token.unwrap_or_default();
    if EmailToken::parse(&raw).is_none() {
        return uncached_html(notespace_render::account::verify_page(
            "",
            "",
            Some(VerifyOutcome::Invalid),
        ));
    }
    let (csrf, set_anon) = match anon_token(&key, &headers) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    anon_page(
        notespace_render::account::verify_page(&csrf, &raw, None),
        set_anon,
    )
}

#[worker::send]
pub async fn verify_submit(
    State(env): State<Env>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let fields = form_fields(&body);
    let raw = fields.get("token").map(String::as_str).unwrap_or("");
    let csrf = fields.get("csrf").map(String::as_str).unwrap_or("");
    if key.verify(csrf, &anon_binding(&headers), now_ms()).is_err() {
        // Back to the button; the token is still unspent.
        return see_other(format!("/verify?token={}", urlencoding(raw)));
    }
    let Some(token) = EmailToken::parse(raw) else {
        return uncached_html(notespace_render::account::verify_page(
            "",
            "",
            Some(VerifyOutcome::Invalid),
        ));
    };
    let Ok(db) = env.d1(crate::DB_BINDING) else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
    };
    let store = crate::store::D1Store::new(db);
    let outcome = match account::confirm_email(&store, &token, now_ms()).await {
        Ok(ConfirmOutcome::Verified { .. }) => VerifyOutcome::Verified,
        Ok(ConfirmOutcome::Invalid) => VerifyOutcome::Invalid,
        Ok(ConfirmOutcome::Stale) => VerifyOutcome::Stale,
        Ok(ConfirmOutcome::Claimed) => VerifyOutcome::Claimed,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    uncached_html(notespace_render::account::verify_page(
        "",
        "",
        Some(outcome),
    ))
}

// ---------------------------------------------------------------------------
// Passwords
// ---------------------------------------------------------------------------

#[cfg(feature = "password")]
pub use passwords::*;

#[cfg(feature = "password")]
mod passwords {
    use super::*;
    use crate::auth_config::AuthConfig;
    use crate::client_address;
    use notespace_core::account::{
        ChangeOutcome, PasswordChange, RecoveryConfig, RequestOutcome, ResetCompletion,
        ResetOutcome, ResetRequest,
    };
    use notespace_core::email::TokenKind;
    use notespace_core::ratelimit::Limit;
    use notespace_render::account::{ForgotError, ResetError};

    /// Peppers and scheme, or the response explaining why there are none.
    fn recovery_config(env: &Env) -> Result<RecoveryConfig, Response> {
        match AuthConfig::resolve(env) {
            AuthConfig::Refused(why) => Err(error(StatusCode::SERVICE_UNAVAILABLE, why)),
            AuthConfig::External => Err(error(StatusCode::NOT_FOUND, "password login is disabled")),
            AuthConfig::Passwords { peppers, scheme } => Ok(RecoveryConfig {
                scheme,
                peppers,
                per_client: Limit {
                    max: 10,
                    window_ms: 60 * 60_000,
                },
                per_address: Limit {
                    max: 3,
                    window_ms: 60 * 60_000,
                },
            }),
        }
    }

    fn fresh_salt() -> Result<String, Response> {
        let bytes = ids::random_hex().map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
        notespace_core::password::encode_salt(&bytes.as_bytes()[..16])
            .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
    }

    #[worker::send]
    pub async fn change_password(
        State(env): State<Env>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> Response {
        let cfg = match recovery_config(&env) {
            Ok(c) => c,
            Err(r) => return r,
        };
        let signed = match signed_in(&env, &headers, "/settings").await {
            Ok(s) => s,
            Err(r) => return r,
        };
        let fields = form_fields(&body);
        let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
        if !signed.verify(field("csrf")) {
            return see_other("/settings?error=expired".into());
        }
        let salt = match fresh_salt() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let mailer = mail::MaybeMailer::from_env(&env);
        let link_cfg = mail::LinkConfig::resolve(&env, host(&headers));
        match account::change_password(
            &signed.store,
            &mailer,
            &link_cfg.links(),
            &cfg,
            PasswordChange {
                user: &signed.user,
                current: field("current"),
                new_password: field("new"),
                salt: &salt,
                keep: Some(&signed.auth),
                now: now_ms(),
            },
        )
        .await
        {
            Ok(ChangeOutcome::Changed) => see_other("/settings?did=password".into()),
            Ok(ChangeOutcome::WrongPassword) => see_other("/settings?error=wrong".into()),
            Ok(ChangeOutcome::ShortPassword { min }) => {
                see_other(format!("/settings?error=short-{min}"))
            }
            Ok(ChangeOutcome::NoPassword) => see_other("/settings?error=nopassword".into()),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }

    #[derive(Deserialize, Default)]
    pub struct ForgotQuery {
        error: Option<String>,
        sent: Option<String>,
    }

    #[worker::send]
    pub async fn forgot_form(
        State(env): State<Env>,
        headers: axum::http::HeaderMap,
        Query(q): Query<ForgotQuery>,
    ) -> Response {
        if let Err(r) = recovery_config(&env) {
            return r;
        }
        let Some(key) = csrf_key(&env) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "CSRF_KEY is not configured",
            );
        };
        let (csrf, set_anon) = match anon_token(&key, &headers) {
            Ok(t) => t,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        let err = q.error.as_deref().and_then(|e| match e {
            "expired" => Some(ForgotError::Expired),
            other => {
                if let Some(why) = other.strip_prefix("email-") {
                    return Some(ForgotError::BadAddress(why.to_string()));
                }
                other
                    .strip_prefix("wait-")
                    .and_then(|s| s.parse().ok())
                    .map(|retry_after_secs| ForgotError::RateLimited { retry_after_secs })
            }
        });
        anon_page(
            notespace_render::account::forgot_page(&csrf, err, q.sent.is_some()),
            set_anon,
        )
    }

    /// Redirects to the same acknowledgement whether or not the address was known.
    #[worker::send]
    pub async fn forgot_submit(
        State(env): State<Env>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> Response {
        let cfg = match recovery_config(&env) {
            Ok(c) => c,
            Err(r) => return r,
        };
        let Some(key) = csrf_key(&env) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "CSRF_KEY is not configured",
            );
        };
        let fields = form_fields(&body);
        let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
        if key
            .verify(field("csrf"), &anon_binding(&headers), now_ms())
            .is_err()
        {
            return see_other("/forgot?error=expired".into());
        }
        let Ok(db) = env.d1(crate::DB_BINDING) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
        };
        let store = crate::store::D1Store::new(db);
        let token = match ids::random_email_token() {
            Ok(t) => t,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        let mailer = mail::MaybeMailer::from_env(&env);
        let link_cfg = mail::LinkConfig::resolve(&env, host(&headers));
        match account::request_reset(
            &store,
            &mailer,
            &link_cfg.links(),
            &cfg,
            ResetRequest {
                email: field("email"),
                client: &client_address(&headers),
                token,
                now: now_ms(),
            },
        )
        .await
        {
            Ok(RequestOutcome::Accepted { delivery }) => {
                // The visitor learns nothing from this; the operator does.
                match delivery {
                    Some(Delivery::Failed(why)) => {
                        worker::console_log!("mail: reset failed: {why}")
                    }
                    Some(Delivery::NotConfigured) => {
                        worker::console_log!(
                            "mail: a reset was requested but no mailer is configured"
                        )
                    }
                    _ => {}
                }
                see_other("/forgot?sent=1".into())
            }
            Ok(RequestOutcome::BadAddress(why)) => see_other(format!(
                "/forgot?error=email-{}",
                urlencoding(&why.to_string())
            )),
            Ok(RequestOutcome::RateLimited { retry_after_secs }) => {
                see_other(format!("/forgot?error=wait-{retry_after_secs}"))
            }
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }

    #[worker::send]
    pub async fn reset_form(
        State(env): State<Env>,
        headers: axum::http::HeaderMap,
        Query(q): Query<TokenQuery>,
    ) -> Response {
        if let Err(r) = recovery_config(&env) {
            return r;
        }
        let Some(key) = csrf_key(&env) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "CSRF_KEY is not configured",
            );
        };
        let raw = q.token.unwrap_or_default();
        let Some(token) = EmailToken::parse(&raw) else {
            return uncached_html(notespace_render::account::reset_page(
                "",
                "",
                None,
                Some(ResetError::Invalid),
            ));
        };
        // Say whose password this is: the mail's address does not.
        let Ok(db) = env.d1(crate::DB_BINDING) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
        };
        let store = crate::store::D1Store::new(db);
        let username = match store
            .peek_email_token(&token.hash(), TokenKind::Reset, now_ms())
            .await
        {
            Ok(Some(name)) => name,
            Ok(None) => {
                return uncached_html(notespace_render::account::reset_page(
                    "",
                    "",
                    None,
                    Some(ResetError::Invalid),
                ))
            }
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        let (csrf, set_anon) = match anon_token(&key, &headers) {
            Ok(t) => t,
            Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
        };
        let err = q.error.as_deref().and_then(|e| match e {
            "invalid" => Some(ResetError::Invalid),
            "expired" => Some(ResetError::Expired),
            other => other
                .strip_prefix("short-")
                .and_then(|n| n.parse().ok())
                .map(|min| ResetError::ShortPassword { min }),
        });
        anon_page(
            notespace_render::account::reset_page(&csrf, &raw, Some(&username), err),
            set_anon,
        )
    }

    #[worker::send]
    pub async fn reset_submit(
        State(env): State<Env>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> Response {
        let cfg = match recovery_config(&env) {
            Ok(c) => c,
            Err(r) => return r,
        };
        let Some(key) = csrf_key(&env) else {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "CSRF_KEY is not configured",
            );
        };
        let fields = form_fields(&body);
        let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
        let raw = field("token");
        let back = |e: &str| see_other(format!("/reset?token={}&error={e}", urlencoding(raw)));
        if key
            .verify(field("csrf"), &anon_binding(&headers), now_ms())
            .is_err()
        {
            return back("expired");
        }
        let Some(token) = EmailToken::parse(raw) else {
            return back("invalid");
        };
        let Ok(db) = env.d1(crate::DB_BINDING) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "no D1 binding");
        };
        let store = crate::store::D1Store::new(db);
        let salt = match fresh_salt() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let mailer = mail::MaybeMailer::from_env(&env);
        let link_cfg = mail::LinkConfig::resolve(&env, host(&headers));
        match account::complete_reset(
            &store,
            &mailer,
            &link_cfg.links(),
            &cfg,
            ResetCompletion {
                token: &token,
                new_password: field("password"),
                salt: &salt,
                now: now_ms(),
            },
        )
        .await
        {
            Ok(ResetOutcome::Done { user }) => see_other(format!(
                "/login?done=reset&name={}",
                urlencoding(&user.name)
            )),
            Ok(ResetOutcome::Invalid) => back("invalid"),
            Ok(ResetOutcome::ShortPassword { min }) => back(&format!("short-{min}")),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
}

/// Password recovery compiled out: the endpoints do not exist.
#[cfg(not(feature = "password"))]
pub async fn change_password() -> Response {
    external()
}
#[cfg(not(feature = "password"))]
pub async fn forgot_form() -> Response {
    external()
}
#[cfg(not(feature = "password"))]
pub async fn forgot_submit() -> Response {
    external()
}
#[cfg(not(feature = "password"))]
pub async fn reset_form() -> Response {
    external()
}
#[cfg(not(feature = "password"))]
pub async fn reset_submit() -> Response {
    external()
}
#[cfg(not(feature = "password"))]
fn external() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "password login is disabled on this instance",
    )
}
