-- Moderation: reports, the action log, and the review queue.
--
-- Three of the eight primitives arrive here. A REPORT IS A SIGNAL (negative weight, routed to
-- review) rather than its own table; an ACTION LOG ENTRY is immutable and optionally public,
-- which is what makes a modlog possible; and the REVIEW QUEUE is one row per post awaiting a
-- human, whatever put it there.
--
-- Per-space thresholds -- how many reports hold a post, how confident the model must be to
-- publish -- live in `space.config` as JSON, not in columns. A preset is a config bundle, and a
-- preset that needs a column is a hole in the model.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Roles
-- ---------------------------------------------------------------------------
--
-- The smallest capability system that gates a review queue. member|moderator|admin. Earned
-- trust (HN karma, Discourse trust levels) comes later and is a different column; this one is
-- GRANTED.
ALTER TABLE user ADD COLUMN role TEXT NOT NULL DEFAULT 'member';

-- ---------------------------------------------------------------------------
-- Signals
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS signal (
  id            INTEGER PRIMARY KEY,
  post_id       INTEGER NOT NULL REFERENCES post(id),
  user_id       INTEGER NOT NULL REFERENCES user(id),
  -- 'report' for now; votes and reactions are the same shape and arrive with ranking.
  kind          TEXT    NOT NULL,
  weight        REAL    NOT NULL DEFAULT 1,
  -- Free text for a report, a label for a Slashdot-style mod. Nullable: an upvote has none.
  reason        TEXT,
  created_at    INTEGER NOT NULL
);

-- One signal of a kind per user per post. A second report from the same account is a repeat,
-- not a second report, and the UNIQUE index is what makes the endpoint idempotent.
CREATE UNIQUE INDEX IF NOT EXISTS idx_signal_once ON signal(post_id, user_id, kind);

-- ---------------------------------------------------------------------------
-- Action log
-- ---------------------------------------------------------------------------
--
-- Append-only. Nothing updates or deletes a row here; a reversal is a new row.

CREATE TABLE IF NOT EXISTS action_log (
  id            INTEGER PRIMARY KEY,
  -- user|model|rule|system. The model is an actor like any other, and its calls are logged
  -- with the same shape a moderator's are -- that is what lets a human rate them later.
  actor_kind    TEXT    NOT NULL,
  actor_id      INTEGER REFERENCES user(id),   -- users only
  -- Display name for the log: a username, a model id, or a rule name.
  actor_name    TEXT    NOT NULL,
  target_kind   TEXT    NOT NULL,              -- post|thread|user
  target_id     INTEGER NOT NULL,
  -- hold|publish|hide|restore|report|approve|reject|appeal|classify|...
  action        TEXT    NOT NULL,
  -- JSON. For a model call: verdict, confidence, categories, rationale, model, prompt version.
  -- Never rendered on the public log, which shows only the action and the target.
  detail        TEXT    NOT NULL DEFAULT '{}',
  public        INTEGER NOT NULL DEFAULT 1,
  created_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_action_log_time ON action_log(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_action_log_target ON action_log(target_kind, target_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Review queue
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS review_item (
  id            INTEGER PRIMARY KEY,
  -- One open item per post, whatever the number of reasons. Reopening an item resolved earlier
  -- updates the row rather than adding one, so the queue has no duplicates to de-duplicate.
  post_id       INTEGER NOT NULL UNIQUE REFERENCES post(id),
  space_id      INTEGER NOT NULL REFERENCES space(id),
  -- Why it is here: classifier|reports|rule|appeal|error.
  reason        TEXT    NOT NULL,
  -- What the model said, when it said anything. Kept on the item so the reviewer sees it
  -- without a join into the log, and so agreement can be computed when the human decides.
  model_verdict     TEXT,                       -- clean|flag|unsure
  model_confidence  REAL,
  model_categories  TEXT,                       -- JSON array of category names
  appeal_text   TEXT,
  opened_at     INTEGER NOT NULL,
  state         TEXT    NOT NULL DEFAULT 'open', -- open|resolved
  resolution    TEXT,                            -- approve|reject
  resolved_by   INTEGER REFERENCES user(id),
  resolved_at   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_review_open ON review_item(state, opened_at);
-- Metamoderation reads this: resolved items in a space, with model verdict and resolution.
CREATE INDEX IF NOT EXISTS idx_review_space_state ON review_item(space_id, state);

-- The consumer's safety net. The Queue is an accelerator; a scheduled sweep over posts still
-- pending is what guarantees nothing waits forever, and it needs this to be a range scan.
CREATE INDEX IF NOT EXISTS idx_post_state_time ON post(state, created_at);
