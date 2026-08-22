-- Sessions (DESIGN.md §4.9).
--
-- In D1 rather than Workers KV, which is what Cloudflare recommends for session data. Their
-- recommendation assumes data that is read at thousands of RPS, rarely written, and tolerant of
-- stale reads. Two of those are false here: KV's free plan allows 1,000 writes/day against D1's
-- 100,000 rows, and KV changes take "up to 60 seconds or more" to propagate -- so a logout or a
-- ban would keep working for a minute somewhere in the world.
--
-- The deciding reason is neither: there is no KV in a self-hosted binary, and a second session
-- implementation for the native target is exactly the drift the Store seam exists to prevent.
-- A table is just SQL.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS session (
  -- SHA-256 of the token, hex. THE TOKEN ITSELF IS NEVER STORED.
  --
  -- Not paranoia about the live database so much as about its copies: backups, and the seven
  -- days of Time Travel snapshots D1 keeps on the free plan. A hash means none of those yield a
  -- usable session.
  token_hash    TEXT    PRIMARY KEY,
  user_id       INTEGER NOT NULL REFERENCES user(id),
  created_at    INTEGER NOT NULL,
  -- Last time the expiry was pushed out. Sliding expiry refreshes at most once a day rather
  -- than on every request -- the difference between one write per user per day and one write
  -- per pageview, which is what makes a session table affordable at all.
  refreshed_at  INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL
);

-- Lookup is by primary key, so no index is needed for the hot path. These two are for the cold
-- ones: "log out everywhere" after a password change, and sweeping expired rows.
CREATE INDEX IF NOT EXISTS idx_session_user ON session(user_id);
CREATE INDEX IF NOT EXISTS idx_session_expires ON session(expires_at);
