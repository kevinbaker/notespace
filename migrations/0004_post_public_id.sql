-- Public ids for posts.
--
-- Every post gets one, at the same 16-character width threads use. An earlier revision argued
-- against this on storage grounds; the argument was wrong, and the correction is worth keeping
-- because it explains the shape of the whole storage design.
--
-- WHY A POST NEEDS AN IDENTITY OF ITS OWN
--
-- A post's address today is (thread_id, path) -- both of which encode "which thread". Splitting
-- and merging threads is routine moderation, and it changes both. A global id survives the move,
-- so a permalink keeps resolving. Same argument as a thread id surviving a retitle, one level
-- down.
--
-- WHAT IT COSTS, AND WHY THAT IS FINE
--
-- Measured at 200k posts: +46 B per post, being ~17 B in the row and ~29 B in the unique index.
-- Against a row that still carries its body that is +2.6%; against a metadata-only row it is
-- +53%, which is the number that matters once bodies move to R2.
--
-- It is affordable anyway because D1 stops being the corpus. Archived threads live in R2, and
-- what stays behind is a stub -- so this overhead applies to the hot working set, not to
-- everything ever posted. See 0005.
--
-- NOT NULLABLE-AND-PROMOTED
--
-- Minting an id on first use would save ~50 B on posts nobody links to, at the cost of a write
-- on the read path and a race between two simultaneous linkers. The saving does not survive
-- archiving anyway. Every post gets one at insert.

ALTER TABLE post ADD COLUMN public_id TEXT;

-- Time-sortable ids append at the index's right edge rather than scattering, which is what makes
-- them cost an integer's lookup time (M0: 2 ms per 5000, same as INTEGER PRIMARY KEY).
CREATE UNIQUE INDEX IF NOT EXISTS idx_post_public_id ON post(public_id);
