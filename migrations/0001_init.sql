-- notespace initial schema.
--
-- Dialect: SQLite, and *only* SQLite. This file runs unmodified against both Cloudflare D1
-- and a native SQLite file (DESIGN.md §3.1). No Postgres-isms, ever.
--
-- M0 covers the read path for a thread page, so this is the space/thread/post/user subset
-- plus the indexes the read path depends on. signal, action_log, capability, rule,
-- review_queue and the FTS5 table arrive in M1-M4.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS user (
  id            INTEGER PRIMARY KEY,
  name          TEXT    NOT NULL UNIQUE,
  created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS space (
  id            INTEGER PRIMARY KEY,
  slug          TEXT    NOT NULL UNIQUE,
  name          TEXT    NOT NULL,
  parent_id     INTEGER REFERENCES space(id),
  ranking       TEXT    NOT NULL DEFAULT 'bump',   -- bump|gravity|best|score_threshold
  depth_cap     INTEGER NOT NULL DEFAULT 8,        -- 0 = flat board
  config        TEXT    NOT NULL DEFAULT '{}'      -- JSON: preset overrides
);

CREATE TABLE IF NOT EXISTS thread (
  id            INTEGER PRIMARY KEY,
  space_id      INTEGER NOT NULL REFERENCES space(id),
  kind          TEXT    NOT NULL DEFAULT 'discussion',
  title         TEXT    NOT NULL,
  url           TEXT,                              -- link threads only
  author_id     INTEGER NOT NULL REFERENCES user(id),
  created_at    INTEGER NOT NULL,
  bumped_at     INTEGER NOT NULL,
  post_count    INTEGER NOT NULL DEFAULT 0,        -- denormalized; avoids COUNT(*) on read
  score         REAL    NOT NULL DEFAULT 0,
  rank          REAL    NOT NULL DEFAULT 0,        -- denormalized, recomputed on write
  state         TEXT    NOT NULL DEFAULT 'visible',-- visible|locked|pinned|hidden|deleted
  cache_version INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_thread_space_rank ON thread(space_id, state, rank DESC);

CREATE TABLE IF NOT EXISTS post (
  id            INTEGER PRIMARY KEY,
  thread_id     INTEGER NOT NULL REFERENCES thread(id),
  parent_id     INTEGER REFERENCES post(id),
  -- Materialized path: fixed-width zero-padded ordinals, '.'-separated.
  -- Lexicographic order == tree preorder. See crates/core/src/path.rs.
  path          TEXT    NOT NULL,
  depth         INTEGER NOT NULL,
  author_id     INTEGER NOT NULL REFERENCES user(id),
  body_md       TEXT    NOT NULL,                  -- source of truth
  body_html     TEXT    NOT NULL,                  -- rendered + sanitized at write time
  created_at    INTEGER NOT NULL,
  edited_at     INTEGER,
  score         REAL    NOT NULL DEFAULT 0,
  state         TEXT    NOT NULL DEFAULT 'visible' -- visible|pending|hidden|deleted
);

-- THE read-path index. A thread page is one range scan over this:
--   WHERE thread_id = ? AND path > ? ORDER BY path LIMIT ?
-- Covering `author_id` here would let the join be served without touching the table, but
-- D1 bills rows read either way; measure before adding columns.
CREATE UNIQUE INDEX IF NOT EXISTS idx_post_thread_path ON post(thread_id, path);
CREATE INDEX IF NOT EXISTS idx_post_author ON post(author_id, created_at DESC);
