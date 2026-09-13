//! One test suite, run against every [`Store`] implementation, checking what the SQL does not
//! enforce: cursor semantics, tree ordering, what "not found" means. Drift there is a silently
//! wrong page rather than a failed build.
//!
//! It lives in `core` because it has to be callable from a native `cargo test` and from a Worker
//! against real D1, where `#[test]` does not exist. Callers seed the store; [`Fixture`] says what
//! the suite expects to find.

use crate::id::PublicId;

/// Page size the suite paginates by. Only has to be self-consistent.
const PAGE_SIZE: u32 = 200;
use crate::email::{
    EmailAddress, EmailToken, StoredToken, TokenKind, TOKEN_BYTES as EMAIL_TOKEN_BYTES,
};
use crate::model::{
    NewPost, NewSpace, NewThread, PostState, Ranking, Role, SanitizedHtml, ThreadEdit, ThreadKind,
    ThreadState, UserState,
};
use crate::moderation::classify::{Call, Verdict};
use crate::moderation::{
    ActorKind, NewAction, NewReview, NewSignal, Resolution, ReviewReason, SIGNAL_REPORT,
};
use crate::path::Path;
use crate::ratelimit::{AttemptKeys, Attempts, Limit};
use crate::session::{Session, SessionPolicy, SessionToken, TOKEN_BYTES};
use crate::space_key::SpacePath;
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
    /// At least [`Fixture::WRITABLE`], or empty to skip the write checks. Supplied because
    /// `core` has no RNG.
    pub writable: Vec<PublicId>,
    pub author_id: i64,
}

impl Fixture {
    /// Ids the write checks consume: four posts, a thread, and its first post.
    pub const WRITABLE: usize = 6;
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

/// Runs every check rather than stopping at the first failure, so a disagreement shows its shape.
pub async fn run_all<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    vec![
        thread_page_returns_its_space(store, fx).await,
        posts_arrive_in_tree_order(store, fx).await,
        limit_is_respected_and_cursor_advances(store, fx).await,
        cursor_is_exclusive(store, fx).await,
        paging_visits_every_post_exactly_once(store, fx).await,
        cursor_is_none_on_the_last_page(store, fx).await,
        absent_thread_is_not_found(store, fx).await,
        thread_version_is_readable_and_absent_for_unknown(store, fx).await,
        a_duplicate_username_is_a_conflict(store, fx).await,
        a_permalink_cursor_lands_on_a_page_holding_the_post(store, fx).await,
        locate_post_finds_its_thread(store, fx).await,
        absent_post_is_not_found(store, fx).await,
        posts_never_expose_body_md(store, fx).await,
    ]
    .into_iter()
    .chain(write_checks(store, fx).await)
    .collect()
}

/// Run last, because they mutate the thread; everything above assumes a stable post count.
async fn write_checks<S: Store>(store: &S, fx: &Fixture) -> Vec<Check> {
    if fx.writable.len() < Fixture::WRITABLE {
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
        // Moderation. These act on writable[0], written above, and leave it visible.
        write_context_describes_the_space_thread_and_author(store, fx).await,
        duplicate_detection_is_bounded_by_the_window(store, fx).await,
        a_state_change_bumps_the_thread_version(store, fx).await,
        the_public_log_omits_private_rows_and_resolves_targets(store, fx).await,
        a_review_opens_once_keeps_its_verdict_and_resolves_once(store, fx).await,
        reports_count_distinct_reporters(store, fx).await,
        the_sweep_lists_pending_posts_nobody_is_looking_at(store, fx).await,
        // Spaces, new threads, editing, profiles.
        spaces_resolve_by_path_and_list_their_threads(store, fx).await,
        a_new_thread_is_created_listed_and_writable(store, fx).await,
        editing_rewrites_the_body_and_bumps_the_version(store, fx).await,
        a_profile_lists_the_users_visible_posts_newest_first(store, fx).await,
        // Email.
        email_tokens_are_spent_exactly_once_and_only_as_their_kind(store, fx).await,
        a_verified_address_belongs_to_one_account(store, fx).await,
        // Administration.
        a_thread_edit_rewrites_the_row_and_bumps_the_version(store, fx).await,
        users_are_listable_and_their_role_and_state_settable(store, fx).await,
        spaces_are_creatable_once_and_editable(store, fx).await,
        stats_and_the_full_log_read_back(store, fx).await,
    ]
}

// ---------------------------------------------------------------------------
// Administration
// ---------------------------------------------------------------------------

async fn a_thread_edit_rewrites_the_row_and_bumps_the_version<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "update_thread rewrites title, url, state and space, and bumps the version";
    let thread = &fx.writable[4]; // created by the compose check
    let (space, before) = match store.thread_head(thread).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("thread_head: {e}")),
    };
    require!(
        NAME,
        before.public_id == *thread,
        "thread_head returned the wrong thread"
    );
    require!(
        NAME,
        space.id == before.space_id,
        "thread_head's space does not match"
    );
    let edit = ThreadEdit {
        title: "Conformance thread, retitled".into(),
        url: None,
        state: ThreadState::Locked,
        space_id: space.id,
        space_path: space.path.clone(),
    };
    if let Err(e) = store.update_thread(thread, &edit).await {
        return Check::fail(NAME, format!("update: {e}"));
    }
    let (_, after) = match store.thread_head(thread).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, after.title == edit.title, "title not rewritten");
    require!(NAME, after.url.is_none(), "url not cleared");
    require!(
        NAME,
        after.state == ThreadState::Locked,
        "state not changed"
    );
    require!(
        NAME,
        after.cache_version > before.cache_version,
        "cache_version went {} -> {}",
        before.cache_version,
        after.cache_version
    );
    match store.update_thread(&fx.absent, &edit).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        other => Check::fail(NAME, format!("absent thread: {other:?}")),
    }
}

