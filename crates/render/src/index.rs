//! The thread index and the space pages. User-agnostic like the thread page, so the nav is
//! static links; "account" goes to a page that asks for sign-in if there is none.

use maud::{html, Markup, DOCTYPE};
use notespace_core::model::{Space, ThreadSummary};

/// Identical for everyone: a "signed in as …" here would make every page per-viewer.
pub fn nav() -> Markup {
    html! {
        nav class="site" {
            a class="brand" href="/" { "notespace" }
            span class="spacer" {}
            a href="/login" { "sign in" }
            " · "
            a href="/register" { "register" }
            " · "
            a href="/settings" { "account" }
        }
    }
}

/// A list of threads, shared by the index and the space page.
pub fn thread_list(threads: &[ThreadSummary]) -> Markup {
    html! {
        @if threads.is_empty() {
            p class="empty" { "No threads yet." }
        } @else {
            ol class="threads" {
                @for t in threads {
                    li {
                        a class="title" href={ "/t/" (t.public_id) } { (t.title) }
                        div class="meta" {
                            (t.post_count)
                            @if t.post_count == 1 { " post" } @else { " posts" }
                            " · "
                            a href={ "/s/" (t.space_path.trim_end_matches('/')) } { (t.space_name) }
                            " · started by "
                            a href={ "/u/" (t.author_name) } { (t.author_name) }
                            " · last activity " (crate::time::stamp(t.bumped_at))
                        }
                    }
                }
            }
        }
    }
}

/// Spaces as a row of links; nothing if there are none.
pub fn space_list(spaces: &[Space]) -> Markup {
    html! {
        @if !spaces.is_empty() {
            ul class="spaces" {
                @for s in spaces {
                    li { a href={ "/s/" (s.path.trim_end_matches('/')) } { (s.name) } }
                }
            }
        }
    }
}

/// Render the index: the top-level spaces, then the most recently active threads anywhere.
pub fn index_page(spaces: &[Space], threads: &[ThreadSummary]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { "notespace" }
                style { (IndexStyle) }
            }
            body {
                (nav())
                main {
                    (space_list(spaces))
                    (thread_list(threads))
                }
            }
        }
    }
}

/// One space: where it sits, what is under it, and its threads (subspaces included).
pub fn space_page(space: &Space, children: &[Space], threads: &[ThreadSummary]) -> Markup {
    let url = space.path.trim_end_matches('/');
    // Every ancestor is a link, so the page is its own breadcrumb.
    let segments: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                link rel="icon" href="data:,";
                title { (space.name) " — notespace" }
                style { (IndexStyle) }
            }
            body {
                (nav())
                main {
                    p class="crumbs" {
                        a href="/" { "notespace" }
                        @for (i, seg) in segments.iter().enumerate() {
                            " / "
                            @if i + 1 == segments.len() {
                                (seg)
                            } @else {
                                a href={ "/s/" (segments[..=i].join("/")) } { (seg) }
                            }
                        }
                    }
                    h1 { (space.name) }
                    p class="actions" {
                        a class="button" href={ "/s/" (url) "/new" } { "Start a thread" }
                    }
                    (space_list(children))
                    (thread_list(threads))
                }
            }
        }
    }
}

pub(crate) struct IndexStyle;

