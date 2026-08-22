//! One test suite, run against every [`Store`] implementation.
//!
//! DESIGN.md §9: "Two test suites: one shared conformance suite run against both `Store`
//! implementations, and pure unit tests for ranking/trust/path logic." This is the first.
//!
//! It lives in `core` rather than in a test directory because it has to be callable from two
//! very different places: a native `cargo test`, and a Worker running against real D1 where
//! `#[test]` does not exist. Both call [`run_all`].
//!
//! # What it is for
//!
//! Not to check that SQLite works. To check that two adapters agree about things the SQL does
//! not enforce: cursor semantics, tree ordering, what "not found" means, whether the space
//! comes back with the thread, whether a post's location survives being asked for. Those are
//! where adapters drift, and drift here is a silently wrong page rather than a failed build.
//!
//! # Fixture
//!
//! Callers seed the store themselves — this module cannot, because inserting is target-specific
//! and there is no write path yet. [`Fixture`] describes what the suite expects to find.

use crate::id::PublicId;
use crate::model::{NewPost, SanitizedHtml};
use crate::path::Path;
use crate::ratelimit::{AttemptKeys, Attempts, Limit};
use crate::session::{Session, SessionPolicy, SessionToken, TOKEN_BYTES};
use crate::store::{Page, Store, StoreError};

/// What the suite expects the store to already contain.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// A thread with at least [`Fixture::post_count`] posts.
    pub thread: PublicId,
    /// Exact number of posts in that thread.
    pub post_count: u32,
    /// The public id of a post in that thread, and the path it sits at.
    pub known_post: PublicId,
    pub known_post_path: Path,
    /// A well-formed id that is definitely absent.
    pub absent: PublicId,
    /// Ids for posts the write checks will create, and an author to attribute them to.
    ///
    /// Supplied rather than generated because `core` has no RNG. Provide at least 4; leave
    /// empty to skip the write checks entirely, which is what a read-only fixture does.
    pub writable: Vec<PublicId>,
    pub author_id: i64,
}

/// Outcome of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub failure: Option<String>,
}

impl Check {
    fn pass(name: &'static str) -> Self {
        Check {
            name,
            failure: None,
        }
    }
    fn fail(name: &'static str, why: impl Into<String>) -> Self {
        Check {
            name,
            failure: Some(why.into()),
        }
    }
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

macro_rules! require {
    ($name:expr, $cond:expr, $($why:tt)*) => {
        if !$cond {
            return Check::fail($name, format!($($why)*));
        }
    };
}

/// Run every check. Returns one [`Check`] per behaviour, in order.
///
/// Does not stop at the first failure: when two adapters disagree it is far more useful to see
/// the whole shape of the disagreement than the first symptom of it.
pub async fn run_all<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    vec![
        thread_page_returns_its_space(store, fx).await,
        posts_arrive_in_tree_order(store, fx).await,
        limit_is_respected_and_cursor_advances(store, fx).await,
        cursor_is_exclusive(store, fx).await,
        paging_visits_every_post_exactly_once(store, fx).await,
        cursor_is_none_on_the_last_page(store, fx).await,
        absent_thread_is_not_found(store, fx).await,
        locate_post_finds_its_thread(store, fx).await,
        absent_post_is_not_found(store, fx).await,
        posts_never_expose_body_md(store, fx).await,
    ]
    .into_iter()
    .chain(write_checks(store, fx).await)
    .collect()
}

/// Write checks, skipped when the fixture supplies no ids to write with.
///
/// These run last because they mutate the thread: everything above assumes a stable post count.
async fn write_checks<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    if fx.writable.len() < 4 {
        return vec![Check::pass("write checks skipped (read-only fixture)")];
    }
    vec![
        appended_post_is_readable(store, fx).await,
        replies_nest_under_their_parent(store, fx).await,
        siblings_get_consecutive_ordinals(store, fx).await,
        writing_the_same_id_twice_conflicts(store, fx).await,
        reply_to_absent_parent_is_not_found(store, fx).await,
        sessions_round_trip_and_carry_the_user(store, fx).await,
        expired_sessions_do_not_resolve(store, fx).await,
        refresh_extends_a_session(store, fx).await,
        logout_is_immediate_and_idempotent(store, fx).await,
        logout_everywhere_ends_every_session(store, fx).await,
        rate_limit_counters_round_trip(store, fx).await,
        a_successful_login_clears_its_bucket(store, fx).await,
        the_sweep_removes_only_old_windows(store, fx).await,
    ]
}

