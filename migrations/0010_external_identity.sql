-- Accounts that sign in through an identity provider (Google, GitHub, ...) rather than with a
-- password. The account row is the same `user` as anyone else's, with `password_hash` NULL;
-- this table is the link from the provider's stable id to it.
--
-- Keyed on (provider, subject), never on email: an email address can change hands at the
-- provider, a subject cannot. The address is kept for the record and copied onto the account
-- at link time, verified if the provider vouched for it.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS external_identity (
  provider      TEXT    NOT NULL,   -- google|github|...
  subject       TEXT    NOT NULL,   -- the provider's id for the account
  user_id       INTEGER NOT NULL REFERENCES user(id),
  email         TEXT,
  created_at    INTEGER NOT NULL,
  last_login_at INTEGER NOT NULL,
  PRIMARY KEY (provider, subject)
);

-- The settings page lists an account's providers; a deletion needs the reverse lookup too.
CREATE INDEX IF NOT EXISTS idx_external_identity_user ON external_identity(user_id);
