//! Read-path templates.
//!
//! Two constraints from DESIGN.md shape everything here:
//!
//! - §3.3: "Baked HTML is user-agnostic. No usernames, no vote state, no unread markers in
//!   the baked blob, or you lose all cache sharing." Nothing in this module takes a viewer.
//!   Personalisation is layered client-side from `GET /api/me/thread/{id}`.
//! - §3.2: 10ms CPU per request. Posts arrive already rendered and in preorder, so this is
//!   a single linear pass with no tree construction and no per-post allocation beyond the
//!   output buffer.

use maud::{html, Markup, PreEscaped, DOCTYPE};
use notespace_core::model::{PostState, Space, ThreadPage};

/// Deepest visual indent. Beyond this, replies stop nesting further so a deep subthread
/// cannot squeeze the text column to nothing on a phone.
const MAX_INDENT: u32 = 8;

/// Render a full thread page.
///
/// `space` supplies `depth_cap`, which is what makes a flat board and a threaded board the
/// same code path (DESIGN.md §6: presets are data, not code).
pub fn thread_page(space: &Space, page: &ThreadPage) -> Markup {
    let t = &page.thread;
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (t.title) " — " (space.name) }
                link rel="alternate" type="application/rss+xml"
                     title=(t.title) href={ "/t/" (t.id) ".rss" };
                style { (PreEscaped(STYLE)) }
            }
            body {
                header class="site" {
                    a href="/" { "notespace" }
                    " / "
                    a href={ "/s/" (space.slug) } { (space.name) }
                }
                main {
                    h1 class="thread-title" { (t.title) }
                    @if let Some(url) = &t.url {
                        p class="thread-url" { a href=(url) rel="nofollow ugc noopener" { (url) } }
                    }
                    p class="thread-meta" {
                        "by " a href={ "/u/" (t.author_name) } { (t.author_name) }
                        " · " (t.post_count) " posts"
                    }

                    ol class="posts" {
                        @for post in &page.posts {
                            @let indent = post.path.render_depth(space.depth_cap).min(MAX_INDENT);
                            li class="post" id={ "p" (post.id) }
                               style={ "--indent:" (indent) }
                               data-depth=(indent) {
                                div class="post-head" {
                                    a class="author" href={ "/u/" (post.author_name) } {
                                        (post.author_name)
                                    }
                                    " "
                                    a class="permalink" href={ "#p" (post.id) } {
                                        time datetime=(post.created_at) { (post.created_at) }
                                    }
                                    @if post.edited_at.is_some() { span class="edited" { " (edited)" } }
                                }
                                @match post.state {
                                    // Tombstones, not deletions: the thread keeps its shape.
                                    PostState::Deleted => div class="post-body tombstone" {
                                        em { "[deleted]" }
                                    },
                                    PostState::Hidden => div class="post-body tombstone" {
                                        em { "[removed by moderator]" }
                                    },
                                    PostState::Pending => div class="post-body tombstone" {
                                        em { "[awaiting review]" }
                                    },
                                    // Already sanitized at write time (see markdown.rs).
                                    // This is the one place PreEscaped is legitimate.
                                    PostState::Visible => div class="post-body" {
                                        (PreEscaped(&post.body_html))
                                    },
                                }
                                div class="post-actions" {
                                    a href={ "/p/" (post.id) "/reply" } { "reply" }
                                }
                            }
                        }
                    }

                    @if let Some(cursor) = &page.next_cursor {
                        nav class="pager" {
                            a rel="next" href={ "/t/" (t.id) "?after=" (cursor.as_str()) } {
                                "next page →"
                            }
                        }
                    }
                }
                // Personalisation (vote state, unread markers) is fetched separately so this
                // document stays identical for every reader and can be cached once.
                script defer src="/static/personalize.js" data-thread=(t.id) {}
            }
        }
    }
}

