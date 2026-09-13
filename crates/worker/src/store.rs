//! D1 implementation of [`Store`]. A thread page is two statements in one `batch()`: metadata
//! plus one indexed range scan over `(thread_id, path)`.

use core::cell::Cell;
use notespace_core::email::{ConsumedToken, EmailAddress, StoredToken, TokenKind};
use notespace_core::id::PublicId;
use notespace_core::model::*;
use notespace_core::moderation::{
    classify::Call, ActorKind, AgreementStats, Category, LogDetail, LogEntry, NewAction, NewReview,
    NewSignal, ReportTally, Resolution, ReviewItem, ReviewPost, ReviewReason, WriteContext,
};
use notespace_core::path::Path;
use notespace_core::ratelimit::{AttemptKeys, Attempts};
use notespace_core::session::{Session, TokenHash};
use notespace_core::space_key::SpacePath;
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
    space_config: String,
}

fn thread_from_row(tr: ThreadRow) -> StoreResult<(Space, Thread)> {
    let space = Space {
        id: tr.space_id,
        path: tr.space_path,
        name: tr.space_name,
        parent_id: None,
        ranking: ranking(&tr.space_ranking),
        depth_cap: tr.space_depth_cap.clamp(0, i64::from(u32::MAX)) as u32,
        config: tr.space_config,
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
    Ok((space, t))
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
struct SpaceRow {
    id: i64,
    path: String,
    name: String,
    parent_id: Option<i64>,
    ranking: String,
    depth_cap: i64,
    config: String,
}

#[derive(Deserialize)]
struct PlainUserRow {
    id: i64,
    name: String,
    state: String,
    role: String,
}

#[derive(Deserialize)]
struct ProfileHeadRow {
    id: i64,
    name: String,
    state: String,
    role: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct ProfilePostRow {
    public_id: String,
    thread_public_id: String,
    thread_title: String,
    body_html: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct AccountRow {
    email: Option<String>,
    email_verified_at: Option<i64>,
    has_password: i64,
}

#[derive(Deserialize)]
struct UserListRow {
    id: i64,
    name: String,
    state: String,
    role: String,
    created_at: i64,
    email: Option<String>,
    email_verified: i64,
    post_count: i64,
}

#[derive(Deserialize)]
struct SpaceDetailRow {
    id: i64,
    path: String,
    name: String,
    parent_id: Option<i64>,
    ranking: String,
    depth_cap: i64,
    config: String,
    thread_count: i64,
}

#[derive(Deserialize)]
struct StatsRow {
    users: i64,
    threads: i64,
    posts: i64,
    pending_posts: i64,
    open_reviews: i64,
    banned_users: i64,
}

#[derive(Deserialize)]
struct FullLogRow {
    id: i64,
    actor_kind: String,
    actor_name: String,
    target_kind: String,
    target_public_id: Option<String>,
    action: String,
    created_at: i64,
    public: i64,
    detail: String,
}

#[derive(Deserialize)]
struct NameRow {
    name: String,
}

#[derive(Deserialize)]
struct ConsumedRow {
    user_id: i64,
    email: String,
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
    role: String,
    password_hash: Option<String>,
}

#[derive(Deserialize)]
struct WriteContextRow {
    space_id: i64,
    space_config: String,
    thread_state: String,
    author_created_at: i64,
    author_role: String,
}

#[derive(Deserialize)]
struct ReviewPostRow {
    id: i64,
    public_id: String,
    thread_public_id: String,
    thread_title: String,
    space_id: i64,
    space_name: String,
    space_config: String,
    author_id: i64,
    author_name: String,
    author_created_at: i64,
    body_md: String,
    created_at: i64,
    state: String,
}

#[derive(Deserialize)]
struct ReviewRow {
    id: i64,
    post_id: i64,
    post_public_id: String,
    thread_public_id: String,
    thread_title: String,
    space_id: i64,
    space_name: String,
    author_id: i64,
    author_name: String,
    body_html: String,
    post_state: String,
    reason: String,
    model_verdict: Option<String>,
    model_confidence: Option<f64>,
    model_categories: Option<String>,
    appeal_text: Option<String>,
    opened_at: i64,
    state: String,
}

#[derive(Deserialize)]
struct CountRow {
    n: i64,
}

#[derive(Deserialize)]
struct PublicIdRow {
    public_id: String,
}

#[derive(Deserialize)]
struct LogRow {
    id: i64,
    actor_kind: String,
    actor_name: String,
    target_kind: String,
    target_public_id: Option<String>,
    action: String,
    created_at: i64,
}

#[derive(Deserialize)]
struct AgreementRow {
    agreed: Option<f64>,
    disagreed: Option<f64>,
}

#[derive(Deserialize)]
struct FoundRow {
    #[allow(dead_code)]
    found: i64,
}

fn parse_id(raw: &str, what: &str) -> StoreResult<PublicId> {
    PublicId::parse(raw).map_err(|e| StoreError::Corrupt(format!("{what} {raw:?}: {e}")))
}

fn summary_from_row(r: ThreadSummaryRow) -> StoreResult<ThreadSummary> {
    Ok(ThreadSummary {
        public_id: parse_id(&r.public_id, "thread public_id")?,
        title: r.title,
        post_count: r.post_count.max(0) as u32,
        bumped_at: r.bumped_at,
        author_name: r.author_name,
        space_name: r.space_name,
        space_path: r.space_path,
    })
}

fn space_from_row(r: SpaceRow) -> Space {
    Space {
        id: r.id,
        path: r.path,
        name: r.name,
        parent_id: r.parent_id,
        ranking: ranking(&r.ranking),
        depth_cap: r.depth_cap.clamp(0, i64::from(u32::MAX)) as u32,
        config: r.config,
    }
}

fn user_from_row(r: PlainUserRow) -> User {
    User {
        id: r.id,
        name: r.name,
        state: r.state.parse().unwrap_or_default(),
        role: r.role.parse().unwrap_or_default(),
    }
}

fn write_context_from_row(r: WriteContextRow) -> WriteContext {
    WriteContext {
        space_id: r.space_id,
        space_config: r.space_config,
        thread_state: thread_state(&r.thread_state),
        author_created_at: r.author_created_at,
        author_role: r.author_role.parse().unwrap_or_default(),
    }
}

/// `Conflict` for a unique-index violation, which D1 reports as a message rather than a code.
fn write_error(e: impl std::fmt::Display) -> StoreError {
    let msg = e.to_string();
    if msg.contains("UNIQUE constraint failed") {
        StoreError::Conflict
    } else {
        StoreError::Backend(msg)
    }
}

fn user_row_from_row(r: UserListRow) -> UserRow {
    UserRow {
        user: User {
            id: r.id,
            name: r.name,
            state: r.state.parse().unwrap_or_default(),
            role: r.role.parse().unwrap_or_default(),
        },
        created_at: r.created_at,
        email: r.email,
        email_verified: r.email_verified != 0,
        post_count: r.post_count.max(0) as u32,
    }
}

fn space_detail_from_row(r: SpaceDetailRow) -> SpaceDetail {
    SpaceDetail {
        space: Space {
            id: r.id,
            path: r.path,
            name: r.name,
            parent_id: r.parent_id,
            ranking: ranking(&r.ranking),
            depth_cap: r.depth_cap.clamp(0, i64::from(u32::MAX)) as u32,
            config: r.config,
        },
        thread_count: r.thread_count.max(0) as u32,
    }
}

fn opt_str(v: Option<&str>) -> worker::wasm_bindgen::JsValue {
    match v {
        Some(s) => s.into(),
        None => worker::wasm_bindgen::JsValue::NULL,
    }
}

fn review_from_row(r: ReviewRow) -> StoreResult<ReviewItem> {
    let categories = r
        .model_categories
        .as_deref()
        .and_then(|c| serde_json::from_str::<Vec<String>>(c).ok())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| Category::parse(c))
        .collect();
    Ok(ReviewItem {
        id: r.id,
        post_id: r.post_id,
        post_public_id: parse_id(&r.post_public_id, "post public_id")?,
        thread_public_id: parse_id(&r.thread_public_id, "thread public_id")?,
        thread_title: r.thread_title,
        space_id: r.space_id,
        space_name: r.space_name,
        author_id: r.author_id,
        author_name: r.author_name,
        body_html: r.body_html,
        post_state: post_state(&r.post_state),
        reason: ReviewReason::parse(&r.reason),
        model_verdict: r.model_verdict.as_deref().and_then(Call::parse),
        model_confidence: r.model_confidence,
        model_categories: categories,
        appeal_text: r.appeal_text,
        opened_at: r.opened_at,
        resolved: r.state == "resolved",
    })
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
    user_role: String,
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
struct PostWithSourceRow {
    #[serde(flatten)]
    post: PostRow,
    body_md: String,
    thread_public_id: String,
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
                new.state.as_str().into(),
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
            state: new.state,
        })
    }

    /// One statement returning rows, with stats recorded.
    async fn query<T: serde::de::DeserializeOwned>(
        &self,
        stmt: &str,
        binds: Vec<worker::wasm_bindgen::JsValue>,
    ) -> StoreResult<Vec<T>> {
        let res = self
            .db
            .prepare(stmt)
            .bind(&binds)
            .map_err(backend)?
            .all()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        res.results().map_err(backend)
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
        let (space, t) = thread_from_row(tr)?;

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
        let rows: Vec<ThreadSummaryRow> = self
            .query(sql::RECENT_THREADS, vec![num(limit as i64)])
            .await?;
        rows.into_iter().map(summary_from_row).collect()
    }

    async fn space_threads(
        &self,
        space: &SpacePath,
        limit: u32,
    ) -> StoreResult<Vec<ThreadSummary>> {
        let (lo, hi) = space.subtree_range();
        let rows: Vec<ThreadSummaryRow> = self
            .query(
                sql::SPACE_THREADS,
                vec![lo.as_str().into(), hi.as_str().into(), num(limit as i64)],
            )
            .await?;
        rows.into_iter().map(summary_from_row).collect()
    }

    async fn space_by_path(&self, path: &SpacePath) -> StoreResult<Option<Space>> {
        let rows: Vec<SpaceRow> = self
            .query(sql::SPACE_BY_PATH, vec![path.as_stored().into()])
            .await?;
        Ok(rows.into_iter().next().map(space_from_row))
    }

    async fn spaces_under(&self, parent: Option<SpaceId>) -> StoreResult<Vec<Space>> {
        let bind = match parent {
            Some(id) => num(id),
            None => worker::wasm_bindgen::JsValue::NULL,
        };
        let rows: Vec<SpaceRow> = self.query(sql::SPACES_UNDER, vec![bind]).await?;
        Ok(rows.into_iter().map(space_from_row).collect())
    }

    async fn space_context(&self, space: SpaceId, author: UserId) -> StoreResult<WriteContext> {
        let rows: Vec<WriteContextRow> = self
            .query(sql::SPACE_CONTEXT, vec![num(space), num(author)])
            .await?;
        rows.into_iter()
            .next()
            .map(write_context_from_row)
            .ok_or(StoreError::NotFound)
    }

    async fn create_thread(&self, t: &NewThread) -> StoreResult<Thread> {
        let res = self
            .db
            .prepare(sql::INSERT_THREAD)
            .bind(&[
                t.public_id.as_str().into(),
                num(t.space_id),
                t.space_path.as_str().into(),
                t.kind.as_str().into(),
                t.title.as_str().into(),
                opt_str(t.url.as_deref()),
                num(t.author_id),
                num(t.created_at),
            ])
            .map_err(backend)?
            .run()
            .await
            .map_err(write_error)?;
        self.last_stats.set(collect_stats(&[&res]));
        let id = res
            .meta()
            .ok()
            .flatten()
            .and_then(|m| m.last_row_id)
            .ok_or_else(|| StoreError::Backend("insert reported no row id".into()))?;
        Ok(Thread {
            id,
            public_id: t.public_id.clone(),
            space_id: t.space_id,
            kind: t.kind,
            title: t.title.clone(),
            url: t.url.clone(),
            author_id: t.author_id,
            author_name: String::new(),
            created_at: t.created_at,
            bumped_at: t.created_at,
            post_count: 0,
            state: ThreadState::Visible,
            cache_version: 0,
        })
    }

    async fn update_post_body(
        &self,
        post: &PublicId,
        body_md: &str,
        body_html: &SanitizedHtml,
        edited_at: Timestamp,
    ) -> StoreResult<()> {
        let update = self
            .db
            .prepare(sql::UPDATE_POST_BODY)
            .bind(&[
                post.as_str().into(),
                body_md.into(),
                body_html.as_str().into(),
                num(edited_at),
            ])
            .map_err(backend)?;
        let bump = self
            .db
            .prepare(sql::BUMP_THREAD_FOR_POST)
            .bind(&[post.as_str().into()])
            .map_err(backend)?;
        // One batch: the body and the page version move together or not at all.
        let results = self.db.batch(vec![update, bump]).await.map_err(backend)?;
        let refs: Vec<&D1Result> = results.iter().collect();
        self.last_stats.set(collect_stats(&refs));
        let changed = results
            .first()
            .and_then(|r| r.meta().ok().flatten())
            .and_then(|m| m.changes)
            .unwrap_or(0);
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn user_profile(&self, name: &str, limit: u32) -> StoreResult<Option<Profile>> {
        let heads: Vec<ProfileHeadRow> = self.query(sql::USER_PROFILE, vec![name.into()]).await?;
        let Some(head) = heads.into_iter().next() else {
            return Ok(None);
        };
        let stats = self.last_stats();
        let rows: Vec<ProfilePostRow> = self
            .query(
                sql::USER_RECENT_POSTS,
                vec![num(head.id), num(limit as i64)],
            )
            .await?;
        self.last_stats.set(stats.plus(self.last_stats()));
        let mut posts = Vec::with_capacity(rows.len());
        for r in rows {
            posts.push(ProfilePost {
                public_id: parse_id(&r.public_id, "post public_id")?,
                thread_public_id: parse_id(&r.thread_public_id, "thread public_id")?,
                thread_title: r.thread_title,
                body_html: r.body_html,
                created_at: r.created_at,
            });
        }
        Ok(Some(Profile {
            user: User {
                id: head.id,
                name: head.name,
                state: head.state.parse().unwrap_or_default(),
                role: head.role.parse().unwrap_or_default(),
            },
            created_at: head.created_at,
            posts,
        }))
    }

    // -- Email ----------------------------------------------------------------

    async fn account(&self, user: UserId) -> StoreResult<Option<Account>> {
        let rows: Vec<AccountRow> = self.query(sql::ACCOUNT, vec![num(user)]).await?;
        Ok(rows.into_iter().next().map(|r| Account {
            email: r.email,
            email_verified_at: r.email_verified_at,
            has_password: r.has_password != 0,
        }))
    }

    async fn user_by_verified_email(&self, email: &str) -> StoreResult<Option<User>> {
        let rows: Vec<PlainUserRow> = self
            .query(sql::USER_BY_VERIFIED_EMAIL, vec![email.into()])
            .await?;
        Ok(rows.into_iter().next().map(user_from_row))
    }

    async fn set_email(&self, user: UserId, email: Option<&str>) -> StoreResult<()> {
        self.run(sql::SET_EMAIL, vec![num(user), opt_str(email)])
            .await
            .map(|_| ())
    }

    async fn mark_email_verified(
        &self,
        user: UserId,
        email: &str,
        now: Timestamp,
    ) -> StoreResult<bool> {
        let res = self
            .db
            .prepare(sql::MARK_EMAIL_VERIFIED)
            .bind(&[num(user), email.into(), num(now)])
            .map_err(backend)?
            .run()
            .await
            .map_err(write_error)?;
        self.last_stats.set(collect_stats(&[&res]));
        Ok(res
            .meta()
            .ok()
            .flatten()
            .and_then(|m| m.changes)
            .unwrap_or(0)
            > 0)
    }

    async fn create_email_token(&self, t: &StoredToken) -> StoreResult<()> {
        self.run(
            sql::INSERT_EMAIL_TOKEN,
            vec![
                t.token_hash.as_str().into(),
                num(t.user_id),
                t.kind.as_str().into(),
                t.email.as_str().into(),
                num(t.created_at),
                num(t.expires_at),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn consume_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<ConsumedToken>> {
        // `RETURNING` makes the update its own read.
        let rows: Vec<ConsumedRow> = self
            .query(
                sql::CONSUME_EMAIL_TOKEN,
                vec![token_hash.into(), kind.as_str().into(), num(now)],
            )
            .await?;
        rows.into_iter()
            .next()
            .map(|r| {
                Ok(ConsumedToken {
                    user_id: r.user_id,
                    email: EmailAddress::parse(&r.email)
                        .map_err(|e| StoreError::Corrupt(format!("token email: {e}")))?,
                })
            })
            .transpose()
    }

    async fn peek_email_token(
        &self,
        token_hash: &str,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<Option<String>> {
        let rows: Vec<NameRow> = self
            .query(
                sql::PEEK_EMAIL_TOKEN,
                vec![token_hash.into(), kind.as_str().into(), num(now)],
            )
            .await?;
        Ok(rows.into_iter().next().map(|r| r.name))
    }

    // -- External identities ---------------------------------------------------

    async fn identity_user(&self, provider: &str, subject: &str) -> StoreResult<Option<User>> {
        let rows: Vec<PlainUserRow> = self
            .query(sql::IDENTITY_USER, vec![provider.into(), subject.into()])
            .await?;
        Ok(rows.into_iter().next().map(user_from_row))
    }

    async fn link_identity(
        &self,
        provider: &str,
        subject: &str,
        user: UserId,
        email: Option<&str>,
        now: Timestamp,
    ) -> StoreResult<()> {
        let res = self
            .db
            .prepare(sql::INSERT_IDENTITY)
            .bind(&[
                provider.into(),
                subject.into(),
                num(user),
                opt_str(email),
                num(now),
            ])
            .map_err(backend)?
            .run()
            .await
            .map_err(write_error)?;
        self.last_stats.set(collect_stats(&[&res]));
        Ok(())
    }

    async fn touch_identity(
        &self,
        provider: &str,
        subject: &str,
        now: Timestamp,
    ) -> StoreResult<()> {
        self.run(
            sql::TOUCH_IDENTITY,
            vec![provider.into(), subject.into(), num(now)],
        )
        .await
        .map(|_| ())
    }

    async fn user_identities(&self, user: UserId) -> StoreResult<Vec<String>> {
        #[derive(Deserialize)]
        struct Row {
            provider: String,
        }
        let rows: Vec<Row> = self.query(sql::USER_IDENTITIES, vec![num(user)]).await?;
        Ok(rows.into_iter().map(|r| r.provider).collect())
    }

    // -- Administration -------------------------------------------------------

    async fn thread_head(&self, thread: &PublicId) -> StoreResult<(Space, Thread)> {
        let rows: Vec<ThreadRow> = self
            .query(sql::THREAD_HEAD, vec![thread.as_str().into()])
            .await?;
        rows.into_iter()
            .next()
            .ok_or(StoreError::NotFound)
            .and_then(thread_from_row)
    }

    async fn update_thread(&self, thread: &PublicId, e: &ThreadEdit) -> StoreResult<()> {
        let changed = self
            .run(
                sql::UPDATE_THREAD,
                vec![
                    thread.as_str().into(),
                    e.title.as_str().into(),
                    opt_str(e.url.as_deref()),
                    e.state.as_str().into(),
                    num(e.space_id),
                    e.space_path.as_str().into(),
                ],
            )
            .await?
            .unwrap_or(0);
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn set_user_role(&self, user: UserId, role: Role) -> StoreResult<()> {
        self.run(sql::SET_USER_ROLE, vec![num(user), role.as_str().into()])
            .await
            .map(|_| ())
    }

    async fn set_user_state(&self, user: UserId, state: UserState) -> StoreResult<()> {
        self.run(sql::SET_USER_STATE, vec![num(user), state.as_str().into()])
            .await
            .map(|_| ())
    }

    async fn list_users(&self, prefix: &str, limit: u32) -> StoreResult<Vec<UserRow>> {
        let rows: Vec<UserListRow> = self
            .query(sql::LIST_USERS, vec![prefix.into(), num(limit as i64)])
            .await?;
        Ok(rows.into_iter().map(user_row_from_row).collect())
    }

    async fn user_row(&self, name: &str) -> StoreResult<Option<UserRow>> {
        let rows: Vec<UserListRow> = self.query(sql::USER_ROW, vec![name.into()]).await?;
        Ok(rows.into_iter().next().map(user_row_from_row))
    }

    async fn space_detail(&self, space: SpaceId) -> StoreResult<Option<SpaceDetail>> {
        let rows: Vec<SpaceDetailRow> = self.query(sql::SPACE_DETAIL, vec![num(space)]).await?;
        Ok(rows.into_iter().next().map(space_detail_from_row))
    }

    async fn all_spaces(&self) -> StoreResult<Vec<SpaceDetail>> {
        let rows: Vec<SpaceDetailRow> = self.query(sql::ALL_SPACES, vec![]).await?;
        Ok(rows.into_iter().map(space_detail_from_row).collect())
    }

    async fn create_space(&self, s: &NewSpace) -> StoreResult<SpaceId> {
        let res = self
            .db
            .prepare(sql::INSERT_SPACE)
            .bind(&[
                s.name.as_str().into(),
                s.path.as_str().into(),
                match s.parent_id {
                    Some(id) => num(id),
                    None => worker::wasm_bindgen::JsValue::NULL,
                },
                s.ranking.as_str().into(),
                num(s.depth_cap as i64),
                s.config.as_str().into(),
            ])
            .map_err(backend)?
            .run()
            .await
            .map_err(write_error)?;
        self.last_stats.set(collect_stats(&[&res]));
        res.meta()
            .ok()
            .flatten()
            .and_then(|m| m.last_row_id)
            .ok_or_else(|| StoreError::Backend("insert reported no row id".into()))
    }

    async fn update_space(
        &self,
        space: SpaceId,
        name: &str,
        ranking: Ranking,
        depth_cap: u32,
        config: &str,
    ) -> StoreResult<()> {
        self.run(
            sql::UPDATE_SPACE,
            vec![
                num(space),
                name.into(),
                ranking.as_str().into(),
                num(depth_cap as i64),
                config.into(),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn site_stats(&self) -> StoreResult<SiteStats> {
        let rows: Vec<StatsRow> = self.query(sql::SITE_STATS, vec![]).await?;
        let r = rows.into_iter().next().ok_or(StoreError::NotFound)?;
        let n = |v: i64| v.max(0) as u32;
        Ok(SiteStats {
            users: n(r.users),
            threads: n(r.threads),
            posts: n(r.posts),
            pending_posts: n(r.pending_posts),
            open_reviews: n(r.open_reviews),
            banned_users: n(r.banned_users),
        })
    }

    async fn full_log(&self, limit: u32) -> StoreResult<Vec<LogDetail>> {
        let rows: Vec<FullLogRow> = self.query(sql::FULL_LOG, vec![num(limit as i64)]).await?;
        Ok(rows
            .into_iter()
            .map(|r| LogDetail {
                entry: LogEntry {
                    id: r.id,
                    actor_kind: ActorKind::parse(&r.actor_kind),
                    actor_name: r.actor_name,
                    target_kind: r.target_kind,
                    target_public_id: r
                        .target_public_id
                        .as_deref()
                        .and_then(|t| PublicId::parse(t).ok()),
                    action: r.action,
                    created_at: r.created_at,
                },
                public: r.public != 0,
                detail: r.detail,
            })
            .collect())
    }

    async fn retire_email_tokens(
        &self,
        user: UserId,
        kind: TokenKind,
        now: Timestamp,
    ) -> StoreResult<u32> {
        Ok(self
            .run(
                sql::RETIRE_EMAIL_TOKENS,
                vec![num(user), kind.as_str().into(), num(now)],
            )
            .await?
            .unwrap_or(0))
    }

    async fn post_by_id(&self, post: &PublicId) -> StoreResult<(Post, PublicId)> {
        let rows: Vec<PostWithSourceRow> = self
            .query(sql::POST_BY_ID, vec![post.as_str().into()])
            .await?;
        let r = rows.into_iter().next().ok_or(StoreError::NotFound)?;
        let mut p = post_from_row(r.post)?;
        p.body_md = Some(r.body_md);
        Ok((p, parse_id(&r.thread_public_id, "thread public_id")?))
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
                role: r.role.parse().unwrap_or_default(),
            },
            password_hash: r.password_hash,
        }))
    }

    async fn user_by_id(&self, id: UserId) -> StoreResult<Option<User>> {
        let rows: Vec<PlainUserRow> = self.query(sql::USER_BY_ID, vec![num(id)]).await?;
        Ok(rows.into_iter().next().map(user_from_row))
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
                role: r.user_role.parse().unwrap_or_default(),
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

    // -- Moderation ---------------------------------------------------------

    async fn write_context(&self, thread: &PublicId, author: UserId) -> StoreResult<WriteContext> {
        let rows: Vec<WriteContextRow> = self
            .query(
                sql::WRITE_CONTEXT,
                vec![thread.as_str().into(), num(author)],
            )
            .await?;
        rows.into_iter()
            .next()
            .map(write_context_from_row)
            .ok_or(StoreError::NotFound)
    }

    async fn author_posted_recently(
        &self,
        author: UserId,
        body_md: &str,
        since: Timestamp,
    ) -> StoreResult<bool> {
        let rows: Vec<FoundRow> = self
            .query(
                sql::AUTHOR_POSTED_RECENTLY,
                vec![num(author), body_md.into(), num(since)],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn post_for_review(&self, post: &PublicId) -> StoreResult<ReviewPost> {
        let rows: Vec<ReviewPostRow> = self
            .query(sql::POST_FOR_REVIEW, vec![post.as_str().into()])
            .await?;
        let r = rows.into_iter().next().ok_or(StoreError::NotFound)?;
        Ok(ReviewPost {
            id: r.id,
            public_id: parse_id(&r.public_id, "post public_id")?,
            thread_public_id: parse_id(&r.thread_public_id, "thread public_id")?,
            thread_title: r.thread_title,
            space_id: r.space_id,
            space_name: r.space_name,
            space_config: r.space_config,
            author_id: r.author_id,
            author_name: r.author_name,
            author_created_at: r.author_created_at,
            body_md: r.body_md,
            created_at: r.created_at,
            state: post_state(&r.state),
        })
    }

    async fn set_post_state(
        &self,
        post: &PublicId,
        state: PostState,
        _now: Timestamp,
    ) -> StoreResult<()> {
        let set = self
            .db
            .prepare(sql::SET_POST_STATE)
            .bind(&[post.as_str().into(), state.as_str().into()])
            .map_err(backend)?;
        let bump = self
            .db
            .prepare(sql::BUMP_THREAD_FOR_POST)
            .bind(&[post.as_str().into()])
            .map_err(backend)?;
        // One batch: the state and the page version move together or not at all.
        let results = self.db.batch(vec![set, bump]).await.map_err(backend)?;
        let refs: Vec<&D1Result> = results.iter().collect();
        self.last_stats.set(collect_stats(&refs));
        let changed = results
            .first()
            .and_then(|r| r.meta().ok().flatten())
            .and_then(|m| m.changes)
            .unwrap_or(0);
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn log_action(&self, a: &NewAction) -> StoreResult<i64> {
        let res = self
            .db
            .prepare(sql::INSERT_ACTION)
            .bind(&[
                a.actor_kind.as_str().into(),
                match a.actor_id {
                    Some(id) => num(id),
                    None => worker::wasm_bindgen::JsValue::NULL,
                },
                a.actor_name.as_str().into(),
                a.target_kind.into(),
                num(a.target_id),
                a.action.into(),
                a.detail.to_string().into(),
                num(a.public as i64),
                num(a.created_at),
            ])
            .map_err(backend)?
            .run()
            .await
            .map_err(backend)?;
        self.last_stats.set(collect_stats(&[&res]));
        res.meta()
            .ok()
            .flatten()
            .and_then(|m| m.last_row_id)
            .ok_or_else(|| StoreError::Backend("log insert reported no row id".into()))
    }

    async fn open_review(&self, review: &NewReview) -> StoreResult<()> {
        let null = worker::wasm_bindgen::JsValue::NULL;
        let (verdict, confidence, categories) = match &review.verdict {
            Some(v) => (
                v.call.as_str().into(),
                worker::wasm_bindgen::JsValue::from_f64(v.confidence),
                serde_json::to_string(&v.categories)
                    .map_err(backend)?
                    .into(),
            ),
            None => (null.clone(), null.clone(), null.clone()),
        };
        self.run(
            sql::UPSERT_REVIEW,
            vec![
                num(review.post_id),
                num(review.space_id),
                review.reason.as_str().into(),
                verdict,
                confidence,
                categories,
                match &review.appeal_text {
                    Some(t) => t.as_str().into(),
                    None => null,
                },
                num(review.opened_at),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn resolve_review(
        &self,
        id: i64,
        resolution: Resolution,
        by: UserId,
        now: Timestamp,
    ) -> StoreResult<Option<ReviewItem>> {
        let rows: Vec<ReviewRow> = self.query(sql::REVIEW_BY_ID, vec![num(id)]).await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let item = review_from_row(row)?;
        if item.resolved {
            return Ok(None);
        }
        // No interactive transaction on D1; the `state = 'open'` guard in the UPDATE is what
        // makes two simultaneous resolutions produce one.
        let changed = self
            .run(
                sql::RESOLVE_REVIEW,
                vec![num(id), resolution.as_str().into(), num(by), num(now)],
            )
            .await?
            .unwrap_or(0);
        Ok((changed > 0).then_some(item))
    }

    async fn open_reviews(&self, limit: u32) -> StoreResult<Vec<ReviewItem>> {
        let rows: Vec<ReviewRow> = self
            .query(sql::OPEN_REVIEWS, vec![num(limit as i64)])
            .await?;
        rows.into_iter().map(review_from_row).collect()
    }

    async fn add_report(&self, sig: &NewSignal) -> StoreResult<ReportTally> {
        let added = self
            .run(
                sql::INSERT_SIGNAL,
                vec![
                    num(sig.post_id),
                    num(sig.user_id),
                    sig.kind.into(),
                    worker::wasm_bindgen::JsValue::from_f64(sig.weight),
                    match &sig.reason {
                        Some(r) => r.as_str().into(),
                        None => worker::wasm_bindgen::JsValue::NULL,
                    },
                    num(sig.created_at),
                ],
            )
            .await?
            .unwrap_or(0)
            > 0;
        let rows: Vec<CountRow> = self
            .query(sql::COUNT_SIGNALS, vec![num(sig.post_id), sig.kind.into()])
            .await?;
        let count = rows.first().map(|r| r.n).unwrap_or(0);
        Ok(ReportTally {
            added,
            count: count.max(0) as u32,
        })
    }

    async fn pending_posts(&self, older_than: Timestamp, limit: u32) -> StoreResult<Vec<PublicId>> {
        let rows: Vec<PublicIdRow> = self
            .query(sql::PENDING_POSTS, vec![num(older_than), num(limit as i64)])
            .await?;
        rows.iter()
            .map(|r| parse_id(&r.public_id, "pending post"))
            .collect()
    }

    async fn public_log(&self, limit: u32) -> StoreResult<Vec<LogEntry>> {
        let rows: Vec<LogRow> = self.query(sql::PUBLIC_LOG, vec![num(limit as i64)]).await?;
        Ok(rows
            .into_iter()
            .map(|r| LogEntry {
                id: r.id,
                actor_kind: ActorKind::parse(&r.actor_kind),
                actor_name: r.actor_name,
                target_kind: r.target_kind,
                target_public_id: r
                    .target_public_id
                    .as_deref()
                    .and_then(|t| PublicId::parse(t).ok()),
                action: r.action,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn agreement(&self, space: SpaceId) -> StoreResult<AgreementStats> {
        let rows: Vec<AgreementRow> = self.query(sql::AGREEMENT, vec![num(space)]).await?;
        let r = rows.into_iter().next();
        Ok(AgreementStats {
            agreed: r.as_ref().and_then(|r| r.agreed).unwrap_or(0.0).max(0.0) as u32,
            disagreed: r.as_ref().and_then(|r| r.disagreed).unwrap_or(0.0).max(0.0) as u32,
        })
    }
}
