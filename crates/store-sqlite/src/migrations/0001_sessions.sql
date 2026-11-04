-- The Audiobookshelf API keys users logged in with, and the App sessions opened with each.
-- Revoking a key deletes its row, and the sessions opened with it go too: this relies on the
-- connection having `PRAGMA foreign_keys = ON`, which `SqliteStore::open` sets.

CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY,
    library_server TEXT NOT NULL CHECK (length(library_server) > 0),
    user_id TEXT NOT NULL CHECK (length(user_id) > 0),
    -- SHA-256 of the key: what keys are compared and looked up by, so the stored key itself
    -- can be sealed (encrypted) without changing any query.
    key_fingerprint BLOB NOT NULL CHECK (length(key_fingerprint) = 32),
    sealed_key BLOB NOT NULL CHECK (length(sealed_key) > 0),
    UNIQUE (library_server, user_id, key_fingerprint)
) STRICT;

CREATE TABLE sessions (
    -- SHA-256 of the session token; the token itself is never stored.
    id BLOB PRIMARY KEY CHECK (length(id) = 32),
    api_key_id INTEGER NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE
) STRICT;

CREATE INDEX sessions_by_api_key ON sessions (api_key_id);
