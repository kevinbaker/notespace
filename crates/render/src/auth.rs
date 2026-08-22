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
             h1{font-size:1.5rem;margin:0 0 1rem}\
             label{display:block;margin:.75rem 0 .25rem;font-size:.9rem}\
             input{width:100%;padding:.5rem;font-size:1rem;border:1px solid #ccc;border-radius:4px}\
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
