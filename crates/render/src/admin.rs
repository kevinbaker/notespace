//! The admin pages. Capability-gated and uncached; every form carries a token. Like the rest
//! of the site they work without JavaScript, so each action is its own small form.

use crate::layout::{crumbs, Shell, Width};
use crate::time::stamp;
use maud::{html, Markup, PreEscaped};
use notespace_core::model::{
    PostState, Ranking, Role, SiteStats, Space, SpaceDetail, Thread, ThreadPage, ThreadState,
    ThreadSummary, UserRow, UserState,
};
use notespace_core::moderation::policy::ModerationPolicy;
use notespace_core::moderation::LogDetail;
use notespace_core::theme::Theme;

/// Something the page says at the top after an action.
pub enum Notice {
    Saved,
    Error(String),
}

fn shell(title: &str, notice: Option<&Notice>, body: Markup) -> Markup {
    Shell {
        title,
        width: Width::Wide,
        crumbs: crumbs([("admin", Some("/admin".into()))]),
        links: html! {
            a href="/mod/queue" { "queue" }
            a href="/admin/users" { "users" }
            a href="/admin/spaces" { "spaces" }
            a href="/admin/log" { "log" }
        },
        ..Default::default()
    }
    .render(html! {
        h1 { (title) }
        @match notice {
            Some(Notice::Saved) => p class="notice" role="status" { "Saved." },
            Some(Notice::Error(e)) => p class="error" role="alert" { (e) },
            None => {}
        }
        (body)
    })
}

pub fn dashboard(stats: &SiteStats, recent: &[ThreadSummary], notice: Option<Notice>) -> Markup {
    shell(
        "Dashboard",
        notice.as_ref(),
        html! {
            dl class="stats" {
                dt { "Open reviews" } dd { a href="/mod/queue" { (stats.open_reviews) } }
                dt { "Pending posts" } dd { (stats.pending_posts) }
                dt { "Users" } dd { a href="/admin/users" { (stats.users) } }
                dt { "Banned" } dd { (stats.banned_users) }
                dt { "Threads" } dd { (stats.threads) }
                dt { "Posts" } dd { (stats.posts) }
            }
            h2 { "Recent threads" }
            table {
                thead { tr { th { "Thread" } th { "Space" } th { "Posts" } th { "Last activity" } th {} } }
                tbody {
                    @for t in recent {
                        tr {
                            td { a href={ "/t/" (t.public_id) } { (t.title) } }
                            td { (t.space_name) }
                            td { (t.post_count) }
                            td { (stamp(t.bumped_at)) }
                            td { a href={ "/admin/thread/" (t.public_id) } { "edit" } }
                        }
                    }
                }
            }
        },
    )
}

fn state_button(
    csrf: &str,
    action: &str,
    field: &str,
    value: &str,
    label: &str,
    current: bool,
) -> Markup {
    html! {
        form method="post" action=(action) class="inline" {
            input type="hidden" name="csrf" value=(csrf);
            input type="hidden" name=(field) value=(value);
            button type="submit" class="small" disabled[current] { (label) }
        }
    }
}

