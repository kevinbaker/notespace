//! SQL shared verbatim by every [`Store`](crate::store::Store) implementation.
//!
//! Plain SQLite only: no Postgres-isms, nothing that needs a D1 extension.

/// Binds: `?1` = thread public id.
pub const THREAD: &str = "\
SELECT t.id, t.public_id, t.space_id, t.kind, t.title, t.url, t.author_id, u.name AS author_name, \
t.created_at, t.bumped_at, t.post_count, t.state, t.cache_version, \
s.path AS space_path, s.name AS space_name, s.ranking AS space_ranking, \
s.depth_cap AS space_depth_cap, s.config AS space_config \
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

/// One post with its markdown, for the reply form's parent context and quoting.
///
/// Binds: `?1` = post public id.
pub const POST_BY_ID: &str = "\
SELECT p.id, p.public_id, p.thread_id, p.parent_id, p.path, p.depth, p.author_id, \
u.name AS author_name, p.body_md, p.body_html, p.created_at, p.edited_at, p.score, p.state, \
t.public_id AS thread_public_id \
FROM post p \
JOIN user u ON u.id = p.author_id \
JOIN thread t ON t.id = p.thread_id \
WHERE p.public_id = ?1";

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

/// Pinned first, then most recently bumped. Locked threads are still listed: they can be read.
///
/// Binds: `?1` = limit.
pub const RECENT_THREADS: &str = "\
SELECT t.public_id, t.title, t.post_count, t.bumped_at, u.name AS author_name, \
s.name AS space_name, s.path AS space_path \
FROM thread t \
JOIN user u ON u.id = t.author_id \
JOIN space s ON s.id = t.space_id \
WHERE t.state IN ('visible', 'pinned', 'locked') \
ORDER BY (t.state = 'pinned') DESC, t.bumped_at DESC \
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

/// Every thread the user posted in turns over, so the tombstones show. Runs in the same batch
/// as `TOMBSTONE_USER_POSTS`, and before it, while the posts still say where they are.
///
/// Binds: `?1` = user id.
pub const BUMP_THREADS_OF_USER: &str = "\
UPDATE thread SET cache_version = cache_version + 1 \
WHERE id IN (SELECT DISTINCT thread_id FROM post WHERE author_id = ?1 AND state != 'deleted')";

/// An account deletion leaves `[deleted]` markers, not holes: replies keep their context.
///
/// Binds: `?1` = user id.
pub const TOMBSTONE_USER_POSTS: &str = "\
UPDATE post SET state = 'deleted' WHERE author_id = ?1 AND state != 'deleted'";

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

/// Binds: `?1` = user id.
pub const USER_BY_ID: &str = "SELECT id, name, state, role FROM user WHERE id = ?1";

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

// ---------------------------------------------------------------------------
// Spaces and new threads
// ---------------------------------------------------------------------------

/// Matched on the stored form, trailing separator included.
///
/// Binds: `?1` = space path.
pub const SPACE_BY_PATH: &str = "\
SELECT id, path, name, parent_id, ranking, depth_cap, config FROM space WHERE path = ?1";

/// `?1 IS NULL` selects the top level: SQLite's `=` never matches NULL.
///
/// Binds: `?1` = parent space id, or NULL.
pub const SPACES_UNDER: &str = "\
SELECT id, path, name, parent_id, ranking, depth_cap, config FROM space \
WHERE (?1 IS NULL AND parent_id IS NULL) OR parent_id = ?1 \
ORDER BY path";

/// One range scan over `idx_thread_subtree`; the bounds come from `SpacePath::subtree_range`.
///
/// Binds: `?1` = range start (inclusive), `?2` = range end (exclusive), `?3` = limit.
pub const SPACE_THREADS: &str = "\
SELECT t.public_id, t.title, t.post_count, t.bumped_at, u.name AS author_name, \
s.name AS space_name, s.path AS space_path \
FROM thread t \
JOIN user u ON u.id = t.author_id \
JOIN space s ON s.id = t.space_id \
WHERE t.space_path >= ?1 AND t.space_path < ?2 AND t.state IN ('visible', 'pinned', 'locked') \
ORDER BY (t.state = 'pinned') DESC, t.bumped_at DESC \
LIMIT ?3";

/// Like `WRITE_CONTEXT` for a thread that does not exist yet: the space and the author.
///
/// Binds: `?1` = space id, `?2` = author user id.
pub const SPACE_CONTEXT: &str = "\
SELECT s.id AS space_id, s.config AS space_config, 'visible' AS thread_state, \
u.created_at AS author_created_at, u.role AS author_role \
FROM space s, user u \
WHERE s.id = ?1 AND u.id = ?2";

