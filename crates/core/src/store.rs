//! The `Store` seam. Everything above it is target-agnostic.
//!
//! Binding on every implementation: identical SQLite dialect, no N+1 (D1 allows 50 statements
//! per invocation — each method states its budget), and no connection pool on wasm.

use crate::email::{ConsumedToken, StoredToken, TokenKind};
use crate::id::PublicId;
use crate::model::{
    Account, NewPost, NewThread, Post, PostState, Profile, SanitizedHtml, Space, SpaceId, Thread,
    ThreadPage, ThreadSummary, Timestamp, User, UserId,
};
use crate::moderation::{
    AgreementStats, LogEntry, NewAction, NewReview, NewSignal, ReportTally, Resolution, ReviewItem,
    ReviewPost, WriteContext,
};
use crate::path::Path;
use crate::ratelimit::{AttemptKeys, Attempts};
use crate::session::{Session, TokenHash};
use crate::space_key::SpacePath;

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

    /// **Budget: 1 statement.**
    async fn user_by_id(&self, id: UserId) -> StoreResult<Option<User>>;

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

    // -- Moderation ---------------------------------------------------------

    /// What the write path needs before deciding whether to hold a post: the space's config,
    /// the thread's state, and the author's age. `NotFound` if either the thread or the user
    /// is absent. **Budget: 1 statement.**
    async fn write_context(&self, thread: &PublicId, author: UserId) -> StoreResult<WriteContext>;

    /// Whether `author` posted exactly `body_md` at or after `since`. Bounded by the author's
    /// own recent posts through `idx_post_author`. **Budget: 1 statement.**
    async fn author_posted_recently(
        &self,
        author: UserId,
        body_md: &str,
        since: Timestamp,
    ) -> StoreResult<bool>;

    /// A post with its markdown, its space's config and its author, for the classifier and the
    /// reviewer. **Budget: 1 statement.**
    async fn post_for_review(&self, post: &PublicId) -> StoreResult<ReviewPost>;

    /// Change a post's state and bump its thread's `cache_version`, so the baked page turns
    /// over. **Budget: 2 statements, one batch.**
    async fn set_post_state(
        &self,
        post: &PublicId,
        state: PostState,
        now: Timestamp,
    ) -> StoreResult<()>;

    /// Append to the action log; returns the row id. **Budget: 1 statement.**
    async fn log_action(&self, action: &NewAction) -> StoreResult<i64>;

    /// Open a review item, or reopen and update the one already standing for the post.
    /// **Budget: 1 statement.**
    async fn open_review(&self, review: &NewReview) -> StoreResult<()>;

    /// Resolve an open item and return it as it was, or `None` if it does not exist or is
    /// already resolved -- so two moderators clicking at once produce one action.
    /// **Budget: 2 statements.**
    async fn resolve_review(
        &self,
        id: i64,
        resolution: Resolution,
        by: UserId,
        now: Timestamp,
    ) -> StoreResult<Option<ReviewItem>>;

    /// Oldest first: the queue is worked in the order things arrived. **Budget: 1 statement.**
    async fn open_reviews(&self, limit: u32) -> StoreResult<Vec<ReviewItem>>;

    /// Record a report. `added` is false when this user already reported this post; `count` is
    /// the number of distinct reporters now standing. **Budget: 2 statements.**
    async fn add_report(&self, signal: &NewSignal) -> StoreResult<ReportTally>;

    /// Posts still `pending` that were created before `older_than`, oldest first. The sweep's
    /// worklist. **Budget: 1 statement.**
    async fn pending_posts(&self, older_than: Timestamp, limit: u32) -> StoreResult<Vec<PublicId>>;

    /// The public action log, newest first. **Budget: 1 statement.**
    async fn public_log(&self, limit: u32) -> StoreResult<Vec<LogEntry>>;

    /// How often reviewers in `space` have agreed with the model. Counts only resolved items
    /// where the model made a call. **Budget: 1 statement.**
    async fn agreement(&self, space: SpaceId) -> StoreResult<AgreementStats>;

    // -- Spaces and threads -------------------------------------------------

    /// **Budget: 1 statement.**
    async fn space_by_path(&self, path: &SpacePath) -> StoreResult<Option<Space>>;

    /// Direct children, `None` for the top level. **Budget: 1 statement.**
    async fn spaces_under(&self, parent: Option<SpaceId>) -> StoreResult<Vec<Space>>;

    /// Threads in the space and every space under it, most recently bumped first.
    /// **Budget: 1 statement**, one range scan.
    async fn space_threads(&self, space: &SpacePath, limit: u32)
        -> StoreResult<Vec<ThreadSummary>>;

    /// [`Store::write_context`] for a thread that does not exist yet. `NotFound` if either the
    /// space or the author is absent. **Budget: 1 statement.**
    async fn space_context(&self, space: SpaceId, author: UserId) -> StoreResult<WriteContext>;

    /// The thread row alone; the body is a post appended afterwards. A reused public id is a
    /// [`StoreError::Conflict`]. **Budget: 1 statement.**
    async fn create_thread(&self, thread: &NewThread) -> StoreResult<Thread>;

    /// Rewrite a post's body and bump its thread's `cache_version`, so the baked page turns
    /// over. `NotFound` for an absent post. **Budget: 2 statements, one batch.**
    async fn update_post_body(
        &self,
        post: &PublicId,
        body_md: &str,
        body_html: &SanitizedHtml,
        edited_at: Timestamp,
    ) -> StoreResult<()>;

    /// The user and their latest `limit` visible posts. `None` for an unknown name; a deleted
    /// account is `Some` with its state, so the page can say so. **Budget: 2 statements.**
    async fn user_profile(&self, name: &str, limit: u32) -> StoreResult<Option<Profile>>;

    // -- Email ----------------------------------------------------------------

    /// **Budget: 1 statement.**
    async fn account(&self, user: UserId) -> StoreResult<Option<Account>>;

    /// At most one, by construction of the schema. **Budget: 1 statement.**
    async fn user_by_verified_email(&self, email: &str) -> StoreResult<Option<User>>;

    /// Replace the address and reset verification. `None` removes it. **Budget: 1 statement.**
    async fn set_email(&self, user: UserId, email: Option<&str>) -> StoreResult<()>;

    /// `Ok(false)` when the account's address is no longer `email`; [`StoreError::Conflict`]
    /// when another account has already verified it. **Budget: 1 statement.**
    async fn mark_email_verified(
        &self,
        user: UserId,
        email: &str,
        now: Timestamp,
    ) -> StoreResult<bool>;

    /// **Budget: 1 statement.**
    async fn create_email_token(&self, token: &StoredToken) -> StoreResult<()>;

    /// Spend a token: `None` if it is unknown, of another kind, expired, or already spent.
    /// Exactly one caller can ever get `Some` for a given token. **Budget: 1 statement.**
    async fn consume_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<ConsumedToken>>;

    /// The username a live token belongs to, without spending it. **Budget: 1 statement.**
    async fn peek_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<String>>;

    /// Spend every outstanding token of a kind, returning how many. **Budget: 1 statement.**
    async fn retire_email_tokens(
        &self,
        user: UserId,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<u32>;
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
