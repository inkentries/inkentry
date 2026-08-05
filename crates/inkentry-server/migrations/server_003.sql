-- spelunk-server migration 003
-- Persistent key-value store for server identity and configuration.

CREATE TABLE IF NOT EXISTS server_meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);
