//! Signing in through a provider. The protocol and its checks are `core::oidc`; this file
//! reads the configuration, makes the HTTP calls, and moves the visitor between the steps
//! with two sealed cookies: `OAUTH` (state and nonce, redirect to callback) and `PENDING`
//! (the identity, callback to the username step).

use crate::platform::{HttpRequest, Platform};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use notespace_app_macros::handler;
use notespace_core::cookie;
use notespace_core::oidc::{
    self, Finish, FinishRejected, OutboundRequest, Pending, Provider, ProviderKind, RemoteIdentity,
    SignIn,
};
use notespace_core::session::SessionPolicy;
use notespace_render::auth::{FinishError, ProviderButton};
use serde::{Deserialize, Serialize};

use crate::{
    anon_binding, anon_page, anon_token, cookie_header, csrf_key, error, form_fields, host, ids,
    mail, now_ms, see_other, uncached_html, urlencoding,
};

/// Client ids are `OIDC_<PROVIDER>_CLIENT_ID` vars; secrets `OIDC_<PROVIDER>_CLIENT_SECRET`.
/// A provider with both is offered; with neither, absent; with one, logged and absent.
pub fn client_id_var(kind: ProviderKind) -> String {
    format!("OIDC_{}_CLIENT_ID", kind.as_str().to_uppercase())
}
pub fn client_secret_var(kind: ProviderKind) -> String {
    format!("OIDC_{}_CLIENT_SECRET", kind.as_str().to_uppercase())
}

/// How long the visitor has between leaving for the provider and coming back.
const STATE_LIFETIME_MS: i64 = 10 * 60 * 1000;
/// How long the username step may take.
const PENDING_LIFETIME_MS: i64 = 30 * 60 * 1000;

/// Every provider the deployment configured, with callbacks on this host.
pub fn providers<P: Platform>(env: &P, host: Option<&str>) -> Vec<Provider> {
    let base = mail::LinkConfig::resolve(env, host);
    ProviderKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let id = env.var(&client_id_var(kind));
            let secret = env.secret(&client_secret_var(kind));
            match (id, secret) {
                (Some(client_id), Some(client_secret)) => Some(Provider {
                    kind,
                    client_id,
                    client_secret,
                    redirect_uri: format!(
                        "{}/auth/{}/callback",
                        base.links().base_url,
                        kind.as_str()
                    ),
                }),
                (None, None) => None,
                _ => {
                    crate::log(&format!(
                        "oidc: {} has a client id or a secret but not both; not offered",
                        kind.as_str()
                    ));
                    None
                }
            }
        })
        .collect()
}

/// The sign-in page's buttons.
pub fn buttons<P: Platform>(
    env: &P,
    host: Option<&str>,
    next: Option<&str>,
) -> Vec<ProviderButton<'static>> {
    providers(env, host)
        .into_iter()
        .map(|p| ProviderButton {
            label: p.kind.label(),
            href: match next {
                Some(n) => format!("/auth/{}?next={}", p.kind.as_str(), urlencoding(n)),
                None => format!("/auth/{}", p.kind.as_str()),
            },
        })
        .collect()
}

fn provider<P: Platform>(env: &P, host: Option<&str>, name: &str) -> Option<Provider> {
    let kind = ProviderKind::parse(name)?;
    providers(env, host).into_iter().find(|p| p.kind == kind)
}

/// What the `OAUTH` cookie seals.
#[derive(Serialize, Deserialize)]
struct Handshake {
    provider: String,
    state: String,
    nonce: String,
    next: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct StartQuery {
    next: Option<String>,
}

/// Off to the provider, with the state sealed in a cookie the callback checks against.
#[handler]
pub async fn start<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<StartQuery>,
) -> Response {
    let Some(p) = provider(&env, host(&headers), &name) else {
        return error(StatusCode::NOT_FOUND, "no such provider");
    };
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let (state, nonce) = match (ids::random_hex(), ids::random_hex()) {
        (Ok(s), Ok(n)) => (s, n),
        _ => return error(StatusCode::INTERNAL_SERVER_ERROR, "csprng"),
    };
    let handshake = Handshake {
        provider: p.kind.as_str().into(),
        state: state.clone(),
        nonce: nonce.clone(),
        next: q
            .next
            .as_deref()
            .and_then(cookie::safe_next)
            .map(str::to_string),
    };
    let sealed = key.seal(
        &serde_json::to_vec(&handshake).unwrap_or_default(),
        now_ms(),
        STATE_LIFETIME_MS,
    );
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, p.authorize_url(&state, &nonce)),
            (
                header::SET_COOKIE,
                cookie::set(cookie::OAUTH, &sealed, STATE_LIFETIME_MS / 1000),
            ),
        ],
    )
        .into_response()
}

