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
//! - **The self-hosted target has no such limit** and uses [`Scheme::OWASP`] unchanged.
//! - [`Scheme::CONSTRAINED`] exists for a free Worker that insists on passwords. It is the
//!   strongest setting measured to fit (4 MiB, t=1, 4.33 ms at p95) and it is **below OWASP**.
//!   It is never the default, and [`Scheme::is_below_recommended`] exists so a deployment can
//!   say so out loud.
//! - [`Scheme::CLIENT_ARGON`] is the third option: the constraint is the *Worker's* CPU, and
//!   the browser's is not scarce. Running OWASP-grade Argon2id there and peppering the result
//!   here restores the work factor that [`Scheme::CONSTRAINED`] gives up, at the cost of
//!   requiring a client that performs the derivation.
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

/// What a stored credential records about how it was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stored<'a> {
    /// True when the record was written under a [`Scheme::Client`] deployment.
    client: bool,
    /// `None` means the hash is unpeppered.
    pepper_id: Option<u32>,
    phc: &'a str,
}

/// Parse `[c][<pepper-id>]$<phc>`.
///
/// The leading `c` marks a client-derived record. Server records are unprefixed, so every hash
/// written before the marker existed still parses to exactly what it meant.
fn split_stored(stored: &str) -> Result<Stored<'_>, PasswordError> {
    let (client, rest) = match stored.strip_prefix('c') {
        Some(rest) => (true, rest),
        None => (false, stored),
    };
    if rest.starts_with('$') {
        return Ok(Stored {
            client,
            pepper_id: None,
            phc: rest,
        });
    }
    let (id, phc) = rest
        .split_once('$')
        .ok_or_else(|| PasswordError::MalformedHash("no PHC section".into()))?;
    let id: u32 = id
        .parse()
        .map_err(|_| PasswordError::MalformedHash(format!("bad pepper id {id:?}")))?;
    // `split_once` ate the `$` that PHC needs to start with.
    let start = rest.len() - phc.len() - 1;
    Ok(Stored {
        client,
        pepper_id: Some(id),
        phc: &rest[start..],
    })
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

    /// Server-side cost applied to an *already* memory-hard client key, under
    /// [`Scheme::CLIENT_ARGON`].
    ///
    /// Deliberately tiny, and that is not a weakness: the input is 32 bytes of CSPRNG-grade
    /// output from an OWASP-parameter Argon2id run. Guessing it directly is not a search anyone
    /// can mount, so grinding it slowly buys nothing. This step exists for one reason only --
    /// to apply the [`Pepper`], so a stolen database does not hand over keys that can be
    /// replayed straight back at the login form.
    ///
    /// Measured in wasm over 15 samples: **1.25 ms at p95, 12% of the 10 ms budget** — against
    /// 4.18 ms (42%) for [`Params::CONSTRAINED`]. So moving the work to the client buys back
    /// 3.3x of the request's CPU as well as the work factor.
    ///
    /// Sampled the same way [`Params::CONSTRAINED`] was, and on the same development machine
    /// rather than Cloudflare's. Treat the ratio as the durable result and the absolute figures
    /// as needing a re-check on a deployed instance: OWASP's 19 MiB re-measured at 48.5 ms p50
    /// on this run against the 25.2 ms recorded in the table at the top of this module, which is
    /// machine noise, not a change in the code.
    pub const HANDOFF: Params = Params {
        m_kib: 1024,
        t: 1,
        p: 1,
    };

    /// Whether these parameters are weaker than OWASP's minimum.
    ///
    /// Not advisory: a deployment using [`Params::CONSTRAINED`] should surface this at startup.
    /// A weakened KDF that nobody mentions is how it stays weakened.
    ///
    /// Read this on the *client* half of [`Scheme::CLIENT_ARGON`], never the server half --
    /// [`Params::HANDOFF`] is below OWASP by design and says nothing about the work actually
    /// done. [`Scheme::is_below_recommended`] applies it to the right half.
    pub const fn is_below_recommended(&self) -> bool {
        self.m_kib < Params::OWASP.m_kib || self.t < Params::OWASP.t
    }
}

