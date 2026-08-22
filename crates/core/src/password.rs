//! Password hashing — and the reason it does not fit a free Worker.
//!
//! # Measured, not assumed
//!
//! Password hashing is deliberately slow; the Workers free plan allows **10 ms of CPU per
//! request**. Those two facts do not reconcile. Measured in wasm under V8
//! (`scripts/kdf-bench.mjs`):
//!
//! | candidate | p50 | vs 10 ms budget |
//! |---|---|---|
//! | Argon2id, OWASP minimum (19 MiB, t=2) | 25.2 ms | **2.5x over** |
//! | Argon2id, RFC 9106 (64 MiB, t=3) | 134.9 ms | 13.5x over |
//! | PBKDF2-SHA256, OWASP minimum (600k) | 456.1 ms | 45.6x over |
//! | PBKDF2-SHA256 at workerd's 100k cap | 75.8 ms | 7.6x over |
//! | …the same via native WebCrypto | 12.7 ms | 1.3x over |
//!
//! Note the last two. `crypto.subtle` runs natively rather than in wasm and is six times
//! faster, but workerd caps PBKDF2 at 100,000 iterations to limit DoS
//! ([workerd#1346](https://github.com/cloudflare/workerd/issues/1346), open since 2023) — and
//! OWASP asks for 600,000. The fastest legal configuration is both **below** the recommended
//! strength and **over** the CPU budget.
//!
//! **There is no way to do OWASP-grade password login on a free-plan Worker.** That is a
//! property of the platform, not of this code.
//!
//! # What follows from that
//!
//! - **OIDC is the answer for a Workers deployment.** Verifying a signed assertion is a
//!   signature check, sub-millisecond, and the CPU problem disappears entirely.
//! - **The self-hosted target has no such limit** and uses [`Params::OWASP`] unchanged.
//! - [`Params::CONSTRAINED`] exists for a free Worker that insists on passwords. It is the
//!   strongest setting measured to fit (8 MiB, t=1, 5.98 ms) and it is **below OWASP**. It is
//!   never the default, and [`Params::is_below_recommended`] exists so a deployment can say so
//!   out loud.
//!
//! # Hashes carry their own parameters
//!
//! Hashes are stored in PHC string format — `$argon2id$v=19$m=19456,t=2,p=1$salt$hash`. The
//! algorithm and cost travel with the hash, so raising them later is a matter of rehashing on
//! next login rather than a migration that cannot work (the plaintext is gone).
//! [`needs_rehash`] is what makes that upgrade path real.

use argon2::{Algorithm, Argon2, Version};
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, Salt, SaltString};

/// Argon2id cost. Memory dominates: it is what makes a GPU attack expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Memory in KiB.
    pub m_kib: u32,
    /// Passes.
    pub t: u32,
    /// Lanes.
    pub p: u32,
}

impl Params {
    /// OWASP's minimum for Argon2id, and the default everywhere the CPU budget allows it.
    /// Measured at 25.2 ms — fine natively, 2.5x over a free Worker's limit.
    pub const OWASP: Params = Params {
        m_kib: 19456,
        t: 2,
        p: 1,
    };

    /// The strongest setting measured to fit the 10 ms free-plan budget: 5.98 ms.
    ///
    /// **Below OWASP**, by a factor of about 2.4 in memory. Present so that a constrained
    /// deployment is an explicit, visible choice rather than a silent downgrade; prefer OIDC.
    ///
    /// Memory over passes on purpose: at equal cost, `m=8192,t=1` resists parallel hardware
    /// better than `m=4096,t=3` (8.61 ms), which is the slower of the two anyway.
    pub const CONSTRAINED: Params = Params {
        m_kib: 8192,
        t: 1,
        p: 1,
    };

    /// Whether these parameters are weaker than OWASP's minimum.
    ///
    /// Not advisory: a deployment using [`Params::CONSTRAINED`] should surface this at startup.
    /// A weakened KDF that nobody mentions is how it stays weakened.
    pub const fn is_below_recommended(&self) -> bool {
        self.m_kib < Params::OWASP.m_kib || self.t < Params::OWASP.t
    }

