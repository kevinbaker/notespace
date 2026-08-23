//! The shared conformance suite, run against the native adapter.
//!
//! The same [`notespace_core::conformance::run_all`] runs against D1 through the Worker's
//! `/__conformance` route. If these two ever disagree, one adapter is wrong — which is the
//! entire point of having a second one.

use notespace_core::conformance::{run_all, Fixture};
use notespace_core::id::PublicId;
use notespace_core::path::Path;
use notespace_store_sqlite::SqliteStore;

/// `include_str!` needs literal paths, so this list is maintained by hand — and a migration
/// added without touching it fails as "no such table" somewhere unrelated.
/// `migration_list_is_complete` below turns that into a clear failure instead.
const MIGRATIONS: [&str; 7] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_thread_public_id.sql"),
    include_str!("../../../migrations/0003_space_paths_and_names.sql"),
    include_str!("../../../migrations/0004_post_public_id.sql"),
    include_str!("../../../migrations/0005_session.sql"),
    include_str!("../../../migrations/0006_login_attempt.sql"),
    include_str!("../../../migrations/0007_user_password.sql"),
];

const MIGRATIONS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");

/// Deterministic ids matching what `notespace-seed` produces, so the fixture below describes
/// the same data the D1 side is seeded with.
fn thread_id() -> PublicId {
    PublicId::new(1_735_689_600_000, 0xC0FFEE).unwrap()
}
fn post_id(i: u64) -> PublicId {
    PublicId::new(1_735_689_600_000 + i, 0x5EED_0000 ^ i as u32).unwrap()
}

const POSTS: u32 = 12;

fn seeded() -> SqliteStore {
    let store = SqliteStore::in_memory(&MIGRATIONS).expect("migrations apply");
    let c = store.conn();
    c.execute_batch(
        "INSERT INTO user (id, name, created_at) VALUES (1, 'alice', 1735689600000);
         INSERT INTO space (id, name, ranking, depth_cap, path)
           VALUES (1, 'General', 'bump', 8, 'general/');",
    )
    .unwrap();
    c.execute(
        "INSERT INTO thread (id, public_id, space_id, kind, title, author_id, created_at,
             bumped_at, post_count, state, cache_version)
         VALUES (1, ?1, 1, 'discussion', 'Conformance', 1, 1735689600000, 1735689600000, ?2,
                 'visible', 0)",
        rusqlite::params![thread_id().as_str(), POSTS],
    )
    .unwrap();
    // A shape with real nesting, so tree order is actually exercised rather than assumed.
    let paths = [
        "0001",
        "0001.0001",
        "0001.0001.0001",
        "0001.0002",
        "0002",
        "0002.0001",
        "0003",
        "0004",
        "0004.0001",
        "0004.0002",
        "0005",
        "0006",
    ];
    assert_eq!(paths.len() as u32, POSTS);
    for (i, path) in paths.iter().enumerate() {
        let i = i as u64;
        c.execute(
            "INSERT INTO post (id, public_id, thread_id, parent_id, path, depth, author_id,
                 body_md, body_html, created_at, score, state)
             VALUES (?1, ?2, 1, NULL, ?3, ?4, 1, ?5, ?6, ?7, 0, 'visible')",
            rusqlite::params![
                i as i64 + 1,
                post_id(i).as_str(),
                path,
                path.matches('.').count() as i64,
                format!("markdown {i}"),
                format!("<p>post {i}</p>"),
                1_735_689_600_000i64 + i as i64,
            ],
        )
        .unwrap();
    }
    store
}

fn fixture() -> Fixture {
    Fixture {
        thread: thread_id(),
        post_count: POSTS,
        // The 4th post, which is nested -- a leaf at depth 0 would not catch a path bug.
        known_post: post_id(3),
        known_post_path: Path::parse("0001.0002").unwrap(),
        absent: PublicId::new(1_735_689_600_000, 0xDEAD).unwrap(),
        writable: (0..4)
            .map(|i| PublicId::new(1_800_000_000_000 + i, 0x0A11_C0DE ^ i as u32).unwrap())
            .collect(),
        author_id: 1,
    }
}