/// Where the memory-hard work happens.
///
/// A free Worker cannot afford OWASP-grade Argon2id ([`Params::CONSTRAINED`] is the strongest
/// setting that fits, at ~4.7x below OWASP in memory). The browser can: it has no 10 ms budget.
/// [`Scheme::CLIENT_ARGON`] moves the expensive half there and leaves the server applying only
/// [`Params::HANDOFF`] to the result.
///
/// # Why this is a scheme and not another `Params` constant
///
/// The two produce stored records that look alike -- both end in a cheap Argon2id PHC string --
/// but mean opposite things. Under [`Scheme::CLIENT_ARGON`] a cheap hash is sound, because its
/// input already cost 19 MiB to compute. Applied to a plaintext password the same record is
/// close to worthless. If one `verify` accepted both, then a config flipped back to
/// [`Scheme::CONSTRAINED`], or one non-JS client posting a raw password, would silently write
/// the worthless kind alongside the sound kind and nothing would ever say so.
///
/// So the two are kept non-interchangeable by construction, twice over:
///
/// - Stored client records carry a `c` marker, and [`verify`] refuses a record whose marker
///   disagrees with the configured scheme rather than guessing.
/// - The hashed input is domain-separated ([`CLIENT_KEY_DOMAIN`]), so even if a marker were
///   forged the digests could not collide.
///
/// Both failures are closed: a mismatch rejects the login. Neither can quietly downgrade one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// The server receives the password and does the whole KDF.
    Server(Params),
    /// The client runs Argon2id at `client` and sends the derived key; the server applies
    /// `server` to it.
    Client { client: Params, server: Params },
}

impl Scheme {
    /// Everything server-side, at OWASP's minimum. The default off-Worker.
    pub const OWASP: Scheme = Scheme::Server(Params::OWASP);

    /// Everything server-side, at the strongest setting a free Worker can afford. Below OWASP.
    pub const CONSTRAINED: Scheme = Scheme::Server(Params::CONSTRAINED);

    /// OWASP-grade Argon2id in the browser; the server only peppers the result.
    ///
    /// The honest trade, stated plainly:
    ///
    /// - **Gained.** A stolen database costs an attacker a full 19 MiB Argon2id run per guess,
    ///   which no free-Worker server-side scheme can charge them.
    /// - **Lost.** Login now requires a client that performs the derivation; a plaintext
    ///   password posted to this scheme is rejected, not accepted weakly. It also needs the
    ///   salt before submit, so the salt lookup must answer for unknown accounts too or it
    ///   becomes the account-enumeration oracle that [`crate::login`] exists to avoid.
    /// - **Unchanged.** Anyone who can read the client key in flight has a password-equivalent
    ///   for that account -- exactly the position they would be in with the password itself.
    ///
    /// No browser client ships with notespace yet, so today this serves callers that implement
    /// [`CLIENT_KEY_BYTES`]-byte derivation themselves. Deployments are warned at startup.
    pub const CLIENT_ARGON: Scheme = Scheme::Client {
        client: Params::OWASP,
        server: Params::HANDOFF,
    };

    /// The parameters this process actually runs.
    pub const fn server_params(&self) -> Params {
        match self {
            Scheme::Server(p) => *p,
            Scheme::Client { server, .. } => *server,
        }
    }

    /// The parameters a client must run, if any. `None` when the server does the work.
    pub const fn client_params(&self) -> Option<Params> {
        match self {
            Scheme::Server(_) => None,
            Scheme::Client { client, .. } => Some(*client),
        }
    }

    /// Whether the memory-hard work -- wherever it happens -- is below OWASP's minimum.
    pub const fn is_below_recommended(&self) -> bool {
        match self {
            Scheme::Server(p) => p.is_below_recommended(),
            // The server half is cheap on purpose; the client half is the one that counts.
            Scheme::Client { client, .. } => client.is_below_recommended(),
        }
    }

