-- Code full-text index, rebuilt for retrieval (ADR-103).
--
-- `chunks_fts` was an external-content table over `chunks(name, content,
-- node_type)` with the default `unicode61` tokenizer. It becomes a
-- contentless table over name, file path, docstring, structural summary and
-- content, stemmed with `porter`. Contentless because four of the five
-- columns are derived rather than copied: nothing needs to read a value back
-- out of the index, and `contentless_delete` lets the triggers remove a row
-- by rowid alone instead of having to reproduce the exact values it was
-- indexed with.
--
-- `name_words` / `body_words` / `path_words` hold the parts of compound
-- identifiers (`LinearRag` -> `linear rag`, `InvoiceDetails.tsx` ->
-- `invoice details`), which the tokenizer cannot produce. They are computed
-- in Rust (`search::lexical::identifier_subwords`) when a chunk or file is
-- written; the step that runs this file backfills them for existing rows,
-- and the chunk backfill is what populates the new table.

ALTER TABLE files ADD COLUMN path_words TEXT;
ALTER TABLE chunks ADD COLUMN name_words TEXT;
ALTER TABLE chunks ADD COLUMN body_words TEXT;

DROP TRIGGER IF EXISTS chunks_fts_insert;
DROP TRIGGER IF EXISTS chunks_fts_delete;
DROP TRIGGER IF EXISTS chunks_fts_update;
DROP TABLE IF EXISTS chunks_fts;

CREATE VIRTUAL TABLE chunks_fts USING fts5(
    name,
    path,
    doc,
    summary,
    content,
    tokenize = 'porter unicode61',
    content = '',
    contentless_delete = 1
);

CREATE TRIGGER chunks_fts_insert
AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, name, path, doc, summary, content)
    VALUES (
        new.id,
        coalesce(new.name, '') || ' ' || coalesce(new.name_words, ''),
        (SELECT path || ' ' || coalesce(path_words, '') FROM files WHERE id = new.file_id),
        CASE WHEN json_valid(new.metadata) THEN json_extract(new.metadata, '$.docstring') END,
        new.summary,
        new.content || ' ' || coalesce(new.body_words, '')
    );
END;

CREATE TRIGGER chunks_fts_delete
AFTER DELETE ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.id;
END;

-- Only the columns the index reads. `graph_rank`, `embed_pending` and
-- `token_count` are rewritten on every index run; an unqualified UPDATE
-- trigger re-tokenised every chunk each time.
CREATE TRIGGER chunks_fts_update
AFTER UPDATE OF file_id, name, content, metadata, summary, name_words, body_words ON chunks BEGIN
    DELETE FROM chunks_fts WHERE rowid = old.id;
    INSERT INTO chunks_fts(rowid, name, path, doc, summary, content)
    VALUES (
        new.id,
        coalesce(new.name, '') || ' ' || coalesce(new.name_words, ''),
        (SELECT path || ' ' || coalesce(path_words, '') FROM files WHERE id = new.file_id),
        CASE WHEN json_valid(new.metadata) THEN json_extract(new.metadata, '$.docstring') END,
        new.summary,
        new.content || ' ' || coalesce(new.body_words, '')
    );
END;