async fn users_are_listable_and_their_role_and_state_settable<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "list_users, user_row, set_user_role and set_user_state agree";
    let name = "conformance-admin-subject";
    let id = match store.create_user(name, NOW, None).await {
        Ok(id) => id,
        Err(StoreError::Conflict) => match store.user_by_name(name).await {
            Ok(Some(c)) => c.user.id,
            other => return Check::fail(NAME, format!("lookup: {other:?}")),
        },
        Err(e) => return Check::fail(NAME, format!("create: {e}")),
    };
    // Back to a known starting point, for reruns.
    if let Err(e) = store.set_user_role(id, Role::Member).await {
        return Check::fail(NAME, format!("{e}"));
    }
    if let Err(e) = store.set_user_state(id, UserState::Active).await {
        return Check::fail(NAME, format!("{e}"));
    }
    let listed = match store.list_users("conformance-admin", 100).await {
        Ok(v) => v,
        Err(e) => return Check::fail(NAME, format!("list: {e}")),
    };
    let Some(row) = listed.iter().find(|r| r.user.id == id) else {
        return Check::fail(NAME, "the user is not listed by prefix");
    };
    require!(NAME, row.user.role == Role::Member, "role not read");
    require!(NAME, row.created_at == NOW, "created_at not read");
    let everyone = match store.list_users("", 1000).await {
        Ok(v) => v,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        everyone.iter().any(|r| r.user.id == id),
        "an empty prefix does not list everyone"
    );
    require!(
        NAME,
        everyone.windows(2).all(|w| w[0].user.id >= w[1].user.id),
        "not newest first"
    );
    if let Err(e) = store.set_user_role(id, Role::Moderator).await {
        return Check::fail(NAME, format!("role: {e}"));
    }
    if let Err(e) = store.set_user_state(id, UserState::Banned).await {
        return Check::fail(NAME, format!("state: {e}"));
    }
    match store.user_row(name).await {
        Ok(Some(r)) => {
            require!(
                NAME,
                r.user.role == Role::Moderator,
                "role change not read back"
            );
            require!(
                NAME,
                r.user.state == UserState::Banned,
                "state change not read back"
            );
        }
        other => return Check::fail(NAME, format!("user_row: {other:?}")),
    }
    let _ = fx;
    match store.user_row("no-such-user-anywhere").await {
        Ok(None) => Check::pass(NAME),
        other => Check::fail(NAME, format!("unknown user: {other:?}")),
    }
}

async fn spaces_are_creatable_once_and_editable<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "create_space is unique by path; space_detail and update_space round-trip";
    let page = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let path = format!("{}conformance-sub/", page.space.path);
    let new = NewSpace {
        name: "Conformance sub".into(),
        path: path.clone(),
        parent_id: Some(page.space.id),
        ranking: Ranking::Bump,
        depth_cap: 4,
        config: "{}".into(),
    };
    let id = match store.create_space(&new).await {
        Ok(id) => id,
        // Left over from an earlier run.
        Err(StoreError::Conflict) => match store.all_spaces().await {
            Ok(all) => match all.iter().find(|s| s.space.path == path) {
                Some(s) => s.space.id,
                None => return Check::fail(NAME, "conflict, but the path is not listed"),
            },
            Err(e) => return Check::fail(NAME, format!("{e}")),
        },
        Err(e) => return Check::fail(NAME, format!("create: {e}")),
    };
    match store.create_space(&new).await {
        Err(StoreError::Conflict) => {}
        other => return Check::fail(NAME, format!("duplicate path: {other:?}")),
    }
    if let Err(e) = store
        .update_space(
            id,
            "Conformance sub, renamed",
            Ranking::Gravity,
            3,
            r#"{"k":1}"#,
        )
        .await
    {
        return Check::fail(NAME, format!("update: {e}"));
    }
    let detail = match store.space_detail(id).await {
        Ok(Some(d)) => d,
        other => return Check::fail(NAME, format!("space_detail: {other:?}")),
    };
    require!(
        NAME,
        detail.space.name == "Conformance sub, renamed",
        "name not updated"
    );
    require!(
        NAME,
        detail.space.ranking == Ranking::Gravity,
        "ranking not updated"
    );
    require!(NAME, detail.space.depth_cap == 3, "depth_cap not updated");
    require!(
        NAME,
        detail.config == r#"{"k":1}"#,
        "config not updated: {}",
        detail.config
    );
    require!(
        NAME,
        detail.space.parent_id == Some(page.space.id),
        "parent not read"
    );
    require!(
        NAME,
        detail.space.path == path,
        "path changed: {}",
        detail.space.path
    );
    match store.all_spaces().await {
        Ok(all) => {
            require!(
                NAME,
                all.iter().any(|s| s.space.id == id),
                "not in all_spaces"
            );
            require!(
                NAME,
                all.windows(2).all(|w| w[0].space.path <= w[1].space.path),
                "not in path order"
            );
            let Some(parent) = all.iter().find(|s| s.space.id == page.space.id) else {
                return Check::fail(NAME, "the fixture's space is missing from all_spaces");
            };
            require!(NAME, parent.thread_count >= 1, "thread_count not counted");
        }
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    match store.space_detail(i64::MAX - 11).await {
        Ok(None) => Check::pass(NAME),
        other => Check::fail(NAME, format!("absent space: {other:?}")),
    }
}