fn keys(fx: &Fixture, tag: &str) -> AttemptKeys {
    let _ = fx;
    AttemptKeys::new(&format!("user-{tag}"), &format!("198.51.100.{}", tag.len()))
}

async fn rate_limit_counters_round_trip<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "login attempt counters round trip, per bucket";
    let k = keys(fx, "roundtrip");
    match store.login_attempts(&k).await {
        Ok((None, None)) => {}
        Ok(_) => return Check::fail(NAME, "fresh keys already had counters"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    // Drive the identity bucket to its limit, exactly as a login handler would.
    let limit = Limit::PER_IDENTITY;
    let mut state = None;
    for i in 1..=limit.max {
        let d = limit.check(state, NOW);
        if !d.allowed() {
            return Check::fail(NAME, format!("denied at attempt {i} of {}", limit.max));
        }
        let next = d.next().expect("allow carries a counter");
        if let Err(e) = store.record_login_attempt(&k.identity, next).await {
            return Check::fail(NAME, format!("record: {e}"));
        }
        state = match store.login_attempts(&k).await {
            Ok((id, _)) => id,
            Err(e) => return Check::fail(NAME, format!("{e}")),
        };
        require!(
            NAME,
            state == Some(next),
            "stored {:?}, read back {:?}",
            next,
            state
        );
    }
    require!(
        NAME,
        !limit.check(state, NOW).allowed(),
        "the limit did not bite after {} attempts",
        limit.max
    );
    // The client bucket is independent: it saw nothing.
    match store.login_attempts(&k).await {
        Ok((_, None)) => Check::pass(NAME),
        Ok((_, Some(_))) => Check::fail(NAME, "identity attempts leaked into the client bucket"),
        Err(e) => Check::fail(NAME, format!("{e}")),
    }
}

async fn a_successful_login_clears_its_bucket<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "a successful login clears the counter";
    let k = keys(fx, "cleared");
    if let Err(e) = store
        .record_login_attempt(
            &k.identity,
            Attempts {
                window_start: NOW,
                count: 4,
            },
        )
        .await
    {
        return Check::fail(NAME, format!("record: {e}"));
    }
    if let Err(e) = store.clear_login_attempts(&k.identity).await {
        return Check::fail(NAME, format!("clear: {e}"));
    }
    match store.login_attempts(&k).await {
        Ok((None, _)) => {}
        Ok((Some(a), _)) => return Check::fail(NAME, format!("counter survived: {a:?}")),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    // Idempotent: clearing an absent bucket is success, not an error.
    match store.clear_login_attempts(&k.identity).await {
        Ok(()) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("second clear errored: {e}")),
    }
}

async fn the_sweep_removes_only_old_windows<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "the sweep removes old windows and spares current ones";
    let old = keys(fx, "sweep-old");
    let fresh = keys(fx, "sweep-fresh");
    let day = 24 * 60 * 60 * 1000;
    if let Err(e) = store
        .record_login_attempt(
            &old.identity,
            Attempts {
                window_start: NOW - day,
                count: 3,
            },
        )
        .await
    {
        return Check::fail(NAME, format!("{e}"));
    }
    if let Err(e) = store
        .record_login_attempt(
            &fresh.identity,
            Attempts {
                window_start: NOW,
                count: 3,
            },
        )
        .await
    {
        return Check::fail(NAME, format!("{e}"));
    }
    match store.sweep_login_attempts(NOW - day / 2).await {
        Ok(n) => require!(
            NAME,
            n >= 1,
            "swept {n} rows, expected at least the old one"
        ),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    match (
        store.login_attempts(&old).await,
        store.login_attempts(&fresh).await,
    ) {
        (Ok((None, _)), Ok((Some(_), _))) => Check::pass(NAME),
        (Ok((Some(_), _)), _) => Check::fail(NAME, "the old window survived the sweep"),
        (_, Ok((None, _))) => Check::fail(NAME, "the sweep took a current window with it"),
        (Err(e), _) | (_, Err(e)) => Check::fail(NAME, format!("{e}")),
    }
}

const NOW: i64 = 1_800_000_000_000;

fn session_for(fx: &Fixture, seed: u8, now: i64) -> (SessionToken, Session) {
    let policy = SessionPolicy::default();
    let token = SessionToken::from_bytes([seed; TOKEN_BYTES]);
    let session = Session {
        token_hash: token.hash(),
        user_id: fx.author_id,
        created_at: now,
        refreshed_at: now,
        expires_at: policy.expiry_from(now),
    };
    (token, session)
}

