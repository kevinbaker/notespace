//! The read-path SQL, shared verbatim by every [`Store`](crate::store::Store) implementation.
//!
//! DESIGN.md §3.1 requires an identical SQLite dialect on both targets. Keeping the statements
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
/// "+1 row read per pageview" DESIGN.md §4.2 budgets for.
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
