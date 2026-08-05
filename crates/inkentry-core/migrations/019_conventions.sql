-- Convention records extracted by the heuristic AST pass.
-- Fully replaced after each index run: DELETE + re-insert.

CREATE TABLE IF NOT EXISTS conventions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    language       TEXT    NOT NULL,
    category       TEXT    NOT NULL,
    description    TEXT    NOT NULL,
    confidence     REAL    NOT NULL DEFAULT 0.0,
    evidence_count INTEGER NOT NULL DEFAULT 0,
    extracted_at   INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_conventions_language
    ON conventions (language);