async fn sessions_round_trip_and_carry_the_user<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "a session resolves to its user";
    let (token, session) = session_for(fx, 0x11, NOW);
    if let Err(e) = store.create_session(&session).await {
        return Check::fail(NAME, format!("create: {e}"));
    }
    let found = match store.lookup_session(&token.hash(), NOW + 1000).await {
        Ok(Some(a)) => a,
        Ok(None) => return Check::fail(NAME, "a session just created did not resolve"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, found.user.id == fx.author_id, "wrong user");
    require!(NAME, !found.user.name.is_empty(), "user came back nameless");
    require!(
        NAME,
        found.session.expires_at == session.expires_at,
        "expiry not preserved"
    );
    // An unknown token must resolve to nothing, not to somebody.
    let stranger = SessionToken::from_bytes([0xEE; TOKEN_BYTES]);
    match store.lookup_session(&stranger.hash(), NOW).await {
        Ok(None) => {}
        Ok(Some(_)) => return Check::fail(NAME, "an unknown token resolved to a session"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    Check::pass(NAME)
}

/// Expiry is enforced by the query, not by a sweep that may not have run.
async fn expired_sessions_do_not_resolve<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "an expired session does not resolve, even if the row is still there";
    let (token, mut session) = session_for(fx, 0x22, NOW);
    session.expires_at = NOW + 1000;
    if let Err(e) = store.create_session(&session).await {
        return Check::fail(NAME, format!("create: {e}"));
    }
    match store.lookup_session(&token.hash(), NOW + 500).await {
        Ok(Some(_)) => {}
        Ok(None) => return Check::fail(NAME, "did not resolve while still live"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    // Exactly at expiry it is already gone, not still valid for one more millisecond.
    match store
        .lookup_session(&token.hash(), session.expires_at)
        .await
    {
        Ok(None) => Check::pass(NAME),
        Ok(Some(_)) => Check::fail(NAME, "resolved at its own expiry instant"),
        Err(e) => Check::fail(NAME, format!("{e}")),
    }
}

async fn refresh_extends_a_session<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "refresh extends expiry without changing identity";
    let policy = SessionPolicy::default();
    let (token, session) = session_for(fx, 0x33, NOW);
    if let Err(e) = store.create_session(&session).await {
        return Check::fail(NAME, format!("create: {e}"));
    }
    // Sliding expiry refreshes at most once a day; before that, nothing should be written.
    require!(
        NAME,
        !policy.should_refresh(&session, NOW + 60_000),
        "policy would refresh a minute in"
    );
    let later = NOW + policy.refresh_after_ms;
    require!(
        NAME,
        policy.should_refresh(&session, later),
        "policy would not refresh after a full interval"
    );

    let new_expiry = policy.expiry_from(later);
    if let Err(e) = store
        .refresh_session(&token.hash(), later, new_expiry)
        .await
    {
        return Check::fail(NAME, format!("refresh: {e}"));
    }
    match store.lookup_session(&token.hash(), later).await {
        Ok(Some(a)) => {
            require!(
                NAME,
                a.session.expires_at == new_expiry,
                "expiry {} not extended to {new_expiry}",
                a.session.expires_at
            );
            require!(
                NAME,
                a.session.created_at == session.created_at,
                "refresh rewrote created_at"
            );
            require!(NAME, a.user.id == fx.author_id, "refresh changed the user");
            Check::pass(NAME)
        }
        Ok(None) => Check::fail(NAME, "session vanished after refresh"),
        Err(e) => Check::fail(NAME, format!("{e}")),
    }
}

async fn logout_is_immediate_and_idempotent<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "logout takes effect immediately and repeats harmlessly";
    let (token, session) = session_for(fx, 0x44, NOW);
    if let Err(e) = store.create_session(&session).await {
        return Check::fail(NAME, format!("create: {e}"));
    }
    if let Err(e) = store.delete_session(&token.hash()).await {
        return Check::fail(NAME, format!("delete: {e}"));
    }
    match store.lookup_session(&token.hash(), NOW).await {
        Ok(None) => {}
        Ok(Some(_)) => return Check::fail(NAME, "session survived logout"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    // Deleting again is success: the caller wanted the token dead and it is.
    match store.delete_session(&token.hash()).await {
        Ok(()) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("second delete errored: {e}")),
    }
}

/// What a ban or a password change relies on.
async fn logout_everywhere_ends_every_session<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "logout everywhere ends every session for the user";
    let mut tokens = Vec::new();
    for seed in [0x55u8, 0x56, 0x57] {
        let (t, s) = session_for(fx, seed, NOW);
        if let Err(e) = store.create_session(&s).await {
            return Check::fail(NAME, format!("create: {e}"));
        }
        tokens.push(t);
    }
    let ended = match store.delete_user_sessions(fx.author_id).await {
        Ok(n) => n,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, ended >= 3, "reported only {ended} sessions ended");
    for t in &tokens {
        match store.lookup_session(&t.hash(), NOW).await {
            Ok(None) => {}
            Ok(Some(_)) => return Check::fail(NAME, "a session survived logout-everywhere"),
            Err(e) => return Check::fail(NAME, format!("{e}")),
        }
    }
    Check::pass(NAME)
}

fn draft(fx: &Fixture, id: &PublicId, parent: Option<&PublicId>, body: &str) -> NewPost {
    NewPost {
        public_id: id.clone(),
        thread: fx.thread.clone(),
        parent: parent.cloned(),
        author_id: fx.author_id,
        body_md: body.to_string(),
        body_html: SanitizedHtml::assert_sanitized(format!("<p>{body}</p>")),
        created_at: 1_800_000_000_000,
    }
}

async fn appended_post_is_readable<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "an appended post is readable and bumps the count";
    let before = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p.thread.post_count,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let post = match store
        .insert_post(&draft(fx, &fx.writable[0], None, "top level"))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("insert: {e}")),
    };
    let loc = match store.locate_post(&post.public_id).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("locate: {e}")),
    };
    require!(NAME, loc.thread == fx.thread, "landed in the wrong thread");
    require!(
        NAME,
        loc.path == post.path,
        "insert reported {} but the row is at {}",
        post.path.as_str(),
        loc.path.as_str()
    );
    let after = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p.thread.post_count,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        after == before + 1,
        "post_count went {before} -> {after}"
    );
    Check::pass(NAME)
}

