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

use core::fmt;

use argon2::{Algorithm, Argon2, Version};
use password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, Salt, SaltString};

/// A server-side secret mixed into every hash — Argon2's own `K` parameter, not a bolted-on
/// construction.
///
/// # What this buys, exactly
///
/// A pepper is stored **outside the database** — a Worker secret, or an environment variable —
/// so a leaked database does not contain it. Against the threat that actually matters here
/// (SQL injection, an exposed backup, one of D1's seven days of Time Travel snapshots), the
/// hashes are then uncrackable *whatever the KDF cost is*. That is what makes
/// [`Params::CONSTRAINED`] tolerable: the weak parameters only matter to an attacker who
/// already has both the data and the secret.
///
/// # What it does not buy
///
/// Nothing against a full server compromise, where both leak together. Nothing against online
/// guessing — that is rate limiting's job. And it is not a substitute for KDF cost: an attacker
/// with the pepper is back to attacking 8 MiB Argon2id.
///
/// Distinct from the salt, which is per-user, stored *with* the hash, and defeats precomputation
/// and batch cracking rather than adding strength to any single password.
#[derive(Clone)]
pub struct Pepper(Vec<u8>);

impl Pepper {
    /// Minimum 32 bytes. A short pepper is guessable, and a guessed pepper is no pepper.
    pub fn new(secret: &[u8]) -> Result<Self, PasswordError> {
        if secret.len() < 32 {
            return Err(PasswordError::WeakPepper(secret.len()));
        }
        Ok(Pepper(secret.to_vec()))
    }
}

/// Redacted: `derive(Debug)` is how secrets reach logs.
impl fmt::Debug for Pepper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pepper(<redacted>)")
    }
}

/// The peppers a deployment will accept, newest first.
///
/// Rotation is the reason this is a ring rather than a value. A pepper cannot be changed in
/// place — the hashes depend on it and the plaintexts are gone — so a rotation keeps the old one
/// for verification while [`verify`] reports that the hash should be rewritten. Logins migrate
/// accounts one at a time, and the old pepper is dropped once the tail is small enough to force
/// a reset.
///
/// `None` means unpeppered, which is what the self-hosted default is until an operator sets one.
#[derive(Debug, Clone, Default)]
pub struct PepperRing {
    pub current: Option<Pepper>,
    /// Accepted on verify, never used for new hashes.
    pub previous: Option<Pepper>,
}

impl PepperRing {
    pub fn none() -> Self {
        PepperRing::default()
    }

    pub fn single(pepper: Pepper) -> Self {
        PepperRing {
            current: Some(pepper),
            previous: None,
        }
    }

    pub fn rotating(current: Pepper, previous: Pepper) -> Self {
        PepperRing {
            current: Some(current),
            previous: Some(previous),
        }
    }

    fn argon2_for<'k>(
        &self,
        which: Option<&'k Pepper>,
        params: Params,
    ) -> Result<Argon2<'k>, PasswordError> {
        let p = argon2::Params::new(params.m_kib, params.t, params.p, None)
            .map_err(|e| PasswordError::BadParams(e.to_string()))?;
        match which {
            Some(pep) => Argon2::new_with_secret(&pep.0, Algorithm::Argon2id, Version::V0x13, p)
                .map_err(|e| PasswordError::BadParams(e.to_string())),
            None => Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, p)),
        }
    }
}

/// Why a verification succeeded, and whether the stored hash should be rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// Wrong password.
    No,
    /// Correct, and the stored hash is current.
    Yes,
    /// Correct, but the hash needs rewriting — weaker parameters, or the previous pepper.
    ///
    /// Login is the only moment the plaintext exists, so it is the only chance to migrate.
    YesRehash,
}

impl Verified {
    pub fn ok(&self) -> bool {
        !matches!(self, Verified::No)
    }
}

/// Shortest password accepted.
///
/// Length is the cheapest strength there is, and the only compensation that costs no CPU at
/// all. With [`Params::CONSTRAINED`] it matters more than usual: a weak KDF turns a weak
/// password into a solved one.
pub const MIN_PASSWORD_CHARS: usize = 12;

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

    /// The strongest setting that fits the 10 ms free-plan budget **at p95**: 4.33 ms, 43%.
    ///
    /// Chosen on p95 rather than median, because a request that overruns is a failed login, not
    /// a slow one. An earlier revision used 8 MiB after a three-sample run reported 5.98 ms;
    /// fifteen samples put its p95 at 11.99 ms — over budget. The neighbours are no better:
    /// 4 MiB t=2 is 9.12 ms (91%) and 6 MiB t=1 is 9.83 ms (98%), and the same request still has
    /// to render a page and talk to D1.
    ///
    /// **Below OWASP by ~4.7x in memory.** Present so a constrained deployment is an explicit,
    /// visible choice rather than a silent downgrade — see [`Params::is_below_recommended`] and
    /// pair it with a [`Pepper`], which restores the property that matters most.
    ///
    /// Measured on the development machine, not on Cloudflare's hardware. Worth re-checking
    /// against a deployed instance before relying on the headroom.
    pub const CONSTRAINED: Params = Params {
        m_kib: 4096,
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
    #[error("pepper must be at least 32 bytes, got {0}")]
    WeakPepper(usize),
    #[error("password must be at least {MIN_PASSWORD_CHARS} characters")]
    TooShort,
    #[error("stored hash is not a valid PHC string: {0}")]
    MalformedHash(String),
    #[error("salt is not valid base64: {0}")]
    BadSalt(String),
    #[error("hashing failed: {0}")]
    Hash(String),
}

