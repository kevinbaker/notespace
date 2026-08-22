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

use notespace_core::password::{Params, PepperSet};
use worker::Env;

/// Secret holding every pepper the deployment has ever used.
///
/// Format: `1=<secret>;2=<secret>;…`, highest id current. One variable rather than one per
/// pepper, because the set is a single fact: a scan over numbered bindings cannot distinguish
/// "id 3 was never used" from "id 3 failed to load", and silently holding fewer peppers than
/// intended strands accounts.
///
/// A secret, not a var: the value of a pepper is that it does not live where the database does.
///
/// Rotating means appending an entry. Nothing already stored changes meaning, because every
/// hash names the id that made it.
pub const PEPPER_BINDING: &str = "PASSWORD_PEPPER";

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
        let Some(spec) = env.secret(PEPPER_BINDING).ok().map(|s| s.to_string()) else {
            return AuthConfig::Refused(
                "PASSWORD_PEPPER is not set. Password login is disabled until it is. Generate \
                 one with `printf '1=%s' \"$(openssl rand -hex 32)\" | wrangler secret put \
                 PASSWORD_PEPPER`. It cannot be generated automatically here -- see \
                 crates/worker/src/auth_config.rs.",
            );
        };
        // Any error is fatal: a partly-loaded pepper set authenticates some accounts and
        // permanently rejects others, which is worse than refusing outright.
        let peppers = match PepperSet::parse(&spec) {
            Ok(p) => p,
            Err(_) => {
                return AuthConfig::Refused(
                    "PASSWORD_PEPPER is malformed. Expected `1=<secret>;2=<secret>;...` with \
                     each secret at least 32 bytes, ids unique, and no placeholder values. \
                     Password login is disabled until it parses.",
                )
            }
        };
        AuthConfig::Passwords {
            peppers,
            // The Worker cannot afford OWASP parameters (DESIGN.md §4.10). The pepper above is
            // what makes that tolerable, which is why it is mandatory rather than advised.
            params: Params::CONSTRAINED,
        }
    }
}