async fn replies_nest_under_their_parent<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "a reply nests under its parent";
    let parent = match store
        .insert_post(&draft(fx, &fx.writable[1], None, "parent"))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("parent: {e}")),
    };
    let child = match store
        .insert_post(&draft(
            fx,
            &fx.writable[2],
            Some(&parent.public_id),
            "child",
        ))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("child: {e}")),
    };
    require!(
        NAME,
        child.path.is_descendant_of(&parent.path),
        "{} is not under {}",
        child.path.as_str(),
        parent.path.as_str()
    );
    require!(
        NAME,
        child.depth == parent.depth + 1,
        "depth {} under a parent at {}",
        child.depth,
        parent.depth
    );
    require!(
        NAME,
        child.parent_id == Some(parent.id),
        "parent_id not set to the parent row"
    );
    Check::pass(NAME)
}

async fn siblings_get_consecutive_ordinals<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "siblings get consecutive ordinals, not reused ones";
    // writable[1] was made a parent above; give it a second child.
    let parent = match store.locate_post(&fx.writable[1]).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let second = match store
        .insert_post(&draft(
            fx,
            &fx.writable[3],
            Some(&fx.writable[1]),
            "sibling",
        ))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        second.path.is_descendant_of(&parent.path),
        "second child escaped the parent"
    );
    // The first child took ordinal 0, so this must be 1 -- not 0 again.
    require!(
        NAME,
        second.path.ordinal() == 1,
        "expected ordinal 1, got {} ({})",
        second.path.ordinal(),
        second.path.as_str()
    );
    Check::pass(NAME)
}

/// The uniqueness that stops a retry from double-posting.
async fn writing_the_same_id_twice_conflicts<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "re-using a public id is rejected, not duplicated";
    // writable[0] was already written by the first check.
    match store
        .insert_post(&draft(fx, &fx.writable[0], None, "duplicate"))
        .await
    {
        Err(StoreError::Conflict) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "wrote a second post with an id already in use"),
    }
}

async fn reply_to_absent_parent_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "replying to a post that does not exist is NotFound";
    let mut d = draft(fx, &fx.absent, Some(&fx.absent), "orphan");
    d.public_id = fx.absent.clone();
    match store.insert_post(&d).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "accepted a reply to a nonexistent parent"),
    }
}

async fn thread_page_returns_its_space<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "thread_page returns the thread's space";
    let page = match store.thread_page(&fx.thread, &Page::first(10)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, page.thread.public_id == fx.thread, "wrong thread");
    require!(
        NAME,
        !page.space.path.is_empty(),
        "space came back with an empty path"
    );
    require!(
        NAME,
        page.space.id == page.thread.space_id,
        "space {} does not match thread.space_id {}",
        page.space.id,
        page.thread.space_id
    );
    Check::pass(NAME)
}

