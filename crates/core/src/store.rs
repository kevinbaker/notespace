//! The `Store` seam.
//!
//! DESIGN.md §3.1: "The `Store` trait is the most important design decision in this
//! document. Everything above it is target-agnostic. Get it wrong and the dual-target
//! promise dies."
//!
//! Constraints binding on every implementation:
//!
//! - Identical SQLite dialect on both targets. No Postgres-isms, ever.
//! - **No N+1.** D1 allows 50 queries per Worker invocation on the free plan. Every method
//!   here is documented with the number of round trips it is allowed to make.
//! - No connection pool on the wasm side: sqlx's pool needs a Tokio runtime that does not
//!   exist there.

use crate::id::PublicId;
use crate::model::{NewPost, Post, ThreadPage};
use crate::path::Path;

/// `?Send` is required: wasm futures are not `Send` (DESIGN.md §3.1).
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
/// This trait is the whole of it: if a handler reaches past this into a concrete adapter, the
/// dual-target promise is already broken, because the other target has no such method. An
/// earlier revision had one method here that nothing called, while the Worker used two inherent
/// methods on `D1Store` — a trait that compiled and carried no weight. Keep every storage call
/// on this side of the line.
///
/// Query budgets are part of the contract, not advice. D1 allows 50 statements per Worker
/// invocation on the free plan, and the read path's whole design is not going per-post.
#[async_trait(?Send)]
pub trait Store {
    /// One page of a thread, with the space it belongs to.
    ///
    /// Threads are addressed by public id; the internal integer never leaves the database
    /// (DESIGN.md §4.2).
    ///
    /// **Budget: 2 statements**, ideally in one round trip — thread header, and an indexed
    /// range scan over `(thread_id, path)`. Measured in production at 2.52 ms p50 for both
    /// together; one statement per post would be 0.5 s.
    async fn thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage>;

    /// Resolve a post's public id to the thread it is in now.
    ///
    /// **Budget: 1 statement.**
    async fn locate_post(&self, post: &PublicId) -> StoreResult<PostLocation>;

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