/// `post_count` starts at zero; the first post bumps it like any other.
///
/// Binds: `?1` public_id, `?2` space_id, `?3` space_path, `?4` kind, `?5` title, `?6` url,
/// `?7` author_id, `?8` created_at.
pub const INSERT_THREAD: &str = "\
INSERT INTO thread (public_id, space_id, space_path, kind, title, url, author_id, created_at, \
bumped_at, post_count, score, rank, state, cache_version) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0, 0, 0, 'visible', 0)";

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Runs in the same batch as `BUMP_THREAD_FOR_POST`.
///
/// Binds: `?1` = post public id, `?2` = body_md, `?3` = body_html, `?4` = edited_at.
pub const UPDATE_POST_BODY: &str = "\
UPDATE post SET body_md = ?2, body_html = ?3, edited_at = ?4 WHERE public_id = ?1";

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// Binds: `?1` = username, lowercase.
pub const USER_PROFILE: &str = "\
SELECT id, name, state, role, created_at FROM user WHERE name = ?1";

/// Served by `idx_post_author`. Visible posts only: a profile is a public page.
///
/// Binds: `?1` = user id, `?2` = limit.
pub const USER_RECENT_POSTS: &str = "\
SELECT p.public_id, p.body_html, p.created_at, \
t.public_id AS thread_public_id, t.title AS thread_title \
FROM post p JOIN thread t ON t.id = p.thread_id \
WHERE p.author_id = ?1 AND p.state = 'visible' \
ORDER BY p.created_at DESC \
LIMIT ?2";

// ---------------------------------------------------------------------------
// Email
// ---------------------------------------------------------------------------

/// Binds: `?1` = user id.
pub const ACCOUNT: &str = "\
SELECT email, email_verified_at, password_hash IS NOT NULL AS has_password \
FROM user WHERE id = ?1";

/// The partial unique index makes this at most one row.
///
/// Binds: `?1` = email, lowercase.
pub const USER_BY_VERIFIED_EMAIL: &str = "\
SELECT id, name, state, role FROM user WHERE email = ?1 AND email_verified_at IS NOT NULL";

/// A new address starts unverified, whatever the old one was.
///
/// Binds: `?1` = user id, `?2` = email or NULL.
pub const SET_EMAIL: &str = "UPDATE user SET email = ?2, email_verified_at = NULL WHERE id = ?1";

/// Conditional on the address still being the one the link was sent to. Verifying an address
/// another account has already proven trips `idx_user_email_verified`, which adapters report
/// as `Conflict`.
///
/// Binds: `?1` = user id, `?2` = email, `?3` = now.
pub const MARK_EMAIL_VERIFIED: &str = "\
UPDATE user SET email_verified_at = ?3 WHERE id = ?1 AND email = ?2";

/// Binds: `?1` token_hash, `?2` user_id, `?3` kind, `?4` email, `?5` created_at, `?6` expires_at.
pub const INSERT_EMAIL_TOKEN: &str = "\
INSERT INTO email_token (token_hash, user_id, kind, email, created_at, expires_at, used_at) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)";

/// Consumes in one statement, so two clicks on the same link cannot both succeed: the row is
/// returned only by the update that marked it used.
///
/// Binds: `?1` = token_hash, `?2` = kind, `?3` = now.
pub const CONSUME_EMAIL_TOKEN: &str = "\
UPDATE email_token SET used_at = ?3 \
WHERE token_hash = ?1 AND kind = ?2 AND used_at IS NULL AND expires_at > ?3 \
RETURNING user_id, email";

/// Whose an unspent, unexpired token is, without spending it: the reset page says so before
/// the visitor types a password.
///
/// Binds: `?1` = token_hash, `?2` = kind, `?3` = now.
pub const PEEK_EMAIL_TOKEN: &str = "\
SELECT u.name FROM email_token t JOIN user u ON u.id = t.user_id \
WHERE t.token_hash = ?1 AND t.kind = ?2 AND t.used_at IS NULL AND t.expires_at > ?3";

/// Issuing a new link retires the older ones of that kind.
///
/// Binds: `?1` = user id, `?2` = kind, `?3` = now.
pub const RETIRE_EMAIL_TOKENS: &str = "\
UPDATE email_token SET used_at = ?3 WHERE user_id = ?1 AND kind = ?2 AND used_at IS NULL";

// ---------------------------------------------------------------------------
// Administration
// ---------------------------------------------------------------------------

/// The thread and its space, without posts; the `THREAD` select, reused by name.
pub const THREAD_HEAD: &str = THREAD;

/// Everything an admin may change, in one statement, with the page version bumped by the
/// same statement so a retitle turns the cached page over.
///
/// Binds: `?1` public_id, `?2` title, `?3` url, `?4` state, `?5` space_id, `?6` space_path.
pub const UPDATE_THREAD: &str = "\
UPDATE thread SET title = ?2, url = ?3, state = ?4, space_id = ?5, space_path = ?6, \
cache_version = cache_version + 1 \
WHERE public_id = ?1";

/// Binds: `?1` = user id, `?2` = role.
pub const SET_USER_ROLE: &str = "UPDATE user SET role = ?2 WHERE id = ?1";

