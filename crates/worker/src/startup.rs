//! One-time startup checks, reported through the Worker log.
//!
//! A deployment running weakened cryptography should say so where an operator will see it. A
//! weak setting nobody mentions is how it survives to production.

use std::cell::Cell;

thread_local! {
    /// Workers are single-threaded and an isolate serves many requests, so this reports once
    /// per isolate rather than once per request.
    static REPORTED: Cell<bool> = const { Cell::new(false) };
}

/// What this deployment is running, for the log line.
pub enum Posture {
    /// Password login compiled out; authentication is external.
    External,
    /// Password login is live, with a pepper. Parameters are still below OWASP on this target.
    #[cfg(feature = "password")]
    PasswordsWithPepper,
    /// Password login is compiled in but refusing to run, with the reason.
    #[cfg(feature = "password")]
    Refused(&'static str),
}

/// Log the posture once per isolate. Warnings are `console_error` so they are not lost in
/// ordinary request logs.
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
        Posture::PasswordsWithPepper => {
            worker::console_log!("notespace: local password login is enabled, pepper configured");
            // True on this target by construction: CONSTRAINED is what fits 10 ms of CPU.
            worker::console_error!(
                "notespace: NOTE -- Argon2 parameters are below the OWASP minimum. The free plan \
                 allows 10 ms of CPU per request and OWASP's minimum needs ~57 ms. The pepper is \
                 what keeps a leaked database uncrackable at these parameters. Prefer OIDC, or a \
                 paid plan. See DESIGN.md 4.10."
            );
        }
    }
}
