//! Starting threads and editing or deleting posts, against a real store.

use notespace_core::compose::{self, ComposeConfig, Draft, Outcome as ComposeOutcome, Rejected};
use notespace_core::edit::{self, DeleteOutcome, EditOutcome, Rejected as EditRejected};
use notespace_core::id::PublicId;
use notespace_core::model::{PostState, Role, SanitizedHtml, User, UserState};
use notespace_core::moderation::NoQueue;
use notespace_core::ratelimit::Limit;
use notespace_core::space_key::SpacePath;
use notespace_core::store::{Page, Store};
use notespace_store_sqlite::SqliteStore;

const MIGRATIONS: [&str; 10] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_thread_public_id.sql"),
    include_str!("../../../migrations/0003_space_paths_and_names.sql"),
    include_str!("../../../migrations/0004_post_public_id.sql"),
    include_str!("../../../migrations/0005_session.sql"),
    include_str!("../../../migrations/0006_login_attempt.sql"),
    include_str!("../../../migrations/0007_user_password.sql"),
    include_str!("../../../migrations/0008_moderation.sql"),
    include_str!("../../../migrations/0009_email_and_spaces.sql"),
    include_str!("../../../migrations/0010_external_identity.sql"),
];

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;
const OLD_USER: i64 = 1;
const NEW_USER: i64 = 2;
const MOD_USER: i64 = 3;

/// Two spaces, one nested, and three accounts: an old member, one created a minute ago, and a
/// moderator.
fn seeded() -> SqliteStore {
    let store = SqliteStore::in_memory(&MIGRATIONS).expect("migrations apply");
    store
        .conn()
        .execute_batch(&format!(
            "INSERT INTO user (id, name, created_at, role) VALUES
               ({OLD_USER}, 'oldtimer', {old}, 'member'),
               ({NEW_USER}, 'newcomer', {new}, 'member'),
               ({MOD_USER}, 'mod', {old}, 'moderator');
             INSERT INTO space (id, name, ranking, depth_cap, path, parent_id) VALUES
               (1, 'General', 'bump', 8, 'general/', NULL),
               (2, 'Meta', 'bump', 8, 'general/meta/', 1);",
            old = NOW - 400 * DAY,
            new = NOW - 60_000,
        ))
        .unwrap();
    store
}

fn user(id: i64) -> User {
    User {
        id,
        name: match id {
            OLD_USER => "oldtimer",
            NEW_USER => "newcomer",
            _ => "mod",
        }
        .into(),
        state: UserState::Active,
        role: if id == MOD_USER {
            Role::Moderator
        } else {
            Role::Member
        },
    }
}

fn config() -> ComposeConfig {
    ComposeConfig {
        per_author: Limit {
            max: 3,
            window_ms: 60_000,
        },
        per_client: Limit {
            max: 100,
            window_ms: 60_000,
        },
    }
}

fn ids(seed: u64) -> (PublicId, Vec<PublicId>) {
    let thread = PublicId::new(1_900_000_000_000 + seed * 32, 0x7EAD ^ seed as u32).unwrap();
    let posts = (1..=4u64)
        .map(|i| PublicId::new(1_900_000_000_000 + seed * 32 + i, 0xBEEF ^ i as u32).unwrap())
        .collect();
    (thread, posts)
}

fn draft<'a>(
    space: &'a SpacePath,
    author: i64,
    title: &'a str,
    url: &'a str,
    body: &'a str,
    thread_id: PublicId,
    post_ids: &'a [PublicId],
) -> Draft<'a> {
    Draft {
        space,
        author,
        title,
        url,
        body_md: body,
        body_html: SanitizedHtml::assert_sanitized(format!("<p>{body}</p>")),
        client: "203.0.113.20",
        thread_id,
        post_ids,
        now: NOW,
    }
}

