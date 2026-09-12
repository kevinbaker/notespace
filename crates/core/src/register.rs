//! Creating an account.
//!
//! This is an enumeration oracle by design — a signup form that will not say "that name is
//! taken" is unusable — which is why the limit is per client rather than per name.
//!
//! The taken-check and the insert are not atomic. `UNIQUE(user.name)` is the guard; the check
//! only avoids spending a hash on a doomed insert.

use crate::account::{self, Delivery};
use crate::email::{EmailAddress, EmailError, EmailToken, Links, Mailer};
use crate::model::{Timestamp, User, UserState};
use crate::password::{self, PepperSet, Scheme, MIN_PASSWORD_CHARS};
use crate::ratelimit::{AttemptKeys, Limit};
use crate::session::{Session, SessionPolicy, SessionToken};
use crate::store::{Store, StoreError, StoreResult};
use crate::username::Username;

/// What a deployment's signup path is configured with.
pub struct RegisterConfig {
    pub scheme: Scheme,
    pub peppers: PepperSet,
    pub sessions: SessionPolicy,
    /// Per client, not per name: an enumerator supplies a different name every time.
    pub per_client: Limit,
    /// Whether an address is mandatory. Only sensible with a mailer configured.
    pub require_email: bool,
}

pub struct Signup<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// Optional unless [`RegisterConfig::require_email`]. Stored unverified; a link is mailed.
    pub email: &'a str,
    pub client: &'a str,
    /// Caller-generated: `core` has no RNG on wasm.
    pub token: SessionToken,
    /// Base64, caller-generated for the same reason.
    pub salt: &'a str,
    /// For the verification link, if there is an address to send it to.
    pub verify_token: EmailToken,
    pub now: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Carries a message safe to show.
    BadName(String),
    Taken,
    ShortPassword {
        min: usize,
    },
    BadEmail(EmailError),
}

pub enum Outcome {
    /// Created and signed in. `verification` says whether a link went out.
    Created {
        session: Session,
        user: User,
        verification: Option<Delivery>,
    },
    Rejected(Rejected),
    RateLimited {
        retry_after_secs: i64,
    },
}

/// Whether `given` is the invite code the deployment requires. `None` means none is required.
/// Constant-time, so a wrong code costs the same whatever prefix it shares with the right one.
pub fn invite_ok(required: Option<&str>, given: &str) -> bool {
    use subtle::ConstantTimeEq;
    match required.map(str::trim).filter(|r| !r.is_empty()) {
        None => true,
        Some(r) => {
            let g = given.trim();
            r.len() == g.len() && bool::from(r.as_bytes().ct_eq(g.as_bytes()))
        }
    }
}

/// Validate without touching storage. An empty address is `None` unless one is required.
pub fn check(
    username: &str,
    password: &str,
    email: &str,
    require_email: bool,
) -> Result<(Username, Option<EmailAddress>), Rejected> {
    let name = Username::parse(username).map_err(|e| Rejected::BadName(e.to_string()))?;
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(Rejected::ShortPassword {
            min: MIN_PASSWORD_CHARS,
        });
    }
    let email = if email.trim().is_empty() && !require_email {
        None
    } else {
        Some(EmailAddress::parse(email).map_err(Rejected::BadEmail)?)
    };
    Ok((name, email))
}