#[tokio::test]
async fn sqlite_adapter_passes_the_shared_suite() {
    let store = seeded();
    let checks = run_all(&store, &fixture()).await;

    assert!(!checks.is_empty(), "suite ran no checks");
    let failures: Vec<_> = checks.iter().filter(|c| !c.passed()).collect();
    for c in &checks {
        println!("  {} {}", if c.passed() { "ok  " } else { "FAIL" }, c.name);
    }
    assert!(
        failures.is_empty(),
        "{} of {} checks failed:\n{}",
        failures.len(),
        checks.len(),
        failures
            .iter()
            .map(|c| format!("  - {}: {}", c.name, c.failure.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every migration on disk must be in [`MIGRATIONS`]. Without this, adding one and forgetting
/// to list it here makes the suite test an older schema than the Worker runs.
#[test]
fn migration_list_is_complete() {
    let mut on_disk: Vec<String> = std::fs::read_dir(MIGRATIONS_DIR)
        .expect("migrations directory")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    on_disk.sort();
    assert_eq!(
        on_disk.len(),
        MIGRATIONS.len(),
        "{} migrations on disk ({on_disk:?}) but {} listed in MIGRATIONS",
        on_disk.len(),
        MIGRATIONS.len()
    );
}

/// The migrations must apply to a plain SQLite as cleanly as they do to D1. This is the cheap
/// half of "identical dialect on both targets" -- a Postgres-ism would fail here in
/// milliseconds instead of at deploy time.
#[test]
fn migrations_apply_to_plain_sqlite() {
    let store = SqliteStore::in_memory(&MIGRATIONS).expect("migrations apply");
    let tables: Vec<String> = store
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for expected in ["post", "space", "thread", "user"] {
        assert!(tables.contains(&expected.to_string()), "missing {expected}");
    }
}

// ---------------------------------------------------------------------------
// The login flow
// ---------------------------------------------------------------------------

use notespace_core::login::{attempt, Attempt, LoginConfig, Outcome};
use notespace_core::password::{self, Params, Pepper, PepperSet, Scheme};
use notespace_core::ratelimit::Limit;
use notespace_core::session::{SessionPolicy, SessionToken, TOKEN_BYTES};
use notespace_core::store::Store;

const NOW: i64 = 1_800_000_000_000;
const PW: &str = "correct horse battery staple";
/// Cheap on purpose: these tests exercise the flow, not the work factor.
const FAST: Scheme = Scheme::Server(Params {
    m_kib: 64,
    t: 1,
    p: 1,
});

fn config() -> LoginConfig {
    let peppers = PepperSet::single(0, Pepper::new(&[9u8; 32]).unwrap());
    LoginConfig {
        scheme: FAST,
        dummy_hash: LoginConfig::dummy_hash_for(FAST, &peppers),
        peppers,
        sessions: SessionPolicy::default(),
        per_identity: Limit {
            max: 3,
            window_ms: 60_000,
        },
        per_client: Limit {
            max: 10,
            window_ms: 60_000,
        },
    }
}

async fn with_account(store: &SqliteStore, cfg: &LoginConfig, name: &str, pw: Option<&str>) {
    let hash =
        pw.map(|p| password::hash(p, "c29tZXNhbHR2YWx1ZTE", cfg.scheme, &cfg.peppers).unwrap());
    store
        .create_user(name, NOW, hash.as_deref())
        .await
        .expect("create user");
}

fn try_login<'a>(name: &'a str, pw: &'a str, client: &'a str, seed: u8) -> Attempt<'a> {
    Attempt {
        username: name,
        password: pw,
        client,
        token: SessionToken::from_bytes([seed; TOKEN_BYTES]),
        now: NOW,
    }
}

#[tokio::test]
async fn a_correct_password_creates_a_session() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "alice2", Some(PW)).await;

    let out = attempt(&store, &cfg, try_login("alice2", PW, "203.0.113.1", 1))
        .await
        .unwrap();
    let Outcome::Success { session, user } = out else {
        panic!("correct password did not log in");
    };
    assert_eq!(user.name, "alice2");
    assert_eq!(session.expires_at, cfg.sessions.expiry_from(NOW));

    // The session is real: it resolves.
    let found = store
        .lookup_session(&session.token_hash, NOW + 1000)
        .await
        .unwrap()
        .expect("session resolves");
    assert_eq!(found.user.id, user.id);
}