const STYLE: &str = "\
:root{--fg:#111;--dim:#666;--line:#e2e2e2;--bg:#fff;--accent:#0b5}\
@media(prefers-color-scheme:dark){:root{--fg:#e8e8e8;--dim:#999;--line:#333;--bg:#141414}}\
*{box-sizing:border-box}\
body{margin:0;padding:0 1rem 4rem;background:var(--bg);color:var(--fg);\
font:16px/1.55 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif}\
main,header.site{max-width:52rem;margin:0 auto}\
header.site{padding:.75rem 0;border-bottom:1px solid var(--line);font-size:.875rem}\
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
.thread-title{font-size:1.5rem;line-height:1.25;margin:1.25rem 0 .25rem}\
.thread-meta,.post-head{color:var(--dim);font-size:.8125rem}\
.posts{list-style:none;padding:0;margin:1.5rem 0}\
.post{margin-left:calc(var(--indent,0)*1.25rem);padding:.6rem 0 .6rem .75rem;\
border-left:2px solid var(--line);margin-bottom:.4rem}\
.post[data-depth='0']{border-left-color:transparent;padding-left:0}\
.post-body{margin:.35rem 0}\
.post-body>*:first-child{margin-top:0}.post-body>*:last-child{margin-bottom:0}\
.post-body pre{overflow-x:auto;padding:.6rem;background:rgba(128,128,128,.12);border-radius:4px}\
.post-body code{font-size:.9em}\
.post-body blockquote{margin:.5rem 0;padding-left:.75rem;border-left:3px solid var(--line);color:var(--dim)}\
.post-body img{max-width:100%;height:auto}\
.post-body table{border-collapse:collapse}\
.post-body th,.post-body td{border:1px solid var(--line);padding:.25rem .5rem}\
.tombstone{color:var(--dim)}\
.post-actions{font-size:.8125rem}\
.pager{margin:2rem 0;font-size:.9375rem}\
@media(max-width:34rem){.post{margin-left:calc(min(var(--indent,0),4)*.6rem)}}\
";

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::model::*;
    use notespace_core::path::Path;

    fn space() -> Space {
        Space {
            id: 1,
            slug: "general".into(),
            name: "General".into(),
            parent_id: None,
            ranking: Ranking::Bump,
            depth_cap: 8,
        }
    }

    fn thread() -> Thread {
        Thread {
            id: 42,
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
            thread: thread(),
            posts,
            next_cursor: None,
        }
    }

    #[test]
    fn renders_post_bodies() {
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&space(), &p).into_string();
        assert!(html.contains("<p>hi</p>"));
        assert!(html.contains(r#"id="p1""#));
    }

    #[test]
    fn thread_title_is_escaped() {
        // Titles are plain text and never pass through the markdown sanitizer, so the
        // template itself must escape them.
        let mut t = thread();
        t.title = "<script>alert(1)</script>".into();
        let p = ThreadPage {
            thread: t,
            posts: vec![],
            next_cursor: None,
        };
        let html = thread_page(&space(), &p).into_string();
        assert!(!html.contains("<script>alert"), "got: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
    }

    #[test]
    fn author_names_are_escaped() {
        let mut pst = post(1, "0001", PostState::Visible, "<p>hi</p>");
        pst.author_name = "<img onerror=x>".into();
        let html = thread_page(&space(), &page(vec![pst])).into_string();
        assert!(!html.contains("<img onerror"), "got: {html}");
    }

    #[test]
    fn deleted_posts_render_as_tombstones_not_content() {
        let p = page(vec![post(1, "0001", PostState::Deleted, "<p>secret</p>")]);
        let html = thread_page(&space(), &p).into_string();
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
            let html = thread_page(&space(), &p).into_string();
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
        let html = thread_page(&space(), &p).into_string();
        assert!(html.contains(r#"data-depth="0""#));
        assert!(html.contains(r#"data-depth="1""#));
        assert!(html.contains(r#"data-depth="2""#));
    }

    #[test]
    fn flat_board_renders_every_post_at_depth_zero() {
        let mut s = space();
        s.depth_cap = 0; // Classic BB preset
        let p = page(vec![
            post(1, "0001", PostState::Visible, "<p>a</p>"),
            post(2, "0001.0001", PostState::Visible, "<p>b</p>"),
        ]);
        let html = thread_page(&s, &p).into_string();
        assert!(
            !html.contains(r#"data-depth="1""#),
            "flat board indented: {html}"
        );
    }

    #[test]
    fn baked_page_contains_no_viewer_identity() {
        // The cache-sharing invariant from DESIGN.md §3.3. If this ever fails, every
        // reader gets their own cache entry and the read path stops being free.
        let p = page(vec![post(1, "0001", PostState::Visible, "<p>hi</p>")]);
        let html = thread_page(&space(), &p).into_string();
        for marker in ["logged in", "csrf", "session", "Sign out", "your vote"] {
            assert!(
                !html.to_lowercase().contains(&marker.to_lowercase()),
                "viewer-specific marker {marker:?} in baked HTML"
            );
        }
    }

    #[test]
    fn pager_appears_only_when_there_is_a_next_page() {
        let mut p = page(vec![post(1, "0001", PostState::Visible, "<p>a</p>")]);
        assert!(!thread_page(&space(), &p)
            .into_string()
            .contains("next page"));
        p.next_cursor = Some(Path::parse("0009").unwrap());
        let html = thread_page(&space(), &p).into_string();
        assert!(html.contains("next page"));
        assert!(html.contains("after=0009"));
    }
}
