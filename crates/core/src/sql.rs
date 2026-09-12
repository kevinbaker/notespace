//! SQL shared verbatim by every [`Store`](crate::store::Store) implementation.
//!
//! Plain SQLite only: no Postgres-isms, nothing that needs a D1 extension.

/// Binds: `?1` = thread public id.
pub const THREAD: &str = "\
SELECT t.id, t.public_id, t.space_id, t.kind, t.title, t.url, t.author_id, u.name AS author_name, \
t.created_at, t.bumped_at, t.post_count, t.state, t.cache_version, \
s.path AS space_path, s.name AS space_name, s.ranking AS space_ranking, \
s.depth_cap AS space_depth_cap \
FROM thread t \
JOIN user u ON u.id = t.author_id \
JOIN space s ON s.id = t.space_id \
WHERE t.public_id = ?1";

/// Read on every pageview to build the cache key; one row.
///
/// Binds: `?1` = thread public id.
pub const THREAD_VERSION: &str = "SELECT cache_version FROM thread WHERE public_id = ?1";

/// `body_md` is absent deliberately; the read path never needs it.
///
/// Binds: `?1` = thread public id, `?2` = path cursor (exclusive), `?3` = limit.
pub const POSTS: &str = "\
SELECT p.id, p.public_id, p.thread_id, p.parent_id, p.path, p.depth, p.author_id, \
u.name AS author_name, p.body_html, p.created_at, p.edited_at, p.score, p.state \
FROM post p \
JOIN user u ON u.id = p.author_id \
WHERE p.thread_id = (SELECT id FROM thread WHERE public_id = ?1) AND p.path > ?2 \
ORDER BY p.path \
LIMIT ?3";

/// Binds: `?1` = post public id.
pub const LOCATE_POST: &str = "\
SELECT t.public_id AS thread_public_id, t.id AS thread_row_id, p.path AS post_path \
FROM post p \
JOIN thread t ON t.id = p.thread_id \
WHERE p.public_id = ?1";

/// Sorts below every valid path, so the first page needs no special-case SQL.
pub const PATH_START: &str = "";

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// Allocates the next child ordinal. Counting children instead would reuse an ordinal, because
/// tombstones stay in the table.
///
/// Binds: `?1` = thread row id, `?2` = lower bound (exclusive), `?3` = upper bound (exclusive).
pub const LAST_PATH_IN_RANGE: &str = "\
SELECT path FROM post \
WHERE thread_id = ?1 AND path > ?2 AND path < ?3 \
ORDER BY path DESC LIMIT 1";

/// Binds: `?1` = thread public id.
pub const THREAD_ROW: &str = "\
SELECT id, post_count FROM thread WHERE public_id = ?1";

/// Rank in page order; index-only.
///
/// Binds: `?1` = thread row id, `?2` = the post's path.
pub const POST_RANK: &str = "SELECT COUNT(*) AS rank FROM post WHERE thread_id = ?1 AND path < ?2";

/// Index-only.
///
/// Binds: `?1` = thread row id, `?2` = offset.
pub const PATH_AT_OFFSET: &str =
    "SELECT path FROM post WHERE thread_id = ?1 ORDER BY path LIMIT 1 OFFSET ?2";

/// Binds: `?1` = limit.
pub const RECENT_THREADS: &str = "\
SELECT t.public_id, t.title, t.post_count, t.bumped_at, u.name AS author_name, \
s.name AS space_name, s.path AS space_path \
FROM thread t \
JOIN user u ON u.id = t.author_id \
JOIN space s ON s.id = t.space_id \
WHERE t.state = 'visible' \
ORDER BY t.bumped_at DESC \
LIMIT ?1";

/// Thread-scoped, so a cross-thread parent is `NotFound` rather than a cross-thread path.
///
/// Binds: `?1` = post public id, `?2` = thread row id.
pub const POST_IN_THREAD: &str = "\
SELECT id, path, depth FROM post WHERE public_id = ?1 AND thread_id = ?2";

