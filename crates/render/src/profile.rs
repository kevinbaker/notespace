//! A member's page. Public and user-agnostic, so briefly cacheable.

use crate::index::{nav, IndexStyle};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use notespace_core::model::{Profile, UserState};

pub fn profile_page(profile: &Profile) -> Markup {
    let u = &profile.user;
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (u.name) " — notespace" }
                style { (IndexStyle) }
            }
            body {
                (nav())
                main {
                    h1 { (u.name) }
                    @match u.state {
                        // The name stays taken; the page says why it resolves to nothing.
                        UserState::Deleted => p class="empty" { "This account has been deleted." },
                        UserState::Banned => p class="empty" { "This account is suspended." },
                        UserState::Active => {
                            p class="meta" {
                                "member since "
                                time datetime=(profile.created_at) { (profile.created_at) }
                            }
                            @if profile.posts.is_empty() {
                                p class="empty" { "No posts yet." }
                            } @else {
                                ol class="threads" {
                                    @for p in &profile.posts {
                                        li {
                                            a class="title" href={ "/p/" (p.public_id) } { (p.thread_title) }
                                            div class="meta" {
                                                time datetime=(p.created_at) { (p.created_at) }
                                                " in "
                                                a href={ "/t/" (p.thread_public_id) } { "the thread" }
                                            }
                                            // Sanitized at write time; the one legitimate PreEscaped.
                                            div class="post-body" { (PreEscaped(&p.body_html)) }
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