#[tokio::test]
async fn a_thread_is_created_with_its_first_post_and_listed_in_its_space() {
    let store = seeded();
    let space = SpacePath::parse("general/meta").unwrap();
    let (tid, pids) = ids(1);
    let (thread, post) = match compose::create(
        &store,
        &NoQueue,
        &config(),
        draft(
            &space,
            OLD_USER,
            "  Hello, meta  ",
            "",
            "first words",
            tid.clone(),
            &pids,
        ),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Posted { thread, post } => (thread, post),
        _ => panic!("expected Posted"),
    };
    assert_eq!(thread.public_id, tid);
    assert_eq!(thread.title, "Hello, meta", "title not trimmed");
    assert_eq!(post.public_id, pids[0]);
    assert_eq!(post.depth, 0);

    let page = store.thread_page(&tid, &Page::first(10)).await.unwrap();
    assert_eq!(page.thread.post_count, 1);
    assert_eq!(page.posts.len(), 1);
    assert_eq!(page.space.id, 2);
    assert_eq!(
        page.thread.kind,
        notespace_core::model::ThreadKind::Discussion
    );

    // Listed in its own space and, via the subtree scan, in the parent.
    for path in ["general/meta", "general"] {
        let listed = store
            .space_threads(&SpacePath::parse(path).unwrap(), 10)
            .await
            .unwrap();
        assert!(
            listed.iter().any(|t| t.public_id == tid),
            "not listed under {path}"
        );
    }
    assert!(store
        .recent_threads(10)
        .await
        .unwrap()
        .iter()
        .any(|t| t.public_id == tid));
}

#[tokio::test]
async fn a_link_thread_records_its_url_and_kind() {
    let store = seeded();
    let space = SpacePath::parse("general").unwrap();
    let (tid, pids) = ids(2);
    match compose::create(
        &store,
        &NoQueue,
        &config(),
        draft(
            &space,
            OLD_USER,
            "A link",
            "https://example.com/story",
            "worth a read",
            tid.clone(),
            &pids,
        ),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Posted { thread, .. } => {
            assert_eq!(thread.kind, notespace_core::model::ThreadKind::Link);
            assert_eq!(thread.url.as_deref(), Some("https://example.com/story"));
        }
        _ => panic!("expected Posted"),
    }
    let page = store.thread_page(&tid, &Page::first(1)).await.unwrap();
    assert_eq!(
        page.thread.url.as_deref(),
        Some("https://example.com/story")
    );
}

#[tokio::test]
async fn an_unknown_space_is_rejected_before_anything_is_written() {
    let store = seeded();
    let space = SpacePath::parse("nowhere").unwrap();
    let (tid, pids) = ids(3);
    match compose::create(
        &store,
        &NoQueue,
        &config(),
        draft(&space, OLD_USER, "Lost", "", "a body", tid.clone(), &pids),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Rejected(Rejected::NoSuchSpace) => {}
        _ => panic!("expected NoSuchSpace"),
    }
    assert!(store.thread_version(&tid).await.unwrap().is_none());
}

/// The first post takes the reply path, so Tier 0 applies to it: a new account's thread is
/// held, and the thread exists around a pending post.
#[tokio::test]
async fn a_new_accounts_first_thread_is_held_for_review() {
    let store = seeded();
    let space = SpacePath::parse("general").unwrap();
    let (tid, pids) = ids(4);
    match compose::create(
        &store,
        &NoQueue,
        &config(),
        draft(
            &space,
            NEW_USER,
            "Hi everyone",
            "",
            "I am new here",
            tid.clone(),
            &pids,
        ),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Held { post, .. } => assert_eq!(post.state, PostState::Pending),
        _ => panic!("expected Held"),
    }
    let page = store.thread_page(&tid, &Page::first(1)).await.unwrap();
    assert_eq!(page.posts[0].state, PostState::Pending);
    assert!(store
        .pending_posts(NOW + 1, 10)
        .await
        .unwrap()
        .contains(&pids[0]));
}

