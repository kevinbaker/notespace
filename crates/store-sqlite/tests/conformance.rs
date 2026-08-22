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
use notespace_core::password::{self, Params, Pepper, PepperSet};
use notespace_core::ratelimit::Limit;
use notespace_core::session::{SessionPolicy, SessionToken, TOKEN_BYTES};
use notespace_core::store::Store;

const NOW: i64 = 1_800_000_000_000;
const PW: &str = "correct horse battery staple";
/// Cheap on purpose: these tests exercise the flow, not the work factor.
const FAST: Params = Params {
    m_kib: 64,
    t: 1,
    p: 1,
};

fn config() -> LoginConfig {
    let peppers = PepperSet::single(0, Pepper::new(&[9u8; 32]).unwrap());
    LoginConfig {
        params: FAST,
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
        pw.map(|p| password::hash(p, "c29tZXNhbHR2YWx1ZTE", cfg.params, &cfg.peppers).unwrap());
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
