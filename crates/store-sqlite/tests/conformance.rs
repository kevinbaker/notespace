//! The shared conformance suite, run against the native adapter. The same `run_all` runs against
//! D1 through the Worker's `/__conformance` route; a disagreement means one adapter is wrong.

use notespace_core::conformance::{run_all, Fixture};
use notespace_core::id::PublicId;
use notespace_core::path::Path;
use notespace_store_sqlite::SqliteStore;

/// `include_str!` needs literal paths; `migration_list_is_complete` guards the hand maintenance.
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

/// Matching what `notespace-seed` produces, so both sides describe the same data.
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
    // Real nesting, so tree order is exercised rather than assumed.
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
        // Nested: a leaf at depth 0 would not catch a path bug.
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

/// The cheap half of dialect parity: a Postgres-ism fails here in milliseconds, not at deploy.
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
/// Cheap on purpose: these exercise the flow, not the work factor.
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

#[tokio::test]
async fn an_unknown_account_is_indistinguishable_from_a_wrong_password() {
    let store = seeded();
    let cfg = config();
    with_account(&store, &cfg, "real", Some(PW)).await;
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
    // Now even the correct password is refused.
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

    // Without the version bump the cached page keeps serving without the reply.
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
// Registration
// ---------------------------------------------------------------------------

use notespace_core::register::{
    self, Outcome as SignupOutcome, RegisterConfig, Rejected as SignupRejected, Signup,
};

fn register_config(per_client: u32) -> RegisterConfig {
    let peppers = PepperSet::single(0, Pepper::new(&[9u8; 32]).unwrap());
    RegisterConfig {
        scheme: FAST,
        peppers,
        sessions: SessionPolicy::default(),
        per_client: Limit {
            max: per_client,
            window_ms: 60_000,
        },
    }
}

fn a_signup<'a>(name: &'a str, pw: &'a str, client: &'a str, seed: u8) -> Signup<'a> {
    Signup {
        username: name,
        password: pw,
        client,
        token: SessionToken::from_bytes([seed; TOKEN_BYTES]),
        salt: "c29tZXNhbHR2YWx1ZTE",
        now: NOW,
    }
}

#[tokio::test]
async fn a_signup_creates_an_account_that_can_then_log_in() {
    let store = seeded();
    let rcfg = register_config(10);
    match register::signup(&store, &rcfg, a_signup("newcomer", PW, "1.2.3.4", 1)).await {
        Ok(SignupOutcome::Created { user, session }) => {
            assert_eq!(user.name, "newcomer");
            let found = store
                .lookup_session(&session.token_hash, NOW)
                .await
                .expect("lookup");
            assert!(found.is_some(), "no session was stored");
        }
        _ => panic!("expected Created"),
    }

    let lcfg = config();
    match attempt(&store, &lcfg, try_login("newcomer", PW, "1.2.3.4", 2)).await {
        Ok(Outcome::Success { user, .. }) => assert_eq!(user.name, "newcomer"),
        _ => panic!("the account it created cannot log in"),
    }
}

#[tokio::test]
async fn the_name_is_stored_lowercase_and_matched_either_way() {
    let store = seeded();
    let rcfg = register_config(10);
    match register::signup(&store, &rcfg, a_signup("MixedCase", PW, "1.2.3.4", 3)).await {
        Ok(SignupOutcome::Created { user, .. }) => assert_eq!(user.name, "mixedcase"),
        _ => panic!("expected Created"),
    }
    // And the same name in any casing is now taken.
    match register::signup(&store, &rcfg, a_signup("MIXEDCASE", PW, "1.2.3.4", 4)).await {
        Ok(SignupOutcome::Rejected(SignupRejected::Taken)) => {}
        _ => panic!("case-different duplicate was accepted"),
    }
}

#[tokio::test]
async fn a_taken_name_is_rejected_without_disturbing_the_existing_account() {
    let store = seeded();
    let rcfg = register_config(10);
    let lcfg = config();
    with_account(&store, &lcfg, "incumbent", Some(PW)).await;

    match register::signup(
        &store,
        &rcfg,
        a_signup("incumbent", "a different one", "9.9.9.9", 5),
    )
    .await
    {
        Ok(SignupOutcome::Rejected(SignupRejected::Taken)) => {}
        _ => panic!("expected Taken"),
    }
    match attempt(&store, &lcfg, try_login("incumbent", PW, "1.1.1.1", 6)).await {
        Ok(Outcome::Success { .. }) => {}
        _ => panic!("the original credential was overwritten"),
    }
}

#[tokio::test]
async fn the_limiter_counts_signups_per_client_whatever_name_is_tried() {
    let store = seeded();
    let rcfg = register_config(2);
    for (i, name) in ["alpha-one", "beta-two"].iter().enumerate() {
        match register::signup(&store, &rcfg, a_signup(name, PW, "7.7.7.7", 10 + i as u8)).await {
            Ok(SignupOutcome::Created { .. }) => {}
            _ => panic!("signup {name} should have succeeded"),
        }
    }
    match register::signup(&store, &rcfg, a_signup("gamma-three", PW, "7.7.7.7", 20)).await {
        Ok(SignupOutcome::RateLimited { retry_after_secs }) => assert!(retry_after_secs > 0),
        _ => panic!("expected RateLimited"),
    }
    // A different client is unaffected.
    match register::signup(&store, &rcfg, a_signup("delta-four", PW, "8.8.8.8", 21)).await {
        Ok(SignupOutcome::Created { .. }) => {}
        _ => panic!("a different client should not be limited"),
    }
}