/// **Budget: 3-7 statements.**
pub async fn signup<S: Store, M: Mailer>(
    store: &S,
    mailer: &M,
    links: &Links<'_>,
    cfg: &RegisterConfig,
    s: Signup<'_>,
) -> StoreResult<Outcome> {
    let (name, email) = match check(s.username, s.password, s.email, cfg.require_email) {
        Ok(n) => n,
        Err(why) => return Ok(Outcome::Rejected(why)),
    };

    // The identity half of the key is constant, so a different name buys no fresh budget.
    let keys = AttemptKeys::new("signup", s.client);
    let (_, client_state) = store.login_attempts(&keys).await?;
    let client = cfg.per_client.check(client_state, s.now);
    if !client.allowed() {
        return Ok(Outcome::RateLimited {
            retry_after_secs: client.retry_after_secs().unwrap_or(1),
        });
    }

    // Advisory only; the unique index below is the guard.
    if store.user_by_name(name.as_str()).await?.is_some() {
        if let Some(next) = client.next() {
            store.record_login_attempt(&keys.client, next).await?;
        }
        return Ok(Outcome::Rejected(Rejected::Taken));
    }

    let hash = match password::hash(s.password, s.salt, cfg.scheme, &cfg.peppers) {
        Ok(h) => h,
        // `check` already ran, so the only reachable failure is one the visitor can act on.
        Err(_) => {
            return Ok(Outcome::Rejected(Rejected::ShortPassword {
                min: MIN_PASSWORD_CHARS,
            }))
        }
    };

    let user_id = match store
        .create_user(name.as_str(), s.now, Some(hash.as_str()))
        .await
    {
        Ok(id) => id,
        Err(StoreError::Conflict) => return Ok(Outcome::Rejected(Rejected::Taken)),
        Err(e) => return Err(e),
    };

    if let Some(next) = client.next() {
        store.record_login_attempt(&keys.client, next).await?;
    }

    let session = Session {
        token_hash: s.token.hash(),
        user_id,
        created_at: s.now,
        refreshed_at: s.now,
        expires_at: cfg.sessions.expiry_from(s.now),
    };
    store.create_session(&session).await?;

    let user = User {
        id: user_id,
        name: name.as_str().to_string(),
        state: UserState::Active,
        role: crate::model::Role::Member,
    };
    // Whether the mail goes has no bearing on the account: it exists and is signed in.
    let verification = match email {
        Some(address) => {
            store.set_email(user_id, Some(address.as_str())).await?;
            Some(
                account::send_verification(
                    store,
                    mailer,
                    links,
                    &user,
                    &address,
                    s.verify_token,
                    s.now,
                )
                .await?,
            )
        }
        None => None,
    };

    Ok(Outcome::Created {
        session,
        user,
        verification,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_PW: &str = "correct horse battery staple";

    #[test]
    fn an_invite_code_is_required_only_when_configured() {
        assert!(invite_ok(None, ""));
        assert!(invite_ok(Some(""), "anything"));
        assert!(invite_ok(Some(" hunter2 "), "hunter2"));
        assert!(!invite_ok(Some("hunter2"), ""));
        assert!(!invite_ok(Some("hunter2"), "hunter"));
        assert!(!invite_ok(Some("hunter2"), "HUNTER2"));
    }

    #[test]
    fn a_valid_signup_passes_the_free_checks() {
        assert!(check("newcomer", GOOD_PW, "", false).is_ok());
    }

    #[test]
    fn a_short_password_is_refused_before_anything_costs() {
        assert_eq!(
            check("newcomer", "short", "", false),
            Err(Rejected::ShortPassword {
                min: MIN_PASSWORD_CHARS
            })
        );
    }

    #[test]
    fn an_address_is_optional_unless_required() {
        assert!(matches!(
            check("newcomer", GOOD_PW, "", false),
            Ok((_, None))
        ));
        assert!(matches!(
            check("newcomer", GOOD_PW, "", true),
            Err(Rejected::BadEmail(EmailError::Empty))
        ));
        assert!(matches!(
            check("newcomer", GOOD_PW, "not-an-address", false),
            Err(Rejected::BadEmail(_))
        ));
        match check("newcomer", GOOD_PW, " Who@Example.org ", false) {
            Ok((_, Some(a))) => assert_eq!(a.as_str(), "who@example.org"),
            other => panic!("{other:?}"),
        }
    }

    /// Pins that registration applies the `username` rules rather than the database's.
    #[test]
    fn an_invalid_name_is_refused_with_something_to_show() {
        for bad in ["", "a", "has space", "trailing-", "-leading", "double--sep"] {
            match check(bad, GOOD_PW, "", false) {
                Err(Rejected::BadName(msg)) => {
                    assert!(!msg.is_empty(), "empty message for {bad:?}")
                }
                other => panic!("accepted {bad:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_reserved_name_is_refused() {
        match check("admin", GOOD_PW, "", false) {
            Err(Rejected::BadName(msg)) => assert!(msg.contains("reserved"), "got {msg:?}"),
            other => panic!("accepted a reserved name: {other:?}"),
        }
    }

    #[test]
    fn the_name_is_reported_before_the_password() {
        assert!(matches!(
            check("!!", "x", "", false),
            Err(Rejected::BadName(_))
        ));
    }
}