/// A `UNIQUE(thread_id, path)` violation here means the caller lost a path race.
///
/// Binds: `?1` public_id, `?2` thread_id, `?3` parent_id, `?4` path, `?5` depth, `?6` author_id,
/// `?7` body_md, `?8` body_html, `?9` created_at, `?10` state.
pub const INSERT_POST: &str = "\
INSERT INTO post (public_id, thread_id, parent_id, path, depth, author_id, body_md, body_html, \
created_at, score, state) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)";

/// Runs in the same batch as the insert.
///
/// Binds: `?1` = thread row id, `?2` = new bumped_at.
pub const BUMP_THREAD: &str = "\
UPDATE thread SET post_count = post_count + 1, bumped_at = ?2, cache_version = cache_version + 1 \
WHERE id = ?1";

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Binds: `?1` token_hash, `?2` user_id, `?3` created_at, `?4` refreshed_at, `?5` expires_at.
pub const INSERT_SESSION: &str = "\
INSERT INTO session (token_hash, user_id, created_at, refreshed_at, expires_at) \
VALUES (?1, ?2, ?3, ?4, ?5)";

/// Filters expiry here because the sweep is a background chore that may not have run.
///
/// Binds: `?1` token_hash, `?2` now (unix ms).
pub const LOOKUP_SESSION: &str = "\
SELECT s.token_hash, s.user_id, s.created_at, s.refreshed_at, s.expires_at, \
u.name AS user_name, u.state AS user_state, u.role AS user_role \
FROM session s JOIN user u ON u.id = s.user_id \
WHERE s.token_hash = ?1 AND s.expires_at > ?2";

/// Binds: `?1` token_hash, `?2` refreshed_at, `?3` expires_at.
pub const REFRESH_SESSION: &str = "\
UPDATE session SET refreshed_at = ?2, expires_at = ?3 WHERE token_hash = ?1";

/// Binds: `?1` token_hash.
pub const DELETE_SESSION: &str = "DELETE FROM session WHERE token_hash = ?1";

/// Binds: `?1` user_id.
pub const DELETE_USER_SESSIONS: &str = "DELETE FROM session WHERE user_id = ?1";

// ---------------------------------------------------------------------------
// Login rate limiting
// ---------------------------------------------------------------------------

/// Both buckets in one statement: this runs before the password hash on every login.
///
/// Binds: `?1` identity key, `?2` client key.
pub const LOGIN_ATTEMPTS: &str = "\
SELECT key, window_start, count FROM login_attempt WHERE key = ?1 OR key = ?2";

/// Binds: `?1` key, `?2` window_start, `?3` count.
pub const RECORD_LOGIN_ATTEMPT: &str = "\
INSERT INTO login_attempt (key, window_start, count) VALUES (?1, ?2, ?3) \
ON CONFLICT(key) DO UPDATE SET window_start = ?2, count = ?3";

/// Binds: `?1` key.
pub const CLEAR_LOGIN_ATTEMPTS: &str = "DELETE FROM login_attempt WHERE key = ?1";

/// Binds: `?1` cutoff timestamp.
pub const SWEEP_LOGIN_ATTEMPTS: &str = "DELETE FROM login_attempt WHERE window_start < ?1";

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// Matched on the stored lowercase name; the caller lowercases before binding.
///
/// Binds: `?1` username.
pub const USER_BY_NAME: &str = "\
SELECT id, name, state, role, password_hash FROM user WHERE name = ?1";

/// Binds: `?1` name, `?2` created_at, `?3` password_hash (NULL for external auth).
pub const INSERT_USER: &str = "\
INSERT INTO user (name, created_at, password_hash, state) VALUES (?1, ?2, ?3, 'active')";

/// Binds: `?1` user id, `?2` password_hash.
pub const SET_PASSWORD_HASH: &str = "UPDATE user SET password_hash = ?2 WHERE id = ?1";

// ---------------------------------------------------------------------------
// Moderation
// ---------------------------------------------------------------------------

