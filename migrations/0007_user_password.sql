-- Local password credentials.
--
-- Nullable, because an account is not required to have a password: an OIDC-backed user never
-- has one, and that is the intended path on the Workers target. NULL means "cannot log in with
-- a password", not "empty password" -- the login handler must treat it as a failure without
-- ever comparing against it.

PRAGMA foreign_keys = ON;

-- PHC string, prefixed with the id of the pepper that made it: `<id>$argon2id$v=19$...`.
-- The parameters and the pepper id both travel with the hash so that raising either is a
-- rehash-on-next-login rather than a migration that cannot work (the plaintext is gone).
ALTER TABLE user ADD COLUMN password_hash TEXT;
