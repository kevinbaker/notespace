//! Signing in through someone else's identity provider: OpenID Connect where the provider
//! speaks it (Google), plain OAuth 2.0 plus a user-info call where it does not (GitHub).
//!
//! Nothing here does I/O or cryptography. The provider table says what to send and how to read
//! what comes back; the Worker fetches, and verifies an ID token's signature through
//! `crypto.subtle` behind [`SignatureCheck`]. The claim checks -- issuer, audience, expiry,
//! nonce -- are here, because they are the part that is easy to get wrong and cheap to test.
//!
//! The flow, per login:
//!
//! ```text
//!   GET /auth/{p}            -> redirect to the provider, state+nonce sealed in a cookie
//!   GET /auth/{p}/callback   -> code for tokens; identity out of the ID token or user-info
//!                               known identity: session, done
//!                               new identity:   sealed in a cookie, ask for a username once
//!   POST /auth/finish        -> create the account, link the identity, session
//! ```

use serde::{Deserialize, Serialize};

use crate::encoding::base64url_decode;
use crate::model::{Timestamp, User, UserState};
use crate::session::{Session, SessionPolicy, SessionToken};
use crate::store::{Store, StoreError, StoreResult};
use crate::username::Username;

/// Which provider. Apple and Facebook are the next two; Apple needs a minted client secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Google,
    GitHub,
}

impl ProviderKind {
    /// The name in URLs and in the identity table.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::Google => "google",
            ProviderKind::GitHub => "github",
        }
    }

    pub fn parse(s: &str) -> Option<ProviderKind> {
        match s {
            "google" => Some(ProviderKind::Google),
            "github" => Some(ProviderKind::GitHub),
            _ => None,
        }
    }

    /// What the button says.
    pub const fn label(&self) -> &'static str {
        match self {
            ProviderKind::Google => "Google",
            ProviderKind::GitHub => "GitHub",
        }
    }

    pub const ALL: [ProviderKind; 2] = [ProviderKind::Google, ProviderKind::GitHub];
}

/// A configured provider.
#[derive(Debug, Clone)]
pub struct Provider {
    pub kind: ProviderKind,
    pub client_id: String,
    pub client_secret: String,
    /// `https://host/auth/{kind}/callback`, and it must match what the provider has on file.
    pub redirect_uri: String,
}

