//! D1 implementation of [`Store`].
//!
//! The whole point of this file is the query count. DESIGN.md §3.2 allows 50 D1 queries per
//! Worker invocation on the free plan, and §3.1 says a thread page must be 1-3 queries, not
//! one-per-post. This implementation uses **two statements in a single batched round trip**:
//!
//! 1. thread metadata + its author
//! 2. one indexed range scan over `(thread_id, path)` for the posts, joined to authors
//!
//! `D1Database::batch` sends both in one request, so the invocation costs two queries
//! against the 50 budget and one network round trip against the CPU budget.

use core::cell::Cell;
use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::path::Path;
use notespace_core::sql;
use notespace_core::store::{async_trait, Page, PostLocation, Store, StoreError, StoreResult};

use serde::Deserialize;
use worker::{D1Database, D1Result, D1ResultMeta};

// The SQL lives in `notespace_core::sql`, shared verbatim with the native adapter so the two
// cannot drift. What is target-specific is binding and row decoding, below.

/// Sum D1's per-statement meta into one figure per request.
///
/// A statement whose meta is absent contributes nothing rather than zero, so a partial report
/// cannot masquerade as a complete one: if any statement is missing a field, the total for that
/// field stays `None`.
fn collect_stats(results: &[&D1Result]) -> QueryStats {
    let metas: Vec<_> = results.iter().map(|r| r.meta().ok().flatten()).collect();
    let all = |f: fn(&D1ResultMeta) -> Option<f64>| -> Option<f64> {
        metas
            .iter()
            .map(|m| m.as_ref().and_then(f))
            .try_fold(0.0, |acc, v| v.map(|v| acc + v))
    };
    QueryStats {
        statements: results.len() as u32,
        rows_read: all(|m| m.rows_read.map(|v| v as f64)).map(|v| v as usize),
        duration_ms: all(|m| m.duration),
    }
}

#[derive(Deserialize)]
struct ThreadRow {
    id: i64,
    public_id: String,
    space_id: i64,
    kind: String,
    title: String,
    url: Option<String>,
    author_id: i64,
    author_name: String,
    created_at: i64,
    bumped_at: i64,
    post_count: i64,
    state: String,
    cache_version: i64,
    space_path: String,
    space_name: String,
    space_ranking: String,
    space_depth_cap: i64,
}

#[derive(Deserialize)]
struct PostLocationRow {
    thread_public_id: String,
    post_path: String,
}

#[derive(Deserialize)]
struct PostRow {
    id: i64,
    public_id: String,
    thread_id: i64,
    parent_id: Option<i64>,
    path: String,
    depth: i64,
    author_id: i64,
    author_name: String,
    body_html: String,
    created_at: i64,
    edited_at: Option<i64>,
    score: f64,
    state: String,
}

fn thread_state(s: &str) -> ThreadState {
    match s {
        "locked" => ThreadState::Locked,
        "pinned" => ThreadState::Pinned,
        "hidden" => ThreadState::Hidden,
        "deleted" => ThreadState::Deleted,
        _ => ThreadState::Visible,
    }
}

fn thread_kind(s: &str) -> ThreadKind {
    match s {
        "link" => ThreadKind::Link,
        "question" => ThreadKind::Question,
        "poll" => ThreadKind::Poll,
        "announcement" => ThreadKind::Announcement,
        _ => ThreadKind::Discussion,
    }
}

/// Unknown states fail closed: anything the code does not recognise is treated as hidden
/// rather than shown. A typo in a migration must not publish moderated content.
fn post_state(s: &str) -> PostState {
    match s {
        "visible" => PostState::Visible,
        "pending" => PostState::Pending,
        "deleted" => PostState::Deleted,
        _ => PostState::Hidden,
    }
}

fn ranking(s: &str) -> Ranking {
    match s {
        "gravity" => Ranking::Gravity,
        "best" => Ranking::Best,
        "score_threshold" => Ranking::ScoreThreshold,
        _ => Ranking::Bump,
    }
}

fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// What D1 reported about the queries behind one page render.
///
/// `rows_read` is the number M0 could only *derive* from the query plan: D1 reports it in
/// production and leaves it `None` under `wrangler dev --local`, where the database is an
/// in-process SQLite rather than a service. `duration` is likewise the real round trip,
/// which local mode cannot show at all because there is no network in the path.
///
/// Reported per request via `Server-Timing`, so the first production deploy answers the
/// open questions in docs/M0-findings.md instead of restating the local numbers.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryStats {
    /// Statements in the batch. Must stay at 2 — the D1 free tier allows 50 per invocation,
    /// and the whole read-path design rests on not going per-post.
    pub statements: u32,
    /// Summed across statements. `None` under local dev.
    pub rows_read: Option<usize>,
    /// Summed across statements, in milliseconds. `None` under local dev.
    pub duration_ms: Option<f64>,
}

impl QueryStats {
    /// `Server-Timing` value, readable in browser devtools and by `curl -D-`.
    pub fn server_timing(&self) -> String {
        let mut out = format!("d1;desc=\"statements={}\"", self.statements);
        if let Some(rows) = self.rows_read {
            out.push_str(&format!(", d1_rows;desc=\"rows_read={rows}\""));
        }
        if let Some(ms) = self.duration_ms {
            out.push_str(&format!(", d1_query;dur={ms}"));
        }
        out
    }
}

pub struct D1Store {
    db: D1Database,
    /// Stats from the most recent query, for the handler to read afterwards.
    ///
    /// Interior mutability rather than a return-value change on purpose: `Store` is the seam
    /// the dual-target promise rests on (DESIGN.md §3.1), and D1's telemetry has no business
    /// in its signatures. A native SQLite adapter reports different things, or nothing.
    last_stats: Cell<QueryStats>,
}

impl D1Store {
    pub fn new(db: D1Database) -> Self {
        Self {
            db,
            last_stats: Cell::new(QueryStats::default()),
        }
    }

    /// Resolve a post's public id to the thread it currently lives in, and its page cursor.
    ///
    /// One query. The point of this indirection is that a post's thread can CHANGE -- splitting
    /// and merging threads is routine moderation -- so a permalink cannot bake in a thread id
    /// and stay correct. `/p/{id}` asks where the post is *now*.
    async fn fetch_post_location(&self, post: &PublicId) -> StoreResult<PostLocation> {
        let stmt = self
            .db
            .prepare(sql::LOCATE_POST)
            .bind(&[post.as_str().into()])
            .map_err(backend)?;
        let res = stmt.all().await.map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<PostLocationRow> = res.results().map_err(backend)?;
        let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;
        Ok(PostLocation {
            thread: PublicId::parse(&row.thread_public_id).map_err(|e| {
                StoreError::Backend(format!("thread has an unparseable public_id: {e}"))
            })?,
            path: Path::parse(&row.post_path)
                .map_err(|e| StoreError::Backend(format!("post has an unparseable path: {e}")))?,
        })
    }

    /// Stats from the most recent query on this store.
    pub fn last_stats(&self) -> QueryStats {
        self.last_stats.get()
    }

