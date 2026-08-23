//! D1 implementation of [`Store`]. A thread page is two statements in one `batch()`: metadata
//! plus one indexed range scan over `(thread_id, path)`.

use core::cell::Cell;
use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::path::Path;
use notespace_core::ratelimit::{AttemptKeys, Attempts};
use notespace_core::session::{Session, TokenHash};
use notespace_core::sql;
use notespace_core::store::{
    async_trait, Authenticated, Credential, NextPath, Page, PostLocation, Store, StoreError,
    StoreResult,
};

use serde::Deserialize;
use worker::{D1Database, D1Result, D1ResultMeta};

// SQL lives in `notespace_core::sql`, shared verbatim; binding and row decoding are the
// target-specific parts.

/// D1 rejects JS bigints (`D1_TYPE_ERROR: Type 'bigint' not supported`), so integers cross as
/// `f64`. Exact inside 2^53, which covers row ids, depths and millisecond timestamps.
fn num(v: i64) -> worker::wasm_bindgen::JsValue {
    worker::wasm_bindgen::JsValue::from_f64(v as f64)
}

/// Absent meta keeps the total `None`, so a partial report cannot look like a complete one.
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
struct ThreadSummaryRow {
    public_id: String,
    title: String,
    post_count: i64,
    bumped_at: i64,
    author_name: String,
    space_name: String,
    space_path: String,
}

#[derive(Deserialize)]
struct VersionRow {
    cache_version: i64,
}

#[derive(Deserialize)]
struct CredentialRow {
    id: i64,
    name: String,
    state: String,
    password_hash: Option<String>,
}

#[derive(Deserialize)]
struct AttemptRow {
    key: String,
    window_start: i64,
    count: i64,
}

#[derive(Deserialize)]
struct SessionRow {
    user_id: i64,
    created_at: i64,
    refreshed_at: i64,
    expires_at: i64,
    user_name: String,
    user_state: String,
}

#[derive(Deserialize)]
struct ThreadRowId {
    id: i64,
}

#[derive(Deserialize)]
struct PostRowId {
    id: i64,
    path: String,
}

#[derive(Deserialize)]
struct PathRow {
    path: String,
}

#[derive(Deserialize)]
struct PostLocationRow {
    thread_public_id: String,
    thread_row_id: i64,
    post_path: String,
}

#[derive(Deserialize)]
struct RankRow {
    rank: i64,
}

/// One row of the post read path.
fn post_from_row(r: PostRow) -> StoreResult<Post> {
    // A path that fails validation would silently corrupt thread ordering.
    let path = Path::parse(&r.path).map_err(|e| {
        StoreError::Corrupt(format!("post {} has invalid path {:?}: {e}", r.id, r.path))
    })?;
    // A row that cannot round-trip its own id is corrupt.
    let public_id = PublicId::parse(&r.public_id).map_err(|e| {
        StoreError::Backend(format!("post {} has an unparseable public_id: {e}", r.id))
    })?;
    Ok(Post {
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
    })
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

/// Unknown states fail closed, so a typo in a migration cannot publish moderated content.
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

/// What D1 reported, emitted per request as `Server-Timing`.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryStats {
    pub statements: u32,
    /// `None` under local dev.
    pub rows_read: Option<usize>,
    /// `None` under local dev.
    pub duration_ms: Option<f64>,
}

impl QueryStats {
    /// `None` is absent information, not zero.
    pub fn plus(self, other: QueryStats) -> QueryStats {
        QueryStats {
            statements: self.statements + other.statements,
            rows_read: match (self.rows_read, other.rows_read) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            },
            duration_ms: match (self.duration_ms, other.duration_ms) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            },
        }
    }

    /// `Server-Timing` value.
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
    /// Interior mutability, so D1's telemetry stays out of the `Store` signatures.
    last_stats: Cell<QueryStats>,
}

impl D1Store {
    pub fn new(db: D1Database) -> Self {
        Self {
            db,
            last_stats: Cell::new(QueryStats::default()),
        }
    }