async fn stats_and_the_full_log_read_back<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "site_stats counts, and full_log includes private rows with detail";
    let stats = match store.site_stats().await {
        Ok(s) => s,
        Err(e) => return Check::fail(NAME, format!("stats: {e}")),
    };
    require!(NAME, stats.users >= 1, "no users counted");
    require!(NAME, stats.threads >= 1, "no threads counted");
    require!(
        NAME,
        stats.posts >= fx.post_count,
        "posts {} < fixture's {}",
        stats.posts,
        fx.post_count
    );
    let log = match store.full_log(200).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("log: {e}")),
    };
    // `the_public_log_omits_private_rows...` wrote one private row; here it must show.
    let Some(private) = log.iter().find(|e| e.entry.action == "conf_private") else {
        return Check::fail(NAME, "the private row is missing from the full log");
    };
    require!(NAME, !private.public, "private row marked public");
    require!(
        NAME,
        private.detail.contains("suite"),
        "detail not read: {}",
        private.detail
    );
    require!(
        NAME,
        private.entry.target_public_id.as_ref() == Some(&fx.writable[0]),
        "target not resolved"
    );
    Check::pass(NAME)
}

// ---------------------------------------------------------------------------
// Spaces, threads, editing, profiles
// ---------------------------------------------------------------------------

async fn spaces_resolve_by_path_and_list_their_threads<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "space_by_path, spaces_under and space_threads agree with thread_page";
    let page = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let path = match SpacePath::parse(&page.space.path) {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("space path {:?}: {e}", page.space.path)),
    };
    let space = match store.space_by_path(&path).await {
        Ok(Some(s)) => s,
        other => return Check::fail(NAME, format!("space_by_path: {other:?}")),
    };
    require!(
        NAME,
        space.id == page.space.id,
        "resolved a different space"
    );
    require!(
        NAME,
        space.path == page.space.path,
        "path not round-tripped"
    );
    let siblings = match store.spaces_under(space.parent_id).await {
        Ok(v) => v,
        Err(e) => return Check::fail(NAME, format!("spaces_under: {e}")),
    };
    require!(
        NAME,
        siblings.iter().any(|s| s.id == space.id),
        "the space is not listed under its parent"
    );
    let listed = match store.space_threads(&path, 1000).await {
        Ok(v) => v,
        Err(e) => return Check::fail(NAME, format!("space_threads: {e}")),
    };
    require!(
        NAME,
        listed.iter().any(|t| t.public_id == fx.thread),
        "the fixture thread is not listed in its space"
    );
    require!(
        NAME,
        listed.windows(2).all(|w| w[0].bumped_at >= w[1].bumped_at),
        "not most-recently-bumped first"
    );
    let ctx = match store.space_context(space.id, fx.author_id).await {
        Ok(c) => c,
        Err(e) => return Check::fail(NAME, format!("space_context: {e}")),
    };
    require!(NAME, ctx.space_id == space.id, "space_context: wrong space");
    require!(
        NAME,
        ctx.author_created_at > 0,
        "space_context: author not read"
    );
    match store.space_context(i64::MAX - 9, fx.author_id).await {
        Err(StoreError::NotFound) => {}
        other => return Check::fail(NAME, format!("absent space: {other:?}")),
    }
    let absent = SpacePath::parse("no-such-space-here").expect("valid path");
    match store.space_by_path(&absent).await {
        Ok(None) => Check::pass(NAME),
        other => Check::fail(NAME, format!("absent path: {other:?}")),
    }
}

async fn a_new_thread_is_created_listed_and_writable<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "create_thread makes a listed, readable thread that accepts posts once";
    let page = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let new = NewThread {
        public_id: fx.writable[4].clone(),
        space_id: page.space.id,
        space_path: page.space.path.clone(),
        kind: ThreadKind::Link,
        title: "Conformance thread".into(),
        url: Some("https://example.com/conformance".into()),
        author_id: fx.author_id,
        created_at: NOW + 5,
    };
    let thread = match store.create_thread(&new).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("create: {e}")),
    };
    require!(NAME, thread.id > 0, "no row id");
    require!(NAME, thread.public_id == new.public_id, "id not preserved");
    match store.create_thread(&new).await {
        Err(StoreError::Conflict) => {}
        other => return Check::fail(NAME, format!("reused id: {other:?}")),
    }
    let first = match store
        .insert_post(&NewPost {
            public_id: fx.writable[5].clone(),
            thread: new.public_id.clone(),
            parent: None,
            author_id: fx.author_id,
            body_md: "first post".into(),
            body_html: SanitizedHtml::assert_sanitized("<p>first post</p>".into()),
            created_at: NOW + 5,
            state: PostState::Visible,
        })
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("first post: {e}")),
    };
    require!(NAME, first.depth == 0, "first post is not at the root");
    let read = match store.thread_page(&new.public_id, &Page::first(10)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("read back: {e}")),
    };
    require!(
        NAME,
        read.thread.title == "Conformance thread",
        "title lost"
    );
    require!(NAME, read.thread.kind == ThreadKind::Link, "kind lost");
    require!(
        NAME,
        read.thread.url.as_deref() == Some("https://example.com/conformance"),
        "url lost"
    );
    require!(
        NAME,
        read.thread.post_count == 1,
        "post_count {}",
        read.thread.post_count
    );
    require!(NAME, read.posts.len() == 1, "{} posts", read.posts.len());
    require!(
        NAME,
        !read.thread.author_name.is_empty(),
        "author not joined"
    );
    let path = match SpacePath::parse(&page.space.path) {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let listed = match store.space_threads(&path, 1000).await {
        Ok(v) => v,
        Err(e) => return Check::fail(NAME, format!("space_threads: {e}")),
    };
    require!(
        NAME,
        listed.first().map(|t| &t.public_id) == Some(&new.public_id),
        "the new thread is not first in its space"
    );
    match store.recent_threads(1000).await {
        Ok(v) => require!(
            NAME,
            v.iter().any(|t| t.public_id == new.public_id),
            "the new thread is missing from the index"
        ),
        Err(e) => return Check::fail(NAME, format!("recent_threads: {e}")),
    }
    Check::pass(NAME)
}

