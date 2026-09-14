//! What this deployment is running, said once per process: the sign-in posture, the site's
//! name and look. Reported through the platform's log so it lands wherever that goes.

use crate::platform::Platform;
use std::sync::atomic::{AtomicBool, Ordering};

static REPORTED: AtomicBool = AtomicBool::new(false);
static SITE_SET: AtomicBool = AtomicBool::new(false);

/// The sign-in posture, read from configuration rather than assumed.
pub enum Posture {
    /// Password login compiled out; authentication is external.
    External,
    /// Password login is live, with a pepper, hashing on the server. `below_owasp` says whether
    /// the parameters are the free plan's reduced ones.
    #[cfg(feature = "password")]
    PasswordsWithPepper { below_owasp: bool },
    /// Password login is live and the memory-hard work has been pushed to the client.
    #[cfg(feature = "password")]
    PasswordsClientArgon,
    /// Password login is compiled in but refusing to run, with the reason.
    #[cfg(feature = "password")]
    Refused(&'static str),
}

#[cfg(not(feature = "password"))]
pub fn posture<P: Platform>(_p: &P) -> Posture {
    Posture::External
}

#[cfg(feature = "password")]
pub fn posture<P: Platform>(p: &P) -> Posture {
    use crate::auth_config::AuthConfig;
    match AuthConfig::resolve(p) {
        AuthConfig::External => Posture::External,
        AuthConfig::Refused(why) => Posture::Refused(why),
        AuthConfig::Passwords { scheme, .. } if scheme.client_params().is_some() => {
            Posture::PasswordsClientArgon
        }
        AuthConfig::Passwords { scheme, .. } => Posture::PasswordsWithPepper {
            below_owasp: scheme.is_below_recommended(),
        },
    }
}

/// `SITE_NAME` and `SITE_THEME`, into the render crate once. The theme is the same `name: value`
/// lines the space form takes, with the site's own CSS after a line reading `---`. A theme that
/// does not validate is logged and ignored: the default look, not no site.
pub fn site_once<P: Platform>(p: &P) {
    if SITE_SET.swap(true, Ordering::Relaxed) {
        return;
    }
    if let Some(name) = p.var("SITE_NAME") {
        notespace_render::layout::set_site_name(name.trim().to_string());
    }
    let Some(text) = p.var("SITE_THEME") else {
        return;
    };
    let (tokens, css) = match text.split_once("\n---\n") {
        Some((t, c)) => (t, c),
        None => (text.as_str(), ""),
    };
    match notespace_core::theme::Theme::parse_lines(tokens).and_then(|t| t.with_css(css)) {
        Ok(theme) => notespace_render::layout::set_site_theme(theme),
        Err(why) => p.log_error(&format!("notespace: SITE_THEME ignored: {why}")),
    }
}

/// Warnings go to the error log, so they are not lost among ordinary request lines.
pub fn report_once<P: Platform>(p: &P, posture: &Posture) {
    if REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    match posture {
        Posture::External => {
            p.log("notespace: local password login is disabled; authentication is external (OIDC)")
        }
        #[cfg(feature = "password")]
        Posture::Refused(why) => p.log_error(&format!(
            "notespace: PASSWORD LOGIN REFUSED TO START -- {why}. Reading is unaffected; anything \
             touching a password returns 503 until this is fixed."
        )),
        #[cfg(feature = "password")]
        Posture::PasswordsWithPepper { below_owasp } => {
            p.log("notespace: local password login is enabled, pepper configured");
            if *below_owasp {
                p.log_error(
                    "notespace: NOTE -- Argon2 parameters are below the OWASP minimum. The free \
                     plan allows 10 ms of CPU per request and OWASP's minimum needs ~57 ms. The \
                     pepper is what keeps a leaked database uncrackable at these parameters. \
                     Prefer OIDC, or PASSWORD_SCHEME=owasp on a paid plan with limits.cpu_ms \
                     raised.",
                );
            }
        }
        #[cfg(feature = "password")]
        Posture::PasswordsClientArgon => {
            p.log(
                "notespace: local password login is enabled, pepper configured, \
                 PASSWORD_SCHEME=client-argon (OWASP-grade Argon2id runs on the client)",
            );
            p.log_error(
                "notespace: NOTE -- client-argon expects the client to post a 32-byte \
                 Argon2id-derived key as hex, not a password. No browser client ships with \
                 notespace yet, so the stock login form will be REJECTED by this deployment. \
                 Use PASSWORD_SCHEME=constrained unless your client performs the derivation.",
            );
        }
    }
}
