-- Each Audiobookshelf user's API key per Library Server, and the App sessions opened with it.
-- A key is replaced by deleting its row, so the sessions opened with it go too.

CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY,
    library_server TEXT NOT NULL,
    user_id TEXT NOT NULL,
    api_key TEXT NOT NULL,
    UNIQUE (library_server, user_id)
) STRICT;

CREATE TABLE sessions (
    -- SHA-256 of the session token; the token itself is never stored.
    id BLOB PRIMARY KEY,
    api_key_id INTEGER NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE
) STRICT;

CREATE INDEX sessions_by_api_key ON sessions (api_key_id);