/// The thread's space and the author, in one statement; the two are unrelated, so it is a cross
/// join of two single rows.
///
/// Binds: `?1` = thread public id, `?2` = author user id.
pub const WRITE_CONTEXT: &str = "\
SELECT s.id AS space_id, s.config AS space_config, t.state AS thread_state, \
u.created_at AS author_created_at, u.role AS author_role \
FROM thread t JOIN space s ON s.id = t.space_id, user u \
WHERE t.public_id = ?1 AND u.id = ?2";

/// Served by `idx_post_author`, so it reads the author's recent posts and nothing else.
///
/// Binds: `?1` = author id, `?2` = body_md, `?3` = since (unix ms).
pub const AUTHOR_POSTED_RECENTLY: &str = "\
SELECT 1 AS found FROM post \
WHERE author_id = ?1 AND created_at >= ?3 AND body_md = ?2 \
LIMIT 1";

/// Binds: `?1` = post public id.
pub const POST_FOR_REVIEW: &str = "\
SELECT p.id, p.public_id, t.public_id AS thread_public_id, t.title AS thread_title, \
s.id AS space_id, s.name AS space_name, s.config AS space_config, \
p.author_id, u.name AS author_name, u.created_at AS author_created_at, \
p.body_md, p.created_at, p.state \
FROM post p \
JOIN thread t ON t.id = p.thread_id \
JOIN space s ON s.id = t.space_id \
JOIN user u ON u.id = p.author_id \
WHERE p.public_id = ?1";

/// Binds: `?1` = post public id, `?2` = new state.
pub const SET_POST_STATE: &str = "UPDATE post SET state = ?2 WHERE public_id = ?1";

/// Runs in the same batch as `SET_POST_STATE`, so the baked page turns over with the state.
///
/// Binds: `?1` = post public id.
pub const BUMP_THREAD_FOR_POST: &str = "\
UPDATE thread SET cache_version = cache_version + 1 \
WHERE id = (SELECT thread_id FROM post WHERE public_id = ?1)";

/// Binds: `?1` actor_kind, `?2` actor_id, `?3` actor_name, `?4` target_kind, `?5` target_id,
/// `?6` action, `?7` detail (JSON), `?8` public (0/1), `?9` created_at.
pub const INSERT_ACTION: &str = "\
INSERT INTO action_log (actor_kind, actor_id, actor_name, target_kind, target_id, action, \
detail, public, created_at) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";

/// One row per post: a second opening reopens and updates. Model fields keep their old value
/// when the new opening has none (an appeal does not erase the verdict it appeals).
///
/// Binds: `?1` post_id, `?2` space_id, `?3` reason, `?4` model_verdict, `?5` model_confidence,
/// `?6` model_categories (JSON), `?7` appeal_text, `?8` opened_at.
pub const UPSERT_REVIEW: &str = "\
INSERT INTO review_item (post_id, space_id, reason, model_verdict, model_confidence, \
model_categories, appeal_text, opened_at, state, resolution, resolved_by, resolved_at) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', NULL, NULL, NULL) \
ON CONFLICT(post_id) DO UPDATE SET \
reason = excluded.reason, \
model_verdict = COALESCE(excluded.model_verdict, review_item.model_verdict), \
model_confidence = COALESCE(excluded.model_confidence, review_item.model_confidence), \
model_categories = COALESCE(excluded.model_categories, review_item.model_categories), \
appeal_text = COALESCE(excluded.appeal_text, review_item.appeal_text), \
opened_at = excluded.opened_at, state = 'open', resolution = NULL, resolved_by = NULL, \
resolved_at = NULL";

