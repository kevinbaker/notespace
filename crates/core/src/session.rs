//! Session tokens and expiry policy. The cookie carries 256 random bits and the database holds
//! only their SHA-256; [`Store`](crate::store::Store) accepts a [`TokenHash`], whose only
//! constructor is [`SessionToken::hash`].
//!
//! Expiry slides but refreshes at most once a day, keeping a sliding session at about one write
//! per user per day rather than one per request.

use core::fmt;

use sha2::{Digest, Sha256};

use crate::model::{Timestamp, UserId};

/// Bytes of randomness in a token. 256 bits, from the platform CSPRNG.
pub const TOKEN_BYTES: usize = 32;

/// Not `Serialize` or `Display`, and `Debug` is redacted: derive macros are how secrets leak.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl SessionToken {
    /// The caller supplies the bytes, because `core` has no RNG.
    pub fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Self {
        SessionToken(bytes)
    }

    /// Parse the cookie value. Rejects anything not exactly the right length.
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = hex_decode(s)?;
        (bytes.len() == TOKEN_BYTES).then(|| {
            let mut buf = [0u8; TOKEN_BYTES];
            buf.copy_from_slice(&bytes);
            SessionToken(buf)
        })
    }

    /// The cookie value.
    pub fn to_cookie_value(&self) -> String {
        hex_encode(&self.0)
    }

    /// The only route from a token to something storable.
    pub fn hash(&self) -> TokenHash {
        let digest = Sha256::digest(self.0);
        TokenHash(hex_encode(&digest))
    }
}

/// Redacted: a token that shows up in a log is a token that leaked.
impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

/// SHA-256 of a [`SessionToken`], hex. Safe to store and to log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenHash(String);

impl TokenHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub token_hash: TokenHash,
    pub user_id: UserId,
    pub created_at: Timestamp,
    pub refreshed_at: Timestamp,
    pub expires_at: Timestamp,
}

/// How long sessions last and how often they are extended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    /// How far ahead of now a session expires when created or refreshed.
    pub lifetime_ms: i64,
    /// Without this, sliding expiry is a write on every authenticated request.
    pub refresh_after_ms: i64,
}

impl Default for SessionPolicy {
    /// 30-day lifetime, refreshed at most once a day.
    fn default() -> Self {
        SessionPolicy {
            lifetime_ms: 30 * 24 * 60 * 60 * 1000,
            refresh_after_ms: 24 * 60 * 60 * 1000,
        }
    }
}

impl SessionPolicy {
    pub fn expiry_from(&self, now: Timestamp) -> Timestamp {
        now + self.lifetime_ms
    }

    /// False for an already-expired session, which is a logout rather than a refresh.
    pub fn should_refresh(&self, session: &Session, now: Timestamp) -> bool {
        !self.is_expired(session, now) && now - session.refreshed_at >= self.refresh_after_ms
    }

    pub fn is_expired(&self, session: &Session, now: Timestamp) -> bool {
        session.expires_at <= now
    }
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn hex_decode(s: &str) -> Option<Vec<u8>> {
    // `is_multiple_of` is stable only from 1.87; the workspace MSRV is 1.82.
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    (0..b.len() / 2)
        .map(|i| {
            let hi = (b[i * 2] as char).to_digit(16)?;
            let lo = (b[i * 2 + 1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(seed: u8) -> SessionToken {
        SessionToken::from_bytes([seed; TOKEN_BYTES])
    }

    #[test]
    fn tokens_round_trip_through_the_cookie() {
        let t = token(0xAB);
        let cookie = t.to_cookie_value();
        assert_eq!(cookie.len(), TOKEN_BYTES * 2);
        assert_eq!(SessionToken::parse(&cookie), Some(t));
    }

    #[test]
    fn malformed_cookies_are_rejected() {
        for bad in [
            "",
            "zz",
            "ab",
            &"a".repeat(63),
            &"a".repeat(65),
            &"g".repeat(64),
        ] {
            assert!(SessionToken::parse(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_hash_is_not_the_token() {
        let t = token(1);
        let h = t.hash();
        assert_ne!(h.as_str(), t.to_cookie_value());
        assert_eq!(h.as_str().len(), 64);
        assert_eq!(t.hash(), token(1).hash());
        assert_ne!(t.hash(), token(2).hash());
    }

    #[test]
    fn debug_does_not_reveal_the_token() {
        let t = token(0xFF);
        let shown = format!("{t:?}");
        assert!(!shown.contains(&t.to_cookie_value()));
        assert!(!shown.contains("ffff"));
    }

    #[test]
    fn refresh_happens_at_most_once_per_interval() {
        let p = SessionPolicy::default();
        let now = 1_800_000_000_000;
        let s = Session {
            token_hash: token(1).hash(),
            user_id: 1,
            created_at: now,
            refreshed_at: now,
            expires_at: p.expiry_from(now),
        };
        assert!(!p.should_refresh(&s, now), "refreshed immediately");
        assert!(
            !p.should_refresh(&s, now + p.refresh_after_ms - 1),
            "refreshed before the interval elapsed"
        );
        assert!(p.should_refresh(&s, now + p.refresh_after_ms));
    }

    #[test]
    fn an_expired_session_is_never_refreshed() {
        let p = SessionPolicy::default();
        let now = 1_800_000_000_000;
        let s = Session {
            token_hash: token(1).hash(),
            user_id: 1,
            created_at: now,
            refreshed_at: now,
            expires_at: now + 1000,
        };
        let later = now + p.lifetime_ms * 2;
        assert!(p.is_expired(&s, later));
        assert!(!p.should_refresh(&s, later));
        // Exactly at the boundary it is already gone.
        assert!(p.is_expired(&s, s.expires_at));
    }
}
