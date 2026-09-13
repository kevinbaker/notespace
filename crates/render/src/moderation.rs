//! Moderation pages. All uncached: the queue is capability-gated and the forms carry CSRF
//! tokens, and the modlog changes with every action. Like everything else they work without
//! JavaScript.

use crate::layout::{Shell, Width};
use maud::{html, Markup, PreEscaped};
use notespace_core::model::PostState;
use notespace_core::moderation::classify::Call;
use notespace_core::moderation::{ActorKind, LogEntry, ReviewItem, ReviewReason};

/// Something the queue page says at the top after an action.
pub enum QueueNotice {
    Approved,
    Rejected,
    /// Somebody else got there first.
    Gone,
}

impl QueueNotice {
    fn message(&self) -> &'static str {
        match self {
            QueueNotice::Approved => "Approved and published.",
            QueueNotice::Rejected => "Rejected and hidden.",
            QueueNotice::Gone => "That item had already been resolved.",
        }
    }
}

fn reason_label(r: ReviewReason) -> &'static str {
    match r {
        ReviewReason::Classifier => "held by the classifier",
        ReviewReason::Reports => "reported by readers",
        ReviewReason::Rule => "held by a rule",
        ReviewReason::Appeal => "appeal from the author",
        ReviewReason::Error => "classifier unavailable",
    }
}

fn state_label(s: PostState) -> &'static str {
    match s {
        PostState::Visible => "visible",
        PostState::Pending => "pending (not shown)",
        PostState::Hidden => "hidden",
        PostState::Deleted => "deleted",
    }
}

/// The queue, oldest first. One form per decision, so each click is one POST with a token.
pub fn queue_page(items: &[ReviewItem], csrf: &str, notice: Option<QueueNotice>) -> Markup {
    Shell {
        title: "Review queue",
        width: Width::Wide,
        ..Default::default()
    }
    .render(html! {
        h1 { "Review queue" }
        p class="muted" {
            (items.len()) " waiting · " a href="/admin" { "admin" } " · "
            a href="/modlog" { "public log" } " · " a href="/" { "home" }
        }
        @if let Some(n) = notice {
            p class="notice" role="status" { (n.message()) }
        }
        @if items.is_empty() {
            p { "Nothing to review." }
        }
        @for item in items {
            article class="item" id={ "r" (item.id) } {
                header {
                    strong { (reason_label(item.reason)) }
                    " · " span class="muted" { (state_label(item.post_state)) }
                    " · in " a href={ "/t/" (item.thread_public_id) } { (item.thread_title) }
                    " · by " a href={ "/u/" (item.author_name) } { (item.author_name) }
                    " · " a href={ "/p/" (item.post_public_id) } { "permalink" }
                    " · " a href={ "/admin/thread/" (item.thread_public_id) "#p" (item.post_public_id) } { "admin" }
                }
                // Sanitized at write time, like the thread page.
                div class="post-body" { (PreEscaped(&item.body_html)) }
                @if let Some(call) = item.model_verdict {
                    p class="verdict" {
                        "Model: " strong { (call.as_str()) }
                        @if let Some(c) = item.model_confidence {
                            " at " (format!("{:.0}%", c * 100.0))
                        }
                        @if !item.model_categories.is_empty() {
                            " · "
                            @for (i, cat) in item.model_categories.iter().enumerate() {
                                @if i > 0 { ", " }
                                code { (cat.as_str()) }
                            }
                        }
                        @if call == Call::Unsure { " (asked for a human)" }
                    }
                }
                @if let Some(text) = &item.appeal_text {
                    blockquote class="appeal" {
                        strong { "Appeal: " } (text)
                    }
                }
                div class="decide" {
                    form method="post" action={ "/mod/review/" (item.id) } {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="resolution" value="approve";
                        button type="submit" class="approve" { "Approve" }
                    }
                    form method="post" action={ "/mod/review/" (item.id) } {
                        input type="hidden" name="csrf" value=(csrf);
                        input type="hidden" name="resolution" value="reject";
                        button type="submit" class="reject" { "Reject" }
                    }
                }
            }
        }
    })
}

