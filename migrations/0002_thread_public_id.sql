-- Public ids for threads.
--
-- Two-tier: `thread.id` stays the INTEGER PRIMARY KEY that every foreign key points at, and
-- `public_id` is the opaque, time-sortable, 16-character id that appears in URLs. Posts get
-- no public id of their own -- they are addressed within their thread -- because a global
-- unique index over every post would cost ~135 MB at 5M posts against a 500 MB ceiling.
--
-- SQLite cannot ADD COLUMN ... UNIQUE, so uniqueness comes from the index below.

ALTER TABLE thread ADD COLUMN public_id TEXT;

-- Time-sortable ids append at the index's right edge instead of scattering, which is what
-- makes them cost what an integer costs (M0: 2 ms per 5000 lookups, same as INTEGER PRIMARY
-- KEY; random ids measured 4 ms).
CREATE UNIQUE INDEX IF NOT EXISTS idx_thread_public_id ON thread(public_id);