/// The thread form plus its first page of posts, each with state controls.
pub fn thread_page(
    csrf: &str,
    page: &ThreadPage,
    spaces: &[SpaceDetail],
    notice: Option<Notice>,
) -> Markup {
    let t: &Thread = &page.thread;
    let id = t.public_id.to_string();
    shell(
        &format!("Thread: {}", t.title),
        notice.as_ref(),
        html! {
            p class="muted" {
                a href={ "/t/" (id) } { "view" } " · by " a href={ "/u/" (t.author_name) } { (t.author_name) }
                " · " (t.post_count) " posts · created " (stamp(t.created_at))
            }
            form method="post" action={ "/admin/thread/" (id) } {
                input type="hidden" name="csrf" value=(csrf);
                label for="title" { "Title" }
                input id="title" name="title" value=(t.title) required maxlength="200";
                label for="url" { "Link" }
                input id="url" name="url" type="url" value=[t.url.as_deref()];
                label for="state" { "State" }
                select id="state" name="state" {
                    @for (s, label) in [
                        (ThreadState::Visible, "visible"),
                        (ThreadState::Pinned, "pinned"),
                        (ThreadState::Locked, "locked — no new replies"),
                        (ThreadState::Hidden, "hidden — not listed, not readable"),
                        (ThreadState::Deleted, "deleted"),
                    ] {
                        option value=(s.as_str()) selected[t.state == s] { (label) }
                    }
                }
                label for="space" { "Space" }
                select id="space" name="space" {
                    @for s in spaces {
                        option value=(s.space.id) selected[s.space.id == t.space_id] {
                            (s.space.path.trim_end_matches('/')) " — " (s.space.name)
                        }
                    }
                }
                button type="submit" { "Save thread" }
            }
            h2 { "Posts" }
            @if page.posts.is_empty() { p class="muted" { "No posts." } }
            @for p in &page.posts {
                article class="item" id={ "p" (p.public_id) } {
                    header {
                        a href={ "/u/" (p.author_name) } { (p.author_name) }
                        " · " (stamp(p.created_at))
                        " · " span class={ "state state-" (p.state.as_str()) } { (p.state.as_str()) }
                        " · " a href={ "/p/" (p.public_id) } { "permalink" }
                        " · " a href={ "/p/" (p.public_id) "/edit" } { "edit text" }
                    }
                    @if p.state == PostState::Visible {
                        div class="post-body" { (PreEscaped(&p.body_html)) }
                    } @else {
                        // The reviewer sees hidden text; that is the point of reviewing it.
                        div class="post-body dim" { (PreEscaped(&p.body_html)) }
                    }
                    div class="actions" {
                        @let act = format!("/admin/post/{}/state", p.public_id);
                        (state_button(csrf, &act, "state", "visible", "restore", p.state == PostState::Visible))
                        (state_button(csrf, &act, "state", "hidden", "hide", p.state == PostState::Hidden))
                        (state_button(csrf, &act, "state", "pending", "send to queue", p.state == PostState::Pending))
                        (state_button(csrf, &act, "state", "deleted", "delete", p.state == PostState::Deleted))
                    }
                }
            }
            @if let Some(cursor) = &page.next_cursor {
                p class="muted" {
                    a href={ "/admin/thread/" (id) "?after=" (cursor.as_str()) } { "next page →" }
                }
            }
        },
    )
}

pub fn users_page(query: &str, users: &[UserRow], notice: Option<Notice>) -> Markup {
    shell(
        "Users",
        notice.as_ref(),
        html! {
            form method="get" action="/admin/users" class="search" {
                input name="q" value=(query) placeholder="name starts with…";
                button type="submit" class="small" { "Search" }
            }
            table {
                thead { tr { th { "Name" } th { "Role" } th { "State" } th { "Email" } th { "Posts" } th { "Joined" } } }
                tbody {
                    @for u in users {
                        tr class={ "user-" (u.user.state.as_str()) } {
                            td { a href={ "/admin/user/" (u.user.name) } { (u.user.name) } }
                            td { (u.user.role.as_str()) }
                            td { (u.user.state.as_str()) }
                            td {
                                @match &u.email {
                                    Some(e) => { (e) @if u.email_verified { " ✓" } @else { " (unconfirmed)" } },
                                    None => "—",
                                }
                            }
                            td { (u.post_count) }
                            td { (stamp(u.created_at)) }
                        }
                    }
                }
            }
            @if users.is_empty() { p class="muted" { "Nobody matches." } }
        },
    )
}

