//! Auth and write forms. Server-rendered and working without JavaScript, like every other page.

use crate::layout::{Shell, Width};
use maud::{html, Markup};

/// Coarse on purpose: separating "no such user" from "wrong password" hands over the account list.
pub enum LoginError {
    /// Wrong username or password. One message for both.
    Rejected,
    /// Too many attempts.
    RateLimited { retry_after_secs: i64 },
    /// Stale form — a back button, or a page left open past the token's life.
    Expired,
    /// A sign-in through a provider did not complete: cancelled, refused, or a bad token.
    Provider,
}

impl LoginError {
    fn message(&self) -> String {
        match self {
            LoginError::Rejected => "Incorrect username or password.".into(),
            LoginError::Provider => {
                "That sign-in did not complete. Try again, or use another way in.".into()
            }
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

/// Something that just happened elsewhere, worth a line above the form.
pub enum LoginNotice {
    /// Names the account, because the address on a reset mail says nothing about the username.
    PasswordReset { username: String },
}

impl LoginNotice {
    fn message(&self) -> String {
        match self {
            LoginNotice::PasswordReset { username } => {
                format!("The password for {username} has been reset. Sign in with the new one.")
            }
        }
    }

    /// What to put in the username field.
    fn username(&self) -> Option<&str> {
        match self {
            LoginNotice::PasswordReset { username } => Some(username),
        }
    }
}

/// One "sign in with …" button. `href` is `/auth/{provider}`, with `next` already on it.
pub struct ProviderButton<'a> {
    pub label: &'a str,
    pub href: String,
}

/// What the sign-in page offers: a password form, provider buttons, or both.
pub struct SignInOptions<'a> {
    pub password_form: bool,
    pub providers: &'a [ProviderButton<'a>],
}

