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
/// `?7` body_md, `?8` body_html, `?9` created_at.
pub const INSERT_POST: &str = "\
INSERT INTO post (public_id, thread_id, parent_id, path, depth, author_id, body_md, body_html, \
created_at, score, state) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 'visible')";

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
u.name AS user_name, u.state AS user_state \
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
SELECT id, name, state, password_hash FROM user WHERE name = ?1";

/// Binds: `?1` name, `?2` created_at, `?3` password_hash (NULL for external auth).
pub const INSERT_USER: &str = "\
INSERT INTO user (name, created_at, password_hash, state) VALUES (?1, ?2, ?3, 'active')";

/// Binds: `?1` user id, `?2` password_hash.
pub const SET_PASSWORD_HASH: &str = "UPDATE user SET password_hash = ?2 WHERE id = ?1";