async fn editing_rewrites_the_body_and_bumps_the_version<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str =
        "update_post_body rewrites both bodies, sets edited_at and bumps the version";
    let post = &fx.writable[0];
    let before = match store.thread_version(&fx.thread).await {
        Ok(Some(v)) => v,
        other => return Check::fail(NAME, format!("version: {other:?}")),
    };
    if let Err(e) = store
        .update_post_body(
            post,
            "top level, edited",
            &SanitizedHtml::assert_sanitized("<p>top level, edited</p>".into()),
            NOW + 9,
        )
        .await
    {
        return Check::fail(NAME, format!("update: {e}"));
    }
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        rp.body_md == "top level, edited",
        "body_md not rewritten"
    );
    let after = match store.thread_version(&fx.thread).await {
        Ok(Some(v)) => v,
        other => return Check::fail(NAME, format!("version: {other:?}")),
    };
    require!(
        NAME,
        after > before,
        "cache_version went {before} -> {after}"
    );
    let page = match store
        .thread_page(&fx.thread, &Page::first(fx.post_count + 10))
        .await
    {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let Some(shown) = page.posts.iter().find(|p| &p.public_id == post) else {
        return Check::fail(NAME, "edited post missing from its page");
    };
    require!(
        NAME,
        shown.body_html.contains("edited"),
        "body_html not rewritten"
    );
    require!(
        NAME,
        shown.edited_at == Some(NOW + 9),
        "edited_at not set: {:?}",
        shown.edited_at
    );
    // Restore, for the checks that follow.
    if let Err(e) = store
        .update_post_body(
            post,
            "top level",
            &SanitizedHtml::assert_sanitized("<p>top level</p>".into()),
            NOW + 10,
        )
        .await
    {
        return Check::fail(NAME, format!("restore: {e}"));
    }
    match store
        .update_post_body(
            &fx.absent,
            "x",
            &SanitizedHtml::assert_sanitized("<p>x</p>".into()),
            NOW,
        )
        .await
    {
        Err(StoreError::NotFound) => Check::pass(NAME),
        other => Check::fail(NAME, format!("absent post: {other:?}")),
    }
}

async fn a_profile_lists_the_users_visible_posts_newest_first<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "user_profile finds the user and lists visible posts newest first";
    let name = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p.thread.author_name,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let profile = match store.user_profile(&name, 1000).await {
        Ok(Some(p)) => p,
        other => return Check::fail(NAME, format!("profile of {name:?}: {other:?}")),
    };
    require!(NAME, profile.user.name == name, "wrong user");
    require!(NAME, profile.created_at > 0, "created_at not read");
    require!(
        NAME,
        profile.posts.iter().any(|p| p.public_id == fx.writable[0]),
        "a visible post by the user is missing"
    );
    require!(
        NAME,
        profile
            .posts
            .windows(2)
            .all(|w| w[0].created_at >= w[1].created_at),
        "not newest first"
    );
    require!(
        NAME,
        profile.posts.iter().all(|p| !p.thread_title.is_empty()),
        "thread title not joined"
    );
    let limited = match store.user_profile(&name, 2).await {
        Ok(Some(p)) => p,
        other => return Check::fail(NAME, format!("{other:?}")),
    };
    require!(NAME, limited.posts.len() <= 2, "limit ignored");
    match store.user_profile("no-such-user-anywhere", 10).await {
        Ok(None) => Check::pass(NAME),
        other => Check::fail(NAME, format!("unknown user: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Email
// ---------------------------------------------------------------------------

fn email_token(seed: u8) -> EmailToken {
    EmailToken::from_bytes([seed; EMAIL_TOKEN_BYTES])
}

fn stored(fx: &Fixture, token: &EmailToken, kind: TokenKind, expires_at: i64) -> StoredToken {
    StoredToken {
        token_hash: token.hash(),
        user_id: fx.author_id,
        kind,
        email: EmailAddress::parse("conformance@example.com").expect("valid"),
        created_at: NOW,
        expires_at,
    }
}

async fn email_tokens_are_spent_exactly_once_and_only_as_their_kind<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "an email token is spent once, as its own kind, before it expires";
    let live = email_token(0x61);
    if let Err(e) = store
        .create_email_token(&stored(fx, &live, TokenKind::Verify, NOW + 1000))
        .await
    {
        return Check::fail(NAME, format!("create: {e}"));
    }
    match store
        .consume_email_token(&live.hash(), TokenKind::Reset, NOW)
        .await
    {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("spent as the wrong kind: {other:?}")),
    }
    match store
        .consume_email_token(&live.hash(), TokenKind::Verify, NOW + 1000)
        .await
    {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("spent at its own expiry: {other:?}")),
    }
    match store
        .peek_email_token(&live.hash(), TokenKind::Verify, NOW)
        .await
    {
        Ok(Some(name)) => require!(NAME, !name.is_empty(), "peek returned an empty name"),
        other => return Check::fail(NAME, format!("peek: {other:?}")),
    }
    let got = match store
        .consume_email_token(&live.hash(), TokenKind::Verify, NOW)
        .await
    {
        Ok(Some(c)) => c,
        other => return Check::fail(NAME, format!("first spend: {other:?}")),
    };
    match store
        .peek_email_token(&live.hash(), TokenKind::Verify, NOW)
        .await
    {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("peek after spend: {other:?}")),
    }
    require!(NAME, got.user_id == fx.author_id, "wrong user");
    require!(
        NAME,
        got.email.as_str() == "conformance@example.com",
        "address not round-tripped"
    );
    match store
        .consume_email_token(&live.hash(), TokenKind::Verify, NOW)
        .await
    {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("second spend: {other:?}")),
    }
    // Retiring spends every outstanding token of a kind and no other.
    let a = email_token(0x62);
    let b = email_token(0x63);
    let other_kind = email_token(0x64);
    for (t, kind) in [
        (&a, TokenKind::Reset),
        (&b, TokenKind::Reset),
        (&other_kind, TokenKind::Verify),
    ] {
        if let Err(e) = store
            .create_email_token(&stored(fx, t, kind, NOW + 1000))
            .await
        {
            return Check::fail(NAME, format!("create: {e}"));
        }
    }
    match store
        .retire_email_tokens(fx.author_id, TokenKind::Reset, NOW)
        .await
    {
        Ok(n) => require!(NAME, n >= 2, "retired {n}, expected at least 2"),
        Err(e) => return Check::fail(NAME, format!("retire: {e}")),
    }
    for t in [&a, &b] {
        match store
            .consume_email_token(&t.hash(), TokenKind::Reset, NOW)
            .await
        {
            Ok(None) => {}
            other => return Check::fail(NAME, format!("retired token still spent: {other:?}")),
        }
    }
    match store
        .consume_email_token(&other_kind.hash(), TokenKind::Verify, NOW)
        .await
    {
        Ok(Some(_)) => Check::pass(NAME),
        other => Check::fail(
            NAME,
            format!("retiring resets took a verify token: {other:?}"),
        ),
    }
}

