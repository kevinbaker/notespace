//! The read-path SQL, shared verbatim by every [`Store`](crate::store::Store) implementation.
//!
//! Both targets must speak an identical SQLite dialect. Keeping the statements
//! in one place makes that structural rather than aspirational: the adapters cannot drift,
//! because there is only one copy of the SQL to drift from. What differs between them is
//! binding and row decoding, not the query.
//!
//! Every statement here is written against plain SQLite. No Postgres-isms, and nothing that
//! depends on a D1 extension.

/// Thread header plus the space it belongs to. Deliberately narrow.
///
/// The space is joined rather than fetched separately because the page render needs
/// `depth_cap`, and a second query for one integer is a round trip the budget does not have.
///
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

/// One page of posts. `path > ?2` is an indexed range scan on `idx_post_thread_path`, not a
/// sort: SQLite walks the index in order and stops at LIMIT.
///
/// `body_md` is intentionally absent; see the note on [`crate::model::Post::body_md`].
///
/// The URL carries a public id, but posts are keyed by the integer `thread_id`. Resolving that
/// in the caller would cost a round trip and undo the batching the whole read path depends on,
/// so it folds into a scalar subquery — a single probe of `idx_thread_public_id`, which is the
/// one extra row read per pageview that addressing threads by public id costs.
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

/// Where a post currently lives. One query, because a permalink has to ask "which thread is
/// this post in *now*" — splitting and merging threads moves posts between them.
///
/// Binds: `?1` = post public id.
pub const LOCATE_POST: &str = "\
SELECT t.public_id AS thread_public_id, p.path AS post_path \
FROM post p \
JOIN thread t ON t.id = p.thread_id \
WHERE p.public_id = ?1";

/// Sorts below every valid path, so it means "start at the beginning of the thread".
///
/// A sentinel keeps the SQL identical for the first page and every later one, which keeps the
/// prepared-statement cache warm and means the first page is not a special case in any adapter.
pub const PATH_START: &str = "";

// ---------------------------------------------------------------------------
// Write path
// ---------------------------------------------------------------------------

/// The deepest path under a subtree, or in a whole thread.
///
/// This is how a child ordinal is allocated in O(1) rather than by scanning siblings. The index
/// `(thread_id, path)` is walked backwards and stopped at the first row, giving the last
/// descendant of the parent in preorder; truncating that to the parent's depth + 1 yields the
/// last *direct child*, whose successor is the ordinal to insert at.
///
/// Counting children instead would be wrong, not merely slower: edit and delete leave
/// tombstones in the table, so a count reuses an ordinal that is still occupied.
///
/// Binds: `?1` = thread row id, `?2` = lower bound (exclusive), `?3` = upper bound (exclusive).
pub const LAST_PATH_IN_RANGE: &str = "\
SELECT path FROM post \
WHERE thread_id = ?1 AND path > ?2 AND path < ?3 \
ORDER BY path DESC LIMIT 1";

/// Resolve a thread's public id to its row id and current post count.
///
/// Binds: `?1` = thread public id.
pub const THREAD_ROW: &str = "\
SELECT id, post_count FROM thread WHERE public_id = ?1";

/// Resolve a post's public id to its row id and path, within a known thread.
///
/// Scoped to the thread on purpose: replying to a post in another thread is not a reparent, it
/// is a bug, and this makes it a `NotFound` rather than a cross-thread path.
///
/// Binds: `?1` = post public id, `?2` = thread row id.
pub const POST_IN_THREAD: &str = "\
SELECT id, path, depth FROM post WHERE public_id = ?1 AND thread_id = ?2";

/// Append a post.
///
/// The `UNIQUE(thread_id, path)` index is load-bearing here rather than merely tidy: allocating
/// a path is read-then-write, so two concurrent replies to the same parent can compute the same
/// ordinal. The loser hits this constraint and retries, which is why the caller must treat a
/// uniqueness violation as "recompute and try again" rather than as a failure.
///
/// Binds: `?1` public_id, `?2` thread_id, `?3` parent_id, `?4` path, `?5` depth, `?6` author_id,
/// `?7` body_md, `?8` body_html, `?9` created_at.
pub const INSERT_POST: &str = "\
INSERT INTO post (public_id, thread_id, parent_id, path, depth, author_id, body_md, body_html, \
created_at, score, state) \
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 'visible')";

