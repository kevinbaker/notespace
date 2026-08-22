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
pub struct Posture {
    pub password_login: bool,
    pub params_below_recommended: bool,
    pub peppered: bool,
}

/// Log the posture once per isolate. Warnings are `console_error` so they are not lost in
/// ordinary request logs.
pub fn report_once(p: &Posture) {
    if REPORTED.with(|r| r.replace(true)) {
        return;
    }
    if !p.password_login {
        worker::console_log!(
            "notespace: local password login is disabled; authentication is external (OIDC)"
        );
        return;
    }
    worker::console_log!("notespace: local password login is enabled");
    if p.params_below_recommended {
        worker::console_error!(
            "notespace: WARNING -- Argon2 parameters are BELOW the OWASP minimum. The Workers \
             free plan allows 10 ms of CPU per request and OWASP's minimum needs ~56 ms, so \
             password login here is weaker than it should be. Prefer OIDC, or a paid plan. \
             See DESIGN.md 4.10."
        );
    }
    if !p.peppered {
        worker::console_error!(
            "notespace: WARNING -- no pepper configured. With reduced Argon2 parameters, a \
             pepper is what keeps a leaked database uncrackable. Set the PASSWORD_PEPPER secret \
             (>= 32 bytes) with `wrangler secret put PASSWORD_PEPPER`."
        );
    } else {
        worker::console_log!("notespace: password pepper configured");
    }
}
