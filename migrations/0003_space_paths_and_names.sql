-- Spaces and users are addressed by name, not by opaque id (DESIGN.md §4.4).
--
-- `/s/sports/hockey` and `/u/testuser`. Slugs are unique PER PARENT, so `sports/general` and
-- `music/general` are different spaces, and the URL carries the whole path.
--
-- These are IDENTIFIERS, and a different thing from a thread's decorative slug. `/t/{id}/{slug}`
-- resolves on the id and ignores the trailing text; `/s/...` and `/u/...` have nothing but the
-- name to resolve on. See crates/core/src/naming.rs.
--
-- Usernames and space names are also different from EACH OTHER -- different reserved lists,
-- different rename policy -- so they are separate types (core::username, core::space_key) and
-- are handled separately below.
--
-- Both are stored in their parsed form, which is lowercase. Normalization is case and ONLY
-- case: `test-user` and `testuser` are two different names, as they are on GitHub. There is
-- therefore no separate "canonical" column anywhere in this file -- the stored text is already
-- the canonical text.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Space paths
-- ---------------------------------------------------------------------------

-- Materialized path, e.g. 'sports/hockey/'. Same reasoning as post.path: resolving
-- /s/sports/hockey is ONE indexed lookup rather than one query per level (measured: 1.6 us and
-- 1 query, against 32.9 us and 3 queries for the walk).
--
-- STORED WITH A TRAILING SEPARATOR, and that is load-bearing. A slug may contain '-' (0x2D),
-- which sorts BELOW '/' (0x2F), so an untrailed subtree range swallows siblings:
--
--   ['sports', 'sports0')   ->  sports, sports-betting, sports/hockey   WRONG
--   ['sports/', 'sports0')  ->  sports/, sports/hockey/                 right
--
-- See crates/core/src/slug.rs::subtree_range and its test.
ALTER TABLE space ADD COLUMN path TEXT;

-- SQLite cannot ADD COLUMN ... UNIQUE, so uniqueness comes from the index below.
--
-- Note this is on the full path, not on (parent_id, key). That is deliberate: SQLite treats
-- NULLs as DISTINCT in a unique index, so UNIQUE(parent_id, key) would happily allow two
-- top-level spaces with the same key, since both have parent_id IS NULL. The path already
-- encodes the parent, so indexing it sidesteps the footgun entirely.
CREATE UNIQUE INDEX IF NOT EXISTS idx_space_path ON space(path);

-- Threads carry their space's path so that "everything under /s/sports" is a single range scan.
-- Denormalized on purpose: the alternative is a recursive CTE, measured at 2634 us against
-- 294 us for the range scan -- 9x slower, and it builds throwaway indexes at runtime.
-- Rewritten for the subtree whenever a space moves, which is rare and bounded.
ALTER TABLE thread ADD COLUMN space_path TEXT;
CREATE INDEX IF NOT EXISTS idx_thread_subtree ON thread(space_path, rank DESC);

-- ---------------------------------------------------------------------------
-- Usernames
-- ---------------------------------------------------------------------------
--
-- Nothing to add: `user.name` is already UNIQUE from 0001, and names are stored in their
-- parsed form, which is lowercase.
--
-- USERNAMES ARE PERMANENT. There is no rename operation, and there is deliberately no history
-- table here. A renameable username is a reusable one, and a reusable one is an impersonation
-- vector: every old link, quote and @mention naming `alice` would silently start pointing at
-- whoever claimed it next. A forum is an archive -- a thread from four years ago is still
-- readable and still cited, and its attributions must keep meaning what they meant.
--
-- The name therefore stays taken after the account is gone: deleting a user marks the row
-- rather than removing it, so the name can never be reissued. An old /u/ link to a deleted
-- account 404s (or shows a tombstone). It never resolves to a different person.
ALTER TABLE user ADD COLUMN state TEXT NOT NULL DEFAULT 'active'; -- active|deleted|banned

-- ---------------------------------------------------------------------------
-- Space renames
-- ---------------------------------------------------------------------------
--
-- Spaces DO rename and move, unlike usernames. A space is a place, not an identity: no post is
-- attributed to a space in a way a rename could falsify, so the argument above does not apply.
--
-- A rename needs a redirect, and this is that redirect -- one nullable self-reference, not a
-- history table. Renaming rewrites the row's `path`; if the old URL should keep working, a
-- tombstone row is left behind with `moved_to_id` pointing at the real space.
--
-- The lookup that resolves any path already finds that row, so a redirect costs ZERO extra
-- queries: resolve the path, and if the row carries `moved_to_id`, 301 to that space's path.
-- An instance that does not care simply lets old paths 404 and stores no tombstone at all.
ALTER TABLE space ADD COLUMN moved_to_id INTEGER REFERENCES space(id);
