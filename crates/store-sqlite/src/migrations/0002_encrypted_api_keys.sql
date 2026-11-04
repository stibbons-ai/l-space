-- API keys are now stored encrypted. Keys stored by an earlier Bridge are in plain text and
-- are dropped rather than encrypted in place, with the sessions opened with them: their
-- users log in again. Plain text left in the file is handled outside this migration: before
-- migrating from schema version 1, `SqliteStore::open` runs `VACUUM`, which drops the free
-- pages where keys the earlier Bridge deleted may linger (if it fails, nothing is migrated and
-- the next start retries), and it sets `PRAGMA secure_delete = ON`, so the pages this
-- migration frees are zeroed. One gap remains: while the `VACUUM` and this migration run,
-- their rollback journals hold copies of the old pages; each is unlinked after its commit, not
-- zeroed, and a crash leaves it as a hot journal until the next open rolls it back.

DROP TABLE sessions;
DROP TABLE api_keys;

CREATE TABLE api_keys (
    id INTEGER PRIMARY KEY,
    library_server TEXT NOT NULL CHECK (length(library_server) > 0),
    user_id TEXT NOT NULL CHECK (length(user_id) > 0),
    -- HMAC-SHA256 of the key under a key derived from the encryption key: what keys are
    -- compared and looked up by, without decrypting any.
    key_fingerprint BLOB NOT NULL CHECK (length(key_fingerprint) = 32),
    -- The key encrypted (XChaCha20-Poly1305) for this row: a 24-byte nonce, then the
    -- ciphertext and its 16-byte tag.
    sealed_key BLOB NOT NULL CHECK (length(sealed_key) >= 40),
    -- Names the encryption key and scheme the row was sealed with.
    key_id BLOB NOT NULL CHECK (length(key_id) = 16),
    UNIQUE (library_server, user_id, key_fingerprint)
) STRICT;

CREATE TABLE sessions (
    -- SHA-256 of the session token; the token itself is never stored.
    id BLOB PRIMARY KEY CHECK (length(id) = 32),
    api_key_id INTEGER NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE
) STRICT;

CREATE INDEX sessions_by_api_key ON sessions (api_key_id);
