//! Read-path templates. Nothing here takes a viewer: the baked HTML is shared byte-for-byte, and
//! personalisation is layered client-side from `GET /api/me/thread/{id}`. Posts arrive already
//! rendered and in preorder, so this is one linear pass.

use crate::layout::{crumbs, Shell, SpaceTheme};
use maud::{html, Markup, PreEscaped};
use notespace_core::model::{PostState, ThreadPage, ThreadState};
use notespace_core::theme::Theme;

/// Deepest visual indent, so a deep subthread cannot squeeze the text column to nothing.
const MAX_INDENT: u32 = 8;

/// `space` supplies `depth_cap`, so flat and threaded boards are the same code path.
pub fn thread_page(page: &ThreadPage) -> Markup {
    let space = &page.space;
    let t = &page.thread;
    let theme = Theme::from_config(&space.config);
    let space_url = format!("/s/{}", space.path.trim_end_matches('/'));
    Shell {
        title: &t.title,
        crumbs: crumbs([(space.name.as_str(), Some(space_url.clone()))]),
        links: html! {
            a href={ "/t/" (t.public_id) ".rss" } { "rss" }
            a href="/modlog" { "modlog" }
        },
        head: html! {
            link rel="alternate" type="application/rss+xml"
                 title=(t.title) href={ "/t/" (t.public_id) ".rss" };
        },
        theme: Some(SpaceTheme {
            url: space_url,
            theme: &theme,
        }),
        ..Default::default()
    }
    .render(html! {
        h1 class="thread-title" { (t.title) }
        @if let Some(url) = &t.url {
            p class="thread-url meta" { a href=(url) rel="nofollow ugc noopener" { (url) } }
        }
        p class="meta" {
            "by " a href={ "/u/" (t.author_name) } { (t.author_name) }
            " · " (t.post_count) " posts"
            @match t.state {
                ThreadState::Pinned => " · pinned",
                ThreadState::Locked => " · locked: no new replies",
                _ => "",
            }
        }

        ol class="posts" {
            @for post in &page.posts {
                @let indent = post.path.render_depth(space.depth_cap).min(MAX_INDENT);
                // The public id: `post.id` would leak the post count into every page.
                li class="post" id={ "p" (post.public_id) }
                   style={ "--depth:" (indent) }
                   data-depth=(indent) {
                    div class="post-head meta" {
                        a class="author" href={ "/u/" (post.author_name) } {
                            (post.author_name)
                        }
                        " "
                        // Keeps resolving after a split or merge moves the post.
                        a class="permalink" href={ "/p/" (post.public_id) } {
                            (crate::time::stamp(post.created_at))
                        }
                        @if post.edited_at.is_some() { span class="edited" { " (edited)" } }
                    }
                    @match post.state {
                        // Tombstones, not deletions: the thread keeps its shape.
                        PostState::Deleted => div class="post-body muted" {
                            em { "[deleted]" }
                        },
                        PostState::Hidden => div class="post-body muted" {
                            em { "[removed by moderator]" }
                        },
                        PostState::Pending => div class="post-body muted" {
                            em { "[awaiting review]" }
                        },
                        // Sanitized at write time; the one legitimate PreEscaped.
                        PostState::Visible => div class="post-body" {
                            (PreEscaped(&post.body_html))
                        },
                    }
                    div class="post-actions" {
                        // Links, not forms: a form needs a CSRF token, and a token here
                        // would reach every reader.
                        a href={
                            "/t/" (t.public_id) "/reply?parent=" (post.public_id)
                        } { "reply" }
                        " · "
                        // Whether the reader is the author is not knowable here; the
                        // edit page says so if not.
                        a href={ "/p/" (post.public_id) "/edit" } rel="nofollow" { "edit" }
                        " · "
                        a href={ "/p/" (post.public_id) "/report" } rel="nofollow" { "report" }
                    }
                }
            }
        }

        @if let Some(cursor) = &page.next_cursor {
            nav class="pager" {
                a rel="next" href={ "/t/" (t.public_id) "?after=" (cursor.as_str()) } {
                    "next page →"
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::id::PublicId;
    use notespace_core::model::*;
    use notespace_core::path::Path;

    fn space() -> Space {
        Space {
            id: 1,
            path: "general/".into(),
            name: "General".into(),
            parent_id: None,
            ranking: Ranking::Bump,
            depth_cap: 8,
            config: "{}".into(),
        }
    }

    fn thread() -> Thread {
        Thread {
            id: 42,
            public_id: PublicId::new(1_735_689_600_000, 0x2468_ACE0).expect("valid timestamp"),
            space_id: 1,
            kind: ThreadKind::Discussion,
            title: "Hello".into(),
            url: None,
            author_id: 1,
            author_name: "alice".into(),
            created_at: 1_700_000_000,
            bumped_at: 1_700_000_000,
            post_count: 1,
            state: ThreadState::Visible,
            cache_version: 0,
        }
    }

    fn post(id: i64, path: &str, state: PostState, html: &str) -> Post {
        let path = Path::parse(path).unwrap();
        Post {
            id,
            public_id: PublicId::new(1_735_689_600_000 + id as u64, id as u32).unwrap(),
            thread_id: 42,
            parent_id: None,
            depth: path.depth() as u32,
            path,
            author_id: 1,
            author_name: "alice".into(),
            body_md: None,
            body_html: html.into(),
            created_at: 1_700_000_000,
            edited_at: None,
            score: 0.0,
            state,
        }
    }

    fn page(posts: Vec<Post>) -> ThreadPage {
        ThreadPage {
            space: space(),
            thread: thread(),
            posts,
            next_cursor: None,
        }
    }

    #[test]
    fn renders_post_bodies() {
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&p).into_string();
        assert!(html.contains("<p>hi</p>"));
        let pid = p.posts[0].public_id.as_str();
        assert!(html.contains(&format!(r#"id="p{pid}""#)));
    }

    #[test]
    fn baked_page_never_exposes_internal_row_ids() {
        let mut posts = Vec::new();
        for i in 1..=4i64 {
            posts.push(post(i, &format!("{i:04}"), PostState::Visible, "<p>x</p>"));
        }
        let p = page(posts);
        let html = thread_page(&p).into_string();
        for post in &p.posts {
            assert!(
                html.contains(post.public_id.as_str()),
                "public id {} missing from the page",
                post.public_id
            );
            for pattern in [
                format!(r#"id="p{}""#, post.id),
                format!(r#"href="/p/{}""#, post.id),
                format!(r##"href="#p{}""##, post.id),
            ] {
                assert!(
                    !html.contains(&pattern),
                    "internal row id leaked: {pattern}"
                );
            }
        }
        // The thread's own internal id likewise.
        assert!(!html.contains(&format!(r#"/t/{}"#, p.thread.id)));
    }

    #[test]
    fn thread_title_is_escaped() {
        let mut t = thread();
        t.title = "<script>alert(1)</script>".into();
        let p = ThreadPage {
            space: space(),
            thread: t,
            posts: vec![],
            next_cursor: None,
        };
        let html = thread_page(&p).into_string();
        assert!(!html.contains("<script>alert"), "got: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
    }

    #[test]
    fn author_names_are_escaped() {
        let mut pst = post(1, "0001", PostState::Visible, "<p>hi</p>");
        pst.author_name = "<img onerror=x>".into();
        let html = thread_page(&page(vec![pst])).into_string();
        assert!(!html.contains("<img onerror"), "got: {html}");
    }

    #[test]
    fn deleted_posts_render_as_tombstones_not_content() {
        let p = page(vec![post(1, "0001", PostState::Deleted, "<p>secret</p>")]);
        let html = thread_page(&p).into_string();
        assert!(!html.contains("secret"), "deleted body leaked: {html}");
        assert!(html.contains("[deleted]"));
    }

    #[test]
    fn hidden_and_pending_posts_hide_their_bodies() {
        for (state, marker) in [
            (PostState::Hidden, "[removed by moderator]"),
            (PostState::Pending, "[awaiting review]"),
        ] {
            let p = page(vec![post(1, "0001", state, "<p>secret</p>")]);
            let html = thread_page(&p).into_string();
            assert!(!html.contains("secret"), "{state:?} leaked: {html}");
            assert!(html.contains(marker));
        }
    }

    #[test]
    fn indentation_follows_path_depth() {
        let p = page(vec![
            post(1, "0001", PostState::Visible, "<p>a</p>"),
            post(2, "0001.0001", PostState::Visible, "<p>b</p>"),
            post(3, "0001.0001.0001", PostState::Visible, "<p>c</p>"),
        ]);
        let html = thread_page(&p).into_string();
        assert!(html.contains(r#"data-depth="0""#));
        assert!(html.contains(r#"data-depth="1""#));
        assert!(html.contains(r#"data-depth="2""#));
    }

    #[test]
    fn flat_board_renders_every_post_at_depth_zero() {
        let mut s = space();
        s.depth_cap = 0; // Classic BB preset
        let mut p = page(vec![
            post(1, "0001", PostState::Visible, "<p>a</p>"),
            post(2, "0001.0001", PostState::Visible, "<p>b</p>"),
        ]);
        // depth_cap 0 flattens the page regardless of the stored paths.
        p.space = s;
        let html = thread_page(&p).into_string();
        assert!(
            !html.contains(r#"data-depth="1""#),
            "flat board indented: {html}"
        );
    }

    /// Every action link points at a route that exists.
    #[test]
    fn action_links_point_at_live_routes() {
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&p).into_string();
        let pid = p.posts[0].public_id.as_str();
        let tid = p.thread.public_id.as_str();
        assert!(
            !html.contains(&format!("/p/{pid}/reply")),
            "a /p/{{id}}/reply link, which 404s"
        );
        assert!(html.contains(&format!("/t/{tid}/reply?parent={pid}")));
        assert!(html.contains(&format!("/p/{pid}/edit")));
        assert!(html.contains(&format!("/p/{pid}/report")));
        assert!(html.contains(&format!("/t/{tid}.rss")));
    }

    #[test]
    fn the_reply_affordance_is_a_link_and_carries_no_token() {
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&p).into_string();
        assert!(html.contains("/reply?parent="), "no reply link");
        assert!(
            !html.contains("<form"),
            "a form was baked into the shared page"
        );
        for marker in ["csrf", "name=\"body\"", "<textarea"] {
            assert!(!html.contains(marker), "baked page contains {marker:?}");
        }
    }

    #[test]
    fn baked_page_contains_no_viewer_identity() {
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&p).into_string();
        for marker in ["logged in", "csrf", "session", "Sign out", "your vote"] {
            assert!(
                !html.to_lowercase().contains(&marker.to_lowercase()),
                "viewer-specific marker {marker:?} in baked HTML"
            );
        }
    }

    #[test]
    fn the_space_theme_rides_into_the_thread_page() {
        let mut p = page(vec![post(1, "0001", PostState::Visible, "<p>a</p>")]);
        p.space.config = r##"{"theme":{"accent":"#1d4ed8","dark":{"accent":"#7aa2ff"}}}"##.into();
        let html = thread_page(&p).into_string();
        let theme = notespace_core::theme::Theme::from_config(&p.space.config);
        assert!(
            html.contains(&format!(
                r#"href="/s/general/theme.css?v={}""#,
                theme.version()
            )),
            "{html}"
        );
        // And an unthemed space links nothing.
        p.space.config = "{}".into();
        assert!(!thread_page(&p).into_string().contains("theme.css"));
    }

    #[test]
    fn pager_appears_only_when_there_is_a_next_page() {
        let mut p = page(vec![post(1, "0001", PostState::Visible, "<p>a</p>")]);
        assert!(!thread_page(&p).into_string().contains("next page"));
        p.next_cursor = Some(Path::parse("0009").unwrap());
        let html = thread_page(&p).into_string();
        assert!(html.contains("next page"));
        assert!(html.contains("after=0009"));
    }
}