#[tokio::test]
async fn the_username_is_matched_case_insensitively() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "casey", Some(PW)).await;
    let out = attempt(&store, &cfg, try_login("CaSeY", PW, "203.0.113.2", 2))
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Success { .. }), "case folding lost");
}

#[tokio::test]
async fn a_wrong_password_is_rejected_and_counted() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "bob", Some(PW)).await;

    let out = attempt(
        &store,
        &cfg,
        try_login("bob", "wrong password!", "203.0.113.3", 3),
    )
    .await
    .unwrap();
    assert!(matches!(out, Outcome::Rejected));

    let keys = notespace_core::ratelimit::AttemptKeys::new("bob", "203.0.113.3");
    let (identity, client) = store.login_attempts(&keys).await.unwrap();
    assert_eq!(identity.map(|a| a.count), Some(1), "identity not counted");
    assert_eq!(client.map(|a| a.count), Some(1), "client not counted");
}

/// The property the whole shape of `attempt` exists for.
#[tokio::test]
async fn an_unknown_account_is_indistinguishable_from_a_wrong_password() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "real", Some(PW)).await;
    // An account with no local password -- an OIDC user -- is the third case that must match.
    with_account(&store, &cfg, "external", None).await;

    for (name, label) in [
        ("real", "wrong password"),
        ("ghost", "no such account"),
        ("external", "account with no password"),
    ] {
        let out = attempt(
            &store,
            &cfg,
            try_login(name, "some wrong guess", "203.0.113.4", 4),
        )
        .await
        .unwrap();
        assert!(
            matches!(out, Outcome::Rejected),
            "{label} produced something other than a plain rejection"
        );
    }
}

#[tokio::test]
async fn a_banned_account_cannot_log_in_with_the_right_password() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "banned", Some(PW)).await;
    store
        .conn()
        .execute("UPDATE user SET state='banned' WHERE name='banned'", [])
        .unwrap();

    let out = attempt(&store, &cfg, try_login("banned", PW, "203.0.113.5", 5))
        .await
        .unwrap();
    assert!(
        matches!(out, Outcome::Rejected),
        "a banned account logged in"
    );
}

#[tokio::test]
async fn the_limiter_bites_before_the_password_is_checked() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "target", Some(PW)).await;

    for i in 1..=cfg.per_identity.max {
        let out = attempt(&store, &cfg, try_login("target", "guess", "203.0.113.6", 6))
            .await
            .unwrap();
        assert!(
            matches!(out, Outcome::Rejected),
            "attempt {i} not merely rejected"
        );
    }
    // Now even the CORRECT password is refused -- which is the point.
    let out = attempt(&store, &cfg, try_login("target", PW, "203.0.113.6", 7))
        .await
        .unwrap();
    let Outcome::RateLimited { retry_after_secs } = out else {
        panic!("the limiter did not bite");
    };
    assert!(retry_after_secs > 0 && retry_after_secs <= 60);
}

#[tokio::test]
async fn a_successful_login_clears_the_counters() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "carol", Some(PW)).await;

    attempt(&store, &cfg, try_login("carol", "wrong!", "203.0.113.7", 8))
        .await
        .unwrap();
    let out = attempt(&store, &cfg, try_login("carol", PW, "203.0.113.7", 9))
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Success { .. }));

    let keys = notespace_core::ratelimit::AttemptKeys::new("carol", "203.0.113.7");
    let (identity, client) = store.login_attempts(&keys).await.unwrap();
    assert_eq!(identity, None, "identity counter survived a success");
    assert_eq!(client, None, "client counter survived a success");
}

