-- Spaces and users are addressed by name, not by opaque id (DESIGN.md §4.4).
--
-- `/s/sports/hockey` and `/u/testuser`. Slugs are unique PER PARENT, so `sports/general` and
-- `music/general` are different spaces, and the URL carries the whole path.
--
-- A slug is not an id, and the difference drives everything below: it is mutable, reusable,
-- and chosen by the person claiming it.
--
-- Slugs are stored in their parsed form, which is lowercase. Normalization is case and ONLY
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
-- Note these are on the full path, not on (parent_id, slug). That is deliberate: SQLite treats
-- NULLs as DISTINCT in a unique index, so UNIQUE(parent_id, slug) would happily allow two
-- top-level spaces with the same slug, since both have parent_id IS NULL. The path already
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

-- Nothing to add: `user.name` is already UNIQUE from 0001, and slugs are stored in their
-- parsed form, which is lowercase. Normalization is case and only case (core/slug.rs), so the
-- stored text IS the canonical text and a separate folded column would be a copy of it.

-- ---------------------------------------------------------------------------
-- Rename, and why released names are never recycled
-- ---------------------------------------------------------------------------

-- A space that moves or is renamed keeps its old paths working. Every historical path redirects
-- to the space's current one; the space's INTEGER id never changed, so nothing else has to move.
CREATE TABLE IF NOT EXISTS space_path_history (
  old_path      TEXT    PRIMARY KEY,
  space_id      INTEGER NOT NULL REFERENCES space(id),
  changed_at    INTEGER NOT NULL
);

-- Usernames are different, and stricter. A released username must NEVER be reclaimable: every
-- old link, mention and quote that names it would silently start pointing at whoever picked it
-- up. That is an impersonation vector, not a broken link.
--
-- So this table is a tombstone as much as a redirect. A row here blocks the canonical name from
-- being claimed again, permanently, whether or not the original account still exists.
-- `is_reserved` in core covers names nobody may take; this covers names somebody already had.
CREATE TABLE IF NOT EXISTS username_history (
  old_name_canonical TEXT    PRIMARY KEY,
  user_id            INTEGER NOT NULL REFERENCES user(id),
  changed_at         INTEGER NOT NULL,
  -- 1 once the account is gone: the name still cannot be reused, but /u/<name> stops
  -- redirecting rather than pointing at a deleted profile.
  account_deleted    INTEGER NOT NULL DEFAULT 0
);