    const fn marker(&self) -> &'static str {
        match self {
            Scheme::Server(_) => "",
            Scheme::Client { .. } => "c",
        }
    }

    const fn is_client(&self) -> bool {
        matches!(self, Scheme::Client { .. })
    }
}

impl Default for Scheme {
    fn default() -> Self {
        Scheme::OWASP
    }
}

/// Bytes of client-derived key [`Scheme::CLIENT_ARGON`] expects, hex-encoded on the wire.
pub const CLIENT_KEY_BYTES: usize = 32;

/// Domain prefix mixed into a client key before the server hashes it.
///
/// Keeps a client-scheme digest from ever equalling a server-scheme one over the same bytes,
/// independently of the stored marker.
pub const CLIENT_KEY_DOMAIN: &[u8] = b"notespace/client-argon/v1\0";

/// The bytes fed to Argon2id for `secret` under `scheme`, after checking the shape is right.
///
/// Under [`Scheme::Server`] that is the password, length-checked. Under [`Scheme::Client`] it is
/// the domain prefix plus the decoded key -- and `MIN_PASSWORD_CHARS` deliberately does *not*
/// apply, because the server never sees the password and cannot judge it. Enforcing password
/// length is the client's job there, and saying so is the point of this branch.
fn kdf_input(secret: &str, scheme: Scheme) -> Result<Vec<u8>, PasswordError> {
    match scheme {
        Scheme::Server(_) => {
            if secret.chars().count() < MIN_PASSWORD_CHARS {
                return Err(PasswordError::TooShort);
            }
            Ok(secret.as_bytes().to_vec())
        }
        Scheme::Client { .. } => {
            let key = decode_client_key(secret)?;
            let mut input = CLIENT_KEY_DOMAIN.to_vec();
            input.extend_from_slice(&key);
            Ok(input)
        }
    }
}

/// Decode a hex client key, rejecting anything that is not exactly [`CLIENT_KEY_BYTES`].
///
/// A plaintext password reaching a [`Scheme::Client`] deployment lands here and is rejected.
/// That is the intended outcome: failing the login is recoverable, storing a cheap hash of a
/// real password is not.
fn decode_client_key(hex: &str) -> Result<Vec<u8>, PasswordError> {
    let b = hex.as_bytes();
    if b.len() != CLIENT_KEY_BYTES * 2 {
        return Err(PasswordError::BadClientKey(format!(
            "expected {} hex characters, got {}",
            CLIENT_KEY_BYTES * 2,
            b.len()
        )));
    }
    let nybble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    b.chunks(2)
        .map(|p| match (nybble(p[0]), nybble(p[1])) {
            (Some(h), Some(l)) => Ok(h << 4 | l),
            _ => Err(PasswordError::BadClientKey("not hexadecimal".into())),
        })
        .collect()
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
    #[error("client-derived key is malformed: {0}")]
    BadClientKey(String),
    #[error(
        "stored credential was written for a {} deployment but this one is {}",
        if *.stored_client { "client-side" } else { "server-side" },
        if *.configured_client { "client-side" } else { "server-side" }
    )]
    SchemeMismatch {
        stored_client: bool,
        configured_client: bool,
    },
}

