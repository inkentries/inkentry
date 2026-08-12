-- index.db, at its final shape.
--
-- There is no migration ladder. Every index this binary opens was created by
-- this binary at the shape declared here; anything else is discarded and
-- rebuilt, because an index is derived from the user's source tree and
-- reindexing reproduces it exactly (ADR-078's reasoning, applied to the store
-- that has no authored data to protect).
--
-- `usage` is the sole exception and the reason "purely derived" would be wrong:
-- it is accumulated command telemetry that no reindex can reproduce. It is
-- carried across a rebuild.

CREATE TABLE files (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    path       TEXT    UNIQUE NOT NULL,
    language   TEXT,
    hash       TEXT    NOT NULL,  -- blake3 hex; used for incremental re-indexing
    indexed_at INTEGER NOT NULL,  -- unix timestamp
    mtime      INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_files_path ON files(path);

CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id       INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    node_type     TEXT    NOT NULL,  -- "function", "struct", "class", "method", etc.
    name          TEXT,              -- symbol name (NULL for anonymous/fallback chunks)
    start_line    INTEGER NOT NULL,
    end_line      INTEGER NOT NULL,
    content       TEXT    NOT NULL,
    metadata      TEXT,              -- JSON: docstring, parent_scope, etc.
    token_count   INTEGER NOT NULL DEFAULT 0,
    graph_rank    REAL    NOT NULL DEFAULT 0.0,
    summary       TEXT,
    embed_pending INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_chunks_file_id ON chunks(file_id);

-- External-content FTS5 over `chunks`, kept in step by the triggers below.
-- `content_rowid` is why `chunks.id` is an integer key and not something more
-- portable: FTS5 rowids are 64-bit integers by definition.
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    name,
    content,
    node_type,
    content=chunks,
    content_rowid=id
);

CREATE TRIGGER chunks_fts_insert
AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, name, content, node_type)
    VALUES (new.id, new.name, new.content, new.node_type);
END;

CREATE TRIGGER chunks_fts_delete
BEFORE DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, name, content, node_type)
    VALUES ('delete', old.id, old.name, old.content, old.node_type);
END;

CREATE TRIGGER chunks_fts_update
AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, name, content, node_type)
    VALUES ('delete', old.id, old.name, old.content, old.node_type);
    INSERT INTO chunks_fts(rowid, name, content, node_type)
    VALUES (new.id, new.name, new.content, new.node_type);
END;

-- INT8[896], not FLOAT: F2LLM vectors are L2-normalised, so int8 is lossless
-- enough for ranking and four times smaller on disk. `storage/search.rs`
-- rescales the int8 distance back to the f32 scale by INT8_SCALE on read.
CREATE VIRTUAL TABLE embeddings USING vec0(
    chunk_id  INTEGER PRIMARY KEY,
    embedding INT8[896]
);

CREATE TABLE graph_edges (
    id          INTEGER PRIMARY KEY,
    source_file TEXT    NOT NULL,
    source_name TEXT,               -- enclosing function/class, NULL = file-level
    target_name TEXT    NOT NULL,   -- imported module or called/referenced symbol
    kind        TEXT    NOT NULL,   -- 'imports' | 'calls' | 'extends' | 'implements'
    line        INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX graph_edges_source_file      ON graph_edges(source_file);
CREATE INDEX graph_edges_source_name      ON graph_edges(source_name);
CREATE INDEX graph_edges_target_name      ON graph_edges(target_name);
CREATE INDEX graph_edges_kind             ON graph_edges(kind);
CREATE INDEX graph_edges_source_name_kind ON graph_edges(source_name, kind);
CREATE INDEX graph_edges_target_name_kind ON graph_edges(target_name, kind);

CREATE TABLE specs (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    path    TEXT    NOT NULL UNIQUE,   -- path as indexed (relative to project root)
    title   TEXT    NOT NULL DEFAULT '',
    is_auto INTEGER NOT NULL DEFAULT 0 -- 1 = auto-discovered by convention / frontmatter
);

CREATE TABLE spec_links (
    spec_id     INTEGER NOT NULL REFERENCES specs(id) ON DELETE CASCADE,
    linked_path TEXT    NOT NULL,      -- file path or directory prefix (e.g. "src/auth/")
    PRIMARY KEY (spec_id, linked_path)
);

CREATE TABLE conventions (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    language       TEXT    NOT NULL,
    category       TEXT    NOT NULL,
    description    TEXT    NOT NULL,
    confidence     REAL    NOT NULL DEFAULT 0.0,
    evidence_count INTEGER NOT NULL DEFAULT 0,
    extracted_at   INTEGER NOT NULL
);

CREATE INDEX idx_conventions_language ON conventions (language);

CREATE TABLE index_meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);

-- The only authored table in this file. Accumulated command telemetry, which
-- reindexing cannot reproduce, so a rebuild carries it across.
CREATE TABLE usage (
    command   TEXT    NOT NULL,
    called_at INTEGER NOT NULL
);

CREATE INDEX idx_usage_called_at ON usage(called_at);