async fn a_verified_address_belongs_to_one_account<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "an address is claimable by many and verifiable by one";
    let addr = "conformance-verified@example.com";
    if let Err(e) = store.set_email(fx.author_id, Some(addr)).await {
        return Check::fail(NAME, format!("set: {e}"));
    }
    match store.account(fx.author_id).await {
        Ok(Some(a)) => {
            require!(NAME, a.email.as_deref() == Some(addr), "address not stored");
            require!(
                NAME,
                !a.email_is_verified(),
                "a fresh address counts as verified"
            );
        }
        other => return Check::fail(NAME, format!("account: {other:?}")),
    }
    match store.user_by_verified_email(addr).await {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("unverified address resolved: {other:?}")),
    }
    match store
        .mark_email_verified(fx.author_id, "someone-else@example.com", NOW)
        .await
    {
        Ok(false) => {}
        other => return Check::fail(NAME, format!("verified a mismatched address: {other:?}")),
    }
    match store.mark_email_verified(fx.author_id, addr, NOW).await {
        Ok(true) => {}
        other => return Check::fail(NAME, format!("verify: {other:?}")),
    }
    match store.user_by_verified_email(addr).await {
        Ok(Some(u)) => require!(NAME, u.id == fx.author_id, "resolved to the wrong user"),
        other => return Check::fail(NAME, format!("verified address: {other:?}")),
    }
    // A second account can claim it but not prove it.
    let rival = match store.create_user("conformance-rival", NOW, None).await {
        Ok(id) => id,
        Err(StoreError::Conflict) => match store.user_by_name("conformance-rival").await {
            Ok(Some(c)) => c.user.id,
            other => return Check::fail(NAME, format!("lookup: {other:?}")),
        },
        Err(e) => return Check::fail(NAME, format!("create: {e}")),
    };
    if let Err(e) = store.set_email(rival, Some(addr)).await {
        return Check::fail(NAME, format!("rival claim: {e}"));
    }
    match store.mark_email_verified(rival, addr, NOW).await {
        Err(StoreError::Conflict) => {}
        other => return Check::fail(NAME, format!("rival verified a taken address: {other:?}")),
    }
    // Removing the address releases it.
    if let Err(e) = store.set_email(fx.author_id, None).await {
        return Check::fail(NAME, format!("clear: {e}"));
    }
    match store.account(fx.author_id).await {
        Ok(Some(a)) => require!(NAME, a.email.is_none(), "address survived removal"),
        other => return Check::fail(NAME, format!("{other:?}")),
    }
    match store.mark_email_verified(rival, addr, NOW).await {
        Ok(true) => {}
        other => return Check::fail(NAME, format!("released address not verifiable: {other:?}")),
    }
    // Tidy: the rival gives it up too, so a rerun starts clean.
    let _ = store.set_email(rival, None).await;
    Check::pass(NAME)
}

// ---------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------