#[tokio::test]
async fn a_rejected_signup_writes_no_account() {
    let store = seeded();
    let rcfg = register_config(10);
    for (name, pw) in [("ok-name", "short"), ("bad name", PW), ("admin", PW)] {
        match register::signup(&store, &rcfg, a_signup(name, pw, "1.2.3.4", 30)).await {
            Ok(SignupOutcome::Rejected(_)) => {}
            _ => panic!("{name:?}/{pw:?} should have been rejected"),
        }
    }
    for name in ["ok-name", "bad name", "admin"] {
        assert!(
            store.user_by_name(name).await.unwrap().is_none(),
            "{name} was created despite rejection"
        );
    }
}

/// A thread with `roots` top-level subtrees, three posts each.
fn wide_thread(roots: u32) -> SqliteStore {
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
    for r in 0..roots {
        let root = Path::root(r).unwrap();
        for path in [
            root.as_str().to_string(),
            root.child(1).unwrap().as_str().to_string(),
            root.child(1)
                .unwrap()
                .child(1)
                .unwrap()
                .as_str()
                .to_string(),
        ] {
            id += 1;
            c.execute(
                "INSERT INTO post (id, public_id, thread_id, parent_id, path, depth, author_id,
                     body_md, body_html, created_at, score, state)
                 VALUES (?1, ?2, 1, NULL, ?3, ?4, 1, 'md', '<p>x</p>', ?5, 0, 'visible')",
                rusqlite::params![
                    id,
                    post_id(id as u64 + 500).as_str(),
                    &path,
                    path.matches('.').count() as i64,
                    1_735_689_600_000i64 + id
                ],
            )
            .unwrap();
        }
    }
    store
}

// ---------------------------------------------------------------------------
// Query budgets
// ---------------------------------------------------------------------------

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static STATEMENTS: AtomicUsize = AtomicUsize::new(0);
static COUNTING: Mutex<()> = Mutex::new(());

fn count_statement(e: TraceEvent<'_>) {
    if let TraceEvent::Stmt(..) = e {
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
    }
}

/// `rusqlite`'s trace callback is a plain `fn`, so the counter is process-global and concurrent
/// budget tests would otherwise add to each other's totals.
struct Counting<'a> {
    store: &'a SqliteStore,
    _lock: MutexGuard<'static, ()>,
}

impl<'a> Counting<'a> {
    fn start(store: &'a SqliteStore) -> Self {
        let lock = COUNTING.lock().unwrap_or_else(|e| e.into_inner());
        STATEMENTS.store(0, Ordering::Relaxed);
        store
            .conn()
            .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_statement));
        Counting { store, _lock: lock }
    }

    fn stop(self) -> usize {
        self.store
            .conn()
            .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
        STATEMENTS.load(Ordering::Relaxed)
    }
}

#[tokio::test]
async fn the_read_path_stays_inside_its_query_budget() {
    use notespace_core::store::Page;
    let store = seeded();

    let c = Counting::start(&store);
    store
        .thread_page(&thread_id(), &Page::first(200))
        .await
        .expect("thread_page");
    let n = c.stop();
    assert_eq!(n, 2, "thread_page ran {n} statements, budget is 2");

    let c = Counting::start(&store);
    store.thread_version(&thread_id()).await.expect("version");
    let n = c.stop();
    assert_eq!(n, 1, "thread_version ran {n} statements, budget is 1");

    let c = Counting::start(&store);
    store.recent_threads(50).await.expect("recent_threads");
    let n = c.stop();
    assert_eq!(n, 1, "recent_threads ran {n} statements, budget is 1");
}

/// Growing the thread does not grow the query count.
#[tokio::test]
async fn the_read_path_does_not_go_per_post() {
    use notespace_core::store::Page;
    let small = wide_thread(2);
    let large = wide_thread(30);

    let c = Counting::start(&small);
    small
        .thread_page(&thread_id(), &Page::first(1000))
        .await
        .expect("small");
    let few = c.stop();

    let c = Counting::start(&large);
    large
        .thread_page(&thread_id(), &Page::first(1000))
        .await
        .expect("large");
    let many = c.stop();

    assert_eq!(
        few, many,
        "a 6-post thread cost {few} statements and a 90-post thread cost {many}"
    );
}

#[tokio::test]
async fn locate_post_stays_inside_its_budget() {
    let store = seeded();

    let c = Counting::start(&store);
    let loc = store.locate_post(&post_id(3), 200).await.expect("locate");
    let n = c.stop();
    assert!(loc.cursor.is_none(), "post 3 should be on the first page");
    assert_eq!(
        n, 2,
        "first-page locate_post ran {n} statements, budget is 2"
    );

    let c = Counting::start(&store);
    let loc = store.locate_post(&post_id(11), 3).await.expect("locate");
    let n = c.stop();
    assert!(
        loc.cursor.is_some(),
        "post 11 at page size 3 needs a cursor"
    );
    assert_eq!(n, 3, "paged locate_post ran {n} statements, budget is 3");
}

/// A mismatch here is a runtime "no D1 binding", never a build failure.
#[test]
fn the_d1_binding_name_matches_wrangler_toml() {
    let src = include_str!("../../worker/src/lib.rs");
    let toml = include_str!("../../../wrangler.toml");

    let in_code = src
        .lines()
        .find_map(|l| l.trim().strip_prefix("const DB_BINDING: &str = \""))
        .and_then(|l| l.split('"').next())
        .expect("DB_BINDING not found in the worker source");
    let in_toml = toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("binding = \""))
        .and_then(|l| l.split('"').next())
        .expect("no `binding` in wrangler.toml");

    assert_eq!(
        in_code, in_toml,
        "worker binds {in_code:?} but wrangler.toml declares {in_toml:?}"
    );
}
