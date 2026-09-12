//! Account pages: settings, forgotten password, reset, and address verification. All uncached.

use crate::auth::PreEscapedStyle;
use maud::{html, Markup, DOCTYPE};
use notespace_core::model::{Account, User};

/// What the settings page says at the top after an action.
pub enum SettingsNotice {
    /// A verification mail went out.
    VerificationSent,
    /// The address is stored but no mail could go: nothing is configured to send it.
    VerificationNotSent,
    /// The address is stored; the send failed.
    VerificationFailed,
    PasswordChanged,
    SignedOutEverywhere,
    Error(SettingsError),
}

pub enum SettingsError {
    BadEmail(String),
    WrongPassword,
    ShortPassword { min: usize },
    NoPassword,
    Expired,
}

impl SettingsNotice {
    fn message(&self) -> String {
        match self {
            SettingsNotice::VerificationSent => {
                "Check your inbox: a confirmation link is on its way.".into()
            }
            SettingsNotice::VerificationNotSent => {
                "Saved. This site cannot send mail, so the address stays unconfirmed for now."
                    .into()
            }
            SettingsNotice::VerificationFailed => {
                "Saved, but the confirmation mail could not be sent. Try resending later.".into()
            }
            SettingsNotice::PasswordChanged => {
                "Password changed. Every other session has been signed out.".into()
            }
            SettingsNotice::SignedOutEverywhere => "Signed out everywhere else.".into(),
            SettingsNotice::Error(e) => match e {
                SettingsError::BadEmail(why) => format!("That address will not work: {why}."),
                SettingsError::WrongPassword => "That is not your current password.".into(),
                SettingsError::ShortPassword { min } => {
                    format!("Passwords need at least {min} characters.")
                }
                SettingsError::NoPassword => {
                    "This account does not sign in with a password.".into()
                }
                SettingsError::Expired => "That form had expired. Please try again.".into(),
            },
        }
    }

    fn is_error(&self) -> bool {
        matches!(self, SettingsNotice::Error(_))
    }
}

pub fn settings_page(
    csrf: &str,
    user: &User,
    account: &Account,
    notice: Option<SettingsNotice>,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { "Account" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Account" }
                    p { "Signed in as " strong { (user.name) } "." }
                    @if let Some(n) = &notice {
                        @if n.is_error() {
                            p class="error" role="alert" { (n.message()) }
                        } @else {
                            p class="notice" role="status" { (n.message()) }
                        }
                    }

                    h2 { "Email" }
                    @match &account.email {
                        Some(addr) => p class="muted" {
                            (addr)
                            @if account.email_is_verified() { " (confirmed)" }
                            @else {
                                " (unconfirmed) "
                                form method="post" action="/settings/email" class="inline" {
                                    input type="hidden" name="csrf" value=(csrf);
                                    input type="hidden" name="email" value=(addr);
                                    button type="submit" class="link" { "resend the link" }
                                }
                            }
                        },
                        None => p class="muted" { "No address on file. One is needed to reset a forgotten password." },
                    }
                    form method="post" action="/settings/email" {
                        input type="hidden" name="csrf" value=(csrf);
                        label for="email" { "Change address" }
                        input id="email" name="email" type="email" autocomplete="email" required;
                        button type="submit" { "Save and send confirmation" }
                    }

                    @if account.has_password {
                        h2 { "Password" }
                        form method="post" action="/settings/password" {
                            input type="hidden" name="csrf" value=(csrf);
                            label for="current" { "Current password" }
                            input id="current" name="current" type="password"
                                autocomplete="current-password" required;
                            label for="new" { "New password" }
                            input id="new" name="new" type="password"
                                autocomplete="new-password" required;
                            button type="submit" { "Change password" }
                        }
                    }

                    h2 { "Sessions" }
                    form method="post" action="/settings/sessions" {
                        input type="hidden" name="csrf" value=(csrf);
                        button type="submit" { "Sign out everywhere else" }
                    }
                    form method="post" action="/logout" {
                        button type="submit" class="secondary" { "Sign out" }
                    }
                    p class="muted" { a href="/" { "Back to the index" } }
                }
            }
        }
    }
}

pub enum ForgotError {
    BadAddress(String),
    RateLimited { retry_after_secs: i64 },
    Expired,
}

impl ForgotError {
    fn message(&self) -> String {
        match self {
            ForgotError::BadAddress(why) => format!("That address will not work: {why}."),
            ForgotError::RateLimited { retry_after_secs } => {
                let mins = (retry_after_secs + 59) / 60;
                format!(
                    "Too many reset requests. Try again in about {} minute{}.",
                    mins.max(1),
                    if mins > 1 { "s" } else { "" }
                )
            }
            ForgotError::Expired => "That form had expired. Please try again.".into(),
        }
    }
}

/// `sent` shows the same acknowledgement whether or not the address was known.
pub fn forgot_page(csrf: &str, error: Option<ForgotError>, sent: bool) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { "Reset your password" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Reset your password" }
                    @if sent {
                        p class="notice" role="status" {
                            "If that address belongs to a confirmed account, a reset link is on \
                             its way. It works for an hour."
                        }
                    } @else {
                        @if let Some(e) = error {
                            p class="error" role="alert" { (e.message()) }
                        }
                        p class="muted" {
                            "Enter the confirmed email address on your account and we will send \
                             a link to choose a new password."
                        }
                        form method="post" action="/forgot" {
                            input type="hidden" name="csrf" value=(csrf);
                            label for="email" { "Email" }
                            input id="email" name="email" type="email" autocomplete="email"
                                required autofocus;
                            button type="submit" { "Send reset link" }
                        }
                    }
                    p class="muted" { a href="/login" { "Back to sign in" } }
                }
            }
        }
    }
}

