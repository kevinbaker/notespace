//! Renders one of every page with made-up content, for looking at the design without a
//! database:
//!
//! ```text
//! cargo run -p notespace-render --example preview -- target/preview
//! ```
//!
//! Links between the pages are rewritten to the files here, and `public/static` is copied in,
//! so the output browses like the site. Forms post nowhere.

use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::moderation::classify::Call;
use notespace_core::moderation::{ActorKind, LogEntry, ReviewItem, ReviewReason};
use notespace_core::path::Path;
use notespace_render::auth::{ParentPost, ProviderButton, ReplyTarget, SignInOptions};
use notespace_render::{
    account, admin, auth, compose, index, markdown_to_html, moderation, page, profile,
};
use std::fs;

const NOW: i64 = 1_757_800_000_000;
const H: i64 = 3_600_000;

fn pid(n: u32) -> PublicId {
    PublicId::new(1_757_000_000_000 + n as u64 * 1000, n).unwrap()
}

fn space(id: i64, path: &str, name: &str, config: &str) -> Space {
    Space {
        id,
        path: path.into(),
        name: name.into(),
        parent_id: None,
        ranking: Ranking::Bump,
        depth_cap: 8,
        config: config.into(),
    }
}

fn summary(
    n: u32,
    title: &str,
    posts: u32,
    space: (&str, &str),
    by: &str,
    ago: i64,
) -> ThreadSummary {
    ThreadSummary {
        public_id: pid(n),
        title: title.into(),
        post_count: posts,
        bumped_at: NOW - ago,
        author_name: by.into(),
        space_name: space.1.into(),
        space_path: space.0.into(),
    }
}

