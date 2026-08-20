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
use crate::model::ThreadPage;
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

#[async_trait(?Send)]
pub trait Store {
    /// Fetch one page of a thread.
    ///
    /// Threads are addressed by their public id, not their internal integer id: the integer
    /// never leaves the database (DESIGN.md §4.2).
    ///
    /// **Budget: at most 2 D1 queries.** One for thread metadata, one indexed range scan
    /// over `(thread_id, path)` for the posts. Implementations that issue a query per post
    /// violate DESIGN.md §3.1 and will not survive the free tier.
    async fn thread_page(&self, thread: PublicId, page: Page) -> StoreResult<ThreadPage>;
}
