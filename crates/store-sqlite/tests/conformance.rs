//! The shared conformance suite, run against the native adapter.
//!
//! The same [`notespace_core::conformance::run_all`] runs against D1 through the Worker's
//! `/__conformance` route. If these two ever disagree, one adapter is wrong — which is the
//! entire point of having a second one.

use notespace_core::conformance::{run_all, Fixture};
use notespace_core::id::PublicId;
use notespace_core::path::Path;
use notespace_store_sqlite::SqliteStore;

const MIGRATIONS: [&str; 4] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_thread_public_id.sql"),
    include_str!("../../../migrations/0003_space_paths_and_names.sql"),
    include_str!("../../../migrations/0004_post_public_id.sql"),
];

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