#[derive(Deserialize, Default)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// One HTTP call on the flow's behalf: status and body back, the rest is `core`'s.
async fn send<P: Platform>(env: &P, req: &OutboundRequest) -> Result<(u16, String), String> {
    let mut out = if req.method == "POST" {
        let mut enc = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &req.form {
            enc.append_pair(k, v);
        }
        HttpRequest::post(
            req.url.clone(),
            "application/x-www-form-urlencoded",
            enc.finish(),
        )
    } else {
        HttpRequest::get(req.url.clone())
    };
    for (k, v) in &req.headers {
        out = out.header(k.to_string(), v.to_string());
    }
    let resp = env.http(out).await?;
    Ok((resp.status, resp.body))
}

/// The provider's assertion about the visitor, checked.
async fn identity<P: Platform>(
    env: &P,
    p: &Provider,
    code: &str,
    nonce: &str,
) -> Result<RemoteIdentity, String> {
    let (status, body) = send(env, &p.token_request(code)).await?;
    let tokens = p
        .parse_token_response(status, &body)
        .map_err(|e| e.to_string())?;
    match p.kind {
        ProviderKind::Google => {
            let jwt = tokens.id_token.ok_or("no id_token")?;
            let token = oidc::parse_id_token(&jwt).map_err(|e| e.to_string())?;
            oidc::check_claims(&token, p, nonce, now_ms() / 1000).map_err(|e| e.to_string())?;
            let jwks_url = p.jwks_url().ok_or("no JWKS url")?;
            let (_, jwks) = send(
                env,
                &OutboundRequest {
                    method: "GET",
                    url: jwks_url.into(),
                    headers: Vec::new(),
                    form: Vec::new(),
                },
            )
            .await?;
            let key = oidc::select_jwk(&jwks, token.kid.as_deref()).map_err(|e| e.to_string())?;
            let ok = env
                .verify_rs256(&key, token.signing_input.as_bytes(), &token.signature)
                .await?;
            if !ok {
                return Err(oidc::OidcError::Signature.to_string());
            }
            Ok(oidc::identity_from_claims(p.kind, &token.claims))
        }
        ProviderKind::GitHub => {
            let access = tokens.access_token.ok_or("no access_token")?;
            let requests = p.userinfo_requests(&access);
            let mut bodies = Vec::with_capacity(requests.len());
            for r in &requests {
                let (status, body) = send(env, r).await?;
                if !(200..300).contains(&status) {
                    return Err(format!("GitHub {} answered {status}", r.url));
                }
                bodies.push(body);
            }
            Provider::parse_github(
                &bodies[0],
                bodies.get(1).map(String::as_str).unwrap_or("[]"),
            )
            .map_err(|e| e.to_string())
        }
    }
}

