//! One-time startup checks, reported through the Worker log.
//!
//! A deployment running weakened cryptography reports it where an operator will see it.

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
    /// Password login is live and the memory-hard work has been pushed to the client.
    #[cfg(feature = "password")]
    PasswordsClientArgon,
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
                 paid plan."
            );
        }
        #[cfg(feature = "password")]
        Posture::PasswordsClientArgon => {
            worker::console_log!(
                "notespace: local password login is enabled, pepper configured, \
                 PASSWORD_SCHEME=client-argon (OWASP-grade Argon2id runs on the client)"
            );
            // Not a weakness warning -- a compatibility one. Said at the same volume because a
            // login form nobody can use is also an outage, just a quieter one.
            worker::console_error!(
                "notespace: NOTE -- client-argon expects the client to post a 32-byte \
                 Argon2id-derived key as hex, not a password. No browser client ships with \
                 notespace yet, so the stock login form will be REJECTED by this deployment. \
                 Use PASSWORD_SCHEME=constrained unless your client performs the derivation."
            );
        }
    }
}
