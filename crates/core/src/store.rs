//! The `Store` seam.
//!
//! Everything above this trait is target-agnostic. One build serves both a Worker and a
//! self-hosted binary only for as long as that stays true.
//!
//! Constraints binding on every implementation:
//!
//! - Identical SQLite dialect on both targets. No Postgres-isms, ever.
//! - **No N+1.** D1 allows 50 queries per Worker invocation on the free plan. Every method
//!   here is documented with the number of round trips it is allowed to make.
//! - No connection pool on the wasm side: sqlx's pool needs a Tokio runtime that does not
//!   exist there.

use crate::id::PublicId;
use crate::model::{NewPost, Post, ThreadPage, ThreadSummary, Timestamp, User, UserId};
use crate::path::Path;
use crate::ratelimit::{AttemptKeys, Attempts};
use crate::session::{Session, TokenHash};

/// `?Send` is required: wasm futures are not `Send`.
pub use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    /// A row came back that does not satisfy a domain invariant (bad path, unknown enum).
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error("backend error: {0}")]
    Backend(String),
    /// A path was allocated concurrently by another writer. Recompute and retry.
    ///
    /// Distinct from [`StoreError::Backend`] because it is expected under load and is the
    /// caller's cue to try again, not to surface an error.
    #[error("write raced another writer")]
    Conflict,
    /// The reply would exceed the space's configured depth cap.
    #[error("reply is too deep: cap is {cap}")]
    TooDeep { cap: u32 },
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A page request against a thread, expressed as a path cursor rather than an offset.
///
/// Offset pagination degrades to a full scan on deep pages; a path cursor stays one indexed
/// range scan no matter how deep into the thread the reader is.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    /// Resume strictly after this path. `None` starts at the top of the thread.
    pub after: Option<Path>,
    /// Maximum posts to return.
    pub limit: u32,
}

impl Page {
    pub const DEFAULT_LIMIT: u32 = 200;

    pub fn first(limit: u32) -> Self {
        Page { after: None, limit }
    }

    pub fn after(cursor: Path, limit: u32) -> Self {
        Page {
            after: Some(cursor),
            limit,
        }
    }
}

/// Where a post currently lives.
///
/// A post's thread can change — splitting and merging threads is routine moderation — so a
/// permalink has to resolve this at request time rather than bake it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostLocation {
    pub thread: PublicId,
    pub path: Path,
}

/// Everything the application needs from storage.
///
/// This trait is the whole of it: a handler reaching past it into a concrete adapter breaks the
/// dual-target promise, because the other target has no such method. Keep every storage call on
/// this side of the line.
///
/// The per-method query budgets below are part of the contract, not advice.
#[async_trait(?Send)]
pub trait Store {
    /// One page of a thread, with the space it belongs to.
    ///
    /// Threads are addressed by public id; the internal integer never leaves the database.
    ///
    /// **Budget: 2 statements**, ideally in one round trip — thread header, and an indexed
    /// range scan over `(thread_id, path)`.
    async fn thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage>;

    /// Resolve a post's public id to the thread it is in now.
    ///
    /// **Budget: 1 statement.**
    async fn locate_post(&self, post: &PublicId) -> StoreResult<PostLocation>;

    /// Threads for the index, most recently active first.
    ///
    /// **Budget: 1 statement.**
    async fn recent_threads(&self, limit: u32) -> StoreResult<Vec<ThreadSummary>>;

    /// The thread's bake version, for building a cache key.
    ///
    /// `None` when no such thread exists. Bumped by every write to the thread, so a key built
    /// from it is invalidated by the write itself rather than by a TTL.
    ///
    /// **Budget: 1 statement, 1 row.**
    async fn thread_version(&self, thread: &PublicId) -> StoreResult<Option<i64>>;

    /// Append a post to a thread, allocating its materialized path.
    ///
    /// **Budget: 4 statements.** Resolve the thread, resolve the parent (skipped for a
    /// top-level post), find the last path under the parent, then insert and bump in one batch.
    ///
    /// # Concurrency
    ///
    /// Allocating a path is read-then-write, so two replies to the same parent at the same
    /// moment can compute the same ordinal. The `UNIQUE(thread_id, path)` index catches the
    /// loser, which surfaces as [`StoreError::Conflict`]. Implementations must *not* paper over
    /// it by picking another ordinal internally — the caller retries, because a retry has to
    /// re-read the parent anyway.
    ///
    /// A conforming implementation therefore never silently drops a post and never writes two
    /// posts to the same path. `writes_never_collide` in the conformance suite pins both.
    async fn insert_post(&self, post: &NewPost) -> StoreResult<Post>;

    /// Start a session. **Budget: 1 statement.**
    async fn create_session(&self, session: &Session) -> StoreResult<()>;

    /// Resolve a cookie to its session and user, or `None` if there is no live session.
    ///
    /// **Budget: 1 statement**, joining `user` — an authenticated request needs both, and two
    /// round trips for one identity is not in the budget.
    ///
    /// Takes a [`TokenHash`], not a token: the raw secret has no path into storage because no
    /// storage method accepts one.
    ///
    /// Expired rows are filtered by the query rather than by the caller, so a sweep that has
    /// not run cannot hand back a dead session.
    async fn lookup_session(
        &self,
        token: &TokenHash,
        now: Timestamp,
    ) -> StoreResult<Option<Authenticated>>;