/// A blocklisted term in the title alone is enough to hold the post under it.
#[tokio::test]
async fn the_title_is_scanned_by_tier_zero() {
    let store = seeded();
    store
        .conn()
        .execute(
            "UPDATE space SET config = '{\"moderation\":{\"blocklist\":[\"casino\"]}}' WHERE id = 1",
            [],
        )
        .unwrap();
    let space = SpacePath::parse("general").unwrap();
    let (tid, pids) = ids(5);
    match compose::create(
        &store,
        &NoQueue,
        &config(),
        draft(
            &space,
            OLD_USER,
            "Best casino bonuses",
            "",
            "a perfectly bland body",
            tid,
            &pids,
        ),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Held { .. } => {}
        ComposeOutcome::Posted { .. } => panic!("a blocklisted title was published"),
        _ => panic!("unexpected outcome"),
    }
}

#[tokio::test]
async fn starting_threads_is_rate_limited_per_author() {
    let store = seeded();
    let space = SpacePath::parse("general").unwrap();
    let cfg = config();
    for i in 0..cfg.per_author.max as u64 {
        let (tid, pids) = ids(10 + i);
        let title = format!("Thread number {i}");
        let body = format!("body number {i}");
        match compose::create(
            &store,
            &NoQueue,
            &cfg,
            draft(&space, OLD_USER, &title, "", &body, tid, &pids),
        )
        .await
        .unwrap()
        {
            ComposeOutcome::Posted { .. } => {}
            _ => panic!("thread {i} refused"),
        }
    }
    let (tid, pids) = ids(20);
    match compose::create(
        &store,
        &NoQueue,
        &cfg,
        draft(
            &space,
            OLD_USER,
            "One too many",
            "",
            "and a body",
            tid.clone(),
            &pids,
        ),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::RateLimited { retry_after_secs } => assert!(retry_after_secs > 0),
        _ => panic!("the limit did not bite"),
    }
    assert!(
        store.thread_version(&tid).await.unwrap().is_none(),
        "written despite the limit"
    );
}

async fn posted(store: &SqliteStore, seed: u64) -> (PublicId, PublicId) {
    let space = SpacePath::parse("general").unwrap();
    let (tid, pids) = ids(seed);
    let title = format!("Thread {seed}");
    let body = format!("original body {seed}");
    match compose::create(
        store,
        &NoQueue,
        &config(),
        draft(&space, OLD_USER, &title, "", &body, tid.clone(), &pids),
    )
    .await
    .unwrap()
    {
        ComposeOutcome::Posted { post, .. } => (tid, post.public_id.clone()),
        _ => panic!("setup: not posted"),
    }
}

#[tokio::test]
async fn the_author_can_edit_and_the_page_turns_over() {
    let store = seeded();
    let (tid, pid) = posted(&store, 30).await;
    let before = store.thread_version(&tid).await.unwrap().unwrap();
    let out = edit::edit(
        &store,
        &NoQueue,
        &pid,
        &user(OLD_USER),
        "revised body",
        SanitizedHtml::assert_sanitized("<p>revised body</p>".into()),
        NOW + 60_000,
    )
    .await
    .unwrap();
    assert!(matches!(out, EditOutcome::Edited));
    let page = store.thread_page(&tid, &Page::first(1)).await.unwrap();
    assert_eq!(page.posts[0].body_html, "<p>revised body</p>");
    assert_eq!(page.posts[0].edited_at, Some(NOW + 60_000));
    assert!(store.thread_version(&tid).await.unwrap().unwrap() > before);
    assert_eq!(
        store.post_for_review(&pid).await.unwrap().body_md,
        "revised body",
        "the source of truth was not rewritten"
    );
}