    /// The post has to be found before its rank can be counted, so this cannot be one batch.
    async fn fetch_post_location(
        &self,
        post: &PublicId,
        page_size: u32,
    ) -> StoreResult<PostLocation> {
        let stmt = self
            .db
            .prepare(sql::LOCATE_POST)
            .bind(&[post.as_str().into()])
            .map_err(backend)?;
        let res = stmt.all().await.map_err(backend)?;
        let mut stats = collect_stats(&[&res]);
        let rows: Vec<PostLocationRow> = res.results().map_err(backend)?;
        let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;

        let rank_res = self
            .db
            .prepare(sql::POST_RANK)
            .bind(&[num(row.thread_row_id), row.post_path.as_str().into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        stats = stats.plus(collect_stats(&[&rank_res]));
        let rank = rank_res
            .results::<RankRow>()
            .map_err(backend)?
            .into_iter()
            .next()
            .map(|r| r.rank)
            .unwrap_or(0);

        let cursor = match notespace_core::store::page_cursor_offset(rank, page_size) {
            None => None,
            Some(offset) => {
                let at = self
                    .db
                    .prepare(sql::PATH_AT_OFFSET)
                    .bind(&[num(row.thread_row_id), num(offset)])
                    .map_err(backend)?
                    .all()
                    .await
                    .map_err(backend)?;
                stats = stats.plus(collect_stats(&[&at]));
                at.results::<PathRow>()
                    .map_err(backend)?
                    .into_iter()
                    .next()
                    .map(|r| Path::parse(&r.path))
                    .transpose()
                    .map_err(|e| StoreError::Corrupt(format!("cursor path: {e}")))?
            }
        };
        self.last_stats.set(stats);

        Ok(PostLocation {
            thread: PublicId::parse(&row.thread_public_id).map_err(|e| {
                StoreError::Backend(format!("thread has an unparseable public_id: {e}"))
            })?,
            path: Path::parse(&row.post_path)
                .map_err(|e| StoreError::Backend(format!("post has an unparseable path: {e}")))?,
            cursor,
        })
    }

    /// D1 has no interactive transaction, so path allocation is a read then a write and
    /// `UNIQUE(thread_id, path)` is the real guard. The insert and the counter bump share a
    /// `batch()`, so a post cannot land without its thread being bumped.
    async fn append_post(&self, new: &NewPost) -> StoreResult<Post> {
        let thread_row = self
            .db
            .prepare(sql::THREAD_ROW)
            .bind(&[new.thread.as_str().into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        let threads: Vec<ThreadRowId> = thread_row.results().map_err(backend)?;
        let thread_id = threads.into_iter().next().ok_or(StoreError::NotFound)?.id;

        let parent = match &new.parent {
            None => None,
            Some(pid) => {
                let res = self
                    .db
                    .prepare(sql::POST_IN_THREAD)
                    .bind(&[pid.as_str().into(), num(thread_id)])
                    .map_err(backend)?
                    .all()
                    .await
                    .map_err(backend)?;
                let rows: Vec<PostRowId> = res.results().map_err(backend)?;
                let row = rows.into_iter().next().ok_or(StoreError::NotFound)?;
                Some((
                    row.id,
                    Path::parse(&row.path)
                        .map_err(|e| StoreError::Corrupt(format!("parent path: {e}")))?,
                ))
            }
        };

        let parent_path = parent.as_ref().map(|(_, p)| p.clone());
        let (lo, hi) = NextPath::search_bounds(parent_path.as_ref());
        let last_res = self
            .db
            .prepare(sql::LAST_PATH_IN_RANGE)
            .bind(&[num(thread_id), lo.into(), hi.into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        let last_rows: Vec<PathRow> = last_res.results().map_err(backend)?;
        let last = last_rows
            .into_iter()
            .next()
            .map(|r| Path::parse(&r.path))
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("sibling path: {e}")))?;
        let path = NextPath::allocate(parent_path.as_ref(), last.as_ref())?;
        let depth = path.depth() as i64;

        let insert = self
            .db
            .prepare(sql::INSERT_POST)
            .bind(&[
                new.public_id.as_str().into(),
                num(thread_id),
                match parent.as_ref() {
                    Some((id, _)) => num(*id),
                    None => worker::wasm_bindgen::JsValue::NULL,
                },
                path.as_str().into(),
                num(depth),
                num(new.author_id),
                new.body_md.as_str().into(),
                new.body_html.as_str().into(),
                num(new.created_at),
            ])
            .map_err(backend)?;
        let bump = self
            .db
            .prepare(sql::BUMP_THREAD)
            .bind(&[num(thread_id), num(new.created_at)])
            .map_err(backend)?;

        // One batch: the post and the counter move together or not at all.
        let row_id = match self.db.batch(vec![insert, bump]).await {
            Ok(results) => {
                // No `last_insert_rowid()` to call afterwards, so this has to come from D1's meta.
                results
                    .first()
                    .and_then(|r| r.meta().ok().flatten())
                    .and_then(|m| m.last_row_id)
                    .ok_or_else(|| StoreError::Backend("insert reported no row id".into()))?
            }
            Err(e) => {
                let msg = e.to_string();
                // D1 surfaces constraint failures as a message, not a code.
                if msg.contains("UNIQUE constraint failed") {
                    return Err(StoreError::Conflict);
                }
                return Err(StoreError::Backend(msg));
            }
        };

        Ok(Post {
            id: row_id,
            public_id: new.public_id.clone(),
            thread_id,
            parent_id: parent.map(|(id, _)| id),
            path,
            depth: depth as u32,
            author_id: new.author_id,
            author_name: String::new(),
            body_md: Some(new.body_md.clone()),
            body_html: new.body_html.as_str().to_string(),
            created_at: new.created_at,
            edited_at: None,
            score: 0.0,
            state: PostState::Visible,
        })
    }

    /// Run a statement that returns no rows, reporting how many it changed.
    async fn run(
        &self,
        stmt: &str,
        binds: Vec<worker::wasm_bindgen::JsValue>,
    ) -> StoreResult<Option<u32>> {
        let res = self
            .db
            .prepare(stmt)
            .bind(&binds)
            .map_err(backend)?
            .run()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        Ok(res
            .meta()
            .ok()
            .flatten()
            .and_then(|m| m.changes)
            .map(|c| c as u32))
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

        // Record before consuming the results; both fields are None under local dev.
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
            // Re-parsed rather than reusing the request's id, so a corrupt row says so.
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

        // The over-fetched row, if it arrived, means another page; the cursor is the last kept path.
        let has_more = rows.len() > page.limit as usize;
        if has_more {
            rows.truncate(page.limit as usize);
        }

        let mut posts = Vec::with_capacity(rows.len());
        for r in rows {
            posts.push(post_from_row(r)?);
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

    async fn locate_post(&self, post: &PublicId, page_size: u32) -> StoreResult<PostLocation> {
        self.fetch_post_location(post, page_size).await
    }

    async fn recent_threads(&self, limit: u32) -> StoreResult<Vec<ThreadSummary>> {
        let res = self
            .db
            .prepare(sql::RECENT_THREADS)
            .bind(&[num(limit as i64)])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<ThreadSummaryRow> = res.results().map_err(backend)?;
        rows.into_iter()
            .map(|r| {
                Ok(ThreadSummary {
                    public_id: PublicId::parse(&r.public_id).map_err(|e| {
                        StoreError::Corrupt(format!("thread public_id {:?}: {e}", r.public_id))
                    })?,
                    title: r.title,
                    post_count: r.post_count.max(0) as u32,
                    bumped_at: r.bumped_at,
                    author_name: r.author_name,
                    space_name: r.space_name,
                    space_path: r.space_path,
                })
            })
            .collect()
    }

    async fn thread_version(&self, thread: &PublicId) -> StoreResult<Option<i64>> {
        let res = self
            .db
            .prepare(sql::THREAD_VERSION)
            .bind(&[thread.as_str().into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<VersionRow> = res.results().map_err(backend)?;
        Ok(rows.into_iter().next().map(|r| r.cache_version))
    }

    async fn insert_post(&self, new: &NewPost) -> StoreResult<Post> {
        self.append_post(new).await
    }

    async fn user_by_name(&self, name: &str) -> StoreResult<Option<Credential>> {
        let res = self
            .db
            .prepare(sql::USER_BY_NAME)
            .bind(&[name.into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<CredentialRow> = res.results().map_err(backend)?;
        Ok(rows.into_iter().next().map(|r| Credential {
            user: User {
                id: r.id,
                name: r.name,
                state: r.state.parse().unwrap_or_default(),
            },
            password_hash: r.password_hash,
        }))
    }

    async fn create_user(
        &self,
        name: &str,
        created_at: Timestamp,
        password_hash: Option<&str>,
    ) -> StoreResult<UserId> {
        let res = self
            .db
            .prepare(sql::INSERT_USER)
            .bind(&[
                name.into(),
                num(created_at),
                match password_hash {
                    Some(h) => h.into(),
                    None => worker::wasm_bindgen::JsValue::NULL,
                },
            ])
            .map_err(backend)?
            .run()
            .await
            .map_err(|e| {
                // Registration's check and this insert are not atomic, so the index is the guard.
                let msg = e.to_string();
                if msg.contains("UNIQUE constraint failed") {
                    StoreError::Conflict
                } else {
                    StoreError::Backend(msg)
                }
            })?;
        self.last_stats.set(collect_stats(&[&res]));
        res.meta()
            .ok()
            .flatten()
            .and_then(|m| m.last_row_id)
            .ok_or_else(|| StoreError::Backend("insert reported no row id".into()))
    }

    async fn set_password_hash(&self, user: UserId, hash: &str) -> StoreResult<()> {
        self.run(sql::SET_PASSWORD_HASH, vec![num(user), hash.into()])
            .await
            .map(|_| ())
    }

    async fn login_attempts(
        &self,
        keys: &AttemptKeys,
    ) -> StoreResult<(Option<Attempts>, Option<Attempts>)> {
        let res = self
            .db
            .prepare(sql::LOGIN_ATTEMPTS)
            .bind(&[keys.identity.as_str().into(), keys.client.as_str().into()])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<AttemptRow> = res.results().map_err(backend)?;
        let (mut identity, mut client) = (None, None);
        for r in rows {
            let a = Attempts {
                window_start: r.window_start,
                count: r.count as u32,
            };
            if r.key == keys.identity {
                identity = Some(a);
            } else if r.key == keys.client {
                client = Some(a);
            }
        }
        Ok((identity, client))
    }

    async fn record_login_attempt(&self, key: &str, attempts: Attempts) -> StoreResult<()> {
        self.run(
            sql::RECORD_LOGIN_ATTEMPT,
            vec![
                key.into(),
                num(attempts.window_start),
                num(attempts.count as i64),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn clear_login_attempts(&self, key: &str) -> StoreResult<()> {
        self.run(sql::CLEAR_LOGIN_ATTEMPTS, vec![key.into()])
            .await
            .map(|_| ())
    }

    async fn sweep_login_attempts(&self, cutoff: Timestamp) -> StoreResult<u32> {
        Ok(self
            .run(sql::SWEEP_LOGIN_ATTEMPTS, vec![num(cutoff)])
            .await?
            .unwrap_or(0))
    }

    async fn create_session(&self, session: &Session) -> StoreResult<()> {
        self.run(
            sql::INSERT_SESSION,
            vec![
                session.token_hash.as_str().into(),
                num(session.user_id),
                num(session.created_at),
                num(session.refreshed_at),
                num(session.expires_at),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn lookup_session(
        &self,
        token: &TokenHash,
        now: Timestamp,
    ) -> StoreResult<Option<Authenticated>> {
        let res = self
            .db
            .prepare(sql::LOOKUP_SESSION)
            .bind(&[token.as_str().into(), num(now)])
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        let rows: Vec<SessionRow> = res.results().map_err(backend)?;
        Ok(rows.into_iter().next().map(|r| Authenticated {
            session: Session {
                token_hash: token.clone(),
                user_id: r.user_id,
                created_at: r.created_at,
                refreshed_at: r.refreshed_at,
                expires_at: r.expires_at,
            },
            user: User {
                id: r.user_id,
                name: r.user_name,
                state: r.user_state.parse().unwrap_or_default(),
            },
        }))
    }

    async fn refresh_session(
        &self,
        token: &TokenHash,
        refreshed_at: Timestamp,
        expires_at: Timestamp,
    ) -> StoreResult<()> {
        self.run(
            sql::REFRESH_SESSION,
            vec![token.as_str().into(), num(refreshed_at), num(expires_at)],
        )
        .await
        .map(|_| ())
    }

    async fn delete_session(&self, token: &TokenHash) -> StoreResult<()> {
        self.run(sql::DELETE_SESSION, vec![token.as_str().into()])
            .await
            .map(|_| ())
    }

    async fn delete_user_sessions(&self, user: UserId) -> StoreResult<u32> {
        let meta = self.run(sql::DELETE_USER_SESSIONS, vec![num(user)]).await?;
        Ok(meta.unwrap_or(0))
    }
}