impl maud::Render for IndexStyle {
    fn render_to(&self, out: &mut String) {
        out.push_str(
            "body{font:16px/1.5 system-ui,sans-serif;margin:0;background:#fbfbfc;color:#1a1a1a}\
             nav.site{display:flex;align-items:center;gap:.5rem;padding:.75rem 1rem;\
             border-bottom:1px solid #e4e4e8;font-size:.9rem}\
             nav.site .brand{font-weight:600;text-decoration:none;color:inherit}\
             nav.site .spacer{flex:1}\
             nav.site a{color:#3355bb}\
             main{max-width:44rem;margin:1.5rem auto;padding:0 1rem}\
             ol.threads{list-style:none;margin:0;padding:0}\
             ol.threads li{padding:.7rem 0;border-bottom:1px solid #ececed}\
             a.title{font-size:1.05rem;text-decoration:none;color:#1a3fa0}\
             a.title:hover{text-decoration:underline}\
             .meta{color:#666;font-size:.85rem;margin-top:.15rem}\
             .meta a{color:inherit}\
             .empty,.crumbs{color:#666}\
             .crumbs{font-size:.85rem;margin:0}.crumbs a{color:inherit}\
             h1{font-size:1.4rem;margin:.25rem 0 .5rem}\
             ul.spaces{list-style:none;margin:0 0 1rem;padding:0;display:flex;flex-wrap:wrap;gap:.5rem}\
             ul.spaces a{display:inline-block;padding:.2rem .6rem;border:1px solid #d6d6db;\
             border-radius:1rem;text-decoration:none;color:#1a3fa0;font-size:.9rem}\
             .actions{margin:.5rem 0 1rem}\
             a.button{display:inline-block;padding:.4rem .8rem;border-radius:4px;\
             background:#1a1a1a;color:#fff;text-decoration:none;font-size:.9rem}\
             .post-body{margin:.35rem 0}.post-body>*:first-child{margin-top:0}\
             .post-body>*:last-child{margin-bottom:0}\
             @media(prefers-color-scheme:dark){body{background:#16181c;color:#e8e8ea}\
             nav.site{border-color:#2a2e37}nav.site a{color:#8ab0ff}\
             ol.threads li{border-color:#23262d}a.title{color:#8ab0ff}\
             ul.spaces a{color:#8ab0ff;border-color:#3a3f4b}\
             a.button{background:#e8e8ea;color:#16181c}\
             .meta,.empty,.crumbs{color:#9aa0ab}}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::id::PublicId;

    fn thread(title: &str, posts: u32) -> ThreadSummary {
        ThreadSummary {
            public_id: PublicId::new(1_735_689_600_000, 0xC0FFEE).unwrap(),
            title: title.into(),
            post_count: posts,
            bumped_at: 1_735_689_600_000,
            author_name: "alice".into(),
            space_name: "General".into(),
            space_path: "general/".into(),
        }
    }

    fn space(path: &str, name: &str) -> Space {
        Space {
            id: 1,
            path: path.into(),
            name: name.into(),
            parent_id: None,
            ranking: notespace_core::model::Ranking::Bump,
            depth_cap: 8,
        }
    }

    #[test]
    fn a_title_is_escaped_rather_than_trusted() {
        let html = index_page(&[], &[thread("<script>alert(1)</script>", 3)]).into_string();
        assert!(!html.contains("<script>alert"), "a title was emitted raw");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_index_contains_no_viewer_identity() {
        let html = index_page(&[], &[thread("Hello", 1)]).into_string();
        for marker in ["signed in", "csrf", "session", "Sign out", "logout"] {
            assert!(!html.contains(marker), "index contains {marker:?}");
        }
    }

    #[test]
    fn an_empty_index_says_so_rather_than_rendering_nothing() {
        let html = index_page(&[], &[]).into_string();
        assert!(html.contains("No threads yet"));
        assert!(
            !html.contains("<ul class=\"spaces\""),
            "an empty space list rendered"
        );
    }

    #[test]
    fn the_index_links_each_top_level_space() {
        let html = index_page(&[space("sports/", "Sports")], &[]).into_string();
        assert!(
            html.contains(r#"href="/s/sports""#),
            "no space link: {html}"
        );
        assert!(
            !html.contains(r#"href="/s/sports/""#),
            "trailing separator leaked into the URL"
        );
    }

    #[test]
    fn the_space_page_is_its_own_breadcrumb_and_offers_a_new_thread() {
        let html = space_page(&space("sports/hockey/", "Hockey"), &[], &[]).into_string();
        assert!(
            html.contains(r#"href="/s/sports""#),
            "ancestor not linked: {html}"
        );
        assert!(
            html.contains(r#"href="/s/sports/hockey/new""#),
            "no new-thread link"
        );
        assert!(html.contains("<h1>Hockey</h1>"));
    }

    #[test]
    fn the_thread_list_links_the_space_and_the_author() {
        let html = thread_list(&[thread("Hello", 1)]).into_string();
        assert!(html.contains(r#"href="/s/general""#));
        assert!(html.contains(r#"href="/u/alice""#));
    }

    #[test]
    fn post_count_is_pluralised() {
        let one = index_page(&[], &[thread("a", 1)]).into_string();
        assert!(one.contains("1 post"), "no count rendered");
        assert!(!one.contains("1 posts"), "pluralised a single post");
        assert!(index_page(&[], &[thread("a", 2)])
            .into_string()
            .contains("2 posts"));
    }
}
