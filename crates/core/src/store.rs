//! The `Store` seam. Everything above it is target-agnostic.
//!
//! Binding on every implementation: identical SQLite dialect, no N+1 (D1 allows 50 statements
//! per invocation — each method states its budget), and no connection pool on wasm.

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
    /// Expected under load; the caller's cue to recompute and retry.
    #[error("write raced another writer")]
    Conflict,
    /// The reply would exceed the space's configured depth cap.
    #[error("reply is too deep: cap is {cap}")]
    TooDeep { cap: u32 },
}

pub type StoreResult<T> = Result<T, StoreError>;

/// A path cursor rather than an offset: offset pagination degrades to a full scan on deep pages.
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

/// Where a post currently lives. Resolved per request, because splitting a thread moves posts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostLocation {
    pub thread: PublicId,
    pub path: Path,
    /// `None` on the first page. Without it a permalink's anchor is not in the document.
    pub cursor: Option<Path>,
}

/// Offset of the post whose path is the cursor for the page holding `rank`. `None` on page one.
pub fn page_cursor_offset(rank: i64, page_size: u32) -> Option<i64> {
    let size = page_size.max(1) as i64;
    let page = rank / size;
    (page > 0).then(|| page * size - 1)
}

/// Everything the application needs from storage. Statement budgets are stated per method and
/// checked for the read path by the query-budget tests in `crates/store-sqlite/tests`.
#[async_trait(?Send)]
pub trait Store {
    /// **Budget: 2 statements**, ideally one round trip.
    async fn thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage>;

    /// `page_size` and the rank's filters have to match [`Store::thread_page`], or the cursor
    /// lands on the wrong page.
    ///
    /// **Budget: 3 statements**, index-only.
    async fn locate_post(&self, post: &PublicId, page_size: u32) -> StoreResult<PostLocation>;

    /// **Budget: 1 statement.**
    async fn recent_threads(&self, limit: u32) -> StoreResult<Vec<ThreadSummary>>;

    /// Bumped by every write, so a cache key built from it is invalidated by the write itself.
    ///
    /// **Budget: 1 statement, 1 row.**
    async fn thread_version(&self, thread: &PublicId) -> StoreResult<Option<i64>>;

    /// **Budget: 4 statements.**
    ///
    /// Path allocation is read-then-write, so a concurrent reply can compute the same ordinal;
    /// the loser surfaces as [`StoreError::Conflict`] and the caller retries.
    async fn insert_post(&self, post: &NewPost) -> StoreResult<Post>;

    /// Start a session. **Budget: 1 statement.**
    async fn create_session(&self, session: &Session) -> StoreResult<()>;

    /// **Budget: 1 statement.** Expiry is filtered by the query, so an unrun sweep cannot hand
    /// back a dead session.
    async fn lookup_session(
        &self,
        token: &TokenHash,
        now: Timestamp,
    ) -> StoreResult<Option<Authenticated>>;

    /// **Budget: 1 statement.** Rate-limited by
    /// [`SessionPolicy`](crate::session::SessionPolicy), or this is a write per pageview.
    async fn refresh_session(
        &self,
        token: &TokenHash,
        refreshed_at: Timestamp,
        expires_at: Timestamp,
    ) -> StoreResult<()>;

    /// Idempotent: deleting an absent session is success. **Budget: 1 statement.**
    async fn delete_session(&self, token: &TokenHash) -> StoreResult<()>;

    /// **Budget: 1 statement.**
    async fn user_by_name(&self, name: &str) -> StoreResult<Option<Credential>>;

    /// **Budget: 1 statement.** A duplicate name is a [`StoreError::Conflict`].
    async fn create_user(
        &self,
        name: &str,
        created_at: Timestamp,
        password_hash: Option<&str>,
    ) -> StoreResult<UserId>;

    /// Rewrite a stored credential. **Budget: 1 statement.**
    async fn set_password_hash(&self, user: UserId, hash: &str) -> StoreResult<()>;

    /// Returns `(identity, client)`. **Budget: 1 statement** — this runs before the password
    /// hash on every attempt.
    async fn login_attempts(
        &self,
        keys: &AttemptKeys,
    ) -> StoreResult<(Option<Attempts>, Option<Attempts>)>;

    /// Record a failed attempt against one bucket. **Budget: 1 statement.**
    async fn record_login_attempt(&self, key: &str, attempts: Attempts) -> StoreResult<()>;

    /// **Budget: 1 statement.**
    async fn clear_login_attempts(&self, key: &str) -> StoreResult<()>;

    /// Housekeeping; nothing depends on it having run. **Budget: 1 statement.**
    async fn sweep_login_attempts(&self, cutoff: Timestamp) -> StoreResult<u32>;

    /// **Budget: 1 statement.** Returns how many sessions ended.
    async fn delete_user_sessions(&self, user: UserId) -> StoreResult<u32>;
}

/// `password_hash` is `None` for an account with no local credential. Reached after hashing, so
/// that it is not a username oracle.
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

/// Path allocation. The bounds and the arithmetic live together because they have to agree.
pub struct NextPath;

impl NextPath {
    /// Both bounds exclusive, so a parent with no children yet matches nothing.
    pub fn search_bounds(parent: Option<&Path>) -> (String, String) {
        match parent {
            Some(p) => (p.as_str().to_string(), p.subtree_end()),
            // "" sorts below every path, and a character above the alphabet's last sorts above
            // every path.
            None => (String::new(), "\u{7f}".to_string()),
        }
    }

    /// `last` is the deepest path under `parent`, not the last direct child; truncating it here
    /// keeps the lookup one indexed probe.
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

#[cfg(test)]
mod cursor_tests {
    use super::page_cursor_offset;

    #[test]
    fn the_first_page_needs_no_cursor() {
        for rank in 0..200 {
            assert_eq!(page_cursor_offset(rank, 200), None, "rank {rank}");
        }
    }

    #[test]
    fn later_pages_point_at_the_last_post_before_them() {
        assert_eq!(page_cursor_offset(200, 200), Some(199));
        assert_eq!(page_cursor_offset(399, 200), Some(199));
        assert_eq!(page_cursor_offset(400, 200), Some(399));
        assert_eq!(page_cursor_offset(205, 200), Some(199));
    }

    /// Disagreement here splits one page's cache entry in two.
    #[test]
    fn every_rank_on_a_page_agrees_on_the_cursor() {
        for page in 1..5i64 {
            let expected = Some(page * 50 - 1);
            for within in 0..50 {
                assert_eq!(page_cursor_offset(page * 50 + within, 50), expected);
            }
        }
    }

    #[test]
    fn a_zero_page_size_does_not_divide_by_zero() {
        assert_eq!(page_cursor_offset(10, 0), Some(9));
    }
}
