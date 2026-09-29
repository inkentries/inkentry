-- Schema version 15: what `memory add` did about the entries near the one it
-- was asked to write.
--
-- `reconcile` is the mode in force (`off` | `block`) and `resolution` is how
-- the write ended (`supersedes` | `relates_to` | `contradicts` | `distinct` |
-- `abandoned`). Both are NULL on every command that is not a `memory add`,
-- on every row written before this step, and on a write that carried no
-- resolution. Both are plain TEXT with no CHECK: a later build may record a
-- new value, and an older one reading it must not fail.
ALTER TABLE events ADD COLUMN reconcile TEXT;
ALTER TABLE events ADD COLUMN resolution TEXT;
