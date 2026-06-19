-- ADR-037 D2: per-project sync watermark.
--
-- `spelunk sync` / `memory pull` persist the high-water mark of the last
-- successful pull (and the last push) so subsequent runs only transfer the
-- delta. Keyed by the server-side project identifier so a single local store
-- that syncs against multiple projects keeps independent cursors.
--
-- Timestamps are RFC 3339 / ISO 8601 strings to match cloud-api's
-- `created_at` wire format (the `since` endpoint takes and returns ISO 8601),
-- avoiding lossy epoch round-tripping.
--
-- Privacy (ADR-005 / ADR-037 security): the cursor stores only a project id and
-- timestamps — never entry content or any PII.

CREATE TABLE IF NOT EXISTS sync_state (
    project_id    TEXT PRIMARY KEY,
    last_synced   TEXT,            -- ISO 8601 watermark of the last pull
    last_pushed   TEXT,            -- ISO 8601 watermark of the last push
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);
