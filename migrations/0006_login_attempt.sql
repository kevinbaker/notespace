-- Login rate limiting.
--
-- The defence against online guessing, which the KDF does nothing about: an attacker pays
-- nothing for a wrong guess, and we pay 3.34 ms of a 10 ms CPU budget. Unthrottled login is
-- therefore both a credential attack and a cheap way to exhaust the account's CPU.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS login_attempt (
  -- Namespaced: "id:<username>" or "ip:<address>". Two buckets, because hammering one account
  -- and spraying one password across many are different attacks with different thresholds.
  key           TEXT    PRIMARY KEY,
  -- Fixed window rather than sliding. Sliding needs a row per attempt; fixed needs one counter,
  -- and its worst case is straddling a boundary for 2x the limit in quick succession -- which
  -- for a limit of 5 is 10, and changes nothing.
  window_start  INTEGER NOT NULL,
  count         INTEGER NOT NULL
);

-- For sweeping stale rows. Without it the table grows one row per attacker address, forever.
CREATE INDEX IF NOT EXISTS idx_login_attempt_window ON login_attempt(window_start);