#[tokio::test]
async fn nobody_else_edits_not_even_a_moderator() {
    let store = seeded();
    let (_, pid) = posted(&store, 31).await;
    for who in [NEW_USER, MOD_USER] {
        let out = edit::edit(
            &store,
            &NoQueue,
            &pid,
            &user(who),
            "hijacked",
            SanitizedHtml::assert_sanitized("<p>hijacked</p>".into()),
            NOW,
        )
        .await
        .unwrap();
        assert!(
            matches!(out, EditOutcome::Rejected(EditRejected::NotYours)),
            "user {who}"
        );
    }
    assert_eq!(
        store.post_for_review(&pid).await.unwrap().body_md,
        "original body 31"
    );
}

/// The text a reader sees after an edit is not the text that was approved.
#[tokio::test]
async fn an_edit_that_trips_tier_zero_is_held_again() {
    let store = seeded();
    store
        .conn()
        .execute(
            "UPDATE space SET config = '{\"moderation\":{\"blocklist\":[\"casino\"]}}' WHERE id = 1",
            [],
        )
        .unwrap();
    let (tid, pid) = posted(&store, 32).await;
    let out = edit::edit(
        &store,
        &NoQueue,
        &pid,
        &user(OLD_USER),
        "now about a casino",
        SanitizedHtml::assert_sanitized("<p>now about a casino</p>".into()),
        NOW,
    )
    .await
    .unwrap();
    assert!(matches!(out, EditOutcome::Held { .. }));
    let page = store.thread_page(&tid, &Page::first(1)).await.unwrap();
    assert_eq!(page.posts[0].state, PostState::Pending);
}

#[tokio::test]
async fn a_locked_thread_refuses_edits() {
    let store = seeded();
    let (tid, pid) = posted(&store, 33).await;
    store
        .conn()
        .execute(
            "UPDATE thread SET state = 'locked' WHERE public_id = ?1",
            [tid.as_str()],
        )
        .unwrap();
    let out = edit::edit(
        &store,
        &NoQueue,
        &pid,
        &user(OLD_USER),
        "too late",
        SanitizedHtml::assert_sanitized("<p>too late</p>".into()),
        NOW,
    )
    .await
    .unwrap();
    assert!(matches!(out, EditOutcome::Rejected(EditRejected::Locked)));
}

#[tokio::test]
async fn deletion_is_a_tombstone_by_the_author_or_a_moderator() {
    let store = seeded();
    let (tid, pid) = posted(&store, 34).await;
    assert!(matches!(
        edit::delete(&store, &pid, &user(NEW_USER), NOW)
            .await
            .unwrap(),
        DeleteOutcome::Rejected(EditRejected::NotYours)
    ));
    assert!(matches!(
        edit::delete(&store, &pid, &user(OLD_USER), NOW)
            .await
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    let page = store.thread_page(&tid, &Page::first(1)).await.unwrap();
    assert_eq!(
        page.posts.len(),
        1,
        "the row must stay for the tree's shape"
    );
    assert_eq!(page.posts[0].state, PostState::Deleted);
    assert_eq!(page.thread.post_count, 1, "post_count counts tombstones");
    // Deleted is final for the author.
    assert!(matches!(
        edit::edit(
            &store,
            &NoQueue,
            &pid,
            &user(OLD_USER),
            "undo",
            SanitizedHtml::assert_sanitized("<p>undo</p>".into()),
            NOW
        )
        .await
        .unwrap(),
        EditOutcome::Rejected(EditRejected::NotEditable)
    ));

    // A moderator deleting someone else's post is in the public log; an author's own is not.
    let (_, other) = posted(&store, 35).await;
    assert!(matches!(
        edit::delete(&store, &other, &user(MOD_USER), NOW)
            .await
            .unwrap(),
        DeleteOutcome::Deleted
    ));
    let log = store.public_log(50).await.unwrap();
    let deletions: Vec<_> = log.iter().filter(|e| e.action == "delete").collect();
    assert_eq!(
        deletions.len(),
        1,
        "expected exactly the moderator's deletion in public: {deletions:?}"
    );
    assert_eq!(deletions[0].actor_name, "mod");
}
