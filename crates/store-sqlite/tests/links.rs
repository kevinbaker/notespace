//! Every link a rendered page emits has to match a route the app actually serves.
//!
//! Three dangling links shipped before this existed: `/s/{path}` and `/u/{name}` had no routes at
//! all, and every post carried a second reply link pointing at `/p/{id}/reply`, which never
//! existed. None of them fail a build, a type check, or any other test — they are only visible as
//! a 404 to whoever clicks.

use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::path::Path;

const WORKER_SRC: &str = include_str!("../../app/src/lib.rs");

/// Route patterns the router registers, as axum spells them: every string literal that follows
/// `.route(`, whether on the same line or, after rustfmt, the next.
fn routes() -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = WORKER_SRC;
    while let Some(i) = rest.find(".route(") {
        rest = &rest[i + ".route(".len()..];
        let trimmed = rest.trim_start();
        if let Some(after_quote) = trimmed.strip_prefix('"') {
            if let Some(pattern) = after_quote.split('"').next() {
                out.push(pattern.to_string());
            }
        }
    }
    // Served in front of the router: by Cloudflare's asset layer, or by the binary's own route.
    out.push("/static/{*path}".into());
    out.sort();
    out.dedup();
    out
}

/// `/t/{id}` matches `/t/abc`; `/s/{*path}` matches any depth beneath `/s/`.
fn matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut r) = (pattern, path);
    loop {
        match p.find('{') {
            None => return p == r,
            Some(i) => {
                if !r.starts_with(&p[..i]) {
                    return false;
                }
                r = &r[i..];
                let close = match p[i..].find('}') {
                    Some(c) => i + c,
                    None => return false,
                };
                let wildcard = p[i + 1..close].starts_with('*');
                p = &p[close + 1..];
                // A named segment stops at the next `/`; a wildcard swallows them.
                let seg_end = if wildcard {
                    r.len()
                } else {
                    r.find('/').unwrap_or(r.len())
                };
                if seg_end == 0 {
                    return false;
                }
                // With a wildcard the rest of the pattern must still match some suffix.
                if wildcard {
                    return (1..=r.len()).any(|n| matches(p, &r[n..]));
                }
                r = &r[seg_end..];
            }
        }
    }
}

