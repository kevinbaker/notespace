//! Starting a thread and editing a post. Uncached, token-bearing pages like the reply form.

use crate::layout::Shell;
use maud::{html, Markup};

/// Why a new thread was refused.
pub enum ComposeError {
    BadTitle { min: usize, max: usize },
    BadUrl,
    Empty,
    TooLong { max: usize },
    NoSuchSpace,
    Duplicate,
    RateLimited { retry_after_secs: i64 },
    Expired,
    Contended,
}

impl ComposeError {
    fn message(&self) -> String {
        match self {
            ComposeError::BadTitle { min, max } => {
                format!("Titles are one line of {min} to {max} characters.")
            }
            ComposeError::BadUrl => "That link needs to start with http:// or https://.".into(),
            ComposeError::Empty => "Write something in the body first.".into(),
            ComposeError::TooLong { max } => {
                format!("That body is too long. The limit is {max} characters.")
            }
            ComposeError::NoSuchSpace => "That space does not exist.".into(),
            ComposeError::Duplicate => "You posted exactly this a moment ago.".into(),
            ComposeError::RateLimited { retry_after_secs } => {
                let mins = (retry_after_secs + 59) / 60;
                format!(
                    "You have started a lot of threads just now. Try again in about {} minute{}.",
                    mins.max(1),
                    if mins > 1 { "s" } else { "" }
                )
            }
            ComposeError::Expired => "That form had expired. Please try again.".into(),
            ComposeError::Contended => "Something raced your post. Please try again.".into(),
        }
    }
}

/// What the visitor typed, echoed back on rejection so nothing is lost.
#[derive(Default)]
pub struct ComposeDraft<'a> {
    pub title: &'a str,
    pub url: &'a str,
    pub body: &'a str,
}

/// `space_url` is the path without `/s/`, e.g. `sports/hockey`.
pub fn new_thread_page(
    csrf: &str,
    space_url: &str,
    space_name: &str,
    draft: &ComposeDraft<'_>,
    error: Option<ComposeError>,
) -> Markup {
    Shell {
        title: &format!("New thread in {}", space_name),
        ..Default::default()
    }
    .render(html! {
        h1 { "New thread in " (space_name) }
        @if let Some(e) = error {
            p class="error" role="alert" { (e.message()) }
        }
        form method="post" action={ "/s/" (space_url) "/new" } {
            input type="hidden" name="csrf" value=(csrf);
            label for="title" { "Title" }
            input id="title" name="title" value=(draft.title) required autofocus
                maxlength="200";
            label for="url" { "Link (optional)" }
            input id="url" name="url" type="url" value=(draft.url)
                placeholder="https://";
            label for="body" { "Body" }
            textarea id="body" name="body" rows="12" required
                placeholder="Markdown is supported." { (draft.body) }
            button type="submit" { "Start thread" }
        }
        p class="muted" {
            a href={ "/s/" (space_url) } { "Back to " (space_name) }
        }
    })
}

/// Why an edit or deletion was refused.
pub enum EditError {
    NotYours,
    NotEditable,
    Locked,
    Empty,
    TooLong { max: usize },
    Expired,
}

impl EditError {
    fn message(&self) -> String {
        match self {
            EditError::NotYours => "Only the author can change this post.".into(),
            EditError::NotEditable => "This post has been removed and cannot be edited.".into(),
            EditError::Locked => "This thread is locked.".into(),
            EditError::Empty => "Write something first.".into(),
            EditError::TooLong { max } => {
                format!("That is too long. The limit is {max} characters.")
            }
            EditError::Expired => "That form had expired. Please try again.".into(),
        }
    }
}

/// `body_md` is the current source, or the rejected draft. `None` for `error` and an empty
/// body means the page is being shown to someone who cannot edit -- the form is omitted.
pub fn edit_page(
    csrf: &str,
    post: &str,
    body_md: Option<&str>,
    error: Option<EditError>,
) -> Markup {
    Shell {
        title: "Edit post",
        ..Default::default()
    }
    .render(html! {
        h1 { "Edit post" }
        @if let Some(e) = error {
            p class="error" role="alert" { (e.message()) }
        }
        @if let Some(body) = body_md {
            form method="post" action={ "/p/" (post) "/edit" } {
                input type="hidden" name="csrf" value=(csrf);
                label for="body" { "Post" }
                textarea id="body" name="body" rows="12" required autofocus { (body) }
                button type="submit" { "Save changes" }
            }
            // Its own form: a delete must never be a link.
            form method="post" action={ "/p/" (post) "/delete" } class="danger" {
                input type="hidden" name="csrf" value=(csrf);
                p class="muted" {
                    "Deleting leaves a "
                    em { "[deleted]" }
                    " marker in place so replies keep their context."
                }
                button type="submit" name="confirm" value="yes" { "Delete this post" }
            }
        }
        p class="muted" {
            a href={ "/p/" (post) } { "Back to the post" }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_draft_is_echoed_back_escaped() {
        let draft = ComposeDraft {
            title: "<b>t</b>",
            url: "https://example.com/?a=1&b=2",
            body: "<script>",
        };
        let html = new_thread_page("tok", "general", "General", &draft, None).into_string();
        assert!(html.contains("&lt;b&gt;t&lt;/b&gt;"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("a=1&amp;b=2"));
        assert!(html.contains(r#"action="/s/general/new""#));
    }

    #[test]
    fn a_non_author_gets_no_form() {
        let html = edit_page("tok", "abc", None, Some(EditError::NotYours)).into_string();
        assert!(!html.contains("<form"));
        assert!(html.contains("Only the author"));
        let html = edit_page("tok", "abc", Some("text"), None).into_string();
        assert!(html.contains(r#"action="/p/abc/edit""#));
        assert!(html.contains(r#"action="/p/abc/delete""#));
    }
}
