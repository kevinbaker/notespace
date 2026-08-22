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
}