/// Keep the denormalized counters honest. Runs in the same batch as the insert.
///
/// `post_count` exists so a thread list does not need `COUNT(*)` per row, and `bumped_at` is
/// what bump-ordered spaces sort on.
///
/// Binds: `?1` = thread row id, `?2` = new bumped_at.
pub const BUMP_THREAD: &str = "\
UPDATE thread SET post_count = post_count + 1, bumped_at = ?2, cache_version = cache_version + 1 \
WHERE id = ?1";

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Create a session. `token_hash` is the SHA-256 of the cookie value, never the value itself.
///
/// Binds: `?1` token_hash, `?2` user_id, `?3` created_at, `?4` refreshed_at, `?5` expires_at.
pub const INSERT_SESSION: &str = "\
INSERT INTO session (token_hash, user_id, created_at, refreshed_at, expires_at) \
VALUES (?1, ?2, ?3, ?4, ?5)";

/// Look up a live session and the user it belongs to, in one statement.
///
/// Joins `user` rather than making the caller fetch it: an authenticated request needs both, and
/// two round trips for one identity is not in the budget.
///
/// Expiry is filtered here rather than in the caller. Sweeping expired rows is a background
/// chore that may not have run, so "the row exists" and "the session is live" are different
/// questions and only the second one is ever asked.
///
/// Binds: `?1` token_hash, `?2` now (unix ms).
pub const LOOKUP_SESSION: &str = "\
SELECT s.token_hash, s.user_id, s.created_at, s.refreshed_at, s.expires_at, \
u.name AS user_name, u.state AS user_state \
FROM session s JOIN user u ON u.id = s.user_id \
WHERE s.token_hash = ?1 AND s.expires_at > ?2";

/// Push a session's expiry out. Rate-limited by `SessionPolicy`, not by this statement.
///
/// Binds: `?1` token_hash, `?2` refreshed_at, `?3` expires_at.
pub const REFRESH_SESSION: &str = "\
UPDATE session SET refreshed_at = ?2, expires_at = ?3 WHERE token_hash = ?1";

/// Log out. Binds: `?1` token_hash.
pub const DELETE_SESSION: &str = "DELETE FROM session WHERE token_hash = ?1";

/// Log out everywhere — after a password change, or when an account is banned.
///
/// Binds: `?1` user_id.
pub const DELETE_USER_SESSIONS: &str = "DELETE FROM session WHERE user_id = ?1";

// ---------------------------------------------------------------------------
// Login rate limiting
// ---------------------------------------------------------------------------

/// Current counters for the two buckets an attempt is checked against.
///
/// One statement for both, because the check runs before the password hash on every login and a
/// second round trip there is a round trip on the attacker's schedule.
///
/// Binds: `?1` identity key, `?2` client key.
pub const LOGIN_ATTEMPTS: &str = "\
SELECT key, window_start, count FROM login_attempt WHERE key = ?1 OR key = ?2";

/// Record an attempt. Upsert so the first failure in a window creates the row.
///
/// Binds: `?1` key, `?2` window_start, `?3` count.
pub const RECORD_LOGIN_ATTEMPT: &str = "\
INSERT INTO login_attempt (key, window_start, count) VALUES (?1, ?2, ?3) \
ON CONFLICT(key) DO UPDATE SET window_start = ?2, count = ?3";

/// Clear a bucket. Used on a successful login so a correct password is never punished.
///
/// Binds: `?1` key.
pub const CLEAR_LOGIN_ATTEMPTS: &str = "DELETE FROM login_attempt WHERE key = ?1";

/// Drop windows that ended long ago.
///
/// Binds: `?1` cutoff timestamp.
pub const SWEEP_LOGIN_ATTEMPTS: &str = "DELETE FROM login_attempt WHERE window_start < ?1";

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// Look up an account for login. Returns the credential alongside the identity so the handler
/// does not need a second round trip.
///
/// Matched on the stored (lowercase) name. Normalization is case and only case, so the caller
/// lowercases before binding.
///
/// Binds: `?1` username.
pub const USER_BY_NAME: &str = "\
SELECT id, name, state, password_hash FROM user WHERE name = ?1";

/// Create an account.
///
/// Binds: `?1` name, `?2` created_at, `?3` password_hash (may be NULL for external auth).
pub const INSERT_USER: &str = "\
INSERT INTO user (name, created_at, password_hash, state) VALUES (?1, ?2, ?3, 'active')";

/// Rewrite a stored credential — after a password change, or a rehash under stronger parameters
/// or a newer pepper.
///
/// Binds: `?1` user id, `?2` password_hash.
pub const SET_PASSWORD_HASH: &str = "UPDATE user SET password_hash = ?2 WHERE id = ?1";
