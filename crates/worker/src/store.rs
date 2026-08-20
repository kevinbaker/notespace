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

use notespace_core::model::*;
use notespace_core::path::Path;
use notespace_core::store::{async_trait, Page, Store, StoreError, StoreResult};
use serde::Deserialize;
use worker::D1Database;

/// Columns for the thread header. Deliberately narrow.
const THREAD_SQL: &str = "\
SELECT t.id, t.space_id, t.kind, t.title, t.url, t.author_id, u.name AS author_name, \
t.created_at, t.bumped_at, t.post_count, t.state, t.cache_version, \
s.slug AS space_slug, s.name AS space_name, s.ranking AS space_ranking, \
s.depth_cap AS space_depth_cap \
FROM thread t \
JOIN user u ON u.id = t.author_id \
JOIN space s ON s.id = t.space_id \
WHERE t.id = ?1";

/// The read-path query. `path > ?2` is an indexed range scan on `idx_post_thread_path`,
/// not a sort: SQLite walks the index in order and stops at LIMIT.
///
/// `body_md` is intentionally absent; see the note on [`Post::body_md`].
const POSTS_SQL: &str = "\
SELECT p.id, p.thread_id, p.parent_id, p.path, p.depth, p.author_id, \
u.name AS author_name, p.body_html, p.created_at, p.edited_at, p.score, p.state \
FROM post p \
JOIN user u ON u.id = p.author_id \
WHERE p.thread_id = ?1 AND p.path > ?2 \
ORDER BY p.path \
LIMIT ?3";

/// Sorts below every valid path, so it means "start at the beginning of the thread".
/// Using a sentinel keeps the SQL identical for the first page and every later page, which
/// keeps D1's prepared-statement cache warm.
const PATH_START: &str = "";

#[derive(Deserialize)]
struct ThreadRow {
    id: i64,
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
    space_slug: String,
    space_name: String,
    space_ranking: String,
    space_depth_cap: i64,
}

#[derive(Deserialize)]
struct PostRow {
    id: i64,
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

pub struct D1Store {
    db: D1Database,
}

impl D1Store {
    pub fn new(db: D1Database) -> Self {
        Self { db }
    }

    /// The `Space` the last-fetched thread belongs to, carried alongside the thread so the
    /// page render does not need a third query for `depth_cap`.
    pub async fn thread_page_with_space(
        &self,
        thread: ThreadId,
        page: Page,
    ) -> StoreResult<(Space, ThreadPage)> {
        let cursor = page
            .after
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or(PATH_START);
        // Over-fetch by one to detect "is there a next page?" without a second COUNT query.
        let fetch = page.limit.saturating_add(1);

        // D1 rejects JS bigints (`D1_TYPE_ERROR: Type 'bigint' not supported`), so integer
        // parameters cross the boundary as f64. Exact for every id below 2^53, which is
        // seven orders of magnitude past what the 500MB D1 ceiling can hold.
        let thread_param = (thread as f64).into();
        let thread_stmt = self
            .db
            .prepare(THREAD_SQL)
            .bind(&[thread_param])
            .map_err(backend)?;
        let posts_stmt = self
            .db
            .prepare(POSTS_SQL)
            .bind(&[(thread as f64).into(), cursor.into(), (fetch as f64).into()])
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

        let thread_rows: Vec<ThreadRow> = thread_res.results().map_err(backend)?;
        let tr = thread_rows.into_iter().next().ok_or(StoreError::NotFound)?;

        let space = Space {
            id: tr.space_id,
            slug: tr.space_slug,
            name: tr.space_name,
            parent_id: None,
            ranking: ranking(&tr.space_ranking),
            depth_cap: tr.space_depth_cap.clamp(0, i64::from(u32::MAX)) as u32,
        };

        let t = Thread {
            id: tr.id,
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
            posts.push(Post {
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

        Ok((
            space,
            ThreadPage {
                thread: t,
                posts,
                next_cursor,
            },
        ))
    }
}

#[async_trait(?Send)]
impl Store for D1Store {
    async fn thread_page(&self, thread: ThreadId, page: Page) -> StoreResult<ThreadPage> {
        self.thread_page_with_space(thread, page)
            .await
            .map(|(_, page)| page)
    }
}
