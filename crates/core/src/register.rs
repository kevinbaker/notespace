//! Creating an account.
//!
//! ```text
//!   validate name  ->  validate password  ->  rate limit  ->  is it taken?  ->  hash  ->  insert
//!         |                                        |                |
//!         |                                        |                +-- advisory; the unique
//!         |                                        |                    index is the real guard
//!         |                                        +-- before the hash, so a signup flood is cheap
//!         +-- free, and rejects most bad input before anything costs
//! ```
//!
//! # Registration is an enumeration oracle, and cannot not be
//!
//! [`crate::login`] goes to some trouble to make "no such account" and "wrong password"
//! indistinguishable. This module gives that away by design: a signup form that will not say
//! "that name is taken" is unusable. Every forum makes the same trade.
//!
//! What that costs is bounded rather than eliminated. The per-client limit is what stops the
//! form being walked as a bulk membership check, and it is the reason the limit here is on the
//! client rather than the name — the name is different every time an enumerator asks.
//!
//! # The check and the insert are not atomic
//!
//! [`Store::user_by_name`] then [`Store::create_user`] is a TOCTOU window: two signups for the
//! same name can both see it free. The `UNIQUE` index on `user.name` is the actual guard, and
//! the loser comes back as [`StoreError::Conflict`], which is reported as
//! [`Rejected::Taken`] exactly like the advisory check would have. Removing the advisory check
//! would still be correct — it exists only to avoid spending an Argon2 hash on a doomed insert.

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
    /// Accounts one client may create per window. The only limit that matters here: an
    /// enumerator supplies a different name every time, so a per-name limit catches nothing.
    pub per_client: Limit,
}

/// One signup attempt.
pub struct Signup<'a> {
    pub username: &'a str,
    pub password: &'a str,
    pub client: &'a str,
    /// Caller-generated, because `core` has no RNG on wasm.
    pub token: SessionToken,
    /// Base64 salt, caller-generated for the same reason.
    pub salt: &'a str,
    pub now: Timestamp,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The name is not a valid username. Carries a message safe to show.
    BadName(String),
    /// Already registered.
    Taken,
    /// Shorter than [`MIN_PASSWORD_CHARS`].
    ShortPassword { min: usize },
}

pub enum Outcome {
    /// Created and signed in. Registration logs you in; making someone type the password they
    /// just chose, twice, is friction with nothing behind it.
    Created {
        session: Session,
        user: User,
    },
    Rejected(Rejected),
    RateLimited {
        retry_after_secs: i64,
    },
}

/// Validate a proposed name and password without touching storage.
///
/// Exposed so a form can check before submitting, and so the checks are testable on their own.
pub fn check(username: &str, password: &str) -> Result<Username, Rejected> {
    let name = Username::parse(username).map_err(|e| Rejected::BadName(e.to_string()))?;
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(Rejected::ShortPassword {
            min: MIN_PASSWORD_CHARS,
        });
    }
    Ok(name)
}

/// Create an account and sign in.
///
/// **Budget: 3-4 statements.** Attempt counters, the name check, the insert, the session.
pub async fn signup<S: Store>(
    store: &S,
    cfg: &RegisterConfig,
    s: Signup<'_>,
) -> StoreResult<Outcome> {
    // 1. Free checks first: most bad input never reaches storage or the KDF.
    let name = match check(s.username, s.password) {
        Ok(n) => n,
        Err(why) => return Ok(Outcome::Rejected(why)),
    };

    // 2. Rate limit before the lookup and the hash. Both buckets use the client address; the
    //    name half of the key is a constant here, so one client cannot get a fresh budget by
    //    trying a different name.
    let keys = AttemptKeys::new("signup", s.client);
    let (_, client_state) = store.login_attempts(&keys).await?;
    let client = cfg.per_client.check(client_state, s.now);
    if !client.allowed() {
        return Ok(Outcome::RateLimited {
            retry_after_secs: client.retry_after_secs().unwrap_or(1),
        });
    }

    // 3. Advisory: avoids spending a hash on a name that is already gone. Not the guard.
    if store.user_by_name(name.as_str()).await?.is_some() {
        if let Some(next) = client.next() {
            store.record_login_attempt(&keys.client, next).await?;
        }
        return Ok(Outcome::Rejected(Rejected::Taken));
    }

    let hash = match password::hash(s.password, s.salt, cfg.scheme, &cfg.peppers) {
        Ok(h) => h,
        // The only reachable failure is a policy one, and `check` already ran; treat anything
        // else as a rejection rather than a 500, since the visitor can act on it.
        Err(_) => {
            return Ok(Outcome::Rejected(Rejected::ShortPassword {
                min: MIN_PASSWORD_CHARS,
            }))
        }
    };

    // 4. The real guard. A `Conflict` here is the TOCTOU race losing, and means the same thing
    //    to the visitor as the advisory check firing.
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

    /// The name rules live in `username`; this pins that registration actually applies them
    /// rather than accepting anything the database will hold.
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

    /// The name is checked before the password, so someone typing a bad name and a short
    /// password is told about the name first rather than fixing one problem at a time.
    #[test]
    fn the_name_is_reported_before_the_password() {
        assert!(matches!(check("!!", "x"), Err(Rejected::BadName(_))));
    }
}