async fn write_context_describes_the_space_thread_and_author<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "write_context returns the space, thread state and author, or NotFound";
    let page = match store.thread_page(&fx.thread, &Page::first(1)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let ctx = match store.write_context(&fx.thread, fx.author_id).await {
        Ok(c) => c,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, ctx.space_id == page.space.id, "wrong space");
    require!(
        NAME,
        ctx.thread_state == page.thread.state,
        "wrong thread state"
    );
    require!(
        NAME,
        ctx.author_created_at > 0,
        "author created_at not read: {}",
        ctx.author_created_at
    );
    require!(
        NAME,
        !ctx.space_config.is_empty(),
        "space config came back empty rather than as JSON"
    );
    match store.write_context(&fx.absent, fx.author_id).await {
        Err(StoreError::NotFound) => {}
        other => return Check::fail(NAME, format!("absent thread: {other:?}")),
    }
    match store.write_context(&fx.thread, i64::MAX - 7).await {
        Err(StoreError::NotFound) => {}
        other => return Check::fail(NAME, format!("absent author: {other:?}")),
    }
    Check::pass(NAME)
}

async fn duplicate_detection_is_bounded_by_the_window<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "author_posted_recently matches the exact body inside the window only";
    // writable[0] was written by `appended_post_is_readable` with body "top level" at NOW.
    let hit = store
        .author_posted_recently(fx.author_id, "top level", NOW)
        .await;
    require!(
        NAME,
        hit == Ok(true),
        "did not find the post just written: {hit:?}"
    );
    let late = store
        .author_posted_recently(fx.author_id, "top level", NOW + 1)
        .await;
    require!(
        NAME,
        late == Ok(false),
        "found a post from before the window: {late:?}"
    );
    let other = store
        .author_posted_recently(fx.author_id, "top level ", NOW)
        .await;
    require!(
        NAME,
        other == Ok(false),
        "matched a body that differs by a space"
    );
    Check::pass(NAME)
}

async fn a_state_change_bumps_the_thread_version<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "set_post_state changes the post and bumps the thread's cache_version";
    let post = &fx.writable[0];
    let before = match store.thread_version(&fx.thread).await {
        Ok(Some(v)) => v,
        other => return Check::fail(NAME, format!("version: {other:?}")),
    };
    if let Err(e) = store.set_post_state(post, PostState::Pending, NOW).await {
        return Check::fail(NAME, format!("set pending: {e}"));
    }
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(e) => return Check::fail(NAME, format!("post_for_review: {e}")),
    };
    require!(NAME, rp.state == PostState::Pending, "state did not change");
    require!(
        NAME,
        rp.body_md == "top level",
        "post_for_review must carry body_md"
    );
    require!(NAME, rp.thread_public_id == fx.thread, "wrong thread");
    let after = match store.thread_version(&fx.thread).await {
        Ok(Some(v)) => v,
        other => return Check::fail(NAME, format!("version: {other:?}")),
    };
    require!(
        NAME,
        after > before,
        "cache_version went {before} -> {after}"
    );
    if let Err(e) = store.set_post_state(post, PostState::Visible, NOW).await {
        return Check::fail(NAME, format!("set visible: {e}"));
    }
    match store
        .set_post_state(&fx.absent, PostState::Hidden, NOW)
        .await
    {
        Err(StoreError::NotFound) => {}
        other => return Check::fail(NAME, format!("absent post: {other:?}")),
    }
    match store.post_for_review(&fx.absent).await {
        Err(StoreError::NotFound) => Check::pass(NAME),
        other => Check::fail(NAME, format!("absent post_for_review: {other:?}")),
    }
}

fn action(post_id: i64, action: &'static str, public: bool) -> NewAction {
    NewAction {
        actor_kind: ActorKind::Rule,
        actor_id: None,
        actor_name: "conformance".into(),
        target_kind: "post",
        target_id: post_id,
        action,
        detail: serde_json::json!({ "suite": true }),
        public,
        created_at: NOW,
    }
}

async fn the_public_log_omits_private_rows_and_resolves_targets<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "the public log shows public rows only, newest first, with public ids";
    let post_id = match store.post_for_review(&fx.writable[0]).await {
        Ok(rp) => rp.id,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let public_id = match store
        .log_action(&action(post_id, "conf_public", true))
        .await
    {
        Ok(id) => id,
        Err(e) => return Check::fail(NAME, format!("log: {e}")),
    };
    let private_id = match store
        .log_action(&action(post_id, "conf_private", false))
        .await
    {
        Ok(id) => id,
        Err(e) => return Check::fail(NAME, format!("log: {e}")),
    };
    require!(NAME, private_id > public_id, "row ids did not advance");
    let log = match store.public_log(50).await {
        Ok(l) => l,
        Err(e) => return Check::fail(NAME, format!("read: {e}")),
    };
    require!(
        NAME,
        log.iter().all(|e| e.action != "conf_private"),
        "a private row appeared in the public log"
    );
    let Some(entry) = log.iter().find(|e| e.id == public_id) else {
        return Check::fail(NAME, "the public row is missing");
    };
    require!(
        NAME,
        entry.target_public_id.as_ref() == Some(&fx.writable[0]),
        "target public id not resolved: {:?}",
        entry.target_public_id
    );
    require!(NAME, entry.actor_kind == ActorKind::Rule, "actor kind lost");
    require!(
        NAME,
        log.windows(2)
            .all(|w| (w[0].created_at, w[0].id) >= (w[1].created_at, w[1].id)),
        "not newest first"
    );
    Check::pass(NAME)
}

