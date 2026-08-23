//! The login form.
//!
//! Server-rendered and works without JavaScript, like every other page. A login
//! form that needs JS is a login form that fails for the people most likely to be on a bad
//! connection.

use maud::{html, Markup, DOCTYPE};

/// Why the previous attempt failed, if it did.
///
/// Deliberately coarse. "No such user" and "wrong password" are one message, because telling
/// them apart hands over the account list.
pub enum LoginError {
    /// Wrong username or password. One message for both.
    Rejected,
    /// Too many attempts.
    RateLimited { retry_after_secs: i64 },
    /// The form was stale — usually a back button or a page left open past the token's life.
    Expired,
}

impl LoginError {
    fn message(&self) -> String {
        match self {
            LoginError::Rejected => "Incorrect username or password.".into(),
            LoginError::RateLimited { retry_after_secs } => {
                let mins = (retry_after_secs + 59) / 60;
                format!(
                    "Too many sign-in attempts. Try again in about {} minute{}.",
                    mins.max(1),
                    if mins > 1 { "s" } else { "" }
                )
            }
            LoginError::Expired => "That form had expired. Please try again.".into(),
        }
    }
}

/// Render the login page.
///
/// `csrf` is minted against the visitor's session or, when there is none yet, an anonymous
/// cookie. `next` is where to go afterwards; the caller must have validated it as a local path,
/// since an unchecked value here is an open redirect.
pub fn login_page(csrf: &str, next: Option<&str>, error: Option<LoginError>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Sign in" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Sign in" }
                    @if let Some(e) = &error {
                        p class="error" role="alert" { (e.message()) }
                    }
                    form method="post" action="/login" {
                        input type="hidden" name="csrf" value=(csrf);
                        @if let Some(n) = next {
                            input type="hidden" name="next" value=(n);
                        }
                        label for="username" { "Username" }
                        input id="username" name="username" type="text"
                              autocomplete="username" required autofocus;
                        label for="password" { "Password" }
                        input id="password" name="password" type="password"
                              autocomplete="current-password" required;
                        button type="submit" { "Sign in" }
                    }
                }
            }
        }
    }
}

/// Minimal styling, inlined. An external stylesheet would be a second request on the one page
/// a visitor sees before they have any cached assets.
struct PreEscapedStyle;

impl maud::Render for PreEscapedStyle {
    fn render_to(&self, out: &mut String) {
        out.push_str(
            "body{font:16px/1.5 system-ui,sans-serif;margin:0;background:#fbfbfc;color:#1a1a1a}\
             .auth{max-width:22rem;margin:4rem auto;padding:0 1rem}\
             .auth.wide{max-width:44rem}\
             h1{font-size:1.5rem;margin:0 0 1rem}\
             label{display:block;margin:.75rem 0 .25rem;font-size:.9rem}\
             input{width:100%;padding:.5rem;font-size:1rem;border:1px solid #ccc;border-radius:4px}\
             textarea{width:100%;padding:.5rem;font:inherit;border:1px solid #ccc;\
             border-radius:4px;resize:vertical}\
             .muted{color:#666;font-size:.9rem}\
             button{margin-top:1.25rem;width:100%;padding:.6rem;font-size:1rem;border:0;\
             border-radius:4px;background:#1a1a1a;color:#fff;cursor:pointer}\
             .error{background:#fdecea;border:1px solid #f5c2c0;padding:.6rem .75rem;\
             border-radius:4px;font-size:.9rem}\
             @media(prefers-color-scheme:dark){body{background:#16181c;color:#e8e8ea}\
             input{background:#1f2229;border-color:#3a3f4b;color:inherit}\
             button{background:#e8e8ea;color:#16181c}\
             .error{background:#3a1d1d;border-color:#6b2b2b}}",
        );
    }
}

/// Why a reply was refused, in words a person can act on.
pub enum ReplyError {
    Empty,
    TooLong {
        max: usize,
    },
    TooDeep {
        cap: u32,
    },
    RateLimited {
        retry_after_secs: i64,
    },
    Expired,
    /// Lost the ordinal race repeatedly. Genuinely transient; say so.
    Contended,
}