/// Logging in under weaker stored parameters must upgrade the hash in place.
#[tokio::test]
async fn login_rehashes_a_stale_credential() {
    let store = seeded();
    let mut cfg = config();
    // Stored under an older pepper than the one now current.
    let old = PepperSet::single(0, Pepper::new(&[1u8; 32]).unwrap());
    let stale = password::hash(PW, "c29tZXNhbHR2YWx1ZTE", FAST, &old).unwrap();
    store.create_user("dave", NOW, Some(&stale)).await.unwrap();

    // Current config holds both peppers, with a different one current.
    let mut set = PepperSet::none();
    set.insert(0, Pepper::new(&[1u8; 32]).unwrap(), false);
    set.insert(1, Pepper::new(&[2u8; 32]).unwrap(), true);
    cfg.dummy_hash = LoginConfig::dummy_hash_for(FAST, &set);
    cfg.peppers = set;

    let out = attempt(&store, &cfg, try_login("dave", PW, "203.0.113.8", 10))
        .await
        .unwrap();
    assert!(
        matches!(out, Outcome::Success { .. }),
        "stale hash did not log in"
    );

    let after: String = store
        .conn()
        .query_row(
            "SELECT password_hash FROM user WHERE name='dave'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(after, stale, "the stale hash was not rewritten");
    assert!(
        after.starts_with("1$"),
        "not rewritten under the current pepper: {after}"
    );
}

// ---------------------------------------------------------------------------
// The write path, against a real store.
// ---------------------------------------------------------------------------

use notespace_core::model::SanitizedHtml;
use notespace_core::reply::{self, Outcome as ReplyOutcome, Rejected, Reply, ReplyConfig};

fn reply_config(per_author: u32) -> ReplyConfig {
    ReplyConfig {
        per_author: Limit {
            max: per_author,
            window_ms: 60_000,
        },
        per_client: Limit {
            max: 100,
            window_ms: 60_000,
        },
    }
}

/// Fresh ids per attempt, as `reply::post` requires.
fn reply_ids(seed: u64) -> Vec<PublicId> {
    (0..=reply::MAX_PATH_RETRIES as u64)
        .map(|i| PublicId::new(1_900_000_000_000 + seed * 16 + i, 0xBEEF ^ i as u32).unwrap())
        .collect()
}

fn a_reply<'a>(body: &'a str, ids: &'a [PublicId], client: &'a str) -> Reply<'a> {
    Reply {
        thread: thread_id(),
        parent: None,
        author: 1,
        body_md: body,
        body_html: SanitizedHtml::assert_sanitized("<p>body</p>".into()),
        client,
        ids,
        now: NOW,
    }
}

#[tokio::test]
async fn a_reply_is_posted_and_lands_at_the_end_of_the_thread() {
    let store = seeded();
    let ids = reply_ids(1);
    let before = store.thread_version(&thread_id()).await.unwrap().unwrap();

    match reply::post(
        &store,
        &reply_config(10),
        a_reply("hello there", &ids, "1.2.3.4"),
    )
    .await
    {
        Ok(ReplyOutcome::Posted(p)) => {
            assert_eq!(p.public_id, ids[0], "stored under the id it was given");
            assert_eq!(p.depth, 0, "no parent means top level");
        }
        _ => panic!("expected a post"),
    }

    // The write must bump the version, or the cached page keeps serving without the reply.
    let after = store.thread_version(&thread_id()).await.unwrap().unwrap();
    assert!(
        after > before,
        "cache_version not bumped: {before} -> {after}"
    );
}

