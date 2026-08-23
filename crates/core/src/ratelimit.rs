//! Rate limiting for authentication attempts, checked before the hash — an attacker pays nothing
//! for a wrong guess but the server pays a full KDF run.
//!
//! Two buckets, both of which an attempt has to pass: per identity, against a password list, and
//! per client, against one password sprayed across many accounts. Windows are fixed rather than
//! sliding, so straddling a boundary allows `2 * limit` in quick succession.

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
    /// Five tries per fifteen minutes.
    pub const PER_IDENTITY: Limit = Limit {
        max: 5,
        window_ms: 15 * 60 * 1000,
    };

    /// Sixty per fifteen minutes: looser, because an office or carrier is one address.
    pub const PER_CLIENT: Limit = Limit {
        max: 60,
        window_ms: 15 * 60 * 1000,
    };

    /// Whether `state` is still inside this window.
    fn in_window(&self, state: &Attempts, now: Timestamp) -> bool {
        now - state.window_start < self.window_ms
    }

    /// `state` is what the store holds, or `None` if nothing is recorded.
    pub fn check(&self, state: Option<Attempts>, now: Timestamp) -> Decision {
        match state {
            // An elapsed window is as good as no record.
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

    /// Only failures persist: counting successes would lock out a busy shared address. The store
    /// clears the identity counter on success instead.
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

/// Keyed by name, not user id, so an attempt against an unknown account is still limited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptKeys {
    pub identity: String,
    pub client: String,
}

impl AttemptKeys {
    /// `identity` is lowercased, so case cannot multiply the limit.
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
        // Rounds up, so the caller is never told to retry immediately.
        assert_eq!(L.check(exhausted, NOW + 59_999).retry_after_secs(), Some(1));
        assert!(L.check(exhausted, NOW).retry_after_secs().unwrap() > 0);
    }

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
            assert!(Limit::PER_IDENTITY.max >= 3);
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
        assert!(a.identity.starts_with("id:") && a.client.starts_with("ip:"));
    }
}