/// `actor_is_admin` decides whether the role control is shown; the server enforces it too.
pub fn user_page(
    csrf: &str,
    u: &UserRow,
    actor_is_admin: bool,
    is_self: bool,
    notice: Option<Notice>,
) -> Markup {
    let name = &u.user.name;
    shell(
        &format!("User: {name}"),
        notice.as_ref(),
        html! {
            p class="muted" {
                a href={ "/u/" (name) } { "profile" } " · joined " (stamp(u.created_at))
                " · " (u.post_count) " posts"
                @if let Some(e) = &u.email {
                    " · " (e) @if u.email_verified { " (confirmed)" } @else { " (unconfirmed)" }
                }
            }
            h2 { "State: " (u.user.state.as_str()) }
            @if is_self {
                p class="muted" { "You cannot change your own account here." }
            } @else {
                div class="actions" {
                    @let act = format!("/admin/user/{name}/state");
                    (state_button(csrf, &act, "state", "active", "active", u.user.state == UserState::Active))
                    (state_button(csrf, &act, "state", "banned", "ban", u.user.state == UserState::Banned))
                    (state_button(csrf, &act, "state", "deleted", "delete account", u.user.state == UserState::Deleted))
                }
                p class="muted" { "A ban ends every session at once. Deleting keeps the name reserved and the posts as tombstones." }
            }
            h2 { "Role: " (u.user.role.as_str()) }
            @if actor_is_admin && !is_self {
                div class="actions" {
                    @let act = format!("/admin/user/{name}/role");
                    (state_button(csrf, &act, "role", "member", "member", u.user.role == Role::Member))
                    (state_button(csrf, &act, "role", "moderator", "moderator", u.user.role == Role::Moderator))
                    (state_button(csrf, &act, "role", "admin", "admin", u.user.role == Role::Admin))
                }
                p class="muted" { "Moderators work the queue and act on posts, threads and accounts. Admins also change roles and spaces." }
            } @else if !actor_is_admin {
                p class="muted" { "Only an admin changes roles." }
            }
        },
    )
}

pub fn spaces_page(
    csrf: &str,
    spaces: &[SpaceDetail],
    is_admin: bool,
    notice: Option<Notice>,
) -> Markup {
    shell(
        "Spaces",
        notice.as_ref(),
        html! {
            table {
                thead { tr { th { "Path" } th { "Name" } th { "Threads" } th { "Depth" } th { "Hold new accounts" } th {} } }
                tbody {
                    @for s in spaces {
                        @let policy = ModerationPolicy::from_config(&s.space.config);
                        tr {
                            td { a href={ "/s/" (s.space.path.trim_end_matches('/')) } { (s.space.path.trim_end_matches('/')) } }
                            td { (s.space.name) }
                            td { (s.thread_count) }
                            td { (s.space.depth_cap) }
                            td { @if policy.enabled { (policy.new_account_hours) " h" } @else { "moderation off" } }
                            td { @if is_admin { a href={ "/admin/space/" (s.space.id) } { "edit" } } }
                        }
                    }
                }
            }
            @if is_admin {
                h2 { "New space" }
                form method="post" action="/admin/spaces" {
                    input type="hidden" name="csrf" value=(csrf);
                    label for="key" { "Key" }
                    input id="key" name="key" required placeholder="hockey" autocapitalize="none";
                    p class="muted" { "Lowercase letters, digits and hyphens; part of the URL. Cannot be changed later." }
                    label for="parent" { "Under" }
                    select id="parent" name="parent" {
                        option value="" { "(top level)" }
                        @for s in spaces {
                            option value=(s.space.id) { (s.space.path.trim_end_matches('/')) }
                        }
                    }
                    label for="name" { "Name" }
                    input id="name" name="name" required placeholder="Ice Hockey";
                    (policy_fields(&ModerationPolicy::default(), Ranking::Bump, 8))
                    (theme_fields(&Theme::default()))
                    button type="submit" { "Create space" }
                }
            }
        },
    )
}