/// Binds: `?1` = user id, `?2` = state.
pub const SET_USER_STATE: &str = "UPDATE user SET state = ?2 WHERE id = ?1";

/// Newest accounts first, optionally by name prefix. `?1 = ''` matches everyone.
///
/// Binds: `?1` = name prefix (already lowercase), `?2` = limit.
pub const LIST_USERS: &str = "\
SELECT u.id, u.name, u.state, u.role, u.created_at, u.email, \
u.email_verified_at IS NOT NULL AS email_verified, \
(SELECT COUNT(*) FROM post p WHERE p.author_id = u.id) AS post_count \
FROM user u \
WHERE ?1 = '' OR u.name LIKE ?1 || '%' \
ORDER BY u.id DESC LIMIT ?2";

/// Binds: `?1` = username, lowercase.
pub const USER_ROW: &str = "\
SELECT u.id, u.name, u.state, u.role, u.created_at, u.email, \
u.email_verified_at IS NOT NULL AS email_verified, \
(SELECT COUNT(*) FROM post p WHERE p.author_id = u.id) AS post_count \
FROM user u WHERE u.name = ?1";

/// Binds: `?1` = space id.
pub const SPACE_DETAIL: &str = "\
SELECT s.id, s.path, s.name, s.parent_id, s.ranking, s.depth_cap, s.config, \
(SELECT COUNT(*) FROM thread t WHERE t.space_id = s.id) AS thread_count \
FROM space s WHERE s.id = ?1";

/// Every space, for the admin list and the move-thread menu.
pub const ALL_SPACES: &str = "\
SELECT s.id, s.path, s.name, s.parent_id, s.ranking, s.depth_cap, s.config, \
(SELECT COUNT(*) FROM thread t WHERE t.space_id = s.id) AS thread_count \
FROM space s ORDER BY s.path";

/// A taken path trips `idx_space_path`, which adapters report as `Conflict`.
///
/// Binds: `?1` name, `?2` path, `?3` parent_id, `?4` ranking, `?5` depth_cap, `?6` config.
pub const INSERT_SPACE: &str = "\
INSERT INTO space (name, path, parent_id, ranking, depth_cap, config) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// The key and parent are not editable here: they are the path, and threads carry it.
///
/// Binds: `?1` id, `?2` name, `?3` ranking, `?4` depth_cap, `?5` config.
pub const UPDATE_SPACE: &str = "\
UPDATE space SET name = ?2, ranking = ?3, depth_cap = ?4, config = ?5 WHERE id = ?1";

/// One statement of subselects, so the dashboard costs one round trip.
pub const SITE_STATS: &str = "\
SELECT \
(SELECT COUNT(*) FROM user) AS users, \
(SELECT COUNT(*) FROM thread) AS threads, \
(SELECT COUNT(*) FROM post) AS posts, \
(SELECT COUNT(*) FROM post WHERE state = 'pending') AS pending_posts, \
(SELECT COUNT(*) FROM review_item WHERE state = 'open') AS open_reviews, \
(SELECT COUNT(*) FROM user WHERE state = 'banned') AS banned_users";

/// The log with nothing held back, newest first.
///
/// Binds: `?1` = limit.
pub const FULL_LOG: &str = "\
SELECT a.id, a.actor_kind, a.actor_name, a.target_kind, a.target_id, a.action, a.created_at, \
a.public, a.detail, \
CASE a.target_kind \
WHEN 'post' THEN (SELECT public_id FROM post WHERE id = a.target_id) \
WHEN 'thread' THEN (SELECT public_id FROM thread WHERE id = a.target_id) \
END AS target_public_id \
FROM action_log a \
ORDER BY a.created_at DESC, a.id DESC LIMIT ?1";

// ---------------------------------------------------------------------------
// External identities
// ---------------------------------------------------------------------------

/// Binds: `?1` = provider, `?2` = subject.
pub const IDENTITY_USER: &str = "\
SELECT u.id, u.name, u.state, u.role \
FROM external_identity i JOIN user u ON u.id = i.user_id \
WHERE i.provider = ?1 AND i.subject = ?2";

/// The primary key is `(provider, subject)`; a repeat trips it, reported as `Conflict`.
///
/// Binds: `?1` provider, `?2` subject, `?3` user_id, `?4` email, `?5` now.
pub const INSERT_IDENTITY: &str = "\
INSERT INTO external_identity (provider, subject, user_id, email, created_at, last_login_at) \
VALUES (?1, ?2, ?3, ?4, ?5, ?5)";

/// Binds: `?1` = provider, `?2` = subject, `?3` = now.
pub const TOUCH_IDENTITY: &str = "\
UPDATE external_identity SET last_login_at = ?3 WHERE provider = ?1 AND subject = ?2";

/// Binds: `?1` = user id.
pub const USER_IDENTITIES: &str = "\
SELECT provider FROM external_identity WHERE user_id = ?1 ORDER BY provider";