impl ReplyError {
    fn message(&self) -> String {
        match self {
            ReplyError::Empty => "Write something first.".into(),
            ReplyError::TooLong { max } => {
                format!("That reply is too long. The limit is {max} characters.")
            }
            ReplyError::TooDeep { cap } => {
                format!("This space only nests {cap} levels deep. Reply higher up the thread.")
            }
            ReplyError::RateLimited { retry_after_secs } => {
                let mins = (retry_after_secs + 59) / 60;
                format!(
                    "You have posted a lot just now. Try again in about {} minute{}.",
                    mins.max(1),
                    if mins > 1 { "s" } else { "" }
                )
            }
            ReplyError::Expired => "That form had expired. Please try again.".into(),
            ReplyError::Contended => "Someone replied at the same moment. Please try again.".into(),
        }
    }
}

/// The reply form, on its own page.
///
/// Deliberately **not** part of the thread page. That page is baked and shared byte-for-byte
/// between every reader, so a per-visitor CSRF token cannot appear in it — putting one there
/// would hand one visitor's token to everybody else who reads the thread, and would break the
/// cache sharing the read path depends on.
///
/// `thread` and `parent` are public ids. `draft` is echoed back so a rejected reply is not lost.
pub fn reply_page(
    csrf: &str,
    thread: &str,
    parent: Option<&str>,
    draft: &str,
    error: Option<ReplyError>,
) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Reply" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth wide" {
                    h1 { "Reply" }
                    @if let Some(e) = error {
                        p class="error" role="alert" { (e.message()) }
                    }
                    form method="post" action={ "/t/" (thread) "/reply" } {
                        input type="hidden" name="csrf" value=(csrf);
                        @if let Some(p) = parent {
                            input type="hidden" name="parent" value=(p);
                        }
                        label for="body" { "Your reply" }
                        textarea id="body" name="body" rows="10" required
                            autofocus placeholder="Markdown is supported." { (draft) }
                        button type="submit" { "Post reply" }
                    }
                    p class="muted" {
                        a href={ "/t/" (thread) } { "Back to the thread" }
                    }
                }
            }
        }
    }
}

/// Why a signup was refused.
pub enum RegisterError {
    /// The name is not usable. Carries the reason, which is safe to show.
    BadName(String),
    Taken,
    ShortPassword {
        min: usize,
    },
    RateLimited {
        retry_after_secs: i64,
    },
    Expired,
}

impl RegisterError {
    fn message(&self) -> String {
        match self {
            RegisterError::BadName(why) => format!("That name will not work: {why}."),
            RegisterError::Taken => "That name is already taken.".into(),
            RegisterError::ShortPassword { min } => {
                format!("Passwords need at least {min} characters.")
            }
            RegisterError::RateLimited { retry_after_secs } => {
                let mins = (retry_after_secs + 59) / 60;
                format!(
                    "Too many accounts created from here. Try again in about {} minute{}.",
                    mins.max(1),
                    if mins > 1 { "s" } else { "" }
                )
            }
            RegisterError::Expired => "That form had expired. Please try again.".into(),
        }
    }
}

/// The signup form.
///
/// `name` is echoed back so a rejection does not make someone retype it. The password never is:
/// re-rendering a submitted password puts it in the page, and from there in any cache or proxy
/// that sees the response.
pub fn register_page(csrf: &str, name: &str, error: Option<RegisterError>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Create an account" }
                style { (PreEscapedStyle) }
            }
            body {
                main class="auth" {
                    h1 { "Create an account" }
                    @if let Some(e) = error {
                        p class="error" role="alert" { (e.message()) }
                    }
                    form method="post" action="/register" {
                        input type="hidden" name="csrf" value=(csrf);
                        label for="username" { "Username" }
                        input id="username" name="username" value=(name) required
                            autocomplete="username" autocapitalize="none" autofocus;
                        label for="password" { "Password" }
                        input id="password" name="password" type="password" required
                            autocomplete="new-password";
                        p class="muted" { "At least 12 characters. Usernames are permanent." }
                        button type="submit" { "Create account" }
                    }
                    p class="muted" {
                        "Already have one? " a href="/login" { "Sign in" } "."
                    }
                }
            }
        }
    }
}