fn actor_label(kind: ActorKind, name: &str) -> String {
    match kind {
        ActorKind::User => name.to_string(),
        ActorKind::Model => format!("model {name}"),
        ActorKind::Rule => format!("rule {name}"),
        ActorKind::System => "the system".to_string(),
    }
}

fn action_phrase(action: &str) -> &str {
    match action {
        "hold" => "held for review",
        "publish" => "published",
        "hide" => "hid",
        "approve" => "approved",
        "reject" => "rejected",
        "appeal" => "appealed",
        other => other,
    }
}

/// The public log. Actions and targets only; never a rationale, never a reporter.
pub fn modlog_page(entries: &[LogEntry]) -> Markup {
    Shell {
        title: "Moderation log",
        ..Default::default()
    }
    .render(html! {
        h1 { "Moderation log" }
        p class="muted" {
            "Every action taken on content here, by people, rules and the model. "
            a href="/" { "Home" }
        }
        @if entries.is_empty() { p { "Nothing yet." } }
        ul class="log" {
            @for e in entries {
                li {
                    (crate::time::stamp(e.created_at))
                    " — " (actor_label(e.actor_kind, &e.actor_name))
                    " " (action_phrase(&e.action)) " "
                    @match &e.target_public_id {
                        Some(id) if e.target_kind == "post" => {
                            a href={ "/p/" (id) } { "a post" }
                        }
                        Some(id) if e.target_kind == "thread" => {
                            a href={ "/t/" (id) } { "a thread" }
                        }
                        _ => { "a " (e.target_kind) }
                    }
                }
            }
        }
    })
}

pub enum ReportError {
    Expired,
    /// Reporting your own post.
    OwnPost,
    Gone,
}

/// What the report page shows once a report has been taken.
pub enum ReportDone {
    Recorded,
    AlreadyReported,
    /// Enough reports: the post is out of sight pending review.
    Held,
}

/// The report form, or the confirmation after one. Uncached: it carries a token.
pub fn report_page(
    csrf: &str,
    post: &str,
    error: Option<ReportError>,
    done: Option<ReportDone>,
) -> Markup {
    Shell {
        title: "Report a post",
        width: Width::Narrow,
        ..Default::default()
    }
    .render(html! {
        h1 { "Report a post" }
        @if let Some(d) = done {
            p role="status" {
                @match d {
                    ReportDone::Recorded => "Thanks. A moderator will take a look.",
                    ReportDone::AlreadyReported => "You had already reported this post.",
                    ReportDone::Held => "Thanks. The post has been taken down pending review.",
                }
            }
            p class="muted" { a href={ "/p/" (post) } { "Back to the post" } }
        } @else {
            @if let Some(e) = error {
                p class="error" role="alert" {
                    @match e {
                        ReportError::Expired => "That form had expired. Please try again.",
                        ReportError::OwnPost => "You cannot report your own post. Delete it instead.",
                        ReportError::Gone => "That post is no longer available.",
                    }
                }
            }
            form method="post" action={ "/p/" (post) "/report" } {
                input type="hidden" name="csrf" value=(csrf);
                label for="reason" { "What is wrong with it? (optional)" }
                textarea id="reason" name="reason" rows="4" maxlength="500"
                    placeholder="Spam, harassment, off topic…" {}
                button type="submit" { "Report" }
            }
            p class="muted" {
                "Reports are private. Enough of them take a post out of sight until a moderator has looked. "
                a href={ "/p/" (post) } { "Back to the post" }
            }
        }
    })
}

pub enum AppealError {
    Expired,
    Empty,
    TooLong { max: usize },
    NotYours,
    NotHidden,
}

