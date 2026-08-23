//! The thread index. User-agnostic like the thread page, so the nav is static links.

use maud::{html, Markup, DOCTYPE};
use notespace_core::model::ThreadSummary;

/// Identical for everyone: a "signed in as …" here would make every page per-viewer.
pub fn nav() -> Markup {
    html! {
        nav class="site" {
            a class="brand" href="/" { "notespace" }
            span class="spacer" {}
            a href="/login" { "sign in" }
            " · "
            a href="/register" { "register" }
        }
    }
}

/// Render the index.
pub fn index_page(threads: &[ThreadSummary]) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "notespace" }
                style { (IndexStyle) }
            }
            body {
                (nav())
                main {
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
                                        " · " (t.space_name)
                                        " · started by " (t.author_name)
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

struct IndexStyle;

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
             .empty{color:#666}\
             @media(prefers-color-scheme:dark){body{background:#16181c;color:#e8e8ea}\
             nav.site{border-color:#2a2e37}nav.site a{color:#8ab0ff}\
             ol.threads li{border-color:#23262d}a.title{color:#8ab0ff}\
             .meta,.empty{color:#9aa0ab}}",
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

    #[test]
    fn a_title_is_escaped_rather_than_trusted() {
        let html = index_page(&[thread("<script>alert(1)</script>", 3)]).into_string();
        assert!(!html.contains("<script>alert"), "a title was emitted raw");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_index_contains_no_viewer_identity() {
        let html = index_page(&[thread("Hello", 1)]).into_string();
        for marker in ["signed in", "csrf", "session", "Sign out", "logout"] {
            assert!(!html.contains(marker), "index contains {marker:?}");
        }
    }

    #[test]
    fn an_empty_index_says_so_rather_than_rendering_nothing() {
        let html = index_page(&[]).into_string();
        assert!(html.contains("No threads yet"));
    }

    #[test]
    fn post_count_is_pluralised() {
        let one = index_page(&[thread("a", 1)]).into_string();
        assert!(one.contains("1 post"), "no count rendered");
        assert!(!one.contains("1 posts"), "pluralised a single post");
        assert!(index_page(&[thread("a", 2)])
            .into_string()
            .contains("2 posts"));
    }
}