/// `href` and `action` targets, minus fragments, query strings and external links.
fn local_targets(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["href=\"", "action=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(attr) {
            rest = &rest[i + attr.len()..];
            let Some(end) = rest.find('"') else { break };
            let raw = &rest[..end];
            rest = &rest[end..];
            let path = raw.split(['?', '#']).next().unwrap_or("");
            if path.starts_with('/') && !path.starts_with("//") {
                out.push(path.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

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

fn summary() -> ThreadSummary {
    ThreadSummary {
        public_id: PublicId::new(1_735_689_600_000, 0xC0FFEE).unwrap(),
        title: "Hello".into(),
        post_count: 2,
        bumped_at: 1_735_689_600_000,
        author_name: "alice".into(),
        space_name: "General".into(),
        space_path: "general/".into(),
    }
}

fn thread_page_fixture() -> ThreadPage {
    let path = Path::parse("0001").unwrap();
    ThreadPage {
        space: space(),
        thread: Thread {
            id: 42,
            public_id: PublicId::new(1_735_689_600_000, 0x2468_ACE0).unwrap(),
            space_id: 1,
            kind: ThreadKind::Discussion,
            title: "Hello".into(),
            url: None,
            author_id: 1,
            author_name: "alice".into(),
            created_at: 1_735_689_600_000,
            bumped_at: 1_735_689_600_000,
            post_count: 1,
            state: ThreadState::Visible,
            cache_version: 0,
        },
        posts: vec![Post {
            id: 1,
            public_id: PublicId::new(1_735_689_600_001, 1).unwrap(),
            thread_id: 42,
            parent_id: None,
            depth: path.depth() as u32,
            path,
            author_id: 1,
            author_name: "alice".into(),
            body_md: None,
            body_html: "<p>hi</p>".into(),
            created_at: 1_735_689_600_000,
            edited_at: None,
            score: 0.0,
            state: PostState::Visible,
        }],
        // Present so the pager's link is covered too.
        next_cursor: Some(Path::parse("0002").unwrap()),
    }
}

fn profile() -> Profile {
    Profile {
        user: User {
            id: 1,
            name: "alice".into(),
            state: UserState::Active,
            role: Role::Member,
        },
        created_at: 1_735_689_600_000,
        posts: vec![ProfilePost {
            public_id: PublicId::new(1_735_689_600_001, 1).unwrap(),
            thread_public_id: PublicId::new(1_735_689_600_000, 0xC0FFEE).unwrap(),
            thread_title: "Hello".into(),
            body_html: "<p>hi</p>".into(),
            created_at: 1_735_689_600_000,
        }],
    }
}

fn every_page() -> Vec<(&'static str, String)> {
    use notespace_render::auth::{ParentPost, ReplyTarget, SignInOptions};
    use notespace_render::{account, auth, compose, index, page, profile as profile_page};
    let t = thread_page_fixture();
    let tid = t.thread.public_id.encode();
    let pid = t.posts[0].public_id.encode();
    let account = Account {
        email: Some("a@example.com".into()),
        email_verified_at: None,
        has_password: true,
    };
    vec![
        ("thread", page::thread_page(&t).into_string()),
        (
            "index",
            index::index_page(&[space()], &[summary()]).into_string(),
        ),
        (
            "space",
            index::space_page(&space(), &[], &[summary()]).into_string(),
        ),
        (
            "profile",
            profile_page::profile_page(&profile()).into_string(),
        ),
        (
            "login",
            auth::login_page(
                "tok",
                Some("/t/abc"),
                None,
                None,
                &SignInOptions {
                    password_form: true,
                    providers: &[],
                },
            )
            .into_string(),
        ),
        (
            "register",
            auth::register_page("tok", "", "", false, false, None, &[]).into_string(),
        ),
        (
            "reply",
            auth::reply_page(
                "tok",
                &tid,
                &ReplyTarget {
                    thread_title: "Hello",
                    parent: Some(ParentPost {
                        public_id: &pid,
                        author_name: "alice",
                        body_html: "<p>hi</p>",
                    }),
                },
                "",
                None,
            )
            .into_string(),
        ),
        (
            "new thread",
            compose::new_thread_page("tok", "general", "General", &Default::default(), None)
                .into_string(),
        ),
        (
            "edit",
            compose::edit_page("tok", &pid, Some("hi"), None).into_string(),
        ),
        (
            "settings",
            account::settings_page("tok", &profile().user, &account, &[], None).into_string(),
        ),
        ("held", auth::held_page(&tid, &pid).into_string()),
        (
            "error",
            notespace_render::error_page(404, "Not Found").into_string(),
        ),
    ]
}

#[test]
fn every_rendered_link_matches_a_live_route() {
    let routes = routes();
    assert!(
        routes.len() >= 8,
        "only parsed {} routes out of the worker source: {routes:?}",
        routes.len()
    );

    let mut dangling = Vec::new();
    for (name, html) in every_page() {
        for target in local_targets(&html) {
            if !routes.iter().any(|r| matches(r, &target)) {
                dangling.push(format!("{name} page links {target}"));
            }
        }
    }
    assert!(
        dangling.is_empty(),
        "links with no route:\n  {}\nroutes: {routes:?}",
        dangling.join("\n  ")
    );
}

#[test]
fn the_matcher_distinguishes_segments_from_wildcards() {
    assert!(matches("/", "/"));
    assert!(matches("/t/{id}", "/t/abc"));
    assert!(!matches("/t/{id}", "/t/abc/def"), "a segment ate a slash");
    assert!(!matches("/t/{id}", "/t/"), "an empty segment matched");
    assert!(matches("/t/{id}/{slug}", "/t/abc/hello"));
    assert!(matches("/t/{id}/reply", "/t/abc/reply"));
    assert!(!matches("/t/{id}/reply", "/t/abc/other"));
    assert!(matches("/s/{*path}", "/s/a"));
    assert!(
        matches("/s/{*path}", "/s/a/b/c"),
        "a wildcard stopped early"
    );
    assert!(!matches("/s/{*path}", "/u/a"));
    assert!(
        !matches("/p/{id}", "/p/abc/reply"),
        "the bug this test exists for"
    );
}