/// The select list shared by the queue listing and the single-item lookup, written once so the
/// two cannot drift and both adapters decode one row shape.
macro_rules! review_select {
    ($tail:literal) => {
        concat!(
            "SELECT r.id, r.post_id, p.public_id AS post_public_id, ",
            "t.public_id AS thread_public_id, t.title AS thread_title, r.space_id, ",
            "s.name AS space_name, p.author_id, u.name AS author_name, p.body_html, ",
            "p.state AS post_state, r.reason, r.model_verdict, r.model_confidence, ",
            "r.model_categories, r.appeal_text, r.opened_at, r.state ",
            "FROM review_item r ",
            "JOIN post p ON p.id = r.post_id ",
            "JOIN thread t ON t.id = p.thread_id ",
            "JOIN space s ON s.id = r.space_id ",
            "JOIN user u ON u.id = p.author_id ",
            $tail
        )
    };
}

/// Oldest first. Binds: `?1` = limit.
pub const OPEN_REVIEWS: &str =
    review_select!("WHERE r.state = 'open' ORDER BY r.opened_at, r.id LIMIT ?1");

/// Binds: `?1` = review id.
pub const REVIEW_BY_ID: &str = review_select!("WHERE r.id = ?1");

/// Conditional on `open`, so the second of two simultaneous resolutions changes nothing.
///
/// Binds: `?1` id, `?2` resolution, `?3` resolved_by, `?4` resolved_at.
pub const RESOLVE_REVIEW: &str = "\
UPDATE review_item SET state = 'resolved', resolution = ?2, resolved_by = ?3, resolved_at = ?4 \
WHERE id = ?1 AND state = 'open'";

/// `OR IGNORE` against `idx_signal_once`: a repeat report is a no-op, and `changes` says which.
///
/// Binds: `?1` post_id, `?2` user_id, `?3` kind, `?4` weight, `?5` reason, `?6` created_at.
pub const INSERT_SIGNAL: &str = "\
INSERT OR IGNORE INTO signal (post_id, user_id, kind, weight, reason, created_at) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// Binds: `?1` post_id, `?2` kind.
pub const COUNT_SIGNALS: &str = "\
SELECT COUNT(*) AS n FROM signal WHERE post_id = ?1 AND kind = ?2";

/// Range scan over `idx_post_state_time`. A post already waiting on a human is excluded, so the
/// sweep classifies each post at most once rather than paying for a model call on every pass.
///
/// Binds: `?1` = older than (unix ms), `?2` = limit.
pub const PENDING_POSTS: &str = "\
SELECT p.public_id FROM post p \
WHERE p.state = 'pending' AND p.created_at < ?1 \
AND NOT EXISTS (SELECT 1 FROM review_item r WHERE r.post_id = p.id AND r.state = 'open') \
ORDER BY p.created_at LIMIT ?2";

/// Newest first, with the target's public id resolved so the page can link to it. `detail` is
/// deliberately not selected.
///
/// Binds: `?1` = limit.
pub const PUBLIC_LOG: &str = "\
SELECT a.id, a.actor_kind, a.actor_name, a.target_kind, a.target_id, a.action, a.created_at, \
CASE a.target_kind \
WHEN 'post' THEN (SELECT public_id FROM post WHERE id = a.target_id) \
WHEN 'thread' THEN (SELECT public_id FROM thread WHERE id = a.target_id) \
END AS target_public_id \
FROM action_log a WHERE a.public = 1 \
ORDER BY a.created_at DESC, a.id DESC LIMIT ?1";

/// Agreement is `clean`/`approve` or `flag`/`reject`; disagreement is the other diagonal.
/// `unsure` is neither. `SUM` over no rows is NULL, which adapters read as zero.
///
/// Binds: `?1` = space id.
pub const AGREEMENT: &str = "\
SELECT \
SUM(CASE WHEN (model_verdict = 'clean' AND resolution = 'approve') \
OR (model_verdict = 'flag' AND resolution = 'reject') THEN 1 ELSE 0 END) AS agreed, \
SUM(CASE WHEN (model_verdict = 'clean' AND resolution = 'reject') \
OR (model_verdict = 'flag' AND resolution = 'approve') THEN 1 ELSE 0 END) AS disagreed \
FROM review_item \
WHERE space_id = ?1 AND state = 'resolved' AND model_verdict IS NOT NULL";
