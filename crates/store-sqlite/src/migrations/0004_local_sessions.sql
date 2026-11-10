-- Local sessions (Local playback), at most one per App session. Each acts with its session's
-- API key and goes with the session: on logout, and when the key is revoked or no longer
-- decrypts (which deletes the key's sessions), through `ON DELETE CASCADE` with `PRAGMA
-- foreign_keys = ON` as `SqliteStore::open` sets.

CREATE TABLE local_sessions (
    -- SHA-256 of the owning session's token, as `sessions.id`: one Local session per session.
    session_id BLOB PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
    -- SHA-256 of the random id in the stream's URL; the id itself is never stored.
    stream_id BLOB NOT NULL UNIQUE CHECK (length(stream_id) = 32),
    library_item TEXT NOT NULL CHECK (length(library_item) > 0),
    episode TEXT CHECK (episode IS NULL OR length(episode) > 0),
    file_ino TEXT NOT NULL CHECK (length(file_ino) > 0),
    file_name TEXT NOT NULL CHECK (length(file_name) > 0),
    -- Milliseconds since the Unix epoch.
    started_at INTEGER NOT NULL
) STRICT;
