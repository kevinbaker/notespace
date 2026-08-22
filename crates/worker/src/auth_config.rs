//! Resolving the password pepper, and refusing to run without one.
//!
//! # Why this cannot be auto-generated on a Worker
//!
//! Auto-creating a secret on first run is the right answer for a self-hosted binary and the
//! wrong one here, for three independent reasons:
//!
//! 1. **There is nowhere safe to put it.** A Worker has no persistent local storage. The only
//!    writable place is D1 — and a pepper stored in the database is not a pepper. The entire
//!    property it provides is that a leaked database does not contain it. Auto-generating into
//!    D1 would produce the appearance of protection with none of the substance, which is worse
//!    than none: an operator would see "pepper configured" and stop worrying.
//! 2. **A Worker cannot write its own secrets.** Doing so needs an API token with account
//!    access, and shipping one to make the deployment self-configuring would be a far larger
//!    hole than the one it closes.
//! 3. **Isolates are plural.** Many run concurrently and are recycled constantly. Each would
//!    generate a different value, so a password hashed by one would fail against every other.
//!    Not merely insecure — broken.
//!
//! So on this target the rule is the other half of the choice: **refuse**. Password login does
//! not start without `PASSWORD_PEPPER`.
//!
//! # What "refuse" means here
//!
//! Not "fail every request". A Worker does not start, it serves; and taking a public forum
//! offline because a login secret is missing turns a security control into an outage, which is
//! how security controls come to be switched off. The refusal is scoped to what it protects:
//! **anything that touches a password returns 503**, reading continues, and the reason is logged
//! at `console_error` on every isolate that starts.
//!
//! The self-hosted target has none of these constraints and should generate a pepper on first
//! run, writing it to a mode-0600 file beside the database. That belongs in `crates/server`
//! (M5), which does not exist yet.

use notespace_core::password::{Params, Pepper, PepperSet};
use worker::Env;

/// Secret holding the pepper. A secret, not a var: the value of a pepper is that it does not
/// live where the database lives.
pub const PEPPER_BINDING: &str = "PASSWORD_PEPPER";

/// Id recorded in hashes made with `PASSWORD_PEPPER`.
///
/// Rotating means moving the old secret to `PASSWORD_PEPPER_<n>` for some unused `n`, and
/// putting the new one in `PASSWORD_PEPPER`. Hashes already written keep naming their own id.
pub const CURRENT_PEPPER_ID: u32 = 0;

/// Historical peppers, as `PASSWORD_PEPPER_1` .. `PASSWORD_PEPPER_N`.
///
/// All of them are kept, and keeping all of them is free: a hash records which pepper made it,
/// so verification does one lookup and one Argon2 run however many are held. Trying them in
/// turn would not be free — at CONSTRAINED that is 3.34 ms each, so three would exceed the
/// 10 ms budget on precisely the path an attacker controls, the failed login.
///
/// Ids are permanent. Reusing one for a different secret strands every account hashed under the
/// old one, which is why they are numbered rather than positional.
pub const PEPPER_PREFIX: &str = "PASSWORD_PEPPER_";

/// How many numbered peppers to look for. Bounded so a missing binding is not an infinite scan.
pub const MAX_PEPPER_ID: u32 = 64;

/// Whether local password login is available, and why not when it is not.
pub enum AuthConfig {
    /// Password login is compiled out. Authentication is external (OIDC).
    External,
    /// Password login is available.
    Passwords { peppers: PepperSet, params: Params },
    /// Password login is compiled in but refuses to run. Auth routes must answer 503.
    Refused(&'static str),
}

impl AuthConfig {
    /// Read the deployment's auth posture from its bindings.
    pub fn resolve(env: &Env) -> AuthConfig {
        if !cfg!(feature = "password") {
            return AuthConfig::External;
        }
        let Some(current) = read_pepper(env, PEPPER_BINDING) else {
            return AuthConfig::Refused(
                "PASSWORD_PEPPER is not set, or is shorter than 32 bytes. Password login is \
                 disabled until it is. Generate one with \
                 `openssl rand -hex 32 | wrangler secret put PASSWORD_PEPPER`. It cannot be \
                 generated automatically here -- see crates/worker/src/auth_config.rs.",
            );
        };
        if is_placeholder(&current) {
            return AuthConfig::Refused(
                "PASSWORD_PEPPER looks like a placeholder (all one byte, or an example value). \
                 Password login is disabled until it is a real random secret.",
            );
        }
        let Ok(pepper) = Pepper::new(current.as_bytes()) else {
            return AuthConfig::Refused("PASSWORD_PEPPER must be at least 32 bytes.");
        };
        // PASSWORD_PEPPER is current; the numbered bindings are every older one, kept forever.
        let mut peppers = PepperSet::single(CURRENT_PEPPER_ID, pepper);
        for id in 0..MAX_PEPPER_ID {
            if id == CURRENT_PEPPER_ID {
                continue;
            }
            if let Some(older) = read_pepper(env, &format!("{PEPPER_PREFIX}{id}"))
                .and_then(|p| Pepper::new(p.as_bytes()).ok())
            {
                peppers.insert(id, older, false);
            }
        }
        AuthConfig::Passwords {
            peppers,
            // The Worker cannot afford OWASP parameters (DESIGN.md §4.10). The pepper above is
            // what makes that tolerable, which is why it is mandatory rather than advised.
            params: Params::CONSTRAINED,
        }
    }
}

fn read_pepper(env: &Env, name: &str) -> Option<String> {
    let v = env.secret(name).ok().map(|s| s.to_string())?;
    (v.len() >= 32).then_some(v)
}

/// Catch a secret that was set to an example value rather than a generated one.
///
/// Cheap, and it catches the copy-paste that a length check alone would wave through.
fn is_placeholder(s: &str) -> bool {
    let b = s.as_bytes();
    b.iter().all(|c| *c == b[0])
        || s.eq_ignore_ascii_case(&"0".repeat(s.len()))
        || s.to_ascii_lowercase().contains("changeme")
        || s.to_ascii_lowercase().contains("example")
}
