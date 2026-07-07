-- Key-value metadata for the local index.db.
-- Records embedding provenance (embedding_model, embedding_dim) so a
-- same-dim successor model cannot silently corrupt the KNN space.
CREATE TABLE IF NOT EXISTS index_meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
);