/// Hash a password with the ring's current pepper.
///
/// `salt_b64` must be at least 16 bytes of CSPRNG output, base64 unpadded. The salt is a
/// parameter because `core` has no RNG on wasm — the same reason ids and timestamps are.
pub fn hash(
    password: &str,
    salt_b64: &str,
    params: Params,
    peppers: &PepperRing,
) -> Result<String, PasswordError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(PasswordError::TooShort);
    }
    let salt = Salt::from_b64(salt_b64).map_err(|e| PasswordError::BadSalt(e.to_string()))?;
    peppers
        .argon2_for(peppers.current.as_ref(), params)?
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

/// Verify a password, trying the current pepper and then the previous one.
///
/// Returns [`Verified::YesRehash`] when the password is right but the stored hash is stale —
/// weaker parameters, or the previous pepper. Login is the only moment the plaintext exists,
/// so it is the only chance to migrate the account.
///
/// `Err` means the *stored* hash is unusable, which is a bug rather than a failed login. A
/// wrong password is [`Verified::No`].
///
/// The work factor is read from the hash, so an account written under old parameters still
/// verifies after the defaults change.
pub fn verify(
    password: &str,
    stored: &str,
    want: Params,
    peppers: &PepperRing,
) -> Result<Verified, PasswordError> {
    let parsed =
        PasswordHash::new(stored).map_err(|e| PasswordError::MalformedHash(e.to_string()))?;

    for (pepper, is_current) in [
        (peppers.current.as_ref(), true),
        (peppers.previous.as_ref(), false),
    ] {
        // Skip the second attempt when there is no previous pepper, and never try unpeppered
        // as a fallback -- silently accepting an unpeppered hash would make the pepper optional
        // in practice.
        if !is_current && peppers.previous.is_none() {
            continue;
        }
        let argon = peppers.argon2_for(pepper, want)?;
        match argon.verify_password(password.as_bytes(), &parsed) {
            Ok(()) => {
                return Ok(if is_current && !needs_rehash(stored, want) {
                    Verified::Yes
                } else {
                    // Right password, stale hash: either the old pepper or weaker parameters.
                    Verified::YesRehash
                });
            }
            Err(password_hash::Error::Password) => continue,
            Err(e) => return Err(PasswordError::MalformedHash(e.to_string())),
        }
    }
    Ok(Verified::No)
}