#[tokio::test]
async fn an_oversized_body_never_reaches_the_database() {
    let store = seeded();
    let ids = reply_ids(2);
    let huge = "a".repeat(reply::MAX_BODY_CHARS + 1);
    let before = store.thread_version(&thread_id()).await.unwrap().unwrap();

    match reply::post(&store, &reply_config(10), a_reply(&huge, &ids, "1.2.3.4")).await {
        Ok(ReplyOutcome::Rejected(Rejected::TooLong { .. })) => {}
        _ => panic!("expected TooLong"),
    }
    assert_eq!(
        store.thread_version(&thread_id()).await.unwrap().unwrap(),
        before,
        "a rejected body still wrote to the thread"
    );
}

/// The limiter has to bite before the insert, not after: the write is the expensive part and
/// the whole point is that a flood does not reach it.
#[tokio::test]
async fn the_limiter_bites_before_the_write() {
    let store = seeded();
    let cfg = reply_config(2);
    for i in 0..2u64 {
        let ids = reply_ids(10 + i);
        match reply::post(&store, &cfg, a_reply("a real reply", &ids, "9.9.9.9")).await {
            Ok(ReplyOutcome::Posted(_)) => {}
            _ => panic!("attempt {i} should have posted"),
        }
    }
    let before = store.thread_version(&thread_id()).await.unwrap().unwrap();
    let ids = reply_ids(99);
    match reply::post(&store, &cfg, a_reply("one too many", &ids, "9.9.9.9")).await {
        Ok(ReplyOutcome::RateLimited { retry_after_secs }) => {
            assert!(retry_after_secs > 0, "no retry hint");
        }
        _ => panic!("expected RateLimited"),
    }
    assert_eq!(
        store.thread_version(&thread_id()).await.unwrap().unwrap(),
        before,
        "a rate-limited attempt still wrote"
    );
}

#[tokio::test]
async fn a_reply_to_an_unknown_thread_is_rejected_not_an_error() {
    let store = seeded();
    let ids = reply_ids(3);
    let mut r = a_reply("hello there", &ids, "1.2.3.4");
    r.thread = PublicId::new(1_735_689_600_000, 0xDEAD).unwrap();
    match reply::post(&store, &reply_config(10), r).await {
        Ok(ReplyOutcome::Rejected(Rejected::NotFound)) => {}
        _ => panic!("expected NotFound"),
    }
}

/// Every id offered must be distinct, or a retry re-submits the id that just lost and cannot
/// possibly win. Cheap to assert, and the failure would only show under contention.
#[tokio::test]
async fn the_retry_ids_are_distinct() {
    let ids = reply_ids(7);
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        assert!(seen.insert(id.encode()), "duplicate retry id {id}");
    }
    assert!(ids.len() > reply::MAX_PATH_RETRIES as usize, "too few ids");
}

// ---------------------------------------------------------------------------
// Fragments, on a thread wide enough to have more than one
// ---------------------------------------------------------------------------

use notespace_core::fragment::{Fragment, FRAGMENT_SPAN};

