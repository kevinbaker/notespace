//! A member's page. Public and user-agnostic, so briefly cacheable.

use crate::layout::{crumbs, Shell};
use maud::{html, Markup, PreEscaped};
use notespace_core::model::{Profile, UserState};

pub fn profile_page(profile: &Profile) -> Markup {
    let u = &profile.user;
    Shell {
        title: &u.name,
        crumbs: crumbs([(u.name.as_str(), None)]),
        ..Default::default()
    }
    .render(html! {
        h1 { (u.name) }
        @match u.state {
            // The name stays taken; the page says why it resolves to nothing.
            UserState::Deleted => p class="empty" { "This account has been deleted." },
            UserState::Banned => p class="empty" { "This account is suspended." },
            UserState::Active => {
                p class="meta" {
                    "member since "
                    (crate::time::stamp(profile.created_at))
                }
                @if profile.posts.is_empty() {
                    p class="empty" { "No posts yet." }
                } @else {
                    ol class="posts" {
                        @for p in &profile.posts {
                            li class="post" data-depth="0" {
                                div class="post-head meta" {
                                    a class="author" href={ "/t/" (p.thread_public_id) } { (p.thread_title) }
                                    " "
                                    a class="permalink" href={ "/p/" (p.public_id) } {
                                        (crate::time::stamp(p.created_at))
                                    }
                                }
                                // Sanitized at write time; the one legitimate PreEscaped.
                                div class="post-body" { (PreEscaped(&p.body_html)) }
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notespace_core::id::PublicId;
    use notespace_core::model::{ProfilePost, Role, User};

    fn profile(state: UserState, posts: Vec<ProfilePost>) -> Profile {
        Profile {
            user: User {
                id: 1,
                name: "alice".into(),
                state,
                role: Role::Member,
            },
            created_at: 1_700_000_000,
            posts,
        }
    }

    fn post(html: &str) -> ProfilePost {
        ProfilePost {
            public_id: PublicId::new(1_735_689_600_000, 1).unwrap(),
            thread_public_id: PublicId::new(1_735_689_600_000, 2).unwrap(),
            thread_title: "A <thread>".into(),
            body_html: html.into(),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn posts_link_to_their_permalink_and_thread() {
        let p = profile(UserState::Active, vec![post("<p>hi</p>")]);
        let html = profile_page(&p).into_string();
        assert!(html.contains(&format!("/p/{}", p.posts[0].public_id)));
        assert!(html.contains(&format!("/t/{}", p.posts[0].thread_public_id)));
        assert!(html.contains("<p>hi</p>"), "body not emitted");
        assert!(html.contains("A &lt;thread&gt;"), "title not escaped");
    }

    #[test]
    fn a_deleted_account_shows_a_tombstone_and_no_posts() {
        let p = profile(UserState::Deleted, vec![post("<p>secret</p>")]);
        let html = profile_page(&p).into_string();
        assert!(html.contains("deleted"));
        assert!(!html.contains("secret"));
    }
}