fn post(n: u32, path: &str, by: &str, md: &str, ago: i64, state: PostState) -> Post {
    let path = Path::parse(path).unwrap();
    Post {
        id: n as i64,
        public_id: pid(100 + n),
        thread_id: 1,
        parent_id: None,
        depth: path.depth() as u32,
        path,
        author_id: 1,
        author_name: by.into(),
        body_md: Some(md.into()),
        body_html: markdown_to_html(md),
        created_at: NOW - ago,
        edited_at: if n == 3 { Some(NOW) } else { None },
        score: 0.0,
        state,
    }
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/preview".into());
    fs::create_dir_all(format!("{out}/static/fonts")).unwrap();
    let public = concat!(env!("CARGO_MANIFEST_DIR"), "/public/static");
    for entry in fs::read_dir(public)
        .unwrap()
        .chain(fs::read_dir(format!("{public}/fonts")).unwrap())
    {
        let path = entry.unwrap().path();
        if path.is_file() {
            let rel = path.strip_prefix(public).unwrap();
            fs::copy(&path, format!("{out}/static/{}", rel.display())).unwrap();
        }
    }

    let general = ("general/", "General");
    let rust = ("dev/rust/", "Rust");
    let hockey = ("sports/hockey/", "Ice Hockey");
    let spaces = vec![
        space(1, "general/", "General", "{}"),
        space(2, "dev/", "Development", "{}"),
        space(3, "sports/", "Sports", "{}"),
        space(4, "meta/", "Meta", "{}"),
    ];
    let threads = vec![
        summary(
            1,
            "Is a Rust forum on a free Cloudflare account actually viable?",
            47,
            general,
            "alice",
            12 * 60_000,
        ),
        summary(
            2,
            "Show: notespace now imports a Hacker News dump through the real write path",
            12,
            rust,
            "kevin",
            2 * H,
        ),
        summary(3, "Trade deadline thread", 213, hockey, "puckhead", 3 * H),
        summary(
            4,
            "What the 8B classifier gets wrong, with examples",
            31,
            ("meta/", "Meta"),
            "mira",
            5 * H,
        ),
        summary(
            5,
            "Materialized paths vs. adjacency lists for deep threads",
            9,
            rust,
            "tomas",
            26 * H,
        ),
        summary(6, "Weekly open thread", 1, general, "alice", 3 * 24 * H),
        summary(
            7,
            "Proposal: public moderation log by default",
            58,
            ("meta/", "Meta"),
            "dana",
            4 * 24 * H,
        ),
        summary(
            8,
            "Goalie interference is still not a real rule",
            88,
            hockey,
            "zamboni",
            6 * 24 * H,
        ),
    ];

    // Index.
    write(&out, "index.html", index::index_page(&spaces, &threads));

    // A space with a theme: token overrides plus a stylesheet of its own, the way a subreddit
    // has one. The sheet is written against the site's class names and tokens.
    let hockey_theme = notespace_core::theme::Theme::parse_lines(
        "accent: #1d4ed8\nmeasure: 52rem\nradius: 0\ndark.accent: #7aa2ff",
    )
    .unwrap()
    .with_css(
        ".site{border-bottom:3px solid var(--accent)}\n\
         .site .brand::after{content:\" · Ice Hockey\";font-weight:400;color:var(--fg-2)}\n\
         .thread-title,.threads .title{text-transform:uppercase;letter-spacing:.02em;font-size:.95em}\n\
         .post[data-depth=\"0\"]{border-top:1px solid var(--line);padding-top:.6rem}",
    )
    .unwrap();
    let themed = space(
        5,
        "sports/hockey/",
        "Ice Hockey",
        &serde_json::json!({ "theme": hockey_theme.to_json() }).to_string(),
    );
    fs::write(
        format!("{out}/hockey-theme.css"),
        notespace_render::layout::theme_css(&hockey_theme),
    )
    .unwrap();
    let hockey_threads: Vec<_> = threads
        .iter()
        .filter(|t| t.space_path == hockey.0)
        .cloned()
        .collect();
    write(
        &out,
        "space.html",
        index::space_page(&themed, &[], &hockey_threads),
    );
    write(
        &out,
        "space-plain.html",
        index::space_page(
            &space(2, "dev/", "Development", "{}"),
            &[
                space(6, "dev/rust/", "Rust", "{}"),
                space(7, "dev/go/", "Go", "{}"),
            ],
            &threads[..5],
        ),
    );

    // A thread, nested.
    let posts = vec![
        post(1, "0001", "alice", "The M0 numbers are in. **0.048 ms** of CPU for a 200-post page against a 10 ms budget, two D1 queries in one batched round trip, and `rows_read` identical between the emulator and production.\n\nThe binding constraint turns out to be the 100k requests/day cap, not compute. Details in [M0-findings](https://example.com/M0).", 26 * H, PostState::Visible),
        post(2, "0001.0001", "tomas", "How much of that 0.048 ms is maud vs. the D1 client deserialising rows?", 25 * H, PostState::Visible),
        post(3, "0001.0001.0001", "alice", "Almost all rendering. The D1 deserialisation is a few microseconds; I measured it separately with the bench-wasm harness:\n\n```\nrender_200   47.9 µs\nrows_200      2.1 µs\n```\n\nSo the page template is the cost, and it is fine.", 24 * H, PostState::Visible),
        post(4, "0001.0001.0001.0001", "tomas", "Good. Then the 100k/day cap is the whole story — and inlining the CSS instead of linking it is the right call for exactly that reason.", 23 * H, PostState::Visible),
        post(5, "0001.0001.0002", "mira", "> two D1 queries in one batched round trip\n\nDoes the second one stay cheap when a thread has more than one page?", 20 * H, PostState::Visible),
        post(6, "0001.0002", "spam-account", "", 19 * H, PostState::Pending),
        post(7, "0001.0003", "dana", "Worth saying out loud: a *small* forum is the design target here. Nobody should read these numbers as a Reddit plan.", 12 * H, PostState::Visible),
        post(8, "0001.0003.0001", "kevin", "Right. The [DESIGN.md](https://example.com/DESIGN) framing is a few hundred active people on a free account, and a single binary above that.", 11 * H, PostState::Visible),
        post(9, "0001.0004", "gone", "", 6 * H, PostState::Deleted),
        post(10, "0002", "puckhead", "Subscribed. Also: the space chips at the top of the index are a nice touch.", 40 * 60_000, PostState::Visible),
    ];
    let thread = Thread {
        id: 1,
        public_id: pid(1),
        space_id: 1,
        kind: ThreadKind::Discussion,
        title: "Is a Rust forum on a free Cloudflare account actually viable?".into(),
        url: Some("https://github.com/kevinbaker/notespace/blob/main/docs/M0-findings.md".into()),
        author_id: 1,
        author_name: "alice".into(),
        created_at: NOW - 26 * H,
        bumped_at: NOW,
        post_count: 47,
        state: ThreadState::Visible,
        cache_version: 3,
    };
    let tp = ThreadPage {
        space: spaces[0].clone(),
        thread: thread.clone(),
        posts: posts.clone(),
        next_cursor: Some(Path::parse("0003").unwrap()),
    };
    write(&out, "thread.html", page::thread_page(&tp));
    let themed_tp = ThreadPage {
        space: themed.clone(),
        thread: Thread {
            title: "Trade deadline thread".into(),
            url: None,
            ..thread.clone()
        },
        posts,
        next_cursor: None,
    };
    write(&out, "thread-themed.html", page::thread_page(&themed_tp));

    // Profile.
    let prof = Profile {
        user: User {
            id: 1,
            name: "alice".into(),
            state: UserState::Active,
            role: Role::Admin,
        },
        created_at: NOW - 400 * 24 * H,
        posts: tp
            .posts
            .iter()
            .filter(|p| p.author_name == "alice")
            .map(|p| ProfilePost {
                public_id: p.public_id.clone(),
                thread_public_id: pid(1),
                thread_title: thread.title.clone(),
                body_html: p.body_html.clone(),
                created_at: p.created_at,
            })
            .collect(),
    };
    write(&out, "profile.html", profile::profile_page(&prof));

    // Auth and forms.
    let providers = [
        ProviderButton {
            label: "Google",
            href: "#".into(),
        },
        ProviderButton {
            label: "GitHub",
            href: "#".into(),
        },
    ];
    write(
        &out,
        "login.html",
        auth::login_page(
            "t",
            None,
            None,
            None,
            &SignInOptions {
                password_form: true,
                providers: &providers,
            },
        ),
    );
    write(
        &out,
        "login-error.html",
        auth::login_page(
            "t",
            None,
            Some(auth::LoginError::Rejected),
            None,
            &SignInOptions {
                password_form: true,
                providers: &[],
            },
        ),
    );
    write(
        &out,
        "register.html",
        auth::register_page("t", "", "", false, false, None, &providers),
    );
    write(
        &out,
        "reply.html",
        auth::reply_page(
            "t",
            pid(1).as_str(),
            &ReplyTarget {
                thread_title: &thread.title,
                parent: Some(ParentPost {
                    public_id: pid(103).as_str(),
                    author_name: "alice",
                    body_html: &tp.posts[2].body_html,
                }),
            },
            "",
            None,
        ),
    );
    write(
        &out,
        "new-thread.html",
        compose::new_thread_page("t", "general", "General", &Default::default(), None),
    );
    write(
        &out,
        "settings.html",
        account::settings_page(
            "t",
            &prof.user,
            &Account {
                email: Some("alice@example.com".into()),
                email_verified_at: None,
                has_password: true,
            },
            &["Google".into()],
            Some(account::SettingsNotice::VerificationSent),
        ),
    );

    // Admin and moderation.
    let stats = SiteStats {
        users: 1_204,
        threads: 3_318,
        posts: 41_902,
        pending_posts: 3,
        open_reviews: 2,
        banned_users: 7,
    };
    write(
        &out,
        "admin.html",
        admin::dashboard(&stats, &threads, Some(admin::Notice::Saved)),
    );
    let users: Vec<UserRow> = ["alice", "kevin", "mira", "puckhead", "spam-account"]
        .iter()
        .enumerate()
        .map(|(i, n)| UserRow {
            user: User {
                id: i as i64 + 1,
                name: n.to_string(),
                state: if *n == "spam-account" {
                    UserState::Banned
                } else {
                    UserState::Active
                },
                role: if i == 0 { Role::Admin } else { Role::Member },
            },
            created_at: NOW - (i as i64 + 1) * 30 * 24 * H,
            email: Some(format!("{n}@example.com")),
            email_verified: i % 2 == 0,
            post_count: 300 / (i as u32 + 1),
        })
        .collect();
    write(
        &out,
        "admin-users.html",
        admin::users_page("", &users, None),
    );
    let details: Vec<SpaceDetail> = spaces
        .iter()
        .chain(std::iter::once(&themed))
        .map(|s| SpaceDetail {
            space: s.clone(),
            thread_count: 40,
        })
        .collect();
    write(
        &out,
        "admin-spaces.html",
        admin::spaces_page("t", &details, true, None),
    );
    write(
        &out,
        "admin-space.html",
        admin::space_page("t", &details[4], None),
    );

    let items = vec![
        ReviewItem {
            id: 1,
            post_id: 6,
            post_public_id: pid(106),
            thread_public_id: pid(1),
            thread_title: thread.title.clone(),
            space_id: 1,
            space_name: "General".into(),
            author_id: 9,
            author_name: "spam-account".into(),
            body_html: markdown_to_html("Great post!! Check out my [casino](https://example.com/a) and [another](https://example.com/b) and [one more](https://example.com/c)."),
            post_state: PostState::Pending,
            reason: ReviewReason::Classifier,
            model_verdict: Some(Call::Flag),
            model_confidence: Some(0.91),
            model_categories: vec![],
            appeal_text: None,
            opened_at: NOW - H,
            resolved: false,
        },
        ReviewItem {
            id: 2,
            post_id: 7,
            post_public_id: pid(107),
            thread_public_id: pid(3),
            thread_title: "Trade deadline thread".into(),
            space_id: 5,
            space_name: "Ice Hockey".into(),
            author_id: 4,
            author_name: "zamboni".into(),
            body_html: markdown_to_html("That trade was robbery and everyone in the front office should be embarrassed."),
            post_state: PostState::Hidden,
            reason: ReviewReason::Appeal,
            model_verdict: Some(Call::Unsure),
            model_confidence: Some(0.55),
            model_categories: vec![],
            appeal_text: Some("This is about a hockey trade, not a person. Strong opinion, not an attack.".into()),
            opened_at: NOW - 3 * H,
            resolved: false,
        },
    ];
    write(
        &out,
        "queue.html",
        moderation::queue_page(&items, "t", None),
    );
    let log: Vec<LogEntry> = [
        (ActorKind::Model, "llama-3.1-8b", "post", "hold", 1),
        (ActorKind::User, "mira", "post", "approve", 2),
        (ActorKind::Rule, "links", "post", "hold", 3),
        (ActorKind::User, "zamboni", "post", "appeal", 4),
        (ActorKind::System, "", "thread", "publish", 5),
    ]
    .iter()
    .enumerate()
    .map(|(i, (k, n, t, a, h))| LogEntry {
        id: i as i64,
        actor_kind: *k,
        actor_name: n.to_string(),
        target_kind: t.to_string(),
        target_public_id: Some(pid(100 + i as u32)),
        action: a.to_string(),
        created_at: NOW - h * H,
    })
    .collect();
    write(&out, "modlog.html", moderation::modlog_page(&log));
    write(
        &out,
        "404.html",
        notespace_render::error_page(404, "Not Found"),
    );
    eprintln!("wrote {out}/");
}

