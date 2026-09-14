//! One-time startup checks, reported through the Worker log.
//!
//! A deployment running weakened cryptography reports it where an operator will see it.

use std::cell::Cell;

thread_local! {
    /// Once per isolate, not once per request.
    static REPORTED: Cell<bool> = const { Cell::new(false) };
}

/// What this deployment is running, for the log line.
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

/// `SITE_NAME` and `SITE_THEME`, into the render crate once per isolate. The theme is the same
/// `name: value` lines the space form takes, with the site's own CSS after a line reading
/// `---`. A theme that does not validate is logged and ignored: the default look, not no site.
pub fn site_once(env: &worker::Env) {
    thread_local! { static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    if let Some(name) = env.var("SITE_NAME").ok().map(|v| v.to_string()) {
        if !name.trim().is_empty() {
            notespace_render::layout::set_site_name(name.trim().to_string());
        }
    }
    let Some(text) = env.var("SITE_THEME").ok().map(|v| v.to_string()) else {
        return;
    };
    let (tokens, css) = match text.split_once("\n---\n") {
        Some((t, c)) => (t, c),
        None => (text.as_str(), ""),
    };
    match notespace_core::theme::Theme::parse_lines(tokens).and_then(|t| t.with_css(css)) {
        Ok(theme) => notespace_render::layout::set_site_theme(theme),
        Err(why) => worker::console_error!("notespace: SITE_THEME ignored: {}", why),
    }
}

/// Warnings go to `console_error`, so they are not lost among ordinary request logs.
pub fn report_once(p: &Posture) {
    if REPORTED.with(|r| r.replace(true)) {
        return;
    }
    match p {
        Posture::External => worker::console_log!(
            "notespace: local password login is disabled; authentication is external (OIDC)"
        ),
        #[cfg(feature = "password")]
        Posture::Refused(why) => worker::console_error!(
            "notespace: PASSWORD LOGIN REFUSED TO START -- {}. Reading is unaffected; anything \
             touching a password returns 503 until this is fixed.",
            why
        ),
        #[cfg(feature = "password")]
        Posture::PasswordsWithPepper { below_owasp } => {
            worker::console_log!("notespace: local password login is enabled, pepper configured");
            if *below_owasp {
                worker::console_error!(
                    "notespace: NOTE -- Argon2 parameters are below the OWASP minimum. The free \
                     plan allows 10 ms of CPU per request and OWASP's minimum needs ~57 ms. The \
                     pepper is what keeps a leaked database uncrackable at these parameters. \
                     Prefer OIDC, or PASSWORD_SCHEME=owasp on a paid plan with limits.cpu_ms \
                     raised."
                );
            }
        }
        #[cfg(feature = "password")]
        Posture::PasswordsClientArgon => {
            worker::console_log!(
                "notespace: local password login is enabled, pepper configured, \
                 PASSWORD_SCHEME=client-argon (OWASP-grade Argon2id runs on the client)"
            );
            // A compatibility warning, not a weakness one, at the same volume.
            worker::console_error!(
                "notespace: NOTE -- client-argon expects the client to post a 32-byte \
                 Argon2id-derived key as hex, not a password. No browser client ships with \
                 notespace yet, so the stock login form will be REJECTED by this deployment. \
                 Use PASSWORD_SCHEME=constrained unless your client performs the derivation."
            );
        }
    }
}