async fn posts_arrive_in_tree_order<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "posts arrive in tree preorder";
    let page = match store
        .thread_page(&fx.thread, &Page::first(fx.post_count))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let paths: Vec<&str> = page.posts.iter().map(|p| p.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    require!(
        NAME,
        paths == sorted,
        "not ordered by path; first divergence at {:?}",
        paths.iter().zip(&sorted).find(|(a, b)| a != b)
    );
    Check::pass(NAME)
}

async fn limit_is_respected_and_cursor_advances<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "limit is respected and a cursor is offered";
    let limit = 5.min(fx.post_count.saturating_sub(1)).max(1);
    let page = match store.thread_page(&fx.thread, &Page::first(limit)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        page.posts.len() as u32 == limit,
        "asked for {limit}, got {}",
        page.posts.len()
    );
    require!(
        NAME,
        page.next_cursor.is_some(),
        "no cursor despite more posts remaining"
    );
    Check::pass(NAME)
}

async fn cursor_is_exclusive<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "cursor is exclusive";
    let first = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let Some(cursor) = first.next_cursor.clone() else {
        return Check::fail(NAME, "no cursor after the first post");
    };
    let second = match store
        .thread_page(&fx.thread, &Page::after(cursor.clone(), 1))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, !second.posts.is_empty(), "second page was empty");
    require!(
        NAME,
        second.posts[0].path.as_str() != cursor.as_str(),
        "cursor {} was returned again; it must be exclusive",
        cursor.as_str()
    );
    Check::pass(NAME)
}

async fn paging_visits_every_post_exactly_once<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "paging visits every post exactly once";
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<Path> = None;
    // Bounded so a broken cursor loops finitely rather than forever.
    for _ in 0..(fx.post_count + 2) {
        let page = match cursor.clone() {
            None => store.thread_page(&fx.thread, &Page::first(7)).await,
            Some(c) => store.thread_page(&fx.thread, &Page::after(c, 7)).await,
        };
        let page = match page {
            Ok(p) => p,
            Err(e) => return Check::fail(NAME, format!("{e}")),
        };
        if page.posts.is_empty() {
            break;
        }
        seen.extend(page.posts.iter().map(|p| p.path.as_str().to_string()));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    require!(
        NAME,
        seen.len() as u32 == fx.post_count,
        "walked {} posts, expected {}",
        seen.len(),
        fx.post_count
    );
    let mut uniq = seen.clone();
    uniq.sort_unstable();
    uniq.dedup();
    require!(
        NAME,
        uniq.len() == seen.len(),
        "{} duplicate posts across pages",
        seen.len() - uniq.len()
    );
    Check::pass(NAME)
}

async fn cursor_is_none_on_the_last_page<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "no cursor on the last page";
    let page = match store
        .thread_page(&fx.thread, &Page::first(fx.post_count + 10))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        page.next_cursor.is_none(),
        "offered a cursor past the end of the thread"
    );
    Check::pass(NAME)
}

async fn absent_thread_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "absent thread is NotFound, not an empty page";
    match store.thread_page(&fx.absent, &Page::first(10)).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "returned a page for a thread that does not exist"),
    }
}

async fn locate_post_finds_its_thread<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "locate_post resolves to the right thread and path";
    let loc = match store.locate_post(&fx.known_post).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        loc.thread == fx.thread,
        "got thread {}, expected {}",
        loc.thread,
        fx.thread
    );
    require!(
        NAME,
        loc.path == fx.known_post_path,
        "got path {}, expected {}",
        loc.path.as_str(),
        fx.known_post_path.as_str()
    );
    Check::pass(NAME)
}

async fn absent_post_is_not_found<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "absent post is NotFound";
    match store.locate_post(&fx.absent).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "located a post that does not exist"),
    }
}

async fn posts_never_expose_body_md<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "read path does not load body_md";
    let page = match store.thread_page(&fx.thread, &Page::first(5)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    // Shipping the markdown alongside the html doubles what the database sends back for a page
    // that renders only the html. An adapter that selects it will pass every other check here.
    for post in &page.posts {
        require!(
            NAME,
            post.body_md.is_none(),
            "post {} carried body_md into the read path",
            post.public_id
        );
    }
    Check::pass(NAME)
}
