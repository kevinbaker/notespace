//! CSRF tokens: signed and stateless, so no storage round trip per form.
//!
//! `<expiry-ms>.<hex HMAC-SHA256(secret, session_hash || "." || expiry)>`
//!
//! Binding to the session rather than to the server secret alone is what stops one visitor's
//! token being replayed by another. Kept independent of `SameSite=Lax`, which does not cover
//! same-site subdomain takeover.

use core::fmt;

use hmac::{Mac, SimpleHmac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::model::Timestamp;
use crate::session::TokenHash;

/// Long enough to write a post, short enough that one scraped from a cached page is useless.
pub const DEFAULT_LIFETIME_MS: i64 = 4 * 60 * 60 * 1000;

/// The server-side signing key. Distinct from any session secret.
#[derive(Clone)]
pub struct CsrfKey(Vec<u8>);

impl CsrfKey {
    /// At least 32 bytes, from the same CSPRNG that mints session tokens.
    pub fn new(secret: &[u8]) -> Result<Self, CsrfError> {
        if secret.len() < 32 {
            return Err(CsrfError::WeakKey(secret.len()));
        }
        Ok(CsrfKey(secret.to_vec()))
    }

    fn sign(&self, binding: &str, expires_at: Timestamp) -> String {
        let mut mac = SimpleHmac::<Sha256>::new_from_slice(&self.0).expect("hmac accepts any key");
        mac.update(binding.as_bytes());
        mac.update(b".");
        mac.update(expires_at.to_string().as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    /// `binding` identifies the visitor: a session's [`TokenHash`], or an anonymous cookie on
    /// the forms that run before there is a session.
    pub fn mint(&self, binding: &str, now: Timestamp, lifetime_ms: i64) -> CsrfToken {
        let expires_at = now + lifetime_ms;
        CsrfToken(format!("{expires_at}.{}", self.sign(binding, expires_at)))
    }

    /// Mint for a logged-in visitor.
    pub fn mint_for_session(
        &self,
        session: &TokenHash,
        now: Timestamp,
        lifetime_ms: i64,
    ) -> CsrfToken {
        self.mint(session.as_str(), now, lifetime_ms)
    }

    /// Sign a payload with an expiry, for a cookie the server has to trust on the way back:
    /// `<expiry>.<base64url payload>.<hex HMAC>`. The payload is readable by the holder, which
    /// is fine for what goes in one (an OAuth state, a provider's assertion about the visitor);
    /// what matters is that it cannot be altered.
    pub fn seal(&self, payload: &[u8], now: Timestamp, lifetime_ms: i64) -> String {
        let expires_at = now + lifetime_ms;
        let body = crate::encoding::base64url(payload);
        let sig = self.sign(&format!("seal.{body}"), expires_at);
        format!("{expires_at}.{body}.{sig}")
    }

    /// The payload back, if the signature holds and it has not expired.
    pub fn open(&self, sealed: &str, now: Timestamp) -> Result<Vec<u8>, CsrfError> {
        let mut parts = sealed.splitn(3, '.');
        let (Some(exp), Some(body), Some(sig)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(CsrfError::Invalid);
        };
        let expires_at: Timestamp = exp.parse().map_err(|_| CsrfError::Invalid)?;
        let expected = self.sign(&format!("seal.{body}"), expires_at);
        if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
            return Err(CsrfError::Invalid);
        }
        if now >= expires_at {
            return Err(CsrfError::Invalid);
        }
        crate::encoding::base64url_decode(body).ok_or(CsrfError::Invalid)
    }

    /// Every failure looks the same, so "expired" cannot be told from "forged".
    pub fn verify(&self, token: &str, binding: &str, now: Timestamp) -> Result<(), CsrfError> {
        let (expiry, mac) = token.split_once('.').ok_or(CsrfError::Invalid)?;
        let expires_at: Timestamp = expiry.parse().map_err(|_| CsrfError::Invalid)?;
        if expires_at <= now {
            return Err(CsrfError::Invalid);
        }
        let expected = self.sign(binding, expires_at);
        // Constant time: a short-circuiting compare forges a MAC one byte at a time.
        if expected.as_bytes().ct_eq(mac.as_bytes()).into() {
            Ok(())
        } else {
            Err(CsrfError::Invalid)
        }
    }
}

/// Redacted: `derive(Debug)` is how secrets reach logs.
impl fmt::Debug for CsrfKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsrfToken(String);

impl CsrfToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CsrfError {
    #[error("csrf key must be at least 32 bytes, got {0}")]
    WeakKey(usize),
    #[error("csrf token is missing, malformed, expired, or does not match this session")]
    Invalid,
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionToken, TOKEN_BYTES};

    const NOW: Timestamp = 1_800_000_000_000;

    fn key() -> CsrfKey {
        CsrfKey::new(&[7u8; 32]).unwrap()
    }
    fn sess(seed: u8) -> String {
        SessionToken::from_bytes([seed; TOKEN_BYTES])
            .hash()
            .as_str()
            .to_string()
    }

    #[test]
    fn a_minted_token_verifies() {
        let (k, s) = (key(), sess(1));
        let t = k.mint(&s, NOW, DEFAULT_LIFETIME_MS);
        assert_eq!(k.verify(t.as_str(), &s, NOW), Ok(()));
        assert_eq!(k.verify(t.as_str(), &s, NOW + 1000), Ok(()));
    }

    /// The login form has no session, so a token has to bind to something else.
    #[test]
    fn a_token_can_bind_to_an_anonymous_cookie() {
        let k = key();
        let t = k.mint("anon-cookie-value-abc", NOW, DEFAULT_LIFETIME_MS);
        assert_eq!(k.verify(t.as_str(), "anon-cookie-value-abc", NOW), Ok(()));
        // Still bound: another visitor's cookie does not open it.
        assert_eq!(
            k.verify(t.as_str(), "anon-cookie-value-xyz", NOW),
            Err(CsrfError::Invalid)
        );
    }

    #[test]
    fn a_token_is_bound_to_its_session() {
        let k = key();
        let t = k.mint(&sess(1), NOW, DEFAULT_LIFETIME_MS);
        assert_eq!(k.verify(t.as_str(), &sess(2), NOW), Err(CsrfError::Invalid));
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let (k, s) = (key(), sess(1));
        let t = k.mint(&s, NOW, 1000);
        assert_eq!(k.verify(t.as_str(), &s, NOW + 999), Ok(()));
        assert_eq!(
            k.verify(t.as_str(), &s, NOW + 1000),
            Err(CsrfError::Invalid)
        );
    }

    #[test]
    fn tampering_is_rejected() {
        let (k, s) = (key(), sess(1));
        let t = k.mint(&s, NOW, DEFAULT_LIFETIME_MS).as_str().to_string();
        let (exp, mac) = t.split_once('.').unwrap();
        for bad in [
            String::new(),
            "nodot".into(),
            format!("{exp}."),
            format!("{}.{mac}", NOW + DEFAULT_LIFETIME_MS * 10),
            // One flipped MAC character.
            format!(
                "{exp}.{}{}",
                if mac.starts_with('a') { 'b' } else { 'a' },
                &mac[1..]
            ),
            // A different key's signature.
            CsrfKey::new(&[9u8; 32])
                .unwrap()
                .mint(&s, NOW, DEFAULT_LIFETIME_MS)
                .as_str()
                .into(),
        ] {
            assert_eq!(
                k.verify(&bad, &s, NOW),
                Err(CsrfError::Invalid),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn short_keys_are_refused() {
        assert!(matches!(
            CsrfKey::new(&[0u8; 31]),
            Err(CsrfError::WeakKey(31))
        ));
        assert!(CsrfKey::new(&[0u8; 32]).is_ok());
    }
}

#[cfg(test)]
mod seal_tests {
    use super::*;

    #[test]
    fn a_sealed_payload_opens_only_intact_and_in_time() {
        let key = CsrfKey::new(&[7u8; 32]).unwrap();
        let sealed = key.seal(b"{\"state\":\"abc\"}", 1000, 500);
        assert_eq!(key.open(&sealed, 1200).unwrap(), b"{\"state\":\"abc\"}");
        assert!(key.open(&sealed, 1500).is_err(), "opened after expiry");
        let tampered = sealed.replacen("abc", "abd", 1);
        assert!(
            key.open(&tampered, 1200).is_err()
                || key.open(&tampered, 1200).unwrap() == b"{\"state\":\"abc\"}"
        );
        let mut forged = sealed.clone();
        forged.replace_range(sealed.len() - 2.., "00");
        assert!(key.open(&forged, 1200).is_err(), "accepted a bad signature");
        let other = CsrfKey::new(&[8u8; 32]).unwrap();
        assert!(other.open(&sealed, 1200).is_err(), "another key opened it");
    }
}
