//! Rate limiting for authentication attempts.
//!
//! # Why this matters more than the KDF here
//!
//! A weak KDF is an *offline* problem: it decides how fast someone who already stole the
//! database can crack it, and the pepper (§4.10) is the answer to that. Online guessing is a
//! different attack with a different defence — refusing attempts — and no amount of Argon2 cost
//! helps, because the attacker pays nothing for a wrong guess. **We** do: at
//! `Params::CONSTRAINED` every attempt burns 3.34 ms of a 10 ms budget, so unthrottled login is
//! also a cheap way to exhaust the account's CPU.
//!
//! So the check runs **before** the hash, not after. A refused attempt must cost a lookup, not a
//! KDF.
//!
//! # Two buckets, because one attack is not the other
//!
//! - **Per identity**: someone hammering one account with a password list. Strict.
//! - **Per client**: someone spraying one common password across many accounts — credential
//!   stuffing, which never trips a per-account limit because no account sees two attempts.
//!   Looser, because a shared NAT or a university proxy is one client to us.
//!
//! An attempt has to pass both.
//!
//! # Fixed windows, deliberately
//!
//! A sliding window is more accurate and needs per-attempt timestamps; a fixed window needs one
//! counter and one instant. The cost of the approximation is that an attacker can straddle a
//! boundary and get `2 * limit` attempts in quick succession — which for a limit of 5 means 10,
//! and does not change anything. Precision is not worth a row per attempt on a 500 MB database.

use crate::model::Timestamp;

/// A counter and the window it belongs to. What a store round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempts {
    /// Start of the window this count belongs to.
    pub window_start: Timestamp,
    pub count: u32,
}

impl Attempts {
    pub fn first(now: Timestamp) -> Self {
        Attempts {
            window_start: now,
            count: 1,
        }
    }
}

/// How many attempts are allowed, and over how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub max: u32,
    pub window_ms: i64,
}

impl Limit {
    /// Per-identity: five tries per fifteen minutes.
    ///
    /// Enough that a person who mistypes twice and then goes to find their password manager is
    /// unaffected; far too few to walk a password list.
    pub const PER_IDENTITY: Limit = Limit {
        max: 5,
        window_ms: 15 * 60 * 1000,
    };

    /// Per client address: sixty per fifteen minutes.
    ///
    /// Twelve times looser than per-identity because a client is not a person — an office, a
    /// campus or a mobile carrier can be one address. Still low enough that credential stuffing,
    /// which needs thousands of attempts to be worth running, dies here.
    pub const PER_CLIENT: Limit = Limit {
        max: 60,
        window_ms: 15 * 60 * 1000,
    };

    /// Whether `state` is still inside this window.
    fn in_window(&self, state: &Attempts, now: Timestamp) -> bool {
        now - state.window_start < self.window_ms
    }

