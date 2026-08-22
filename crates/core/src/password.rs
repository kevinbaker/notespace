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
use std::collections::BTreeMap;

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

/// Every pepper a deployment has ever used, keyed by id.
///
/// # Why the id, and not just a list
///
/// Verifying a *wrong* password against a list means trying each pepper in turn, and each try is
/// a full Argon2 run. Measured at [`Params::CONSTRAINED`], that is 3.34 ms per attempt: two
/// peppers fit the 10 ms budget, three do not (10.03 ms). A list therefore caps rotation at one
/// generation, on the failed-login path specifically — the path an attacker controls.
///
/// So the stored hash records which pepper made it, and verification looks up exactly that one.
/// Cost is constant no matter how many are held, which is what makes keeping all of them
/// practical: a pepper never has to be retired on a deadline, and no account is ever stranded by
/// a rotation that finished before it came back.
///
/// # Storage format
///
/// `<id>$<phc>`, e.g. `3$argon2id$v=19$m=4096,t=1,p=1$…`. An empty prefix (`$argon2id$…`, which
/// is what PHC produces on its own) means unpeppered. Unambiguous because a PHC string always
/// begins with `$`.
#[derive(Debug, Clone, Default)]
pub struct PepperSet {
    peppers: BTreeMap<u32, Pepper>,
    /// Which one new hashes use. `None` means unpeppered.
    current: Option<u32>,
}

impl PepperSet {
    /// No peppers. What the self-hosted default is until an operator sets one.
    pub fn none() -> Self {
        PepperSet::default()
    }

    /// Add a pepper. The highest id added with `make_current` wins for new hashes.
    ///
    /// Ids must be stable across deploys: they are recorded in every hash written under them.
    /// Reusing an id for a different secret strands every account that used the old one.
    pub fn insert(&mut self, id: u32, pepper: Pepper, make_current: bool) -> &mut Self {
        self.peppers.insert(id, pepper);
        if make_current {
            self.current = Some(id);
        }
        self
    }