/// Back from the provider. Every failure lands on the sign-in page with a reason, and the
/// reason is generic on purpose: what exactly failed is in the log.
#[handler]
pub async fn callback<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let fail = |why: &str| -> Response {
        crate::log(&format!("oidc: {name} sign-in failed: {why}"));
        let mut resp = see_other("/login?error=provider".into());
        if let Ok(v) = cookie::clear(cookie::OAUTH).parse() {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        resp
    };
    let Some(p) = provider(&env, host(&headers), &name) else {
        return error(StatusCode::NOT_FOUND, "no such provider");
    };
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    if let Some(e) = &q.error {
        // The visitor pressed cancel, or the provider refused. Not an attack.
        return fail(&format!(
            "{e} {}",
            q.error_description.as_deref().unwrap_or("")
        ));
    }
    let Some(sealed) = cookie::get(cookie_header(&headers), cookie::OAUTH) else {
        return fail("no handshake cookie");
    };
    let handshake: Handshake = match key
        .open(&sealed, now_ms())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(h) => h,
        None => return fail("handshake cookie invalid or expired"),
    };
    if handshake.provider != p.kind.as_str() || q.state.as_deref() != Some(handshake.state.as_str())
    {
        return fail("state mismatch");
    }
    let Some(code) = q.code.as_deref().filter(|c| !c.is_empty()) else {
        return fail("no code");
    };
    let identity = match identity(&env, &p, code, &handshake.nonce).await {
        Ok(i) => i,
        Err(why) => return fail(&why),
    };

    let store = env.store();
    let token = match ids::random_session_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let sessions = SessionPolicy::default();
    match oidc::sign_in(store, &identity, token.clone(), &sessions, now_ms()).await {
        Ok(SignIn::Done { .. }) => {
            let target = handshake.next.as_deref().unwrap_or("/");
            let mut resp = see_other(target.into());
            let h = resp.headers_mut();
            let [session, marker] =
                cookie::set_session(&token.to_cookie_value(), sessions.lifetime_ms / 1000);
            for c in [session, marker, cookie::clear(cookie::OAUTH)] {
                if let Ok(v) = c.parse() {
                    h.append(header::SET_COOKIE, v);
                }
            }
            resp
        }
        Ok(SignIn::NeedsAccount) => {
            let pending = Pending {
                identity,
                next: handshake.next,
            };
            let sealed = key.seal(
                &serde_json::to_vec(&pending).unwrap_or_default(),
                now_ms(),
                PENDING_LIFETIME_MS,
            );
            let mut resp = see_other("/auth/finish".into());
            let h = resp.headers_mut();
            for c in [
                cookie::set(cookie::PENDING, &sealed, PENDING_LIFETIME_MS / 1000),
                cookie::clear(cookie::OAUTH),
            ] {
                if let Ok(v) = c.parse() {
                    h.append(header::SET_COOKIE, v);
                }
            }
            resp
        }
        Ok(SignIn::Refused) => fail("account cannot sign in"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The sealed identity, if the visitor is mid-signup.
fn pending<P: Platform>(env: &P, headers: &axum::http::HeaderMap) -> Option<Pending> {
    let key = csrf_key(env)?;
    let sealed = cookie::get(cookie_header(headers), cookie::PENDING)?;
    let bytes = key.open(&sealed, now_ms()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Deserialize, Default)]
pub struct FinishQuery {
    error: Option<String>,
    name: Option<String>,
}

#[handler]
pub async fn finish_form<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    Query(q): Query<FinishQuery>,
) -> Response {
    let Some(p) = pending(&env, &headers) else {
        return see_other("/login?error=expired".into());
    };
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
        "taken" => Some(FinishError::Taken),
        "expired" => Some(FinishError::Expired),
        other => other
            .strip_prefix("name-")
            .map(|why| FinishError::BadName(why.to_string())),
    });
    let suggested = q
        .name
        .clone()
        .unwrap_or_else(|| p.identity.suggested_username());
    anon_page(
        notespace_render::auth::finish_page(
            &csrf,
            p.identity.provider.label(),
            &suggested,
            p.identity.email.as_deref(),
            err,
        ),
        set_anon,
    )
}

#[handler]
pub async fn finish_submit<P: Platform>(
    State(env): State<P>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    let Some(p) = pending(&env, &headers) else {
        return see_other("/login?error=expired".into());
    };
    let Some(key) = csrf_key(&env) else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "CSRF_KEY is not configured",
        );
    };
    let fields = form_fields(&body);
    let field = |k: &str| fields.get(k).map(String::as_str).unwrap_or("");
    let name = field("username").to_string();
    let back = |e: &str| {
        see_other(format!(
            "/auth/finish?name={}&error={e}",
            urlencoding(&name)
        ))
    };
    if key
        .verify(field("csrf"), &anon_binding(&headers), now_ms())
        .is_err()
    {
        return back("expired");
    }
    let store = env.store();
    let token = match ids::random_session_token() {
        Ok(t) => t,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let sessions = SessionPolicy::default();
    match oidc::finish(
        store,
        &p.identity,
        &name,
        token.clone(),
        &sessions,
        now_ms(),
    )
    .await
    {
        Ok(Finish::Created { .. }) => {
            let target = p.next.as_deref().unwrap_or("/");
            let mut resp = see_other(target.into());
            let h = resp.headers_mut();
            let [session, marker] =
                cookie::set_session(&token.to_cookie_value(), sessions.lifetime_ms / 1000);
            for c in [session, marker, cookie::clear(cookie::PENDING)] {
                if let Ok(v) = c.parse() {
                    h.append(header::SET_COOKIE, v);
                }
            }
            resp
        }
        Ok(Finish::Rejected(FinishRejected::Taken)) => back("taken"),
        Ok(Finish::Rejected(FinishRejected::BadName(why))) => {
            back(&format!("name-{}", urlencoding(&why)))
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The page a build without password login serves at `/login` and `/register`: the provider
/// buttons alone.
#[cfg_attr(feature = "password", allow(dead_code))]
pub fn providers_only_page<P: Platform>(
    env: &P,
    headers: &axum::http::HeaderMap,
    next: Option<&str>,
    error: Option<notespace_render::auth::LoginError>,
) -> Response {
    let buttons = buttons(env, host(headers), next);
    uncached_html(notespace_render::auth::login_page(
        "",
        next,
        error,
        None,
        &notespace_render::auth::SignInOptions {
            password_form: false,
            providers: &buttons,
        },
    ))
}