/// Whether a stored hash was made with weaker parameters than `want`.
///
/// Does not consider the pepper — [`verify`] knows which pepper matched and folds that in.
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

    // Cheap parameters: these exercise the plumbing, not the work factor.
    const FAST: Params = Params {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    const SALT: &str = "c29tZXNhbHR2YWx1ZTE";
    const PW: &str = "correct horse battery staple";

    fn pepper(b: u8) -> Pepper {
        Pepper::new(&[b; 32]).unwrap()
    }

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let ring = PepperRing::none();
        let h = hash(PW, SALT, FAST, &ring).unwrap();
        assert_eq!(verify(PW, &h, FAST, &ring), Ok(Verified::Yes));
        assert_eq!(
            verify("Correct horse battery staple!", &h, FAST, &ring),
            Ok(Verified::No)
        );
    }

    #[test]
    fn the_hash_carries_its_own_parameters() {
        let h = hash(PW, SALT, FAST, &PepperRing::none()).unwrap();
        assert!(
            h.starts_with("$argon2id$v=19$m=64,t=1,p=1$"),
            "unexpected PHC: {h}"
        );
    }

    /// The whole point of a pepper: the stored hash is useless without the out-of-band secret.
    #[test]
    fn a_peppered_hash_does_not_verify_without_the_pepper() {
        let ring = PepperRing::single(pepper(1));
        let h = hash(PW, SALT, FAST, &ring).unwrap();
        assert_eq!(verify(PW, &h, FAST, &ring), Ok(Verified::Yes));
        // A leaked database, without the Worker secret, yields nothing.
        assert_eq!(verify(PW, &h, FAST, &PepperRing::none()), Ok(Verified::No));
        // Nor does the wrong pepper.
        assert_eq!(
            verify(PW, &h, FAST, &PepperRing::single(pepper(2))),
            Ok(Verified::No)
        );
    }

    /// Peppering must actually change the stored bytes, or it is doing nothing.
    #[test]
    fn the_pepper_reaches_the_hash() {
        let plain = hash(PW, SALT, FAST, &PepperRing::none()).unwrap();
        let a = hash(PW, SALT, FAST, &PepperRing::single(pepper(1))).unwrap();
        let b = hash(PW, SALT, FAST, &PepperRing::single(pepper(2))).unwrap();
        assert_ne!(plain, a);
        assert_ne!(a, b, "different peppers produced the same hash");
    }

    /// Rotation: the old pepper still verifies, and says the hash must be rewritten.
    #[test]
    fn rotation_accepts_the_previous_pepper_and_asks_for_a_rehash() {
        let old = PepperRing::single(pepper(1));
        let stored = hash(PW, SALT, FAST, &old).unwrap();

        let rotating = PepperRing::rotating(pepper(2), pepper(1));
        assert_eq!(
            verify(PW, &stored, FAST, &rotating),
            Ok(Verified::YesRehash)
        );
        // A wrong password is still wrong under either pepper.
        assert_eq!(
            verify("wrong password here", &stored, FAST, &rotating),
            Ok(Verified::No)
        );

        // Once rewritten under the new pepper, no further rehash is asked for.
        let rewritten = hash(PW, SALT, FAST, &rotating).unwrap();
        assert_eq!(verify(PW, &rewritten, FAST, &rotating), Ok(Verified::Yes));
        // And the old pepper alone no longer opens it.
        assert_eq!(verify(PW, &rewritten, FAST, &old), Ok(Verified::No));
    }

    /// An unpeppered hash must not quietly pass once a pepper is configured, or the pepper is
    /// optional in practice and every old account stays unprotected.
    #[test]
    fn an_unpeppered_hash_is_rejected_once_a_pepper_is_set() {
        let stored = hash(PW, SALT, FAST, &PepperRing::none()).unwrap();
        assert_eq!(
            verify(PW, &stored, FAST, &PepperRing::single(pepper(1))),
            Ok(Verified::No)
        );
    }

    #[test]
    fn weaker_stored_parameters_ask_for_a_rehash() {
        let ring = PepperRing::none();
        let weak = hash(PW, SALT, FAST, &ring).unwrap();
        assert!(needs_rehash(&weak, Params::OWASP));
        assert!(!needs_rehash(&weak, FAST));
        assert!(needs_rehash("garbage", Params::OWASP));
        // Verify reports it rather than making the caller ask separately.
        assert_eq!(
            verify(PW, &weak, Params::OWASP, &ring),
            Ok(Verified::YesRehash)
        );
    }

    #[test]
    fn a_wrong_password_is_a_verdict_not_an_error() {
        let ring = PepperRing::none();
        let h = hash(PW, SALT, FAST, &ring).unwrap();
        assert_eq!(verify("nope nope nope", &h, FAST, &ring), Ok(Verified::No));
        // Whereas an unusable stored hash is a bug and says so.
        assert!(verify(PW, "not-a-phc-string", FAST, &ring).is_err());
        assert!(verify(PW, "", FAST, &ring).is_err());
    }

    #[test]
    fn short_passwords_and_short_peppers_are_refused() {
        let ring = PepperRing::none();
        assert_eq!(
            hash("short", SALT, FAST, &ring),
            Err(PasswordError::TooShort)
        );
        assert_eq!(
            hash(&"a".repeat(MIN_PASSWORD_CHARS - 1), SALT, FAST, &ring),
            Err(PasswordError::TooShort)
        );
        assert!(hash(&"a".repeat(MIN_PASSWORD_CHARS), SALT, FAST, &ring).is_ok());
        assert!(matches!(
            Pepper::new(&[0u8; 31]),
            Err(PasswordError::WeakPepper(31))
        ));
        assert!(Pepper::new(&[0u8; 32]).is_ok());
    }

    #[test]
    fn the_same_password_hashes_differently_under_different_salts() {
        let ring = PepperRing::none();
        let a = hash(PW, SALT, FAST, &ring).unwrap();
        let b = hash(PW, "ZGlmZmVyZW50c2FsdDEy", FAST, &ring).unwrap();
        assert_ne!(a, b, "salt is not reaching the hash");
    }

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
    fn secrets_are_redacted_in_debug_output() {
        let shown = format!("{:?}", pepper(0xAB));
        assert!(
            !shown.contains("171") && !shown.contains("ab"),
            "pepper leaked: {shown}"
        );
        assert!(format!("{:?}", PepperRing::single(pepper(1))).contains("redacted"));
    }
}