    /// Parse the deployment's whole pepper history from one string.
    ///
    /// Format: `1=<secret>;2=<secret>;…`. Whitespace around entries is ignored, a trailing
    /// `;` is allowed, and **the highest id is the current one** — so rotating means appending
    /// an entry, and nothing else moves.
    ///
    /// One variable rather than one per pepper because the set is a single fact about the
    /// deployment: a scan over numbered bindings cannot tell "id 3 was never used" from "id 3
    /// failed to load", and silently holding fewer peppers than intended strands accounts.
    ///
    /// Every failure here is fatal by design. A pepper set that is *partly* right is the worst
    /// outcome available — it authenticates some accounts and permanently rejects others.
    pub fn parse(spec: &str) -> Result<PepperSet, PasswordError> {
        let mut set = PepperSet::none();
        let mut highest: Option<u32> = None;
        for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let (id, secret) = entry.split_once('=').ok_or_else(|| {
                PasswordError::BadPepperSpec(
                    "each entry must be `<id>=<secret>`, e.g. `1=<64 hex chars>`".into(),
                )
            })?;
            let id: u32 = id.trim().parse().map_err(|_| {
                PasswordError::BadPepperSpec(format!("{:?} is not a pepper id", id.trim()))
            })?;
            let secret = secret.trim();
            if set.peppers.contains_key(&id) {
                // Two secrets under one id means half the accounts cannot be verified, and
                // which half depends on parse order. Never guess.
                return Err(PasswordError::BadPepperSpec(format!(
                    "pepper id {id} appears twice"
                )));
            }
            let pepper = Pepper::new(secret.as_bytes()).map_err(|_| {
                PasswordError::BadPepperSpec(format!(
                    "pepper {id} is {} bytes; at least 32 are required",
                    secret.len()
                ))
            })?;
            if looks_unrandom(secret) {
                return Err(PasswordError::BadPepperSpec(format!(
                    "pepper {id} looks like a placeholder rather than a generated secret"
                )));
            }
            set.peppers.insert(id, pepper);
            highest = Some(highest.map_or(id, |h| h.max(id)));
        }
        if set.peppers.is_empty() {
            return Err(PasswordError::BadPepperSpec("no peppers configured".into()));
        }
        set.current = highest;
        Ok(set)
    }

    /// A single pepper, used for new hashes.
    pub fn single(id: u32, pepper: Pepper) -> Self {
        let mut s = PepperSet::none();
        s.insert(id, pepper, true);
        s
    }

    pub fn current_id(&self) -> Option<u32> {
        self.current
    }

    pub fn len(&self) -> usize {
        self.peppers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peppers.is_empty()
    }

    fn get(&self, id: u32) -> Option<&Pepper> {
        self.peppers.get(&id)
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

/// Split a stored value into its pepper id and the PHC string.
///
/// `None` id means the hash is unpeppered.
/// Catch a secret that is the right length but obviously not generated.
///
/// Cheap, and it catches the copy-paste a length check waves through.
fn looks_unrandom(s: &str) -> bool {
    let b = s.as_bytes();
    b.iter().all(|c| *c == b[0])
        || s.to_ascii_lowercase().contains("changeme")
        || s.to_ascii_lowercase().contains("example")
        || s.to_ascii_lowercase().contains("your-secret")
}

fn split_stored(stored: &str) -> Result<(Option<u32>, &str), PasswordError> {
    if stored.starts_with('$') {
        return Ok((None, stored));
    }
    let (id, phc) = stored
        .split_once('$')
        .ok_or_else(|| PasswordError::MalformedHash("no PHC section".into()))?;
    let id: u32 = id
        .parse()
        .map_err(|_| PasswordError::MalformedHash(format!("bad pepper id {id:?}")))?;
    // `split_once` ate the `$` that PHC needs to start with.
    let start = stored.len() - phc.len() - 1;
    Ok((Some(id), &stored[start..]))
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
    /// The hash names a pepper this deployment does not hold. Not a failed login: the account
    /// cannot be verified at all and needs a reset.
    #[error("hash was made with pepper {0}, which is not configured")]
    UnknownPepper(u32),
    #[error("PASSWORD_PEPPER is malformed: {0}")]
    BadPepperSpec(String),
}

/// Hash a password with the ring's current pepper.
///
/// `salt_b64` must be at least 16 bytes of CSPRNG output, base64 unpadded. The salt is a
/// parameter because `core` has no RNG on wasm — the same reason ids and timestamps are.
pub fn hash(
    password: &str,
    salt_b64: &str,
    params: Params,
    peppers: &PepperSet,
) -> Result<String, PasswordError> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(PasswordError::TooShort);
    }
    let salt = Salt::from_b64(salt_b64).map_err(|e| PasswordError::BadSalt(e.to_string()))?;
    let current = peppers.current;
    let pepper = match current {
        Some(id) => Some(peppers.get(id).ok_or(PasswordError::UnknownPepper(id))?),
        None => None,
    };
    let phc = peppers
        .argon2_for(pepper, params)?
        .hash_password(password.as_bytes(), salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))?;
    // Record which pepper made this, so verification tries exactly one.
    Ok(match current {
        Some(id) => format!("{id}{phc}"),
        None => phc,
    })
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
    peppers: &PepperSet,
) -> Result<Verified, PasswordError> {
    let (hash_pepper_id, phc) = split_stored(stored)?;
    let parsed = PasswordHash::new(phc).map_err(|e| PasswordError::MalformedHash(e.to_string()))?;

    // Exactly one lookup, and therefore exactly one Argon2 run, however many peppers are held.
    let pepper = match hash_pepper_id {
        Some(id) => Some(peppers.get(id).ok_or(PasswordError::UnknownPepper(id))?),
        None => None,
    };
    let argon = peppers.argon2_for(pepper, want)?;

    match argon.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => {
            let stale_pepper = hash_pepper_id != peppers.current;
            Ok(if stale_pepper || needs_rehash(stored, want) {
                Verified::YesRehash
            } else {
                Verified::Yes
            })
        }
        Err(password_hash::Error::Password) => Ok(Verified::No),
        Err(e) => Err(PasswordError::MalformedHash(e.to_string())),
    }
}

