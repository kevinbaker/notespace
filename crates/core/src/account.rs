//! Account self-service: proving an address, recovering a password, changing either.
//!
//! Every link is a [`EmailToken`] whose hash is stored and spent once. Password reset is the
//! one flow that turns an inbox into an account, so it is held to the login module's standard:
//! rate-limited before it costs anything, and silent about whether the address is known.

use crate::email::{
    self, ConsumedToken, EmailAddress, EmailError, EmailToken, Links, MailError, Mailer,
    StoredToken, TokenKind,
};
use crate::model::{Timestamp, User};
use crate::store::{Store, StoreError, StoreResult};

/// How a send went. Never fatal to the flow that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// No mailer on this deployment; the token exists and nothing carries it.
    NotConfigured,
    Failed(String),
}

impl From<Result<(), MailError>> for Delivery {
    fn from(r: Result<(), MailError>) -> Self {
        match r {
            Ok(()) => Delivery::Sent,
            Err(MailError::NotConfigured) => Delivery::NotConfigured,
            Err(e) => Delivery::Failed(e.to_string()),
        }
    }
}

/// A link about to be issued: for whom, proving which address, carried by which message.
struct Issue<'a> {
    user: &'a User,
    email: &'a EmailAddress,
    kind: TokenKind,
    token: &'a EmailToken,
    message: email::Message,
}

/// Retire the account's older links of this kind, store the new one, and mail it.
/// **Budget: 2 statements.**
async fn issue<S: Store, M: Mailer>(
    store: &S,
    mailer: &M,
    i: Issue<'_>,
    now: Timestamp,
) -> StoreResult<Delivery> {
    store.retire_email_tokens(i.user.id, i.kind, now).await?;
    store
        .create_email_token(&StoredToken {
            token_hash: i.token.hash(),
            user_id: i.user.id,
            kind: i.kind,
            email: i.email.clone(),
            created_at: now,
            expires_at: now + i.kind.lifetime_ms(),
        })
        .await?;
    Ok(mailer.send(&i.message).await.into())
}

/// Mail a verification link for the address on file. **Budget: 2 statements.**
pub async fn send_verification<S: Store, M: Mailer>(
    store: &S,
    mailer: &M,
    links: &Links<'_>,
    user: &User,
    email: &EmailAddress,
    token: EmailToken,
    now: Timestamp,
) -> StoreResult<Delivery> {
    let message = email::verification(links, email, &user.name, &token);
    issue(
        store,
        mailer,
        Issue {
            user,
            email,
            kind: TokenKind::Verify,
            token: &token,
            message,
        },
        now,
    )
    .await
}

/// From settings: store a new address unverified and mail the link. **Budget: 3 statements.**
pub async fn change_email<S: Store, M: Mailer>(
    store: &S,
    mailer: &M,
    links: &Links<'_>,
    user: &User,
    email: &str,
    token: EmailToken,
    now: Timestamp,
) -> StoreResult<Result<Delivery, EmailError>> {
    let address = match EmailAddress::parse(email) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };
    store.set_email(user.id, Some(address.as_str())).await?;
    Ok(Ok(send_verification(
        store, mailer, links, user, &address, token, now,
    )
    .await?))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfirmOutcome {
    Verified {
        user_id: i64,
    },
    /// Unknown, expired, or already used.
    Invalid,
    /// The account's address changed after this link was sent.
    Stale,
    /// Another account proved this address first.
    Claimed,
}

/// Follow a verification link. **Budget: 2 statements.**
pub async fn confirm_email<S: Store>(
    store: &S,
    token: &EmailToken,
    now: Timestamp,
) -> StoreResult<ConfirmOutcome> {
    let Some(ConsumedToken { user_id, email }) = store
        .consume_email_token(&token.hash(), TokenKind::Verify, now)
        .await?
    else {
        return Ok(ConfirmOutcome::Invalid);
    };
    match store
        .mark_email_verified(user_id, email.as_str(), now)
        .await
    {
        Ok(true) => Ok(ConfirmOutcome::Verified { user_id }),
        Ok(false) => Ok(ConfirmOutcome::Stale),
        Err(StoreError::Conflict) => Ok(ConfirmOutcome::Claimed),
        Err(e) => Err(e),
    }
}

