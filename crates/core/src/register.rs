//! Creating an account.
//!
//! This is an enumeration oracle by design — a signup form that will not say "that name is
//! taken" is unusable — which is why the limit is per client rather than per name.
//!
//! The taken-check and the insert are not atomic. `UNIQUE(user.name)` is the guard; the check
//! only avoids spending a hash on a doomed insert.

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
}

pub struct Signup<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub client: &'a str,
    /// Caller-generated: `core` has no RNG on wasm.
    pub token: SessionToken,
    /// Base64, caller-generated for the same reason.
    pub salt: &'a str,
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
}

pub enum Outcome {
    /// Created and signed in.
    Created {
        session: Session,
        user: User,
    },
    Rejected(Rejected),
    RateLimited {
        retry_after_secs: i64,
    },
}

/// Validate without touching storage.
pub fn check(username: &str, password: &str) -> Result<Username, Rejected> {
    let name = Username::parse(username).map_err(|e| Rejected::BadName(e.to_string()))?;
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(Rejected::ShortPassword {
            min: MIN_PASSWORD_CHARS,
        });
    }
    Ok(name)
}

/// **Budget: 3-4 statements.**
pub async fn signup<S: Store>(
    store: &S,
    cfg: &RegisterConfig,
    s: Signup<'_>,
) -> StoreResult<Outcome> {
    let name = match check(s.username, s.password) {
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

    Ok(Outcome::Created {
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

    const GOOD_PW: &str = "correct horse battery staple";

    #[test]
    fn a_valid_signup_passes_the_free_checks() {
        assert!(check("newcomer", GOOD_PW).is_ok());
    }

    #[test]
    fn a_short_password_is_refused_before_anything_costs() {
        assert_eq!(
            check("newcomer", "short"),
            Err(Rejected::ShortPassword {
                min: MIN_PASSWORD_CHARS
            })
        );
    }

    /// Pins that registration applies the `username` rules rather than the database's.
    #[test]
    fn an_invalid_name_is_refused_with_something_to_show() {
        for bad in ["", "a", "has space", "trailing-", "-leading", "double--sep"] {
            match check(bad, GOOD_PW) {
                Err(Rejected::BadName(msg)) => {
                    assert!(!msg.is_empty(), "empty message for {bad:?}")
                }
                other => panic!("accepted {bad:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_reserved_name_is_refused() {
        match check("admin", GOOD_PW) {
            Err(Rejected::BadName(msg)) => assert!(msg.contains("reserved"), "got {msg:?}"),
            other => panic!("accepted a reserved name: {other:?}"),
        }
    }

    #[test]
    fn the_name_is_reported_before_the_password() {
        assert!(matches!(check("!!", "x"), Err(Rejected::BadName(_))));
    }
}