async fn a_review_opens_once_keeps_its_verdict_and_resolves_once<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "a review item is one row per post, keeps its verdict, and resolves once";
    let rp = match store.post_for_review(&fx.writable[0]).await {
        Ok(rp) => rp,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    let verdict = Verdict {
        call: Call::Clean,
        confidence: 0.6,
        categories: vec![crate::moderation::Category::OffTopic],
        rationale: "suite".into(),
        model: "suite-model".into(),
    };
    let open = |reason, verdict, appeal: Option<&str>| NewReview {
        post_id: rp.id,
        space_id: rp.space_id,
        reason,
        verdict,
        appeal_text: appeal.map(str::to_string),
        opened_at: NOW,
    };
    if let Err(e) = store
        .open_review(&open(ReviewReason::Classifier, Some(verdict), None))
        .await
    {
        return Check::fail(NAME, format!("open: {e}"));
    }
    // Reopened as an appeal: no verdict supplied, so the old one must survive.
    if let Err(e) = store
        .open_review(&open(ReviewReason::Appeal, None, Some("please")))
        .await
    {
        return Check::fail(NAME, format!("reopen: {e}"));
    }
    let items = match store.open_reviews(100).await {
        Ok(i) => i,
        Err(e) => return Check::fail(NAME, format!("list: {e}")),
    };
    let mine: Vec<_> = items.iter().filter(|i| i.post_id == rp.id).collect();
    require!(NAME, mine.len() == 1, "{} items for one post", mine.len());
    let item = mine[0];
    require!(
        NAME,
        item.reason == ReviewReason::Appeal,
        "reason not updated"
    );
    require!(
        NAME,
        item.model_verdict == Some(Call::Clean),
        "verdict was lost on reopen"
    );
    require!(
        NAME,
        item.model_confidence == Some(0.6),
        "confidence was lost on reopen"
    );
    require!(
        NAME,
        item.model_categories == vec![crate::moderation::Category::OffTopic],
        "categories did not round trip: {:?}",
        item.model_categories
    );
    require!(
        NAME,
        item.appeal_text.as_deref() == Some("please"),
        "appeal text missing"
    );
    require!(NAME, item.post_public_id == fx.writable[0], "wrong post");
    require!(
        NAME,
        !item.body_html.is_empty(),
        "the reviewer needs the body"
    );
    require!(NAME, !item.resolved, "listed as open but marked resolved");

    let before = match store.agreement(rp.space_id).await {
        Ok(a) => a,
        Err(e) => return Check::fail(NAME, format!("agreement: {e}")),
    };
    let resolved = store
        .resolve_review(item.id, Resolution::Approve, fx.author_id, NOW)
        .await;
    let Ok(Some(was)) = resolved else {
        return Check::fail(NAME, format!("resolve: {resolved:?}"));
    };
    require!(NAME, was.id == item.id, "resolved a different item");
    match store
        .resolve_review(item.id, Resolution::Reject, fx.author_id, NOW)
        .await
    {
        Ok(None) => {}
        other => return Check::fail(NAME, format!("second resolve: {other:?}")),
    }
    match store.open_reviews(100).await {
        Ok(items) => require!(
            NAME,
            items.iter().all(|i| i.post_id != rp.id),
            "still listed after resolving"
        ),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    }
    // clean + approve is agreement.
    let after = match store.agreement(rp.space_id).await {
        Ok(a) => a,
        Err(e) => return Check::fail(NAME, format!("agreement: {e}")),
    };
    require!(
        NAME,
        after.agreed == before.agreed + 1 && after.disagreed == before.disagreed,
        "agreement {before:?} -> {after:?}"
    );
    Check::pass(NAME)
}

async fn reports_count_distinct_reporters<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "a report counts once per reporter, and the tally is distinct reporters";
    let post_id = match store.post_for_review(&fx.writable[0]).await {
        Ok(rp) => rp.id,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    // A second account, created here or found from an earlier run.
    let reporter = match store.create_user("conformance-reporter", NOW, None).await {
        Ok(id) => id,
        Err(StoreError::Conflict) => match store.user_by_name("conformance-reporter").await {
            Ok(Some(c)) => c.user.id,
            other => return Check::fail(NAME, format!("lookup: {other:?}")),
        },
        Err(e) => return Check::fail(NAME, format!("create: {e}")),
    };
    let sig = |user: i64| NewSignal {
        post_id,
        user_id: user,
        kind: SIGNAL_REPORT,
        weight: -1.0,
        reason: Some("suite".into()),
        created_at: NOW,
    };
    let first = match store.add_report(&sig(fx.author_id)).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        first.added && first.count == 1,
        "first report: {first:?}"
    );
    let repeat = match store.add_report(&sig(fx.author_id)).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        !repeat.added && repeat.count == 1,
        "repeat report: {repeat:?}"
    );
    let second = match store.add_report(&sig(reporter)).await {
        Ok(t) => t,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(
        NAME,
        second.added && second.count == 2,
        "second reporter: {second:?}"
    );
    Check::pass(NAME)
}

