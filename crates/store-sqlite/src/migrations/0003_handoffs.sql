-- Active Handoffs, at most one per Group. Each acts with an API key stored in `api_keys`, as a
-- session does, and goes with it when the key is revoked or no longer decrypts (`ON DELETE
-- CASCADE`, with `PRAGMA foreign_keys = ON` as `SqliteStore::open` sets). Ending a session
-- leaves its key, and so the Handoffs acting with it.

CREATE TABLE handoffs (
    -- The Group's id (its coordinator's UUID): one Handoff per Group.
    group_id TEXT PRIMARY KEY CHECK (length(group_id) > 0),
    -- SHA-256 of the random id in the stream's URL; the id itself is never stored.
    stream_id BLOB NOT NULL UNIQUE CHECK (length(stream_id) = 32),
    library_item TEXT NOT NULL CHECK (length(library_item) > 0),
    episode TEXT CHECK (episode IS NULL OR length(episode) > 0),
    file_ino TEXT NOT NULL CHECK (length(file_ino) > 0),
    file_name TEXT NOT NULL CHECK (length(file_name) > 0),
    -- The starter's username when they started it; who they are is the key's row.
    username TEXT NOT NULL,
    api_key_id INTEGER NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE,
    -- Milliseconds since the Unix epoch.
    started_at INTEGER NOT NULL
) STRICT;

CREATE INDEX handoffs_by_api_key ON handoffs (api_key_id);