/// An HTTP request the Worker makes on the flow's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    /// `application/x-www-form-urlencoded`, when there is one.
    pub form: Vec<(String, String)>,
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Provider {
    /// Where to send the visitor. `state` ties the callback to this visit; `nonce` ties the
    /// ID token to it (Google only; GitHub has no ID token to put it in).
    pub fn authorize_url(&self, state: &str, nonce: &str) -> String {
        match self.kind {
            ProviderKind::Google => format!(
                "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id={}&redirect_uri={}&scope=openid%20email%20profile&state={}&nonce={}&prompt=select_account",
                encode(&self.client_id),
                encode(&self.redirect_uri),
                encode(state),
                encode(nonce),
            ),
            ProviderKind::GitHub => format!(
                "https://github.com/login/oauth/authorize?client_id={}&redirect_uri={}&scope=read%3Auser%20user%3Aemail&state={}",
                encode(&self.client_id),
                encode(&self.redirect_uri),
                encode(state),
            ),
        }
    }

    /// The code-for-token exchange.
    pub fn token_request(&self, code: &str) -> OutboundRequest {
        let url = match self.kind {
            ProviderKind::Google => "https://oauth2.googleapis.com/token",
            ProviderKind::GitHub => "https://github.com/login/oauth/access_token",
        };
        OutboundRequest {
            method: "POST",
            url: url.into(),
            headers: vec![("Accept", "application/json".into())],
            form: vec![
                ("grant_type".into(), "authorization_code".into()),
                ("code".into(), code.into()),
                ("client_id".into(), self.client_id.clone()),
                ("client_secret".into(), self.client_secret.clone()),
                ("redirect_uri".into(), self.redirect_uri.clone()),
            ],
        }
    }

    /// What the token endpoint said. Google returns an `id_token`; GitHub an `access_token`
    /// for the user-info calls.
    pub fn parse_token_response(&self, status: u16, body: &str) -> Result<Tokens, OidcError> {
        let v: serde_json::Value = serde_json::from_str(body)
            .map_err(|_| OidcError::Provider("token response is not JSON".into()))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            let desc = v
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            return Err(OidcError::Provider(
                format!("{err} {desc}").trim().to_string(),
            ));
        }
        if !(200..300).contains(&status) {
            return Err(OidcError::Provider(format!(
                "token endpoint answered {status}"
            )));
        }
        let tokens = Tokens {
            id_token: v
                .get("id_token")
                .and_then(|t| t.as_str())
                .map(str::to_string),
            access_token: v
                .get("access_token")
                .and_then(|t| t.as_str())
                .map(str::to_string),
        };
        match self.kind {
            ProviderKind::Google if tokens.id_token.is_none() => {
                Err(OidcError::Provider("no id_token from Google".into()))
            }
            ProviderKind::GitHub if tokens.access_token.is_none() => {
                Err(OidcError::Provider("no access_token from GitHub".into()))
            }
            _ => Ok(tokens),
        }
    }

    /// Where an ID token's signing keys are published.
    pub fn jwks_url(&self) -> Option<&'static str> {
        match self.kind {
            ProviderKind::Google => Some("https://www.googleapis.com/oauth2/v3/certs"),
            ProviderKind::GitHub => None,
        }
    }

    /// The user-info calls, for a provider without an ID token. Two for GitHub: the profile,
    /// and the email list, because the profile's `email` is only the public one.
    pub fn userinfo_requests(&self, access_token: &str) -> Vec<OutboundRequest> {
        match self.kind {
            ProviderKind::Google => Vec::new(),
            ProviderKind::GitHub => [
                "https://api.github.com/user",
                "https://api.github.com/user/emails",
            ]
            .into_iter()
            .map(|url| OutboundRequest {
                method: "GET",
                url: url.into(),
                headers: vec![
                    ("Authorization", format!("Bearer {access_token}")),
                    ("Accept", "application/vnd.github+json".into()),
                    // GitHub refuses requests without one.
                    ("User-Agent", "notespace".into()),
                ],
                form: Vec::new(),
            })
            .collect(),
        }
    }

    /// GitHub's two responses into an identity. The verified primary address wins; an
    /// unverified one is not an address we would send a reset to.
    pub fn parse_github(user_json: &str, emails_json: &str) -> Result<RemoteIdentity, OidcError> {
        let user: serde_json::Value = serde_json::from_str(user_json)
            .map_err(|_| OidcError::Provider("GitHub user is not JSON".into()))?;
        let id = user
            .get("id")
            .and_then(|i| i.as_i64())
            .ok_or_else(|| OidcError::Provider("GitHub user has no id".into()))?;
        let login = user
            .get("login")
            .and_then(|l| l.as_str())
            .map(str::to_string);
        let name = user
            .get("name")
            .and_then(|l| l.as_str())
            .map(str::to_string);
        let emails: Vec<serde_json::Value> = serde_json::from_str(emails_json).unwrap_or_default();
        let verified_primary = emails
            .iter()
            .find(|e| e["primary"].as_bool() == Some(true) && e["verified"].as_bool() == Some(true))
            .or_else(|| {
                emails
                    .iter()
                    .find(|e| e["verified"].as_bool() == Some(true))
            })
            .and_then(|e| e["email"].as_str())
            .map(str::to_string);
        Ok(RemoteIdentity {
            provider: ProviderKind::GitHub,
            subject: id.to_string(),
            email: verified_primary.clone(),
            email_verified: verified_primary.is_some(),
            suggested_name: login.or(name),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub id_token: Option<String>,
    pub access_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcError {
    #[error("the provider refused: {0}")]
    Provider(String),
    #[error("the ID token is malformed")]
    Malformed,
    #[error("the ID token's {0} does not check out")]
    Claim(&'static str),
    #[error("no signing key {0} in the provider's key set")]
    NoKey(String),
    #[error("the ID token's signature is bad")]
    Signature,
}

/// Who the provider says the visitor is. Sealed into a cookie between the callback and the
/// username step, so nothing here may be secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIdentity {
    pub provider: ProviderKind,
    /// The provider's stable id for the account -- never an email, which can change hands.
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    /// Something to prefill the username field with.
    pub suggested_name: Option<String>,
}

impl RemoteIdentity {
    /// A username out of the suggestion, if the rules accept one.
    pub fn suggested_username(&self) -> String {
        let raw = self
            .suggested_name
            .as_deref()
            .or_else(|| self.email.as_deref().and_then(|e| e.split('@').next()))
            .unwrap_or("");
        let cleaned: String = raw
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let cleaned: String = cleaned.chars().take(crate::username::MAX_CHARS).collect();
        let cleaned = cleaned.trim_end_matches('-').to_string();
        if Username::parse(&cleaned).is_ok() {
            cleaned
        } else {
            String::new()
        }
    }
}

// ---------------------------------------------------------------------------
// ID tokens
// ---------------------------------------------------------------------------

/// A JWT split into what the signature covers and the signature.
#[derive(Debug, Clone, PartialEq)]
pub struct IdToken {
    pub kid: Option<String>,
    pub alg: String,
    pub claims: Claims,
    /// `<header>.<payload>`, the bytes the signature is over.
    pub signing_input: String,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    #[serde(default)]
    pub aud: Audience,
    pub exp: i64,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
}

/// `aud` is a string or an array of them.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(untagged)]
pub enum Audience {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, id: &str) -> bool {
        match self {
            Audience::None => false,
            Audience::One(a) => a == id,
            Audience::Many(v) => v.iter().any(|a| a == id),
        }
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// Split and decode, without verifying anything.
pub fn parse_id_token(jwt: &str) -> Result<IdToken, OidcError> {
    let mut parts = jwt.split('.');
    let (Some(h), Some(p), Some(s), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(OidcError::Malformed);
    };
    let header: Header = serde_json::from_slice(&base64url_decode(h).ok_or(OidcError::Malformed)?)
        .map_err(|_| OidcError::Malformed)?;
    let claims: Claims = serde_json::from_slice(&base64url_decode(p).ok_or(OidcError::Malformed)?)
        .map_err(|_| OidcError::Malformed)?;
    Ok(IdToken {
        kid: header.kid,
        alg: header.alg,
        claims,
        signing_input: format!("{h}.{p}"),
        signature: base64url_decode(s).ok_or(OidcError::Malformed)?,
    })
}

/// Google's issuer appears in both spellings.
const GOOGLE_ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// The claim checks. `now` is unix seconds here, because `exp` is.
pub fn check_claims(
    token: &IdToken,
    provider: &Provider,
    nonce: &str,
    now_secs: i64,
) -> Result<(), OidcError> {
    if token.alg != "RS256" {
        return Err(OidcError::Claim("alg"));
    }
    let issuer_ok = match provider.kind {
        ProviderKind::Google => GOOGLE_ISSUERS.contains(&token.claims.iss.as_str()),
        ProviderKind::GitHub => false,
    };
    if !issuer_ok {
        return Err(OidcError::Claim("iss"));
    }
    if !token.claims.aud.contains(&provider.client_id) {
        return Err(OidcError::Claim("aud"));
    }
    // A minute of skew: the provider's clock and ours are not the same clock.
    if token.claims.exp + 60 < now_secs {
        return Err(OidcError::Claim("exp"));
    }
    if token.claims.nonce.as_deref() != Some(nonce) {
        return Err(OidcError::Claim("nonce"));
    }
    if token.claims.sub.is_empty() {
        return Err(OidcError::Claim("sub"));
    }
    Ok(())
}

/// An RSA public key as the JWKS publishes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Jwk {
    pub kty: String,
    #[serde(default)]
    pub kid: Option<String>,
    #[serde(default)]
    pub alg: Option<String>,
    pub n: String,
    pub e: String,
}

/// The key the token names, out of the provider's set.
pub fn select_jwk(jwks_json: &str, kid: Option<&str>) -> Result<Jwk, OidcError> {
    #[derive(Deserialize)]
    struct Jwks {
        keys: Vec<Jwk>,
    }
    let set: Jwks = serde_json::from_str(jwks_json)
        .map_err(|_| OidcError::Provider("JWKS is not JSON".into()))?;
    set.keys
        .into_iter()
        .filter(|k| k.kty == "RSA")
        .find(|k| kid.is_none() || k.kid.as_deref() == kid)
        .ok_or_else(|| OidcError::NoKey(kid.unwrap_or("(none)").to_string()))
}

/// The one thing `core` cannot do: check an RSA signature. `?Send` for wasm.
#[async_trait::async_trait(?Send)]
pub trait SignatureCheck {
    async fn verify_rs256(
        &self,
        key: &Jwk,
        signing_input: &[u8],
        signature: &[u8],
    ) -> Result<bool, String>;
}

/// The identity out of a verified Google ID token.
pub fn identity_from_claims(kind: ProviderKind, c: &Claims) -> RemoteIdentity {
    RemoteIdentity {
        provider: kind,
        subject: c.sub.clone(),
        email: c.email.clone(),
        email_verified: c.email_verified.unwrap_or(false) && c.email.is_some(),
        suggested_name: c.name.clone(),
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// What the callback needs to seal for the username step. Short-lived; its cookie is the
/// only place it lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub identity: RemoteIdentity,
    pub next: Option<String>,
}

pub enum SignIn {
    /// A known identity: the session is created.
    Done { session: Session, user: User },
    /// Never seen: ask for a username, then [`finish`].
    NeedsAccount,
    /// Known, but the account cannot sign in.
    Refused,
}

/// **Budget: 2-3 statements.**
pub async fn sign_in<S: Store>(
    store: &S,
    identity: &RemoteIdentity,
    token: SessionToken,
    sessions: &SessionPolicy,
    now: Timestamp,
) -> StoreResult<SignIn> {
    let Some(user) = store
        .identity_user(identity.provider.as_str(), &identity.subject)
        .await?
    else {
        return Ok(SignIn::NeedsAccount);
    };
    if !user.state.can_act() {
        return Ok(SignIn::Refused);
    }
    store
        .touch_identity(identity.provider.as_str(), &identity.subject, now)
        .await?;
    let session = Session {
        token_hash: token.hash(),
        user_id: user.id,
        created_at: now,
        refreshed_at: now,
        expires_at: sessions.expiry_from(now),
    };
    store.create_session(&session).await?;
    Ok(SignIn::Done { session, user })
}

#[derive(Debug, PartialEq, Eq)]
pub enum FinishRejected {
    BadName(String),
    Taken,
}

pub enum Finish {
    Created { session: Session, user: User },
    Rejected(FinishRejected),
}

/// The username step: an account with no password, the identity linked to it, the provider's
/// verified address on file as verified, and a session. **Budget: 4-5 statements.**
pub async fn finish<S: Store>(
    store: &S,
    identity: &RemoteIdentity,
    username: &str,
    token: SessionToken,
    sessions: &SessionPolicy,
    now: Timestamp,
) -> StoreResult<Finish> {
    let name = match Username::parse(username) {
        Ok(n) => n,
        Err(e) => return Ok(Finish::Rejected(FinishRejected::BadName(e.to_string()))),
    };
    // Someone else may have linked this identity between the callback and now.
    if store
        .identity_user(identity.provider.as_str(), &identity.subject)
        .await?
        .is_some()
    {
        return Ok(Finish::Rejected(FinishRejected::Taken));
    }
    let user_id = match store.create_user(name.as_str(), now, None).await {
        Ok(id) => id,
        Err(StoreError::Conflict) => return Ok(Finish::Rejected(FinishRejected::Taken)),
        Err(e) => return Err(e),
    };
    store
        .link_identity(
            identity.provider.as_str(),
            &identity.subject,
            user_id,
            identity.email.as_deref(),
            now,
        )
        .await?;
    if let Some(email) = identity.email.as_deref() {
        store
            .set_email(user_id, Some(&email.to_lowercase()))
            .await?;
        if identity.email_verified {
            // Another account may have verified it first; then it stays unverified here.
            let _ = store
                .mark_email_verified(user_id, &email.to_lowercase(), now)
                .await;
        }
    }
    let session = Session {
        token_hash: token.hash(),
        user_id,
        created_at: now,
        refreshed_at: now,
        expires_at: sessions.expiry_from(now),
    };
    store.create_session(&session).await?;
    Ok(Finish::Created {
        session,
        user: User {
            id: user_id,
            name: name.as_str().to_string(),
            state: UserState::Active,
            role: crate::model::Role::Member,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::base64url;

    fn google() -> Provider {
        Provider {
            kind: ProviderKind::Google,
            client_id: "cid.apps.googleusercontent.com".into(),
            client_secret: "sekrit".into(),
            redirect_uri: "https://forum.example/auth/google/callback".into(),
        }
    }

    fn github() -> Provider {
        Provider {
            kind: ProviderKind::GitHub,
            client_id: "Iv1.abc".into(),
            client_secret: "sekrit".into(),
            redirect_uri: "https://forum.example/auth/github/callback".into(),
        }
    }

    #[test]
    fn authorize_urls_carry_state_and_encode_the_redirect() {
        let u = google().authorize_url("st&ate", "n0nce");
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        assert!(u.contains("redirect_uri=https%3A%2F%2Fforum.example%2Fauth%2Fgoogle%2Fcallback"));
        assert!(u.contains("state=st%26ate"));
        assert!(u.contains("nonce=n0nce"));
        assert!(u.contains("scope=openid%20email%20profile"));
        let g = github().authorize_url("s", "ignored");
        assert!(g.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(
            !g.contains("nonce"),
            "GitHub has no ID token to carry a nonce"
        );
        assert!(g.contains("user%3Aemail"));
    }

    #[test]
    fn the_token_exchange_is_a_form_post_with_the_secret() {
        let r = google().token_request("c0de");
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://oauth2.googleapis.com/token");
        assert!(r.form.contains(&("code".into(), "c0de".into())));
        assert!(r.form.contains(&("client_secret".into(), "sekrit".into())));
        assert!(r
            .form
            .contains(&("grant_type".into(), "authorization_code".into())));
        assert!(
            r.headers.contains(&("Accept", "application/json".into())),
            "GitHub answers form-encoded otherwise"
        );
    }

    #[test]
    fn token_responses_are_read_per_provider_and_errors_surface() {
        let t = google()
            .parse_token_response(200, r#"{"access_token":"a","id_token":"x.y.z"}"#)
            .unwrap();
        assert_eq!(t.id_token.as_deref(), Some("x.y.z"));
        assert!(google()
            .parse_token_response(200, r#"{"access_token":"a"}"#)
            .is_err());
        assert!(github()
            .parse_token_response(200, r#"{"access_token":"gho_1"}"#)
            .is_ok());
        match github().parse_token_response(200, r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#) {
            Err(OidcError::Provider(m)) => assert!(m.contains("bad_verification_code")),
            other => panic!("{other:?}"),
        }
        assert!(google().parse_token_response(500, "{}").is_err());
    }

    #[test]
    fn github_identity_prefers_the_verified_primary_address() {
        let id = Provider::parse_github(
            r#"{"id":123,"login":"octocat","name":"The Octocat","email":null}"#,
            r#"[{"email":"old@example.com","primary":false,"verified":true},{"email":"me@example.com","primary":true,"verified":true}]"#,
        )
        .unwrap();
        assert_eq!(id.subject, "123");
        assert_eq!(id.email.as_deref(), Some("me@example.com"));
        assert!(id.email_verified);
        assert_eq!(id.suggested_name.as_deref(), Some("octocat"));
        let none = Provider::parse_github(r#"{"id":5,"login":"x"}"#, "[]").unwrap();
        assert_eq!(none.email, None);
        assert!(!none.email_verified);
    }

    fn jwt(header: &str, claims: &str) -> String {
        format!(
            "{}.{}.{}",
            base64url(header.as_bytes()),
            base64url(claims.as_bytes()),
            base64url(b"sig")
        )
    }

    #[test]
    fn id_tokens_parse_and_every_claim_is_checked() {
        let now = 1_800_000_000;
        let good = jwt(
            r#"{"alg":"RS256","kid":"k1"}"#,
            &format!(
                r#"{{"iss":"https://accounts.google.com","sub":"42","aud":"cid.apps.googleusercontent.com","exp":{},"nonce":"n","email":"a@example.com","email_verified":true,"name":"A"}}"#,
                now + 300
            ),
        );
        let t = parse_id_token(&good).unwrap();
        assert_eq!(t.kid.as_deref(), Some("k1"));
        assert_eq!(t.signature, b"sig");
        assert_eq!(t.signing_input, good[..good.rfind('.').unwrap()]);
        assert_eq!(check_claims(&t, &google(), "n", now), Ok(()));
        let id = identity_from_claims(ProviderKind::Google, &t.claims);
        assert_eq!(id.subject, "42");
        assert!(id.email_verified);

        let cases = [
            (
                r#"{"alg":"HS256"}"#,
                r#"{"iss":"https://accounts.google.com","sub":"42","aud":"cid.apps.googleusercontent.com","exp":1800000300,"nonce":"n"}"#,
                "alg",
            ),
            (
                r#"{"alg":"RS256"}"#,
                r#"{"iss":"https://evil.example","sub":"42","aud":"cid.apps.googleusercontent.com","exp":1800000300,"nonce":"n"}"#,
                "iss",
            ),
            (
                r#"{"alg":"RS256"}"#,
                r#"{"iss":"accounts.google.com","sub":"42","aud":"other","exp":1800000300,"nonce":"n"}"#,
                "aud",
            ),
            (
                r#"{"alg":"RS256"}"#,
                r#"{"iss":"accounts.google.com","sub":"42","aud":["x","cid.apps.googleusercontent.com"],"exp":1700000000,"nonce":"n"}"#,
                "exp",
            ),
            (
                r#"{"alg":"RS256"}"#,
                r#"{"iss":"accounts.google.com","sub":"42","aud":"cid.apps.googleusercontent.com","exp":1800000300,"nonce":"other"}"#,
                "nonce",
            ),
            (
                r#"{"alg":"RS256"}"#,
                r#"{"iss":"accounts.google.com","sub":"","aud":"cid.apps.googleusercontent.com","exp":1800000300,"nonce":"n"}"#,
                "sub",
            ),
        ];
        for (h, c, which) in cases {
            let t = parse_id_token(&jwt(h, c)).unwrap();
            assert_eq!(
                check_claims(&t, &google(), "n", now),
                Err(OidcError::Claim(which)),
                "{which}"
            );
        }
        // An array audience that includes us passes.
        let t = parse_id_token(&jwt(r#"{"alg":"RS256"}"#, r#"{"iss":"accounts.google.com","sub":"1","aud":["x","cid.apps.googleusercontent.com"],"exp":1800000300,"nonce":"n"}"#)).unwrap();
        assert_eq!(check_claims(&t, &google(), "n", now), Ok(()));
        for bad in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
            assert!(
                matches!(parse_id_token(bad), Err(OidcError::Malformed)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_named_key_is_selected_from_the_set() {
        let jwks = r#"{"keys":[{"kty":"RSA","kid":"a","n":"1","e":"AQAB"},{"kty":"EC","kid":"b","n":"","e":""},{"kty":"RSA","kid":"c","n":"2","e":"AQAB","alg":"RS256"}]}"#;
        assert_eq!(select_jwk(jwks, Some("c")).unwrap().n, "2");
        assert_eq!(select_jwk(jwks, None).unwrap().kid.as_deref(), Some("a"));
        assert_eq!(
            select_jwk(jwks, Some("zz")),
            Err(OidcError::NoKey("zz".into()))
        );
        assert!(
            matches!(select_jwk(jwks, Some("b")), Err(OidcError::NoKey(_))),
            "an EC key is not an RS256 key"
        );
    }

    #[test]
    fn usernames_are_suggested_within_the_rules() {
        let id = |name: Option<&str>, email: Option<&str>| RemoteIdentity {
            provider: ProviderKind::Google,
            subject: "1".into(),
            email: email.map(str::to_string),
            email_verified: true,
            suggested_name: name.map(str::to_string),
        };
        assert_eq!(
            id(Some("The Octocat"), None).suggested_username(),
            "the-octocat"
        );
        assert_eq!(
            id(None, Some("Jane.Doe+x@example.com")).suggested_username(),
            "jane-doe-x"
        );
        assert_eq!(
            id(Some("admin"), None).suggested_username(),
            "",
            "reserved names are not suggested"
        );
        assert_eq!(id(Some("!!"), None).suggested_username(), "");
        assert!(
            id(Some(&"a".repeat(60)), None).suggested_username().len()
                <= crate::username::MAX_CHARS
        );
    }
}