/// The appeal form for the author of a hidden post, or the confirmation.
pub fn appeal_page(csrf: &str, post: &str, error: Option<AppealError>, filed: bool) -> Markup {
    Shell {
        title: "Appeal",
        ..Default::default()
    }
    .render(html! {
        h1 { "Appeal a removal" }
        @if filed {
            p role="status" { "Your appeal is in the queue. A moderator will look at it." }
            p class="muted" { a href={ "/p/" (post) } { "Back to the post" } }
        } @else {
            @if let Some(e) = error {
                p class="error" role="alert" {
                    @match e {
                        AppealError::Expired => "That form had expired. Please try again.",
                        AppealError::Empty => "Say why the post should be restored.",
                        AppealError::TooLong { max } => { "Keep it under " (max) " characters." }
                        AppealError::NotYours => "Only the author of a post can appeal its removal.",
                        AppealError::NotHidden => "This post is not hidden, so there is nothing to appeal.",
                    }
                }
            }
            form method="post" action={ "/p/" (post) "/appeal" } {
                input type="hidden" name="csrf" value=(csrf);
                label for="text" { "Why should this post be restored?" }
                textarea id="text" name="text" rows="6" required maxlength="2000" {}
                button type="submit" { "Send appeal" }
            }
            p class="muted" { a href={ "/p/" (post) } { "Back to the post" } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::id::PublicId;
    use notespace_core::moderation::Category;

    fn item(id: i64) -> ReviewItem {
        ReviewItem {
            id,
            post_id: 7,
            post_public_id: PublicId::new(1_735_689_600_000, 7).unwrap(),
            thread_public_id: PublicId::new(1_735_689_600_000, 1).unwrap(),
            thread_title: "A <thread>".into(),
            space_id: 1,
            space_name: "General".into(),
            author_id: 2,
            author_name: "newcomer".into(),
            body_html: "<p>hello</p>".into(),
            post_state: PostState::Pending,
            reason: ReviewReason::Classifier,
            model_verdict: Some(Call::Flag),
            model_confidence: Some(0.87),
            model_categories: vec![Category::Spam],
            appeal_text: Some("<script>alert(1)</script>".into()),
            opened_at: 1_800_000_000_000,
            resolved: false,
        }
    }

    #[test]
    fn the_queue_shows_the_verdict_and_escapes_everything_but_the_body() {
        let html = queue_page(&[item(3)], "tok", Some(QueueNotice::Approved)).into_string();
        assert!(html.contains("<p>hello</p>"), "body is emitted as rendered");
        assert!(html.contains("A &lt;thread&gt;"), "title is escaped");
        assert!(html.contains("&lt;script&gt;"), "appeal text is escaped");
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("flag"));
        assert!(html.contains("87%"));
        assert!(html.contains("spam"));
        assert!(html.contains(r#"action="/mod/review/3""#));
        assert_eq!(
            html.matches(r#"name="csrf" value="tok""#).count(),
            2,
            "one token per decision"
        );
        assert!(html.contains("Approved and published."));
    }

    #[test]
    fn the_modlog_names_actors_and_links_targets_without_detail() {
        let entries = vec![LogEntry {
            id: 1,
            actor_kind: ActorKind::Model,
            actor_name: "@cf/meta/llama-3.1-8b-instruct".into(),
            target_kind: "post".into(),
            target_public_id: Some(PublicId::new(1_735_689_600_000, 7).unwrap()),
            action: "hide".into(),
            created_at: 1_800_000_000_000,
        }];
        let html = modlog_page(&entries).into_string();
        assert!(html.contains("model @cf/meta/llama-3.1-8b-instruct hid"));
        assert!(html.contains(r#"href="/p/"#));
        assert!(!html.contains("rationale"));
    }

    #[test]
    fn report_and_appeal_forms_carry_the_token_and_post_back_to_themselves() {
        let r = report_page("tok", "ABC", None, None).into_string();
        assert!(r.contains(r#"action="/p/ABC/report""#));
        assert!(r.contains(r#"value="tok""#));
        let done = report_page("tok", "ABC", None, Some(ReportDone::Held)).into_string();
        assert!(!done.contains("<form"), "no form after the report is taken");
        assert!(done.contains("taken down"));

        let a =
            appeal_page("tok", "ABC", Some(AppealError::TooLong { max: 9 }), false).into_string();
        assert!(a.contains(r#"action="/p/ABC/appeal""#));
        assert!(a.contains("under 9 characters"));
        let filed = appeal_page("tok", "ABC", None, true).into_string();
        assert!(!filed.contains("<form"));
    }
}
