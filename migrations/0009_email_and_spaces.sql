-- Email addresses, the tokens that prove them, and the backfill that makes space listings work.
--
-- An address is optional. It exists for two things a forum cannot do without one -- password
-- reset and notification -- and for nothing else; nothing on the read path selects it.
--
-- UNIQUENESS IS ON VERIFIED ADDRESSES ONLY. Anyone can type anyone's address into a signup
-- form, so a unique index over every stored address would let a stranger squat on one, and a
-- signup that says "that address is taken" is an oracle over every member's email. The partial
-- index below lets any number of accounts CLAIM an address and exactly one PROVE it; the claim
-- is worthless until the link in the mail is followed.

PRAGMA foreign_keys = ON;

-- Stored lowercase; matched exactly.
ALTER TABLE user ADD COLUMN email TEXT;
ALTER TABLE user ADD COLUMN email_verified_at INTEGER;

CREATE UNIQUE INDEX IF NOT EXISTS idx_user_email_verified
  ON user(email) WHERE email_verified_at IS NOT NULL;

-- One row per link sent. The same shape as `session`: the mail carries 256 random bits and the
-- table holds their SHA-256, so a leaked database or a Time Travel snapshot yields no usable link.
CREATE TABLE IF NOT EXISTS email_token (
  token_hash    TEXT    PRIMARY KEY,
  user_id       INTEGER NOT NULL REFERENCES user(id),
  kind          TEXT    NOT NULL,   -- verify|reset
  -- The address the link was sent to. A verify token proves THIS address, not whatever the
  -- account's address is by the time the link is followed.
  email         TEXT    NOT NULL,
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  -- Single use. Set by the one statement that consumes the token, so two clicks cannot both win.
  used_at       INTEGER
);

-- Issuing a new link retires the account's older ones of the same kind.
CREATE INDEX IF NOT EXISTS idx_email_token_user ON email_token(user_id, kind);
CREATE INDEX IF NOT EXISTS idx_email_token_expires ON email_token(expires_at);

-- ---------------------------------------------------------------------------
-- Space listings
-- ---------------------------------------------------------------------------
--
-- 0003 added `thread.space_path` for subtree scans but nothing before now wrote it for seeded
-- threads. Fill it from the space, so `/s/{path}` can range-scan rather than join.
UPDATE thread SET space_path = (SELECT path FROM space WHERE space.id = thread.space_id)
  WHERE space_path IS NULL;