    /// Push a session's expiry out. **Budget: 1 statement.**
    ///
    /// Rate-limited by [`SessionPolicy`](crate::session::SessionPolicy), not here: called on
    /// every request, sliding expiry would be a write per pageview.
    async fn refresh_session(
        &self,
        token: &TokenHash,
        refreshed_at: Timestamp,
        expires_at: Timestamp,
    ) -> StoreResult<()>;

    /// Log out. Idempotent — deleting an absent session is success, not `NotFound`, because the
    /// caller's goal is "this token no longer works" and it already does not.
    ///
    /// **Budget: 1 statement.**
    async fn delete_session(&self, token: &TokenHash) -> StoreResult<()>;

    /// An account and its credential, by name. **Budget: 1 statement.**
    ///
    /// `None` for an unknown name. The caller must not branch on that before hashing — see
    /// [`Credential`].
    async fn user_by_name(&self, name: &str) -> StoreResult<Option<Credential>>;

    /// Create an account. **Budget: 1 statement.** `password_hash` is `None` for external auth.
    async fn create_user(
        &self,
        name: &str,
        created_at: Timestamp,
        password_hash: Option<&str>,
    ) -> StoreResult<UserId>;

    /// Rewrite a stored credential. **Budget: 1 statement.**
    async fn set_password_hash(&self, user: UserId, hash: &str) -> StoreResult<()>;

    /// Current attempt counters for both buckets.
    ///
    /// **Budget: 1 statement.** Runs before the password hash on every login attempt, so a
    /// second round trip here is a round trip on the attacker's schedule.
    ///
    /// Returns `(identity, client)`; either is `None` when nothing is recorded.
    async fn login_attempts(
        &self,
        keys: &AttemptKeys,
    ) -> StoreResult<(Option<Attempts>, Option<Attempts>)>;

    /// Record a failed attempt against one bucket. **Budget: 1 statement.**
    async fn record_login_attempt(&self, key: &str, attempts: Attempts) -> StoreResult<()>;

    /// Clear a bucket after a successful login. **Budget: 1 statement.**
    ///
    /// A limiter that punishes success locks out the people using a shared address correctly,
    /// and gets switched off.
    async fn clear_login_attempts(&self, key: &str) -> StoreResult<()>;

    /// Drop windows that ended before `cutoff`. Housekeeping; nothing depends on it having run,
    /// since expiry is decided by the window in the row rather than by the row's absence.
    ///
    /// **Budget: 1 statement.** Returns rows removed.
    async fn sweep_login_attempts(&self, cutoff: Timestamp) -> StoreResult<u32>;

    /// Log out everywhere: after a password change, or when an account is banned.
    ///
    /// **Budget: 1 statement.** Returns how many sessions ended.
    async fn delete_user_sessions(&self, user: UserId) -> StoreResult<u32>;
}

/// An account as the login path needs it.
///
/// `password_hash` is `None` when the account has no local credential — an OIDC user, or one
/// whose password was cleared. That is a login failure, but the handler must reach it *after*
/// hashing something, not by returning early: an early return is measurably faster and turns
/// the login form into a username oracle.
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub user: User,
    pub password_hash: Option<String>,
}

/// A live session and whose it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Authenticated {
    pub session: Session,
    pub user: User,
}

/// Path allocation, shared by every adapter.
///
/// The rule is small but easy to get subtly wrong in two places, so it lives in one: an adapter
/// supplies the last path under the parent, and this decides what the new path is. Both the
/// bounds of the lookup and the arithmetic on the result are here, because they have to agree.
pub struct NextPath;

impl NextPath {
    /// Half-open bounds for "the deepest path under this parent".
    ///
    /// For a reply, that is the parent's own subtree. For a top-level post it is the whole
    /// thread, since the last root is a prefix of the thread's last path in preorder.
    ///
    /// Both bounds are exclusive, which is why the lower one is the parent's own path rather
    /// than its first child: a parent with no children yet must match nothing.
    pub fn search_bounds(parent: Option<&Path>) -> (String, String) {
        match parent {
            Some(p) => (p.as_str().to_string(), p.subtree_end()),
            // "" sorts below every path, and a character above the alphabet's last sorts above
            // every path.
            None => (String::new(), "\u{7f}".to_string()),
        }
    }

    /// The path a new post should take.
    ///
    /// `last` is the deepest existing path under `parent`, as returned by a lookup bounded by
    /// [`NextPath::search_bounds`] — *not* the last direct child. Truncating it here is what
    /// keeps the lookup a single indexed probe instead of a scan across siblings.
    pub fn allocate(parent: Option<&Path>, last: Option<&Path>) -> StoreResult<Path> {
        // `Path::depth()` counts separators, so a root is depth 0 and a child of `parent` sits
        // one below it.
        let child_depth = parent.map_or(0, |p| p.depth() + 1);
        let last_sibling = last.and_then(|l| l.ancestor_at_depth(child_depth));
        let next = match (last_sibling, parent) {
            // Somebody is already at this level: take the next ordinal.
            (Some(sib), _) => sib.next_sibling(),
            // First child of an existing post.
            (None, Some(p)) => p.child(0),
            // First post in the thread.
            (None, None) => Path::root(0),
        };
        next.map_err(|e| StoreError::Backend(format!("path allocation: {e}")))
    }
}