#[cfg(feature = "password")]
pub use recovery::*;

#[cfg(feature = "password")]
mod recovery {
    use super::*;
    use crate::password::{self, PepperSet, Scheme, Verified, MIN_PASSWORD_CHARS};
    use crate::ratelimit::{AttemptKeys, Limit};
    use crate::session::Session;

    pub struct RecoveryConfig {
        pub scheme: Scheme,
        pub peppers: PepperSet,
        /// Requests per client address.
        pub per_client: Limit,
        /// Requests per target address, so one inbox cannot be flooded.
        pub per_address: Limit,
    }

    pub enum RequestOutcome {
        /// Always, for any well-formed address. `delivery` is `None` when no mail was owed:
        /// the address is unknown, unverified, or on an account that cannot sign in.
        Accepted {
            delivery: Option<Delivery>,
        },
        BadAddress(EmailError),
        RateLimited {
            retry_after_secs: i64,
        },
    }

    /// One "I forgot my password" submission.
    pub struct ResetRequest<'a> {
        pub email: &'a str,
        pub client: &'a str,
        /// Caller-generated: `core` has no RNG.
        pub token: EmailToken,
        pub now: Timestamp,
    }

    /// **Budget: 2-6 statements.**
    pub async fn request_reset<S: Store, M: Mailer>(
        store: &S,
        mailer: &M,
        links: &Links<'_>,
        cfg: &RecoveryConfig,
        r: ResetRequest<'_>,
    ) -> StoreResult<RequestOutcome> {
        let (client, token, now) = (r.client, r.token, r.now);
        let address = match EmailAddress::parse(r.email) {
            Ok(a) => a,
            Err(e) => return Ok(RequestOutcome::BadAddress(e)),
        };
        let keys = AttemptKeys::new(&format!("reset:{address}"), client);
        let (address_state, client_state) = store.login_attempts(&keys).await?;
        let per_address = cfg.per_address.check(address_state, now);
        let per_client = cfg.per_client.check(client_state, now);
        if !per_address.allowed() || !per_client.allowed() {
            let wait = per_address
                .retry_after_secs()
                .into_iter()
                .chain(per_client.retry_after_secs())
                .max()
                .unwrap_or(1);
            return Ok(RequestOutcome::RateLimited {
                retry_after_secs: wait,
            });
        }
        // Counted whether or not mail goes, so an unknown address costs the same as a known one.
        if let Some(next) = per_address.next() {
            store.record_login_attempt(&keys.identity, next).await?;
        }
        if let Some(next) = per_client.next() {
            store.record_login_attempt(&keys.client, next).await?;
        }

        let Some(user) = store.user_by_verified_email(address.as_str()).await? else {
            return Ok(RequestOutcome::Accepted { delivery: None });
        };
        if !user.state.can_act() {
            return Ok(RequestOutcome::Accepted { delivery: None });
        }
        let message = email::password_reset(links, &address, &user.name, &token);
        let delivery = issue(
            store,
            mailer,
            Issue {
                user: &user,
                email: &address,
                kind: TokenKind::Reset,
                token: &token,
                message,
            },
            now,
        )
        .await?;
        Ok(RequestOutcome::Accepted {
            delivery: Some(delivery),
        })
    }

    pub enum ResetOutcome {
        /// The password is changed and every session is ended.
        Done {
            user: User,
        },
        /// Unknown, expired, or already used.
        Invalid,
        ShortPassword {
            min: usize,
        },
    }

    /// The reset form, submitted.
    pub struct ResetCompletion<'a> {
        pub token: &'a EmailToken,
        pub new_password: &'a str,
        /// Base64, caller-generated.
        pub salt: &'a str,
        pub now: Timestamp,
    }

    /// Follow a reset link. The password is checked before the token is spent, so a typo does
    /// not cost the link. **Budget: 5-6 statements.**
    pub async fn complete_reset<S: Store, M: Mailer>(
        store: &S,
        mailer: &M,
        links: &Links<'_>,
        cfg: &RecoveryConfig,
        c: ResetCompletion<'_>,
    ) -> StoreResult<ResetOutcome> {
        let (token, new_password, salt, now) = (c.token, c.new_password, c.salt, c.now);
        if new_password.chars().count() < MIN_PASSWORD_CHARS {
            return Ok(ResetOutcome::ShortPassword {
                min: MIN_PASSWORD_CHARS,
            });
        }
        let Some(ConsumedToken { user_id, email }) = store
            .consume_email_token(&token.hash(), TokenKind::Reset, now)
            .await?
        else {
            return Ok(ResetOutcome::Invalid);
        };
        let hash = match password::hash(new_password, salt, cfg.scheme, &cfg.peppers) {
            Ok(h) => h,
            Err(_) => {
                return Ok(ResetOutcome::ShortPassword {
                    min: MIN_PASSWORD_CHARS,
                })
            }
        };
        store.set_password_hash(user_id, &hash).await?;
        // Whoever had the old password, or a session from it, is out.
        store.delete_user_sessions(user_id).await?;
        store
            .retire_email_tokens(user_id, TokenKind::Reset, now)
            .await?;
        let Some(user) = store.user_by_id(user_id).await? else {
            return Err(StoreError::NotFound);
        };
        let _ = mailer
            .send(&email::password_changed(links, &email, &user.name))
            .await;
        Ok(ResetOutcome::Done { user })
    }

    pub enum ChangeOutcome {
        Changed,
        WrongPassword,
        ShortPassword {
            min: usize,
        },
        /// The account signs in some other way.
        NoPassword,
    }

    /// The settings form, submitted. Ends every other session and keeps `keep`, so the person
    /// doing the changing is not signed out by it.
    pub struct PasswordChange<'a> {
        pub user: &'a User,
        pub current: &'a str,
        pub new_password: &'a str,
        /// Base64, caller-generated.
        pub salt: &'a str,
        pub keep: Option<&'a Session>,
        pub now: Timestamp,
    }

    /// From settings, with the current password as proof. **Budget: 5-7 statements.**
    pub async fn change_password<S: Store, M: Mailer>(
        store: &S,
        mailer: &M,
        links: &Links<'_>,
        cfg: &RecoveryConfig,
        c: PasswordChange<'_>,
    ) -> StoreResult<ChangeOutcome> {
        let (user, current, new_password, salt, keep, now) =
            (c.user, c.current, c.new_password, c.salt, c.keep, c.now);
        if new_password.chars().count() < MIN_PASSWORD_CHARS {
            return Ok(ChangeOutcome::ShortPassword {
                min: MIN_PASSWORD_CHARS,
            });
        }
        let Some(stored) = store
            .user_by_name(&user.name)
            .await?
            .and_then(|c| c.password_hash)
        else {
            return Ok(ChangeOutcome::NoPassword);
        };
        let ok = password::verify(current, &stored, cfg.scheme, &cfg.peppers)
            .unwrap_or(Verified::No)
            .ok();
        if !ok {
            return Ok(ChangeOutcome::WrongPassword);
        }
        let hash = match password::hash(new_password, salt, cfg.scheme, &cfg.peppers) {
            Ok(h) => h,
            Err(_) => {
                return Ok(ChangeOutcome::ShortPassword {
                    min: MIN_PASSWORD_CHARS,
                })
            }
        };
        store.set_password_hash(user.id, &hash).await?;
        store.delete_user_sessions(user.id).await?;
        // A reset link still in some inbox must not outlive the password it was for.
        store
            .retire_email_tokens(user.id, TokenKind::Reset, now)
            .await?;
        if let Some(session) = keep {
            store.create_session(session).await?;
        }
        if let Some(account) = store.account(user.id).await? {
            if let (Some(addr), true) = (account.email.as_deref(), account.email_is_verified()) {
                if let Ok(to) = EmailAddress::parse(addr) {
                    let _ = mailer
                        .send(&email::password_changed(links, &to, &user.name))
                        .await;
                }
            }
        }
        Ok(ChangeOutcome::Changed)
    }
}
