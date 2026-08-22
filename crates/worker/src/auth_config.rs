//! Resolving the password pepper, and refusing to run without one.
//!
//! The pepper is never auto-generated on this target: there is nowhere safe to put it, a Worker
//! cannot write its own secrets, and concurrent isolates would each generate a different value.
//! Password login therefore does not start without `PASSWORD_PEPPER`.
//!
//! "Refuse" is scoped: **anything touching a password returns 503**, reading continues, and the
//! reason is logged at `console_error` on every isolate that starts.

use notespace_core::password::{PepperSet, Scheme};
use worker::{console_error, Env};

/// Optional var selecting where the memory-hard work happens.
///
/// `constrained` (default) hashes server-side. `client-argon` expects the client to have run
/// OWASP-grade Argon2id already and to post the derived key.
pub const SCHEME_BINDING: &str = "PASSWORD_SCHEME";

/// Secret holding every pepper the deployment has ever used.
///
/// Format: `1=<secret>;2=<secret>;…`, highest id current. Rotating means appending an entry;
/// nothing already stored changes meaning, because every hash names the id that made it.
pub const PEPPER_BINDING: &str = "PASSWORD_PEPPER";

/// Whether local password login is available, and why not when it is not.
pub enum AuthConfig {
    /// Password login is compiled out. Authentication is external (OIDC).
    External,
    /// Password login is available.
    Passwords { peppers: PepperSet, scheme: Scheme },
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
        // Any error is fatal: a partly-loaded set would strand an arbitrary subset of accounts.
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
        let scheme = match env
            .var(SCHEME_BINDING)
            .ok()
            .map(|v| v.to_string())
            .as_deref()
        {
            // Below OWASP; the mandatory pepper is what makes it tolerable.
            None | Some("constrained") => Scheme::CONSTRAINED,
            Some("client-argon") => Scheme::CLIENT_ARGON,
            Some(other) => {
                // Never fall back to the default: a misspelling must not silently weaken.
                console_error!(
                    "{SCHEME_BINDING}={other:?} is not a known scheme. Expected \
                     \"constrained\" or \"client-argon\". Password login is disabled."
                );
                return AuthConfig::Refused(
                    "PASSWORD_SCHEME is not a known scheme. Expected \"constrained\" or \
                     \"client-argon\".",
                );
            }
        };
        AuthConfig::Passwords { peppers, scheme }
    }
}