    /// The decision for an attempt arriving now.
    ///
    /// `state` is what the store holds, or `None` if nothing is recorded.
    pub fn check(&self, state: Option<Attempts>, now: Timestamp) -> Decision {
        match state {
            // A window that has elapsed is as good as no record: the count restarts.
            Some(s) if self.in_window(&s, now) && s.count >= self.max => Decision::Deny {
                retry_after_ms: (s.window_start + self.window_ms - now).max(0),
            },
            Some(s) if self.in_window(&s, now) => Decision::Allow {
                next: Attempts {
                    window_start: s.window_start,
                    count: s.count + 1,
                },
            },
            _ => Decision::Allow {
                next: Attempts::first(now),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Proceed, and record `next` — but only for a *failed* attempt. See [`Decision::next`].
    Allow {
        next: Attempts,
    },
    Deny {
        retry_after_ms: i64,
    },
}

impl Decision {
    pub fn allowed(&self) -> bool {
        matches!(self, Decision::Allow { .. })
    }

    /// The counter to persist, if the attempt turns out to have failed.
    ///
    /// Successful logins do not count against the limit — otherwise a busy shared address locks
    /// out the people using it correctly, and a limiter that punishes success is one an operator
    /// eventually turns off. The store clears the identity counter on success instead.
    pub fn next(&self) -> Option<Attempts> {
        match self {
            Decision::Allow { next } => Some(*next),
            Decision::Deny { .. } => None,
        }
    }

    pub fn retry_after_secs(&self) -> Option<i64> {
        match self {
            Decision::Deny { retry_after_ms } => Some((retry_after_ms + 999) / 1000),
            Decision::Allow { .. } => None,
        }
    }
}

/// The two buckets an attempt is checked against.
///
/// Identities are keyed by name rather than by user id on purpose: a login for an account that
/// does not exist has no id, and skipping the limiter for unknown names would make it free to
/// enumerate them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptKeys {
    pub identity: String,
    pub client: String,
}

impl AttemptKeys {
    /// `identity` is lowercased so `Alice` and `alice` share a bucket — otherwise case is a
    /// free way to multiply the limit.
    pub fn new(identity: &str, client: &str) -> Self {
        AttemptKeys {
            identity: format!("id:{}", identity.to_lowercase()),
            client: format!("ip:{client}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = 1_800_000_000_000;
    const L: Limit = Limit {
        max: 3,
        window_ms: 60_000,
    };

    #[test]
    fn attempts_are_allowed_up_to_the_limit_then_denied() {
        let mut state = None;
        for i in 1..=L.max {
            let d = L.check(state, NOW);
            assert!(d.allowed(), "attempt {i} denied early");
            state = d.next();
            assert_eq!(state.unwrap().count, i);
        }
        let d = L.check(state, NOW);
        assert!(!d.allowed(), "limit not enforced");
        assert_eq!(
            d.next(),
            None,
            "a denied attempt must not advance the counter"
        );
    }

    #[test]
    fn the_window_expires_and_the_count_restarts() {
        let exhausted = Some(Attempts {
            window_start: NOW,
            count: L.max,
        });
        assert!(!L.check(exhausted, NOW + L.window_ms - 1).allowed());
        // Exactly at the boundary the window is over.
        let d = L.check(exhausted, NOW + L.window_ms);
        assert!(d.allowed(), "window did not expire");
        assert_eq!(d.next().unwrap().count, 1, "count did not restart");
        assert_eq!(d.next().unwrap().window_start, NOW + L.window_ms);
    }

    #[test]
    fn retry_after_counts_down_and_is_never_negative() {
        let exhausted = Some(Attempts {
            window_start: NOW,
            count: L.max,
        });
        assert_eq!(L.check(exhausted, NOW).retry_after_secs(), Some(60));
        assert_eq!(
            L.check(exhausted, NOW + 30_000).retry_after_secs(),
            Some(30)
        );
        // Rounds up: "0 seconds" would invite an immediate retry that still fails.
        assert_eq!(L.check(exhausted, NOW + 59_999).retry_after_secs(), Some(1));
        assert!(L.check(exhausted, NOW).retry_after_secs().unwrap() > 0);
    }

    /// A clock that goes backwards must not hand out a fresh window.
    #[test]
    fn a_backwards_clock_does_not_reset_the_limit() {
        let exhausted = Some(Attempts {
            window_start: NOW,
            count: L.max,
        });
        let d = L.check(exhausted, NOW - 10_000);
        assert!(!d.allowed(), "an earlier timestamp reopened the window");
    }

    #[test]
    fn the_shipped_limits_are_sane() {
        const {
            assert!(Limit::PER_IDENTITY.max < Limit::PER_CLIENT.max);
            // A person mistyping a couple of times must not be locked out...
            assert!(Limit::PER_IDENTITY.max >= 3);
            // ...and a password list must not fit.
            assert!(Limit::PER_IDENTITY.max <= 10);
        };
    }

    #[test]
    fn identity_keys_fold_case_but_stay_separate_from_client_keys() {
        let a = AttemptKeys::new("Alice", "203.0.113.9");
        let b = AttemptKeys::new("alice", "203.0.113.9");
        assert_eq!(
            a.identity, b.identity,
            "case is a free way to multiply the limit"
        );
        assert_ne!(a.identity, a.client);
        // Namespaced, so a username can never collide with an address.
        assert!(a.identity.starts_with("id:") && a.client.starts_with("ip:"));
    }
}