    fn to_argon2(self) -> Result<Argon2<'static>, PasswordError> {
        let params = argon2::Params::new(self.m_kib, self.t, self.p, None)
            .map_err(|e| PasswordError::BadParams(e.to_string()))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for Params {
    fn default() -> Self {
        Params::OWASP
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    #[error("invalid argon2 parameters: {0}")]
    BadParams(String),
    #[error("stored hash is not a valid PHC string: {0}")]
    MalformedHash(String),
    #[error("salt is not valid base64: {0}")]
    BadSalt(String),
    #[error("hashing failed: {0}")]
    Hash(String),
}

/// Hash a password. `salt_b64` must be at least 16 bytes of CSPRNG output, base64 (no padding).
///
/// The salt is a parameter because `core` has no RNG on wasm — the same reason ids and
/// timestamps are passed in.
pub fn hash(password: &str, salt_b64: &str, params: Params) -> Result<String, PasswordError> {
    let salt = Salt::from_b64(salt_b64).map_err(|e| PasswordError::BadSalt(e.to_string()))?;
    params
        .to_argon2()?
        .hash_password(password.as_bytes(), salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))
}

/// Encode raw salt bytes for [`hash`].
pub fn encode_salt(bytes: &[u8]) -> Result<String, PasswordError> {
    SaltString::encode_b64(bytes)
        .map(|s| s.as_str().to_string())
        .map_err(|e| PasswordError::BadSalt(e.to_string()))
}

/// Verify a password against a stored PHC hash.
///
/// Returns `Ok(false)` for a wrong password and `Err` only when the *stored* hash is unusable —
/// those are different problems and only the second is a bug.
///
/// The comparison inside is constant-time; the work factor is read from the hash, so a hash
/// written under old parameters still verifies after the defaults change.
pub fn verify(password: &str, stored: &str) -> Result<bool, PasswordError> {
    let parsed =
        PasswordHash::new(stored).map_err(|e| PasswordError::MalformedHash(e.to_string()))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(PasswordError::MalformedHash(e.to_string())),
    }
}

/// Whether a stored hash was made with weaker parameters than `want`, and should be rewritten.
///
/// The upgrade path: on a successful login the plaintext is in hand for the only moment it ever
/// will be, so that is the one opportunity to rehash. Without this, raising the defaults leaves
/// every existing account on the old cost forever.
pub fn needs_rehash(stored: &str, want: Params) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        // Unreadable: rewriting it is the only way it becomes readable.
        return true;
    };
    if parsed.algorithm.as_str() != "argon2id" {
        return true;
    }
    let Ok(current) = argon2::Params::try_from(&parsed) else {
        return true;
    };
    current.m_cost() < want.m_kib || current.t_cost() < want.t
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cheap parameters: these tests exercise the plumbing, not the work factor.
    const FAST: Params = Params {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    const SALT: &str = "c29tZXNhbHR2YWx1ZTE"; // 16 bytes, base64 unpadded

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let h = hash("correct horse battery staple", SALT, FAST).unwrap();
        assert!(verify("correct horse battery staple", &h).unwrap());
        assert!(!verify("Correct horse battery staple", &h).unwrap());
        assert!(!verify("", &h).unwrap());
    }

    /// The parameters have to survive in the hash, or old accounts stop verifying the moment
    /// the defaults change.
    #[test]
    fn the_hash_carries_its_own_parameters() {
        let h = hash("pw", SALT, FAST).unwrap();
        assert!(
            h.starts_with("$argon2id$v=19$m=64,t=1,p=1$"),
            "unexpected PHC: {h}"
        );
        // Verifying does not depend on the caller knowing the cost.
        assert!(verify("pw", &h).unwrap());
    }

    #[test]
    fn the_same_password_hashes_differently_under_different_salts() {
        let a = hash("pw", SALT, FAST).unwrap();
        let b = hash("pw", "ZGlmZmVyZW50c2FsdDEy", FAST).unwrap();
        assert_ne!(a, b, "salt is not reaching the hash");
        assert!(verify("pw", &a).unwrap() && verify("pw", &b).unwrap());
    }

    #[test]
    fn a_wrong_password_is_false_not_an_error() {
        let h = hash("pw", SALT, FAST).unwrap();
        assert_eq!(verify("nope", &h), Ok(false));
        // Whereas an unusable stored hash is an error, because it is a bug not a login failure.
        assert!(verify("pw", "not-a-phc-string").is_err());
        assert!(verify("pw", "").is_err());
    }

    #[test]
    fn rehash_is_triggered_by_weaker_stored_parameters() {
        let weak = hash("pw", SALT, FAST).unwrap();
        assert!(needs_rehash(&weak, Params::OWASP));
        assert!(needs_rehash(&weak, Params::CONSTRAINED));
        assert!(
            !needs_rehash(&weak, FAST),
            "equal parameters need no rehash"
        );
        assert!(needs_rehash("garbage", Params::OWASP));
    }

    /// The constrained profile is a documented compromise, and this states its terms.
    #[test]
    fn constrained_is_weaker_than_owasp_and_says_so() {
        assert!(Params::CONSTRAINED.is_below_recommended());
        assert!(!Params::OWASP.is_below_recommended());
        assert_eq!(
            Params::default(),
            Params::OWASP,
            "the default must be the strong one"
        );
        const { assert!(Params::CONSTRAINED.m_kib < Params::OWASP.m_kib) };
    }

    #[test]
    fn bad_salts_are_rejected() {
        assert!(hash("pw", "!!!not base64!!!", FAST).is_err());
        assert!(
            hash("pw", "aa", FAST).is_err(),
            "a two-character salt was accepted"
        );
    }
}