pub fn space_page(csrf: &str, s: &SpaceDetail, notice: Option<Notice>) -> Markup {
    let policy = ModerationPolicy::from_config(&s.space.config);
    let theme = Theme::from_config(&s.space.config);
    let url = s.space.path.trim_end_matches('/');
    shell(
        &format!("Space: {}", s.space.name),
        notice.as_ref(),
        html! {
            p class="muted" {
                a href={ "/s/" (url) } { "/s/" (url) } " · " (s.thread_count) " threads"
            }
            form method="post" action={ "/admin/space/" (s.space.id) } {
                input type="hidden" name="csrf" value=(csrf);
                label for="name" { "Name" }
                input id="name" name="name" value=(s.space.name) required;
                (policy_fields(&policy, s.space.ranking, s.space.depth_cap))
                (theme_fields(&theme))
                button type="submit" { "Save space" }
            }
        },
    )
}

/// The moderation policy and layout settings, as a form. Shared by create and edit.
fn policy_fields(p: &ModerationPolicy, ranking: Ranking, depth_cap: u32) -> Markup {
    html! {
        label for="depth_cap" { "Reply nesting depth (0 = flat board)" }
        input id="depth_cap" name="depth_cap" type="number" min="0" max="64" value=(depth_cap);
        label for="ranking" { "Ranking" }
        select id="ranking" name="ranking" {
            @for (r, label) in [
                (Ranking::Bump, "bump — most recent activity first"),
                (Ranking::Gravity, "gravity — score decaying with age"),
                (Ranking::Best, "best — highest score"),
                (Ranking::ScoreThreshold, "score threshold"),
            ] {
                option value=(r.as_str()) selected[r == ranking] { (label) }
            }
        }
        h2 { "Moderation" }
        p class="muted" { "What holds a post for the classifier, and how far the classifier is trusted." }
        label { input type="checkbox" name="enabled" value="1" checked[p.enabled]; " Moderation on" }
        label for="new_account_hours" { "Hold every post from accounts younger than (hours; 0 = never)" }
        input id="new_account_hours" name="new_account_hours" type="number" min="0" value=(p.new_account_hours);
        label for="max_links" { "Links per post before holding (established accounts)" }
        input id="max_links" name="max_links" type="number" min="0" value=(p.max_links);
        label for="max_links_new" { "… for new accounts" }
        input id="max_links_new" name="max_links_new" type="number" min="0" value=(p.max_links_new);
        label for="report_threshold" { "Reports needed to pull a post back for review" }
        input id="report_threshold" name="report_threshold" type="number" min="1" value=(p.report_threshold);
        label for="duplicate_window_hours" { "Refuse an identical post from the same author within (hours)" }
        input id="duplicate_window_hours" name="duplicate_window_hours" type="number" min="0" value=(p.duplicate_window_hours);
        label for="publish_confidence" { "Publish without a human when the model says clean at confidence ≥" }
        input id="publish_confidence" name="publish_confidence" type="number" min="0" max="1" step="0.05" value=(p.publish_confidence);
        label for="hide_confidence" { "Hide pending review when the model flags at confidence ≥" }
        input id="hide_confidence" name="hide_confidence" type="number" min="0" max="1" step="0.05" value=(p.hide_confidence);
        label for="blocklist" { "Blocklist (one term per line; case-insensitive)" }
        textarea id="blocklist" name="blocklist" rows="4" { (p.blocklist.join("\n")) }
        label for="rules" { "Space rules, in prose (given to the classifier)" }
        textarea id="rules" name="rules" rows="4" { (p.rules.as_deref().unwrap_or("")) }
        label { input type="checkbox" name="public_modlog" value="1" checked[p.public_modlog]; " Public moderation log" }
    }
}

/// The space's look: the stylesheet's custom properties, one per line, as
/// [`Theme::parse_lines`] reads them. The names are listed so nobody has to open the sheet.
fn theme_fields(t: &Theme) -> Markup {
    html! {
        h2 { "Theme" }
        p class="muted" {
            "One " code { "name: value" } " per line. Properties: "
            code { "bg bg-2 fg fg-2 line accent on-accent danger warn font mono size lh radius measure indent" }
            ". Prefix a name with " code { "light." } " or " code { "dark." }
            " to set it for one colour scheme only. Colours, lengths and font stacks; no URLs."
        }
        label for="theme" { "Overrides" }
        textarea id="theme" name="theme" rows="5" spellcheck="false"
            placeholder="accent: #0a7a45\ndark.accent: #3fbf7f\nfont: Georgia, serif" { (t.to_lines()) }
        label for="theme_css" { "Stylesheet" }
        p class="muted" {
            "The space's own CSS, applied after the site's. Write it against the site's class names "
            "and tokens (" code { "var(--accent)" } "); backgrounds may use " code { "url()" }
            ". Served as its own file, so a reader downloads it once per change."
        }
        textarea id="theme_css" name="theme_css" rows="8" spellcheck="false"
            placeholder=".site{border-bottom:3px solid var(--accent)}" { (t.css) }
    }
}