/// Whether a stored hash was made with weaker parameters than `want`.
///
/// Does not consider the pepper — [`verify`] knows which one the hash names and folds that in.
pub fn needs_rehash(stored: &str, want: Params) -> bool {
    let Ok((_, phc)) = split_stored(stored) else {
        return true;
    };
    let Ok(parsed) = PasswordHash::new(phc) else {
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

    /// Every pepper ever used, which is the point: none has to be retired on a deadline.
    fn many() -> PepperSet {
        let mut s = PepperSet::none();
        for id in 1..=8u32 {
            s.insert(id, pepper(id as u8), id == 8);
        }
        s
    }

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let ring = PepperSet::none();
        let h = hash(PW, SALT, FAST, &ring).unwrap();
        assert_eq!(verify(PW, &h, FAST, &ring), Ok(Verified::Yes));
        assert_eq!(
            verify("Correct horse battery staple!", &h, FAST, &ring),
            Ok(Verified::No)
        );
    }

    /// The whole point of a pepper: the stored hash is useless without the out-of-band secret.
    #[test]
    fn a_peppered_hash_does_not_verify_without_the_pepper() {
        let set = PepperSet::single(1, pepper(1));
        let h = hash(PW, SALT, FAST, &set).unwrap();
        assert_eq!(verify(PW, &h, FAST, &set), Ok(Verified::Yes));
        // A leaked database without the secret names a pepper nobody holds.
        assert!(matches!(
            verify(PW, &h, FAST, &PepperSet::none()),
            Err(PasswordError::UnknownPepper(1))
        ));
        // The wrong secret under the right id is simply a failed login.
        assert_eq!(
            verify(PW, &h, FAST, &PepperSet::single(1, pepper(99))),
            Ok(Verified::No)
        );
    }

    #[test]
    fn the_stored_hash_names_its_pepper() {
        let h = hash(PW, SALT, FAST, &PepperSet::single(7, pepper(7))).unwrap();
        assert!(h.starts_with("7$argon2id$"), "no pepper id in {h}");
        assert_eq!(split_stored(&h).unwrap().0, Some(7));
        // Unpeppered hashes are plain PHC and stay recognisable as such.
        let plain = hash(PW, SALT, FAST, &PepperSet::none()).unwrap();
        assert!(plain.starts_with("$argon2id$"));
        assert_eq!(split_stored(&plain).unwrap().0, None);
    }

    /// Holding many peppers must not cost anything at verify time — one lookup, one Argon2 run.
    #[test]
    fn any_pepper_in_the_set_verifies_regardless_of_how_many_there_are() {
        let set = many();
        assert_eq!(set.len(), 8);
        // A hash from each historical pepper still opens, including the oldest.
        for id in 1..=8u32 {
            let older = PepperSet::single(id, pepper(id as u8));
            let h = hash(PW, SALT, FAST, &older).unwrap();
            let expect = if id == set.current_id().unwrap() {
                Verified::Yes
            } else {
                // Right password, old pepper: rewrite it under the current one.
                Verified::YesRehash
            };
            assert_eq!(verify(PW, &h, FAST, &set), Ok(expect), "pepper {id}");
        }
    }

    /// A wrong password must not walk the whole set — that is the CPU budget blowing up.
    #[test]
    fn a_wrong_password_costs_one_attempt_not_one_per_pepper() {
        let set = many();
        let h = hash(PW, SALT, FAST, &PepperSet::single(3, pepper(3))).unwrap();
        // Only pepper 3 is consulted; the other seven are never touched.
        assert_eq!(
            verify("wrong password here", &h, FAST, &set),
            Ok(Verified::No)
        );
        assert_eq!(split_stored(&h).unwrap().0, Some(3));
    }

    /// Reusing an id for a different secret strands accounts, so it must not silently pass.
    #[test]
    fn a_hash_naming_an_absent_pepper_is_an_error_not_a_failed_login() {
        let h = hash(PW, SALT, FAST, &PepperSet::single(42, pepper(42))).unwrap();
        let without = PepperSet::single(1, pepper(1));
        assert!(matches!(
            verify(PW, &h, FAST, &without),
            Err(PasswordError::UnknownPepper(42))
        ));
    }

    #[test]
    fn rotation_asks_for_a_rehash_and_settles_after_one() {
        let old = PepperSet::single(1, pepper(1));
        let stored = hash(PW, SALT, FAST, &old).unwrap();

        let mut rotated = PepperSet::none();
        rotated
            .insert(1, pepper(1), false)
            .insert(2, pepper(2), true);

        assert_eq!(verify(PW, &stored, FAST, &rotated), Ok(Verified::YesRehash));
        assert_eq!(
            verify("wrong password here", &stored, FAST, &rotated),
            Ok(Verified::No)
        );

        let rewritten = hash(PW, SALT, FAST, &rotated).unwrap();
        assert!(rewritten.starts_with("2$"));
        assert_eq!(verify(PW, &rewritten, FAST, &rotated), Ok(Verified::Yes));
    }

    #[test]
    fn the_pepper_reaches_the_hash() {
        let plain = hash(PW, SALT, FAST, &PepperSet::none()).unwrap();
        let a = hash(PW, SALT, FAST, &PepperSet::single(1, pepper(1))).unwrap();
        let b = hash(PW, SALT, FAST, &PepperSet::single(1, pepper(2))).unwrap();
        assert_ne!(plain, a);
        assert_ne!(
            a, b,
            "different peppers under the same id produced the same hash"
        );
    }

    #[test]
    fn weaker_stored_parameters_ask_for_a_rehash() {
        let set = PepperSet::none();
        let weak = hash(PW, SALT, FAST, &set).unwrap();
        assert!(needs_rehash(&weak, Params::OWASP));
        assert!(!needs_rehash(&weak, FAST));
        assert!(needs_rehash("garbage", Params::OWASP));
        assert_eq!(
            verify(PW, &weak, Params::OWASP, &set),
            Ok(Verified::YesRehash)
        );
    }

    #[test]
    fn a_wrong_password_is_a_verdict_not_an_error() {
        let set = PepperSet::none();
        let h = hash(PW, SALT, FAST, &set).unwrap();
        assert_eq!(verify("nope nope nope", &h, FAST, &set), Ok(Verified::No));
        assert!(verify(PW, "not-a-phc-string", FAST, &set).is_err());
        assert!(verify(PW, "", FAST, &set).is_err());
        assert!(verify(PW, "notanumber$argon2id$x", FAST, &set).is_err());
    }

    #[test]
    fn short_passwords_and_short_peppers_are_refused() {
        let set = PepperSet::none();
        assert_eq!(
            hash("short", SALT, FAST, &set),
            Err(PasswordError::TooShort)
        );
        assert!(hash(&"a".repeat(MIN_PASSWORD_CHARS), SALT, FAST, &set).is_ok());
        assert!(matches!(
            Pepper::new(&[0u8; 31]),
            Err(PasswordError::WeakPepper(31))
        ));
        assert!(Pepper::new(&[0u8; 32]).is_ok());
    }

    #[test]
    fn the_same_password_hashes_differently_under_different_salts() {
        let set = PepperSet::none();
        let a = hash(PW, SALT, FAST, &set).unwrap();
        let b = hash(PW, "ZGlmZmVyZW50c2FsdDEy", FAST, &set).unwrap();
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

    const S1: &str = "1111111111111111111111111111111111111111111111111111111111111112";
    const S2: &str = "2222222222222222222222222222222222222222222222222222222222222223";
    const S3: &str = "3333333333333333333333333333333333333333333333333333333333333334";

    #[test]
    fn a_pepper_list_parses_and_the_highest_id_is_current() {
        let set = PepperSet::parse(&format!("1={S1};2={S2};5={S3}")).unwrap();
        assert_eq!(set.len(), 3);
        assert_eq!(set.current_id(), Some(5), "highest id must be current");
        // Every id in the list is usable, not just the current one.
        for id in [1u32, 2, 5] {
            assert!(set.get(id).is_some(), "pepper {id} missing");
        }
    }

    #[test]
    fn parsing_tolerates_whitespace_and_a_trailing_separator() {
        let a = PepperSet::parse(&format!("1={S1};2={S2}")).unwrap();
        let b = PepperSet::parse(&format!("  1 = {S1} ; 2 = {S2} ; ")).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(a.current_id(), b.current_id());
        // And the secrets actually survived the trimming.
        let h = hash(PW, SALT, FAST, &a).unwrap();
        assert_eq!(verify(PW, &h, FAST, &b), Ok(Verified::Yes));
    }

    /// Every one of these is fatal on purpose: a partly-correct pepper set authenticates some
    /// accounts and permanently rejects others.
    #[test]
    fn a_malformed_pepper_list_is_refused_rather_than_partly_loaded() {
        for (spec, why) in [
            (String::new(), "empty"),
            ("   ;  ".into(), "only separators"),
            (S1.to_string(), "no id="),
            (format!("x={S1}"), "non-numeric id"),
            (format!("1={S1};1={S2}"), "duplicate id"),
            ("1=tooshort".into(), "secret under 32 bytes"),
            (format!("1={}", "a".repeat(64)), "placeholder secret"),
            (
                format!("1={}", "changeme-changeme-changeme-changeme"),
                "placeholder word",
            ),
        ] {
            assert!(
                matches!(
                    PepperSet::parse(&spec),
                    Err(PasswordError::BadPepperSpec(_))
                ),
                "accepted a spec that is {why}: {spec:?}"
            );
        }
    }

    /// Rotation is appending an entry. Nothing already stored changes meaning.
    #[test]
    fn appending_an_entry_rotates_without_stranding_anything() {
        let before = PepperSet::parse(&format!("1={S1}")).unwrap();
        let stored = hash(PW, SALT, FAST, &before).unwrap();
        assert!(stored.starts_with("1$"));

        let after = PepperSet::parse(&format!("1={S1};2={S2}")).unwrap();
        assert_eq!(after.current_id(), Some(2));
        // The old hash still verifies, and asks to be rewritten under the new current pepper.
        assert_eq!(verify(PW, &stored, FAST, &after), Ok(Verified::YesRehash));
        assert!(hash(PW, SALT, FAST, &after).unwrap().starts_with("2$"));
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let shown = format!("{:?}", pepper(0xAB));
        assert!(
            !shown.contains("171") && !shown.contains("ab"),
            "pepper leaked: {shown}"
        );
        assert!(format!("{:?}", PepperSet::single(1, pepper(1))).contains("redacted"));
    }
}