    async fn fetch_thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage> {
        let cursor = page
            .after
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or(sql::PATH_START);
        // Over-fetch by one to detect "is there a next page?" without a second COUNT query.
        let fetch = page.limit.saturating_add(1);

        // Both statements key off the public id, which crosses as TEXT. Integer parameters
        // would need to cross as f64: D1 rejects JS bigints outright
        // (`D1_TYPE_ERROR: Type 'bigint' not supported`).
        let public = thread.encode();
        let thread_stmt = self
            .db
            .prepare(sql::THREAD)
            .bind(&[public.as_str().into()])
            .map_err(backend)?;
        let posts_stmt = self
            .db
            .prepare(sql::POSTS)
            .bind(&[public.as_str().into(), cursor.into(), (fetch as f64).into()])
            .map_err(backend)?;

        // One round trip, two statements. This is the line that must not regress.
        let mut results = self
            .db
            .batch(vec![thread_stmt, posts_stmt])
            .await
            .map_err(backend)?;

        let posts_res = results.pop().ok_or_else(|| {
            StoreError::Backend("batch returned fewer results than statements".into())
        })?;
        let thread_res = results.pop().ok_or_else(|| {
            StoreError::Backend("batch returned fewer results than statements".into())
        })?;

        // Record what D1 reported before consuming the results. Both fields are None under
        // local dev; in production they are the real numbers.
        self.last_stats
            .set(collect_stats(&[&thread_res, &posts_res]));

        let thread_rows: Vec<ThreadRow> = thread_res.results().map_err(backend)?;
        let tr = thread_rows.into_iter().next().ok_or(StoreError::NotFound)?;

        let space = Space {
            id: tr.space_id,
            path: tr.space_path,
            name: tr.space_name,
            parent_id: None,
            ranking: ranking(&tr.space_ranking),
            depth_cap: tr.space_depth_cap.clamp(0, i64::from(u32::MAX)) as u32,
        };

        let t = Thread {
            id: tr.id,
            // Re-parsed rather than reusing the request's id: a row whose stored id does not
            // round-trip is corrupt, and should say so instead of being papered over.
            public_id: PublicId::parse(&tr.public_id).map_err(|e| {
                StoreError::Corrupt(format!(
                    "thread {} has invalid public_id {:?}: {e}",
                    tr.id, tr.public_id
                ))
            })?,
            space_id: tr.space_id,
            kind: thread_kind(&tr.kind),
            title: tr.title,
            url: tr.url,
            author_id: tr.author_id,
            author_name: tr.author_name,
            created_at: tr.created_at,
            bumped_at: tr.bumped_at,
            post_count: tr.post_count.max(0) as u32,
            state: thread_state(&tr.state),
            cache_version: tr.cache_version,
        };

        let mut rows: Vec<PostRow> = posts_res.results().map_err(backend)?;

        // We asked for limit+1. If we got it, there is another page; drop the extra row and
        // use the last *kept* post's path as the cursor.
        let has_more = rows.len() > page.limit as usize;
        if has_more {
            rows.truncate(page.limit as usize);
        }

        let mut posts = Vec::with_capacity(rows.len());
        for r in rows {
            // A path that fails validation would silently corrupt thread ordering, so it is
            // an error rather than something to paper over.
            let path = Path::parse(&r.path).map_err(|e| {
                StoreError::Corrupt(format!("post {} has invalid path {:?}: {e}", r.id, r.path))
            })?;
            let public_id = PublicId::parse(&r.public_id).map_err(|e| {
                // A row that cannot round-trip its own id is corrupt, not merely unexpected.
                StoreError::Backend(format!("post {} has an unparseable public_id: {e}", r.id))
            })?;
            posts.push(Post {
                public_id,
                id: r.id,
                thread_id: r.thread_id,
                parent_id: r.parent_id,
                depth: r.depth.max(0) as u32,
                path,
                author_id: r.author_id,
                author_name: r.author_name,
                body_md: None,
                body_html: r.body_html,
                created_at: r.created_at,
                edited_at: r.edited_at,
                score: r.score,
                state: post_state(&r.state),
            });
        }

        let next_cursor = if has_more {
            posts.last().map(|p| p.path.clone())
        } else {
            None
        };

        Ok(ThreadPage {
            space,
            thread: t,
            posts,
            next_cursor,
        })
    }
}

#[async_trait(?Send)]
impl Store for D1Store {
    async fn thread_page(&self, thread: &PublicId, page: &Page) -> StoreResult<ThreadPage> {
        self.fetch_thread_page(thread, page).await
    }

    async fn locate_post(&self, post: &PublicId) -> StoreResult<PostLocation> {
        self.fetch_post_location(post).await
    }
}