/// A thread with `roots` top-level subtrees, each carrying a couple of descendants.
///
/// The shared fixture is only six subtrees wide, so every post lands in fragment 0 and the
/// partition check there passes without ever crossing a boundary. This one crosses several.
fn wide(roots: u32) -> SqliteStore {
    let store = SqliteStore::in_memory(&MIGRATIONS).expect("migrations apply");
    let c = store.conn();
    c.execute_batch(
        "INSERT INTO user (id, name, created_at) VALUES (1, 'alice', 1735689600000);
         INSERT INTO space (id, name, ranking, depth_cap, path)
           VALUES (1, 'General', 'bump', 8, 'general/');",
    )
    .unwrap();
    c.execute(
        "INSERT INTO thread (id, public_id, space_id, kind, title, author_id, created_at,
             bumped_at, post_count, state, cache_version)
         VALUES (1, ?1, 1, 'discussion', 'Wide', 1, 1735689600000, 1735689600000, 0,
                 'visible', 0)",
        rusqlite::params![thread_id().as_str()],
    )
    .unwrap();
    let mut id = 0i64;
    let insert = |path: &str, depth: i64, id: &mut i64| {
        *id += 1;
        c.execute(
            "INSERT INTO post (id, public_id, thread_id, parent_id, path, depth, author_id,
                 body_md, body_html, created_at, score, state)
             VALUES (?1, ?2, 1, NULL, ?3, ?4, 1, 'md', '<p>x</p>', ?5, 0, 'visible')",
            rusqlite::params![
                *id,
                post_id(*id as u64 + 500).as_str(),
                path,
                depth,
                1_735_689_600_000i64 + *id
            ],
        )
        .unwrap();
    };
    for r in 0..roots {
        let root = notespace_core::path::Path::root(r).unwrap();
        insert(root.as_str(), 0, &mut id);
        let kid = root.child(1).unwrap();
        insert(kid.as_str(), 1, &mut id);
        insert(kid.child(1).unwrap().as_str(), 2, &mut id);
    }
    store
}

#[tokio::test]
async fn fragments_tile_a_wide_thread_exactly() {
    let roots = FRAGMENT_SPAN * 3 + 2; // spans four fragments, the last one partial
    let store = wide(roots);
    let total = (roots * 3) as usize;

    let whole = store
        .thread_page(
            &thread_id(),
            &notespace_core::store::Page::first(total as u32 + 1),
        )
        .await
        .expect("paged read");
    assert_eq!(whole.posts.len(), total, "fixture did not build");

    let mut seen: Vec<String> = Vec::new();
    let mut non_empty = 0;
    for i in 0..8u32 {
        let posts = store
            .thread_fragment(&thread_id(), Fragment::new(i), total as u32 + 1)
            .await
            .expect("fragment read");
        if !posts.is_empty() {
            non_empty += 1;
        }
        for p in &posts {
            assert_eq!(
                Fragment::containing(&p.path),
                Fragment::new(i),
                "{} came back from fragment {i}",
                p.path
            );
        }
        seen.extend(posts.iter().map(|p| p.path.as_str().to_string()));
    }

    assert_eq!(non_empty, 4, "expected four populated fragments");
    let expected: Vec<String> = whole
        .posts
        .iter()
        .map(|p| p.path.as_str().to_string())
        .collect();
    assert_eq!(seen, expected, "fragments do not tile the thread in order");
}

/// The boundary case that a fixed-span scheme gets wrong if the range is built naively: the
/// last root of a fragment and the first of the next must not bleed into each other.
#[tokio::test]
async fn a_fragment_boundary_separates_adjacent_subtrees() {
    let store = wide(FRAGMENT_SPAN * 2);
    let first = store
        .thread_fragment(&thread_id(), Fragment::new(0), 1000)
        .await
        .unwrap();
    let second = store
        .thread_fragment(&thread_id(), Fragment::new(1), 1000)
        .await
        .unwrap();

    let last_of_first = first.last().expect("fragment 0 is populated");
    let first_of_second = second.first().expect("fragment 1 is populated");
    assert!(
        last_of_first.path.as_str() < first_of_second.path.as_str(),
        "fragment 0 ends at {} but fragment 1 starts at {}",
        last_of_first.path,
        first_of_second.path
    );
    // Every subtree is 3 posts; the span is in top-level subtrees, not posts.
    assert_eq!(first.len(), (FRAGMENT_SPAN * 3) as usize);
    assert_eq!(second.len(), (FRAGMENT_SPAN * 3) as usize);
}

/// Asking past the end is how a caller learns there is nothing more.
#[tokio::test]
async fn a_fragment_past_the_end_is_empty_not_an_error() {
    let store = wide(2);
    let posts = store
        .thread_fragment(&thread_id(), Fragment::new(9), 1000)
        .await
        .expect("past-the-end fragment should not error");
    assert!(posts.is_empty());
}
