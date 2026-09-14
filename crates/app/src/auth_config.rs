//! Resolving the password pepper, and refusing to run without one.

use crate::platform::Platform;
use notespace_core::password::{PepperSet, Scheme};

pub const SCHEME_BINDING: &str = "PASSWORD_SCHEME";

/// `1=<secret>;2=<secret>;…`, highest id current.
pub const PEPPER_BINDING: &str = "PASSWORD_PEPPER";

pub enum AuthConfig {
    /// Password login compiled out; authentication is external.
    External,
    Passwords {
        peppers: PepperSet,
        scheme: Scheme,
    },
    /// Compiled in but refusing to run; auth routes answer 503.
    Refused(&'static str),
}

impl AuthConfig {
    pub fn resolve<P: Platform>(p: &P) -> AuthConfig {
        if !cfg!(feature = "password") {
            return AuthConfig::External;
        }
        let Some(spec) = p.secret(PEPPER_BINDING) else {
            return AuthConfig::Refused(
                "PASSWORD_PEPPER is not set. Password login is disabled until it is. Generate \
                 one with `printf '1=%s' \"$(openssl rand -hex 32)\" | wrangler secret put \
                 PASSWORD_PEPPER`. It cannot be generated automatically here -- see \
                 crates/app/src/auth_config.rs.",
            );
        };
        // A partly-loaded set would strand an arbitrary subset of accounts.
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
        let scheme = match p.var(SCHEME_BINDING).as_deref() {
            // Below OWASP; the mandatory pepper is what makes it tolerable.
            None | Some("constrained") => Scheme::CONSTRAINED,
            // OWASP's minimum, ~57 ms of CPU: for a paid plan with `limits.cpu_ms` raised.
            Some("owasp") => Scheme::OWASP,
            Some("client-argon") => Scheme::CLIENT_ARGON,
            Some(other) => {
                // Refuse rather than default, so a misspelling cannot silently weaken hashing.
                p.log_error(&format!(
                    "{SCHEME_BINDING}={other:?} is not a known scheme. Expected \
                     \"constrained\", \"owasp\" or \"client-argon\". Password login is disabled."
                ));
                return AuthConfig::Refused(
                    "PASSWORD_SCHEME is not a known scheme. Expected \"constrained\", \"owasp\" \
                     or \"client-argon\".",
                );
            }
        };
        AuthConfig::Passwords { peppers, scheme }
    }
}
