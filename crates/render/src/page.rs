//! Read-path templates. Nothing here takes a viewer: the baked HTML is shared byte-for-byte, and
//! personalisation is layered client-side from `GET /api/me/thread/{id}`. Posts arrive already
//! rendered and in preorder, so this is one linear pass.

use maud::{html, Markup, PreEscaped, DOCTYPE};
use notespace_core::model::{PostState, ThreadPage};

/// Deepest visual indent, so a deep subthread cannot squeeze the text column to nothing.
const MAX_INDENT: u32 = 8;

/// `space` supplies `depth_cap`, so flat and threaded boards are the same code path.
pub fn thread_page(page: &ThreadPage) -> Markup {
    let space = &page.space;
    let t = &page.thread;
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { (t.title) " — " (space.name) }
                link rel="alternate" type="application/rss+xml"
                     title=(t.title) href={ "/t/" (t.public_id) ".rss" };
                style { (PreEscaped(STYLE)) }
            }
            body {
                header class="site" {
                    a href="/" { "notespace" }
                    " / "
                    a href={ "/s/" (space.path.trim_end_matches('/')) } { (space.name) }
                    // Which of these applies to the reader is not knowable in a baked page.
                    span class="site-auth" {
                        a href={ "/t/" (t.public_id) ".rss" } { "rss" }
                        " · "
                        a href="/modlog" { "modlog" }
                        " · "
                        a href="/login" { "sign in" }
                        " · "
                        a href="/register" { "register" }
                        " · "
                        a href="/settings" { "account" }
                    }
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
                            // The public id: `post.id` would leak the post count into every page.
                            li class="post" id={ "p" (post.public_id) }
                               style={ "--indent:" (indent) }
                               data-depth=(indent) {
                                div class="post-head" {
                                    a class="author" href={ "/u/" (post.author_name) } {
                                        (post.author_name)
                                    }
                                    " "
                                    // Keeps resolving after a split or merge moves the post.
                                    a class="permalink" href={ "/p/" (post.public_id) } {
                                        (crate::time::stamp(post.created_at))
                                    }
                                    @if post.edited_at.is_some() { span class="edited" { " (edited)" } }
                                    " "
                                    // A link, not a form: a CSRF token here reaches every reader.
                                    a class="reply" href={
                                        "/t/" (t.public_id) "/reply?parent=" (post.public_id)
                                    } { "reply" }
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
                                    // Sanitized at write time; the one legitimate PreEscaped.
                                    PostState::Visible => div class="post-body" {
                                        (PreEscaped(&post.body_html))
                                    },
                                }
                                div class="post-actions" {
                                    a href={
                                        "/t/" (t.public_id) "/reply?parent=" (post.public_id)
                                    } { "reply" }
                                    " · "
                                    // Links for the same reason reply is: each form needs a token.
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
                }
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
    fn pager_appears_only_when_there_is_a_next_page() {
        let mut p = page(vec![post(1, "0001", PostState::Visible, "<p>a</p>")]);
        assert!(!thread_page(&p).into_string().contains("next page"));
        p.next_cursor = Some(Path::parse("0009").unwrap());
        let html = thread_page(&p).into_string();
        assert!(html.contains("next page"));
        assert!(html.contains("after=0009"));
    }
}