pub fn log_page(entries: &[LogDetail]) -> Markup {
    shell(
        "Action log",
        None,
        html! {
            p class="muted" { "Everything, including what the public log leaves out." }
            table class="log" {
                thead { tr { th { "When" } th { "Who" } th { "Did" } th { "To" } th { "Detail" } } }
                tbody {
                    @for e in entries {
                        tr class={ @if !e.public { "private" } } {
                            td { (stamp(e.entry.created_at)) }
                            td { (e.entry.actor_name) }
                            td { (e.entry.action) @if !e.public { " (private)" } }
                            td {
                                @match (&e.entry.target_kind[..], &e.entry.target_public_id) {
                                    ("post", Some(id)) => a href={ "/p/" (id) } { "post" },
                                    ("thread", Some(id)) => a href={ "/admin/thread/" (id) } { "thread" },
                                    (kind, _) => (kind),
                                }
                            }
                            td { code { (e.detail) } }
                        }
                    }
                }
            }
        },
    )
}

/// Which space a thread is in, for the move menu: a helper the handler uses.
pub fn space_option_label(s: &Space) -> String {
    format!("{} — {}", s.path.trim_end_matches('/'), s.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::model::User;

    fn row(role: Role) -> UserRow {
        UserRow {
            user: User {
                id: 2,
                name: "bob".into(),
                state: UserState::Active,
                role,
            },
            created_at: 1_700_000_000,
            email: Some("b@example.com".into()),
            email_verified: false,
            post_count: 3,
        }
    }

    #[test]
    fn the_role_control_is_shown_to_admins_only_and_never_for_oneself() {
        let html = user_page("tok", &row(Role::Member), true, false, None).into_string();
        assert!(html.contains("/admin/user/bob/role"));
        assert!(html.contains("/admin/user/bob/state"));
        let html = user_page("tok", &row(Role::Member), false, false, None).into_string();
        assert!(!html.contains("/admin/user/bob/role"));
        assert!(html.contains("Only an admin"));
        let html = user_page("tok", &row(Role::Admin), true, true, None).into_string();
        assert!(!html.contains("/admin/user/bob/state"));
        assert!(html.contains("your own account"));
    }

    #[test]
    fn the_space_form_round_trips_the_policy_and_the_theme() {
        let s = SpaceDetail {
            space: Space {
                id: 1,
                path: "general/".into(),
                name: "General".into(),
                parent_id: None,
                ranking: Ranking::Gravity,
                depth_cap: 3,
                config: r##"{"moderation":{"new_account_hours":2,"blocklist":["a","b"],"enabled":false},"theme":{"accent":"#c00","dark":{"bg":"#000"}}}"##
                    .into(),
            },
            thread_count: 0,
        };
        let html = space_page("tok", &s, None).into_string();
        assert!(
            html.contains("accent: #c00\ndark.bg: #000\n</textarea>"),
            "{html}"
        );
        assert!(html.contains(r#"name="theme_css""#));
        assert!(html.contains(r#"name="new_account_hours" type="number" min="0" value="2""#));
        assert!(html.contains("a\nb</textarea>"));
        assert!(!html.contains(r#"name="enabled" value="1" checked"#));
        assert!(html.contains(r#"value="gravity" selected"#));
        assert!(html.contains(r#"name="depth_cap" type="number" min="0" max="64" value="3""#));
    }
}
