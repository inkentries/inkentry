-- Schema version 13 (ADR-098 D5/D6): a local event log, and optional origin
-- columns on `notes`.
--
-- `events` is local working state in the same sense as any other projection
-- detail: it is never written to the git-notes carrier and no sync path reads
-- it (D5). `inkentry metrics clear` empties it. The index on `at` is what
-- every reader (the 7-day usage summary, the metrics snapshot's events block)
-- filters on.
CREATE TABLE events (
    at             INTEGER NOT NULL,
    command        TEXT    NOT NULL,
    surface        TEXT    NOT NULL,
    trigger        TEXT    NOT NULL,
    actor_kind     TEXT    NOT NULL,
    session_ref    TEXT,
    code_results   INTEGER,
    memory_results INTEGER,
    returned_ids   TEXT,
    tokens_out     INTEGER,
    latency_ms     INTEGER,
    ok             INTEGER NOT NULL
);
CREATE INDEX idx_events_at ON events(at);

-- `origin` (D6): who or what produced an entry. Nullable and additive — an
-- entry written before this step, or by an older binary, reads every column
-- NULL, which `notes.rs`'s row mappers already treat as "no origin recorded"
-- the same way an absent carrier key does.
ALTER TABLE notes ADD COLUMN origin_actor_kind TEXT;
ALTER TABLE notes ADD COLUMN origin_tool TEXT;
ALTER TABLE notes ADD COLUMN origin_model TEXT;