/// Site links become file links, so the preview browses.
fn write(dir: &str, name: &str, m: maud::Markup) {
    let mut s = m.into_string();
    for (from, to) in [
        ("href=\"/\"", "href=\"index.html\""),
        ("href=\"/static/", "href=\"static/"),
        (
            "href=\"/s/sports/hockey/theme.css",
            "href=\"hockey-theme.css",
        ),
        ("href=\"/login\"", "href=\"login.html\""),
        ("href=\"/register\"", "href=\"register.html\""),
        ("href=\"/settings\"", "href=\"settings.html\""),
        ("href=\"/admin\"", "href=\"admin.html\""),
        ("href=\"/admin/users\"", "href=\"admin-users.html\""),
        ("href=\"/admin/spaces\"", "href=\"admin-spaces.html\""),
        ("href=\"/mod/queue\"", "href=\"queue.html\""),
        ("href=\"/modlog\"", "href=\"modlog.html\""),
        ("href=\"/s/sports/hockey\"", "href=\"space.html\""),
        ("href=\"/s/general/new\"", "href=\"new-thread.html\""),
        ("href=\"/u/alice\"", "href=\"profile.html\""),
    ] {
        s = s.replace(from, to);
    }
    // Any thread, any space, any reply: the one sample of each.
    let re = |s: String, prefix: &str, to: &str| -> String {
        let mut out = String::new();
        let mut rest = s.as_str();
        while let Some(i) = rest.find(prefix) {
            out.push_str(&rest[..i]);
            let after = &rest[i + prefix.len()..];
            let end = after.find('"').unwrap_or(after.len());
            let target = &after[..end];
            out.push_str(if target.contains("/reply") {
                "href=\"reply.html"
            } else {
                to
            });
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    };
    s = re(s, "href=\"/t/", "href=\"thread.html");
    s = re(s, "href=\"/s/", "href=\"space-plain.html");
    s = re(s, "href=\"/p/", "href=\"thread.html");
    s = re(s, "href=\"/u/", "href=\"profile.html");
    fs::write(format!("{dir}/{name}"), s).unwrap();
}