/// Hash a password with the ring's current pepper.
///
/// `salt_b64` must be at least 16 bytes of CSPRNG output, base64 unpadded. The salt is a
/// parameter because `core` has no RNG on wasm — the same reason ids and timestamps are.
pub fn hash(
    secret: &str,
    salt_b64: &str,
    scheme: Scheme,
    peppers: &PepperSet,
) -> Result<String, PasswordError> {
    let input = kdf_input(secret, scheme)?;
    let salt = Salt::from_b64(salt_b64).map_err(|e| PasswordError::BadSalt(e.to_string()))?;
    let current = peppers.current;
    let pepper = match current {
        Some(id) => Some(peppers.get(id).ok_or(PasswordError::UnknownPepper(id))?),
        None => None,
    };
    let phc = peppers
        .argon2_for(pepper, scheme.server_params())?
        .hash_password(&input, salt)
        .map(|h| h.to_string())
        .map_err(|e| PasswordError::Hash(e.to_string()))?;
    // Record the scheme and which pepper made this, so verification tries exactly one of each.
    Ok(match current {
        Some(id) => format!("{}{id}{phc}", scheme.marker()),
        None => format!("{}{phc}", scheme.marker()),
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
    secret: &str,
    stored: &str,
    want: Scheme,
    peppers: &PepperSet,
) -> Result<Verified, PasswordError> {
    let rec = split_stored(stored)?;
    // Never guess across schemes. A client record verified as a server one would be checking a
    // plaintext password against a 1 MiB hash; the reverse would reject every valid login with
    // no explanation. Both are bugs worth naming rather than absorbing.
    if rec.client != want.is_client() {
        return Err(PasswordError::SchemeMismatch {
            stored_client: rec.client,
            configured_client: want.is_client(),
        });
    }
    let input = kdf_input(secret, want)?;
    let parsed =
        PasswordHash::new(rec.phc).map_err(|e| PasswordError::MalformedHash(e.to_string()))?;

    // Exactly one lookup, and therefore exactly one Argon2 run, however many peppers are held.
    let pepper = match rec.pepper_id {
        Some(id) => Some(peppers.get(id).ok_or(PasswordError::UnknownPepper(id))?),
        None => None,
    };
    let argon = peppers.argon2_for(pepper, want.server_params())?;

    match argon.verify_password(&input, &parsed) {
        Ok(()) => {
            let stale_pepper = rec.pepper_id != peppers.current;
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
pub fn needs_rehash(stored: &str, want: Scheme) -> bool {
    let Ok(rec) = split_stored(stored) else {
        return true;
    };
    if rec.client != want.is_client() {
        // Not a rehash: the plaintext under one scheme cannot produce the other's record.
        // `verify` rejects this before we are reached; returning true here would invite a
        // caller to "fix" it by writing a record it has no valid input for.
        return false;
    }
    let want = want.server_params();
    let Ok(parsed) = PasswordHash::new(rec.phc) else {
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

    /// Cheap server-side scheme: these tests are about pepper and format handling, not cost.
    const FAST: Scheme = Scheme::Server(Params {
        m_kib: 64,
        t: 1,
        p: 1,
    });
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
        assert_eq!(split_stored(&h).unwrap().pepper_id, Some(7));
        // Unpeppered hashes are plain PHC and stay recognisable as such.
        let plain = hash(PW, SALT, FAST, &PepperSet::none()).unwrap();
        assert!(plain.starts_with("$argon2id$"));
        assert_eq!(split_stored(&plain).unwrap().pepper_id, None);
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
        assert_eq!(split_stored(&h).unwrap().pepper_id, Some(3));
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
        assert!(needs_rehash(&weak, Scheme::OWASP));
        assert!(!needs_rehash(&weak, FAST));
        assert!(needs_rehash("garbage", Scheme::OWASP));
        assert_eq!(
            verify(PW, &weak, Scheme::OWASP, &set),
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

#[cfg(test)]
mod client_scheme_tests {
    use super::*;

    const SALT: &str = "c29tZXNhbHR2YWx1ZTE";
    const PW: &str = "correct horse battery staple";
    /// 32 bytes, as a client would send after running Argon2id.
    const KEY: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";

    /// Server params only, so the tests stay fast; the scheme logic is what is under test.
    const CHEAP: Params = Params {
        m_kib: 64,
        t: 1,
        p: 1,
    };
    fn client_scheme() -> Scheme {
        Scheme::Client {
            client: Params::OWASP,
            server: CHEAP,
        }
    }
    fn server_scheme() -> Scheme {
        Scheme::Server(CHEAP)
    }
    fn peppers() -> PepperSet {
        PepperSet::parse(&format!("1={}", "a1b2c3d4".repeat(8))).unwrap()
    }

    #[test]
    fn a_client_record_is_marked_and_a_server_record_is_not() {
        let p = peppers();
        let c = hash(KEY, SALT, client_scheme(), &p).unwrap();
        let s = hash(PW, SALT, server_scheme(), &p).unwrap();
        assert!(c.starts_with("c1$"), "client record marked: {c}");
        assert!(s.starts_with("1$"), "server record unmarked: {s}");
    }

    /// The property the whole `Scheme` split exists for. Without it, flipping a deployment back
    /// to `constrained` would start fast-hashing plaintext passwords into records that look
    /// exactly like the sound ones.
    #[test]
    fn a_password_cannot_be_verified_against_a_client_record() {
        let p = peppers();
        let stored = hash(KEY, SALT, client_scheme(), &p).unwrap();
        assert!(matches!(
            verify(PW, &stored, server_scheme(), &p),
            Err(PasswordError::SchemeMismatch { .. })
        ));
    }

    #[test]
    fn a_client_key_cannot_be_verified_against_a_server_record() {
        let p = peppers();
        let stored = hash(PW, SALT, server_scheme(), &p).unwrap();
        assert!(matches!(
            verify(KEY, &stored, client_scheme(), &p),
            Err(PasswordError::SchemeMismatch { .. })
        ));
    }

    #[test]
    fn the_right_key_verifies_and_a_wrong_one_does_not() {
        let p = peppers();
        let stored = hash(KEY, SALT, client_scheme(), &p).unwrap();
        assert_eq!(
            verify(KEY, &stored, client_scheme(), &p).unwrap(),
            Verified::Yes
        );
        let wrong = KEY.replace("9f8e", "0000");
        assert_eq!(
            verify(&wrong, &stored, client_scheme(), &p).unwrap(),
            Verified::No
        );
    }

    /// A plaintext password posted to a client-scheme deployment must fail closed, not be
    /// accepted and stored under a 1 MiB hash.
    #[test]
    fn a_plaintext_password_is_refused_by_a_client_scheme() {
        let p = peppers();
        assert!(matches!(
            hash(PW, SALT, client_scheme(), &p),
            Err(PasswordError::BadClientKey(_))
        ));
        assert!(matches!(
            hash(&"z".repeat(64), SALT, client_scheme(), &p),
            Err(PasswordError::BadClientKey(_))
        ));
    }

    /// Domain separation, checked independently of the marker: even stripping the `c` must not
    /// let the two schemes collide over the same bytes.
    #[test]
    fn the_two_schemes_do_not_collide_over_identical_input() {
        let p = peppers();
        let as_client = hash(KEY, SALT, client_scheme(), &p).unwrap();
        let as_server = hash(KEY, SALT, server_scheme(), &p).unwrap();
        let unmarked = as_client.strip_prefix('c').unwrap();
        assert_ne!(unmarked, as_server, "domain prefix must change the digest");
        // And with the marker filed off it still does not verify, because the input differed.
        assert_eq!(
            verify(KEY, unmarked, server_scheme(), &p).unwrap(),
            Verified::No
        );
    }

    /// `MIN_PASSWORD_CHARS` is meaningless server-side here: the server sees a key, never the
    /// password. This pins that the check is skipped deliberately rather than by accident.
    #[test]
    fn client_keys_are_not_subject_to_the_password_length_rule() {
        let p = peppers();
        assert!(hash(KEY, SALT, client_scheme(), &p).is_ok());
        assert!(matches!(
            hash("short", SALT, server_scheme(), &p),
            Err(PasswordError::TooShort)
        ));
    }

    #[test]
    fn client_argon_is_not_below_recommended_but_constrained_is() {
        assert!(!Scheme::CLIENT_ARGON.is_below_recommended());
        assert!(Scheme::CONSTRAINED.is_below_recommended());
        assert!(!Scheme::OWASP.is_below_recommended());
        // The server half being cheap is the point, and must not be read as the work factor.
        assert!(Scheme::CLIENT_ARGON.server_params().is_below_recommended());
    }

    #[test]
    fn a_cross_scheme_record_is_never_reported_as_needing_a_rehash() {
        let p = peppers();
        let stored = hash(KEY, SALT, client_scheme(), &p).unwrap();
        assert!(!needs_rehash(&stored, server_scheme()));
        assert!(!needs_rehash(&stored, client_scheme()));
    }
}