fn provider_buttons(providers: &[ProviderButton<'_>], lead: &str) -> Markup {
    html! {
        @if !providers.is_empty() {
            p class="muted providers-lead" { (lead) }
            div class="providers" {
                @for p in providers {
                    a class="provider" href=(p.href) { "Continue with " (p.label) }
                }
            }
        }
    }
}

/// `next` has to have been validated as a local path by the caller; unchecked, it is an open redirect.
pub fn login_page(
    csrf: &str,
    next: Option<&str>,
    error: Option<LoginError>,
    notice: Option<LoginNotice>,
    options: &SignInOptions<'_>,
) -> Markup {
    let prefill = notice.as_ref().and_then(|n| n.username()).unwrap_or("");
    Shell {
        title: "Sign in",
        width: Width::Narrow,
        ..Default::default()
    }
    .render(html! {
        h1 { "Sign in" }
        @if let Some(n) = &notice {
            p class="notice" role="status" { (n.message()) }
        }
        @if let Some(e) = &error {
            p class="error" role="alert" { (e.message()) }
        }
        (provider_buttons(options.providers, if options.password_form { "" } else { "Sign in with an account you already have." }))
        @if options.password_form {
            @if !options.providers.is_empty() { p class="muted or" { "or with a password" } }
            form method="post" action="/login" {
                input type="hidden" name="csrf" value=(csrf);
                @if let Some(n) = next {
                    input type="hidden" name="next" value=(n);
                }
                label for="username" { "Username" }
                input id="username" name="username" type="text" value=(prefill)
                      autocomplete="username" required autofocus[prefill.is_empty()];
                label for="password" { "Password" }
                input id="password" name="password" type="password"
                      autocomplete="current-password" required
                      autofocus[!prefill.is_empty()];
                button type="submit" { "Sign in" }
            }
            p class="muted" {
                a href="/forgot" { "Forgot your password?" }
                " · "
                "New here? " a href="/register" { "Create an account" }
            }
        } @else if options.providers.is_empty() {
            p class="error" role="alert" {
                "Signing in is not set up on this site yet."
            }
        }
    })
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
    /// Lost the ordinal race repeatedly. Transient.
    Contended,
    Locked,
    /// The same body was posted moments ago by the same author.
    Duplicate,
}

impl ReplyError {
    fn message(&self) -> String {
        match self {
            ReplyError::Locked => "This thread is locked.".into(),
            ReplyError::Duplicate => "You posted exactly this a moment ago.".into(),
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

/// What the reply is to: the post, or the thread when it is a top-level reply.
pub struct ReplyTarget<'a> {
    pub thread_title: &'a str,
    /// `None` for a top-level reply.
    pub parent: Option<ParentPost<'a>>,
}

pub struct ParentPost<'a> {
    pub public_id: &'a str,
    pub author_name: &'a str,
    /// Sanitized at write time, like the thread page.
    pub body_html: &'a str,
}

/// On its own page, because the baked thread page is shared byte-for-byte and cannot carry a
/// per-visitor CSRF token. `draft` is echoed back so a rejected reply is not lost. The parent
/// is shown above the form; `/static/reply.js` adds the quote buttons when it runs, and
/// without it the parent is simply there to read.
pub fn reply_page(
    csrf: &str,
    thread: &str,
    target: &ReplyTarget<'_>,
    draft: &str,
    error: Option<ReplyError>,
) -> Markup {
    let parent = target.parent.as_ref();
    Shell {
        title: "Reply",
        tail: html! {
            @if parent.is_some() {
                                script defer src="/static/reply.js" {}
                            }
        },
        ..Default::default()
    }
    .render(html! {
        h1 {
            @match parent {
                Some(p) => { "Reply to " (p.author_name) },
                None => "Reply to the thread",
            }
        }
        p class="muted" {
            "in " a href={ "/t/" (thread) } { (target.thread_title) }
        }
        @if let Some(p) = parent {
            blockquote class="parent" id="parent" {
                div class="post-body" { (maud::PreEscaped(p.body_html)) }
                p class="muted parent-actions" {
                    a href={ "/p/" (p.public_id) } { "permalink" }
                }
            }
        }
        @if let Some(e) = error {
            p class="error" role="alert" { (e.message()) }
        }
        form method="post" action={ "/t/" (thread) "/reply" } {
            input type="hidden" name="csrf" value=(csrf);
            @if let Some(p) = parent {
                input type="hidden" name="parent" value=(p.public_id);
            }
            label for="body" { "Your reply" }
            textarea id="body" name="body" rows="10" required
                autofocus placeholder="Markdown is supported." { (draft) }
            button type="submit" { "Post reply" }
        }
        p class="muted" {
            a href={ "/t/" (thread) } { "Back to the thread" }
        }
    })
}

/// Why a signup was refused.
pub enum RegisterError {
    /// Carries the reason, which is safe to show.
    BadName(String),
    Taken,
    ShortPassword {
        min: usize,
    },
    /// Carries the reason, which is safe to show.
    BadEmail(String),
    /// Wrong or missing invite code.
    BadInvite,
    RateLimited {
        retry_after_secs: i64,
    },
    Expired,
}

impl RegisterError {
    fn message(&self) -> String {
        match self {
            RegisterError::BadName(why) => format!("That name will not work: {why}."),
            RegisterError::BadEmail(why) => format!("That email address will not work: {why}."),
            RegisterError::BadInvite => "That invite code is not right.".into(),
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

/// `name` and `email` are echoed back on rejection; the password never is, or it lands in
/// every cache en route.
pub fn register_page(
    csrf: &str,
    name: &str,
    email: &str,
    require_email: bool,
    invite_required: bool,
    error: Option<RegisterError>,
    providers: &[ProviderButton<'_>],
) -> Markup {
    Shell {
        title: "Create an account",
        width: Width::Narrow,
        ..Default::default()
    }
    .render(html! {
        h1 { "Create an account" }
        @if let Some(e) = error {
            p class="error" role="alert" { (e.message()) }
        }
        (provider_buttons(providers, ""))
        @if !providers.is_empty() { p class="muted or" { "or with a password" } }
        form method="post" action="/register" {
            input type="hidden" name="csrf" value=(csrf);
            label for="username" { "Username" }
            input id="username" name="username" value=(name) required
                autocomplete="username" autocapitalize="none" autofocus;
            label for="password" { "Password" }
            input id="password" name="password" type="password" required
                autocomplete="new-password";
            p class="muted" { "At least 12 characters. Usernames are permanent." }
            label for="email" {
                @if require_email { "Email" } @else { "Email (optional)" }
            }
            input id="email" name="email" type="email" value=(email)
                autocomplete="email" required[require_email];
            p class="muted" {
                "Used to reset a forgotten password, and for nothing else."
            }
            @if invite_required {
                label for="invite" { "Invite code" }
                input id="invite" name="invite" required autocomplete="off";
            }
            button type="submit" { "Create account" }
        }
        p class="muted" {
            "Already have one? " a href="/login" { "Sign in" } "."
        }
    })
}

/// Shown after a reply was held for moderation. The post exists and has a permalink, but the
/// thread page shows it as awaiting review, which would read as a failure without this.
pub fn held_page(thread: &str, post: &str) -> Markup {
    Shell {
        title: "Reply received",
        ..Default::default()
    }
    .render(html! {
        h1 { "Reply received" }
        p {
            "Your reply is waiting for a quick check before it appears. New accounts and \
             posts with several links go through this; it usually takes a minute, and \
             sometimes a moderator has to look."
        }
        p class="muted" {
            a href={ "/p/" (post) } { "Your reply" } " · "
            a href={ "/t/" (thread) } { "Back to the thread" }
        }
    })
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    #[test]
    fn the_parent_is_shown_and_the_script_loads_only_with_one() {
        let with = reply_page(
            "tok",
            "abc",
            &ReplyTarget {
                thread_title: "T",
                parent: Some(ParentPost {
                    public_id: "p1",
                    author_name: "alice",
                    body_html: "<p>hi</p>",
                }),
            },
            "",
            None,
        )
        .into_string();
        assert!(with.contains("Reply to alice"));
        assert!(with.contains("<p>hi</p>"));
        assert!(with.contains("/static/reply.js"));
        assert!(
            !with.contains("class=\"quote\""),
            "quoting is the script's; no dead controls without it"
        );
        let without = reply_page(
            "tok",
            "abc",
            &ReplyTarget {
                thread_title: "T",
                parent: None,
            },
            "",
            None,
        )
        .into_string();
        assert!(without.contains("Reply to the thread"));
        assert!(!without.contains("reply.js"));
    }
}

/// Why the username step was refused.
pub enum FinishError {
    BadName(String),
    Taken,
    Expired,
}

impl FinishError {
    fn message(&self) -> String {
        match self {
            FinishError::BadName(why) => format!("That name will not work: {why}."),
            FinishError::Taken => "That name is already taken.".into(),
            FinishError::Expired => {
                "That sign-in had expired. Start again from the sign-in page.".into()
            }
        }
    }
}

/// The one-time step after a first sign-in through a provider: pick a username. The provider's
/// name is shown so the visitor knows which account this is.
pub fn finish_page(
    csrf: &str,
    provider_label: &str,
    suggested: &str,
    email: Option<&str>,
    error: Option<FinishError>,
) -> Markup {
    Shell {
        title: "Choose a username",
        width: Width::Narrow,
        ..Default::default()
    }
    .render(html! {
        h1 { "Choose a username" }
        p class="muted" {
            "Signed in with " (provider_label)
            @if let Some(e) = email { " as " (e) }
            ". This is the name you will post under; it cannot be changed later."
        }
        @if let Some(e) = error {
            p class="error" role="alert" { (e.message()) }
        }
        form method="post" action="/auth/finish" {
            input type="hidden" name="csrf" value=(csrf);
            label for="username" { "Username" }
            input id="username" name="username" value=(suggested) required
                autocomplete="username" autocapitalize="none" autofocus;
            button type="submit" { "Create account" }
        }
    })
}