async fn the_sweep_lists_pending_posts_nobody_is_looking_at<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "pending_posts lists pending posts without an open review, oldest first";
    let post = &fx.writable[0];
    let rp = match store.post_for_review(post).await {
        Ok(rp) => rp,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    if let Err(e) = store.set_post_state(post, PostState::Pending, NOW).await {
        return Check::fail(NAME, format!("{e}"));
    }
    let listed = store.pending_posts(NOW + 1, 1000).await;
    require!(
        NAME,
        matches!(&listed, Ok(ids) if ids.contains(post)),
        "pending post not listed: {listed:?}"
    );
    let too_early = store.pending_posts(NOW, 1000).await;
    require!(
        NAME,
        matches!(&too_early, Ok(ids) if !ids.contains(post)),
        "listed a post newer than the cutoff"
    );
    // Now someone is looking at it.
    if let Err(e) = store
        .open_review(&NewReview {
            post_id: rp.id,
            space_id: rp.space_id,
            reason: ReviewReason::Error,
            verdict: None,
            appeal_text: None,
            opened_at: NOW,
        })
        .await
    {
        return Check::fail(NAME, format!("{e}"));
    }
    let queued = store.pending_posts(NOW + 1, 1000).await;
    require!(
        NAME,
        matches!(&queued, Ok(ids) if !ids.contains(post)),
        "a post with an open review was listed for the sweep"
    );
    // Tidy: resolve and restore.
    let items = store.open_reviews(1000).await.unwrap_or_default();
    if let Some(item) = items.iter().find(|i| i.post_id == rp.id) {
        let _ = store
            .resolve_review(item.id, Resolution::Approve, fx.author_id, NOW)
            .await;
    }
    if let Err(e) = store.set_post_state(post, PostState::Visible, NOW).await {
        return Check::fail(NAME, format!("{e}"));
    }
    Check::pass(NAME)
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
    // The client bucket is independent.
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
    // Clearing an absent bucket is success, not an error.
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
    // Exactly at expiry it is already gone.
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
        state: crate::model::PostState::Visible,
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
    let loc = match store.locate_post(&post.public_id, PAGE_SIZE).await {
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
    let parent = match store.locate_post(&fx.writable[1], PAGE_SIZE).await {
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
    // Bounded so a broken cursor loops finitely.
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

/// An unknown thread is `None`; a zero would key every missing thread to the same cached page.
async fn thread_version_is_readable_and_absent_for_unknown<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "thread_version reads, and is None for an unknown thread";
    let known = match store.thread_version(&fx.thread).await {
        Ok(Some(v)) => v,
        Ok(None) => return Check::fail(NAME, "no version for the fixture thread"),
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    require!(NAME, known >= 0, "negative version {known}");
    match store.thread_version(&fx.absent).await {
        Ok(None) => Check::pass(NAME),
        Ok(Some(v)) => Check::fail(NAME, format!("unknown thread reported version {v}")),
        Err(e) => Check::fail(NAME, format!("unknown thread errored: {e}")),
    }
}

/// The loser of the check-then-insert race is a `Conflict`, not a 500.
async fn a_duplicate_username_is_a_conflict<S: Store>(store: &S, _fx: &Fixture) -> Check {
    const NAME: &str = "creating a user with a taken name is Conflict, not a backend error";
    let name = "conformance-dup-check";
    if let Err(e) = store.create_user(name, 1_800_000_000_000, None).await {
        return Check::fail(NAME, format!("first create failed: {e}"));
    }
    match store.create_user(name, 1_800_000_000_000, None).await {
        Err(StoreError::Conflict) => Check::pass(NAME),
        Err(e) => Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => Check::fail(NAME, "the duplicate was accepted"),
    }
}

/// Following the cursor lands on a page that contains the post. Also checked at a page size of
/// 1, because a fixture smaller than a page never leaves page zero.
async fn a_permalink_cursor_lands_on_a_page_holding_the_post<S: Store>(
    store: &S,
    fx: &Fixture,
) -> Check {
    const NAME: &str = "a permalink cursor lands on a page holding the post";
    for size in [1u32, 3, PAGE_SIZE] {
        let loc = match store.locate_post(&fx.known_post, size).await {
            Ok(l) => l,
            Err(e) => return Check::fail(NAME, format!("size {size}: {e}")),
        };
        let page = Page {
            after: loc.cursor.clone(),
            limit: size,
        };
        let rendered = match store.thread_page(&fx.thread, &page).await {
            Ok(p) => p,
            Err(e) => return Check::fail(NAME, format!("size {size}: {e}")),
        };
        require!(
            NAME,
            rendered.posts.iter().any(|p| p.path == loc.path),
            "size {size}: cursor {:?} gave a page of {} starting at {:?}, without {}",
            loc.cursor.as_ref().map(|c| c.as_str()),
            rendered.posts.len(),
            rendered.posts.first().map(|p| p.path.as_str()),
            loc.path
        );
    }
    Check::pass(NAME)
}

async fn locate_post_finds_its_thread<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "locate_post resolves to the right thread and path";
    let loc = match store.locate_post(&fx.known_post, PAGE_SIZE).await {
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
    const NAME: &str = "absent post is NotFound, and post_by_id carries the source";
    match store.locate_post(&fx.absent, PAGE_SIZE).await {
        Err(StoreError::NotFound) => {}
        Err(e) => return Check::fail(NAME, format!("wrong error: {e}")),
        Ok(_) => return Check::fail(NAME, "located a post that does not exist"),
    }
    match store.post_by_id(&fx.absent).await {
        Err(StoreError::NotFound) => {}
        other => return Check::fail(NAME, format!("post_by_id of absent: {other:?}")),
    }
    match store.post_by_id(&fx.known_post).await {
        Ok((post, thread)) => {
            require!(NAME, post.public_id == fx.known_post, "wrong post");
            require!(NAME, thread == fx.thread, "wrong thread");
            require!(NAME, post.body_md.is_some(), "post_by_id must load body_md");
            require!(NAME, !post.author_name.is_empty(), "author not joined");
            Check::pass(NAME)
        }
        Err(e) => Check::fail(NAME, format!("{e}")),
    }
}

async fn posts_never_expose_body_md<S: Store>(store: &S, fx: &Fixture) -> Check {
    const NAME: &str = "read path does not load body_md";
    let page = match store.thread_page(&fx.thread, &Page::first(5)).await {
        Ok(p) => p,
        Err(e) => return Check::fail(NAME, format!("{e}")),
    };
    // Shipping the markdown too doubles what a page that renders only the html sends back.
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
