//! The login flow, orchestrated where it can be tested.
//!
//! Lives here rather than in a handler because the order of operations *is* the security
//! property, and an order is worth a test. Handlers translate HTTP; this decides.
//!
//! ```text
//!   rate limit  ->  look up  ->  verify  ->  session
//!        |                          |
//!        |                          +-- always runs, even for an unknown account
//!        +-- before the hash, so a refused attempt costs a lookup and not 3.34 ms
//! ```
//!
//! # Two properties that are easy to lose
//!
//! **The form must not be a username oracle.** Returning early for an unknown account is
//! measurably faster than one that reaches Argon2 — a few milliseconds, trivially detectable
//! over a handful of requests. So an unknown account, and an account with no local password,
//! both still hash against [`LoginConfig::dummy_hash`] before failing. `verify` is deliberately
//! given work it will throw away.
//!
//! **Every failure looks the same.** [`Outcome::Rejected`] carries no reason. "No such user",
//! "wrong password" and "that account has no password" are one answer, because distinguishing
//! them hands an attacker the account list.

use crate::model::{Timestamp, User};
use crate::password::{self, Params, PepperSet, Verified};
use crate::ratelimit::{AttemptKeys, Limit};
use crate::session::{Session, SessionPolicy, SessionToken};
use crate::store::{Store, StoreResult};

/// What a deployment's login path is configured with.
pub struct LoginConfig {
    pub params: Params,
    pub peppers: PepperSet,
    pub sessions: SessionPolicy,
    pub per_identity: Limit,
    pub per_client: Limit,
    /// A real hash of a random value, in this deployment's parameters.
    ///
    /// Verified against when there is no account, so the failing path costs what the succeeding
    /// one does. It must use the *current* parameters and pepper, or the timing it is there to
    /// hide reappears as a difference in cost.
    pub dummy_hash: String,
}

impl LoginConfig {
    /// Build the dummy hash. Do this once at startup, not per request.
    ///
    /// The password hashed here is never a valid credential: it is not reachable through any
    /// form, and the salt is fixed because nothing about this hash is secret — only its cost.
    pub fn dummy_hash_for(params: Params, peppers: &PepperSet) -> String {
        password::hash(
            "\0not-a-password\0dummy-for-timing-only",
            "ZHVtbXlzYWx0Zm9ydGltaW5n",
            params,
            peppers,
        )
        .unwrap_or_default()
    }
}

/// What the caller supplies for one attempt.
pub struct Attempt<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// Client address, for the second rate-limit bucket.
    pub client: &'a str,
    /// Caller-generated, because `core` has no RNG on wasm.
    pub token: SessionToken,
    pub now: Timestamp,
}

pub enum Outcome {
    /// Set this session cookie.
    Success { session: Session, user: User },
    /// Wrong. Deliberately without a reason.
    Rejected,
    /// Too many attempts.
    RateLimited { retry_after_secs: i64 },
}

/// Run one login attempt.
///
/// **Budget: 3-6 statements.** Attempt counters, the account, then on success a session and two
/// counter clears; on failure one counter write. A rate-limited attempt costs one statement and
/// no hashing at all.
pub async fn attempt<S: Store>(
    store: &S,
    cfg: &LoginConfig,
    a: Attempt<'_>,
) -> StoreResult<Outcome> {
    let keys = AttemptKeys::new(a.username, a.client);

    // 1. Rate limit, before anything expensive. This is the only step an attacker can force
    //    repeatedly, so it has to be the cheap one.
    let (identity_state, client_state) = store.login_attempts(&keys).await?;
    let identity = cfg.per_identity.check(identity_state, a.now);
    let client = cfg.per_client.check(client_state, a.now);
    if !identity.allowed() || !client.allowed() {
        let wait = identity
            .retry_after_secs()
            .into_iter()
            .chain(client.retry_after_secs())
            .max()
            .unwrap_or(1);
        return Ok(Outcome::RateLimited {
            retry_after_secs: wait,
        });
    }

    // 2. Look up the account. Note what does *not* happen here: an early return.
    let found = store.user_by_name(&a.username.to_lowercase()).await?;

    // 3. Verify. An absent account, and an account with no local password, both hash the dummy
    //    so that failing costs what succeeding costs.
    let stored = found
        .as_ref()
        .and_then(|c| c.password_hash.as_deref())
        .unwrap_or(cfg.dummy_hash.as_str());
    let verdict = password::verify(a.password, stored, cfg.params, &cfg.peppers)
        // A hash this deployment cannot read is not a login failure to report to the visitor,
        // but it is also not a reason to let them in.
        .unwrap_or(Verified::No);

    // A banned or deleted account never authenticates, whatever the password was.
    let usable = found
        .as_ref()
        .map(|c| c.user.state.can_act() && c.password_hash.is_some())
        .unwrap_or(false);

    if !verdict.ok() || !usable {
        if let Some(next) = identity.next() {
            store.record_login_attempt(&keys.identity, next).await?;
        }
        if let Some(next) = client.next() {
            store.record_login_attempt(&keys.client, next).await?;
        }
        return Ok(Outcome::Rejected);
    }

    let credential = found.expect("usable implies found");

    // 4. Upgrade the stored hash while the plaintext is in hand — the only moment it is.
    if verdict == Verified::YesRehash {
        if let Ok(salt) = password::encode_salt(&a.token.hash().as_str().as_bytes()[..16]) {
            if let Ok(fresh) = password::hash(a.password, &salt, cfg.params, &cfg.peppers) {
                store.set_password_hash(credential.user.id, &fresh).await?;
            }
        }
    }

    let session = Session {
        token_hash: a.token.hash(),
        user_id: credential.user.id,
        created_at: a.now,
        refreshed_at: a.now,
        expires_at: cfg.sessions.expiry_from(a.now),
    };
    store.create_session(&session).await?;

    // 5. Success clears the counters, so a correct password is never punished.
    store.clear_login_attempts(&keys.identity).await?;
    store.clear_login_attempts(&keys.client).await?;

    Ok(Outcome::Success {
        session,
        user: credential.user,
    })
}
