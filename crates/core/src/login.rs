//! The login flow.
//!
//! The order of operations is the security property:
//!
//! ```text
//!   rate limit  ->  look up  ->  verify  ->  session
//!        |                          |
//!        |                          +-- always runs, even for an unknown account
//!        +-- before the hash, so a refused attempt costs a lookup and no Argon2 run
//! ```
//!
//! Two invariants any change here must preserve:
//!
//! - **No username oracle.** An unknown account, and an account with no local password, both
//!   still hash against [`LoginConfig::dummy_hash`] before failing, so timing does not
//!   distinguish them from a wrong password.
//! - **Every failure looks the same.** [`Outcome::Rejected`] carries no reason.

use crate::model::{Timestamp, User};
use crate::password::{self, PepperSet, Scheme, Verified};
use crate::ratelimit::{AttemptKeys, Limit};
use crate::session::{Session, SessionPolicy, SessionToken};
use crate::store::{Store, StoreResult};

/// What a deployment's login path is configured with.
pub struct LoginConfig {
    pub scheme: Scheme,
    pub peppers: PepperSet,
    pub sessions: SessionPolicy,
    pub per_identity: Limit,
    pub per_client: Limit,
    /// A real hash of a fixed value, under this deployment's current scheme and pepper.
    ///
    /// Verified against when there is no account, so the failing path costs what the succeeding
    /// one does. Must track the current parameters, or the timing difference reappears.
    pub dummy_hash: String,
}

impl LoginConfig {
    /// Build the dummy hash. Once at startup, not per request.
    ///
    /// The input must satisfy the scheme's shape rules, or hashing fails and the dummy becomes
    /// `""`, which verifies instantly against everything. Check
    /// [`Self::dummy_hash_is_real`] before serving logins.
    pub fn dummy_hash_for(scheme: Scheme, peppers: &PepperSet) -> String {
        let secret = match scheme {
            Scheme::Server(_) => "\0not-a-password\0dummy-for-timing-only",
            // 32 bytes, fixed: nothing here is secret except the cost of hashing it.
            Scheme::Client { .. } => {
                "64756d6d792d636c69656e742d6b65792d666f722d74696d696e672d6f6e6c7900"
            }
        };
        password::hash(secret, "ZHVtbXlzYWx0Zm9ydGltaW5n", scheme, peppers).unwrap_or_default()
    }

    /// Whether the dummy hash actually built. `false` means unknown-account logins return
    /// faster than real ones — an account-enumeration oracle.
    pub fn dummy_hash_is_real(&self) -> bool {
        !self.dummy_hash.is_empty()
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
/// **Budget: 3-6 statements.** A rate-limited attempt costs one statement and no hashing.
pub async fn attempt<S: Store>(
    store: &S,
    cfg: &LoginConfig,
    a: Attempt<'_>,
) -> StoreResult<Outcome> {
    let keys = AttemptKeys::new(a.username, a.client);

    // 1. Rate limit, before anything expensive.
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

    // 2. Look up the account. Deliberately no early return on a miss.
    let found = store.user_by_name(&a.username.to_lowercase()).await?;

    // 3. Verify. A missing account or credential hashes the dummy instead.
    let stored = found
        .as_ref()
        .and_then(|c| c.password_hash.as_deref())
        .unwrap_or(cfg.dummy_hash.as_str());
    // An unreadable stored hash is not a reason to let anyone in.
    let verdict =
        password::verify(a.password, stored, cfg.scheme, &cfg.peppers).unwrap_or(Verified::No);

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

    // 4. Upgrade the stored hash while the plaintext is in hand.
    if verdict == Verified::YesRehash {
        if let Ok(salt) = password::encode_salt(&a.token.hash().as_str().as_bytes()[..16]) {
            if let Ok(fresh) = password::hash(a.password, &salt, cfg.scheme, &cfg.peppers) {
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

    // 5. Success clears the counters.
    store.clear_login_attempts(&keys.identity).await?;
    store.clear_login_attempts(&keys.client).await?;

    Ok(Outcome::Success {
        session,
        user: credential.user,
    })
}
