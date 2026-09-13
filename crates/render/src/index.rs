//! The thread index and the space pages. User-agnostic like the thread page, so the nav is
//! static links; "account" goes to a page that asks for sign-in if there is none.

use crate::layout::{crumbs, Shell, SpaceTheme, Width};
use maud::{html, Markup};
use notespace_core::model::{Space, ThreadSummary};
use notespace_core::theme::Theme;

/// A list of threads, shared by the index and the space page. The count sits in its own
/// column so a page of them scans.
pub fn thread_list(threads: &[ThreadSummary]) -> Markup {
    html! {
        @if threads.is_empty() {
            p class="empty" { "No threads yet." }
        } @else {
            ol class="threads" {
                @for t in threads {
                    li {
                        a class="title" href={ "/t/" (t.public_id) } { (t.title) }
                        span class="count" title={
                            (t.post_count) @if t.post_count == 1 { " post" } @else { " posts" }
                        } { (t.post_count) }
                        div class="meta" {
                            a href={ "/s/" (t.space_path.trim_end_matches('/')) } { (t.space_name) }
                            " · "
                            a href={ "/u/" (t.author_name) } { (t.author_name) }
                            " · " (crate::time::stamp(t.bumped_at))
                        }
                    }
                }
            }
        }
    }
}

/// Spaces as a row of chips; nothing if there are none.
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
    Shell::default().render(html! {
        (space_list(spaces))
        (thread_list(threads))
    })
}

/// One space: where it sits, what is under it, and its threads (subspaces included).
pub fn space_page(space: &Space, children: &[Space], threads: &[ThreadSummary]) -> Markup {
    let url = space.path.trim_end_matches('/');
    let theme = Theme::from_config(&space.config);
    // Every ancestor is a link, so the header is the page's breadcrumb.
    let segments: Vec<&str> = url.split('/').filter(|s| !s.is_empty()).collect();
    let trail = segments.iter().enumerate().map(|(i, seg)| {
        let href = (i + 1 < segments.len()).then(|| format!("/s/{}", segments[..=i].join("/")));
        (*seg, href)
    });
    Shell {
        title: &space.name,
        width: Width::Normal,
        crumbs: crumbs(trail),
        theme: Some(SpaceTheme {
            url: format!("/s/{url}"),
            theme: &theme,
        }),
        ..Default::default()
    }
    .render(html! {
        h1 { (space.name) }
        p class="actions" {
            a class="button" href={ "/s/" (url) "/new" } { "Start a thread" }
        }
        (space_list(children))
        (thread_list(threads))
    })
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
            config: "{}".into(),
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