pub enum ResetError {
    /// Unknown, expired, or already used.
    Invalid,
    ShortPassword {
        min: usize,
    },
    Expired,
}

impl ResetError {
    fn message(&self) -> String {
        match self {
            ResetError::Invalid => {
                "That reset link is not valid any more. Request a new one below.".into()
            }
            ResetError::ShortPassword { min } => {
                format!("Passwords need at least {min} characters.")
            }
            ResetError::Expired => "That form had expired. Please try again.".into(),
        }
    }
}

/// The link lands here with the token in the query; the form re-posts it so following the
/// link does not spend it. A mail scanner that prefetches the link changes nothing.
pub fn reset_page(csrf: &str, token: &str, error: Option<ResetError>) -> Markup {
    let invalid = matches!(error, Some(ResetError::Invalid));
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { "Choose a new password" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Choose a new password" }
                    @if let Some(e) = error {
                        p class="error" role="alert" { (e.message()) }
                    }
                    @if invalid {
                        p class="muted" { a href="/forgot" { "Request a new link" } }
                    } @else {
                        form method="post" action="/reset" {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="token" value=(token);
                            label for="password" { "New password" }
                            input id="password" name="password" type="password"
                                autocomplete="new-password" required autofocus;
                            p class="muted" { "At least 12 characters. Every other session will be signed out." }
                            button type="submit" { "Set password" }
                        }
                    }
                }
            }
        }
    }
}

pub enum VerifyOutcome {
    Verified,
    Invalid,
    Stale,
    Claimed,
}

/// Before `outcome`, a confirm button: following a link must not spend it, since mail
/// scanners follow links.
pub fn verify_page(csrf: &str, token: &str, outcome: Option<VerifyOutcome>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { "Confirm your email" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Confirm your email" }
                    @match outcome {
                        None => form method="post" action="/verify" {
                            input type="hidden" name="csrf" value=(csrf);
                            input type="hidden" name="token" value=(token);
                            p { "Press the button to confirm this address belongs to you." }
                            button type="submit" { "Confirm" }
                        },
                        Some(VerifyOutcome::Verified) => {
                            p class="notice" role="status" { "Thanks -- your address is confirmed." }
                            p class="muted" { a href="/settings" { "Back to your account" } }
                        }
                        Some(VerifyOutcome::Invalid) => {
                            p class="error" role="alert" {
                                "That link is not valid any more. Request a new one from your account page."
                            }
                            p class="muted" { a href="/settings" { "Your account" } }
                        }
                        Some(VerifyOutcome::Stale) => {
                            p class="error" role="alert" {
                                "The address on your account changed after this link was sent. \
                                 Confirm the newer address instead."
                            }
                            p class="muted" { a href="/settings" { "Your account" } }
                        }
                        Some(VerifyOutcome::Claimed) => {
                            p class="error" role="alert" {
                                "Another account has already confirmed this address."
                            }
                            p class="muted" { a href="/settings" { "Your account" } }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::model::{Role, UserState};

    fn user() -> User {
        User {
            id: 1,
            name: "alice".into(),
            state: UserState::Active,
            role: Role::Member,
        }
    }

    #[test]
    fn the_settings_page_shows_the_address_and_its_status() {
        let account = Account {
            email: Some("a@example.com".into()),
            email_verified_at: None,
            has_password: true,
        };
        let html = settings_page("tok", &user(), &account, None).into_string();
        assert!(html.contains("a@example.com"));
        assert!(html.contains("unconfirmed"));
        assert!(html.contains("resend"));
        assert!(html.contains(r#"action="/settings/password""#));
        let verified = Account {
            email_verified_at: Some(1),
            ..account
        };
        let html = settings_page("tok", &user(), &verified, None).into_string();
        assert!(html.contains("(confirmed)"));
        assert!(!html.contains("resend"));
    }

    #[test]
    fn an_external_account_gets_no_password_form() {
        let account = Account {
            email: None,
            email_verified_at: None,
            has_password: false,
        };
        let html = settings_page("tok", &user(), &account, None).into_string();
        assert!(!html.contains("/settings/password"));
    }

    /// Following a link must not spend it: both landing pages POST the token.
    #[test]
    fn links_land_on_a_form_rather_than_acting() {
        let reset = reset_page("tok", "abc123", None).into_string();
        assert!(reset.contains(r#"name="token" value="abc123""#));
        assert!(reset.contains(r#"method="post" action="/reset""#));
        let verify = verify_page("tok", "abc123", None).into_string();
        assert!(verify.contains(r#"name="token" value="abc123""#));
        assert!(verify.contains(r#"method="post" action="/verify""#));
    }

    #[test]
    fn the_forgot_acknowledgement_does_not_say_whether_the_address_exists() {
        let html = forgot_page("tok", None, true).into_string();
        assert!(html.contains("If that address"));
        assert!(!html.contains("<form"));
    }
}
