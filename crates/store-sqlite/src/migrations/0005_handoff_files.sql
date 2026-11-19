-- A Handoff now holds all of its Item's audio files, in playing order, each with its length
-- when the Library Server told it (#19). A Handoff stored before keeps its one file, of
-- unknown length. A Handoff's files go with it (`ON DELETE CASCADE`, with `PRAGMA
-- foreign_keys = ON` as `SqliteStore::open` sets), also when its key goes.

CREATE TABLE handoff_files (
    stream_id BLOB NOT NULL REFERENCES handoffs (stream_id) ON DELETE CASCADE,
    -- Its place in playing order, from 0.
    file_index INTEGER NOT NULL CHECK (file_index >= 0),
    ino TEXT NOT NULL CHECK (length(ino) > 0),
    name TEXT NOT NULL CHECK (length(name) > 0),
    -- Its length in nanoseconds; NULL if unknown.
    duration_ns INTEGER CHECK (duration_ns IS NULL OR duration_ns >= 0),
    PRIMARY KEY (stream_id, file_index)
) STRICT;

INSERT INTO handoff_files (stream_id, file_index, ino, name, duration_ns)
    SELECT stream_id, 0, file_ino, file_name, NULL FROM handoffs;

ALTER TABLE handoffs DROP COLUMN file_ino;
ALTER TABLE handoffs DROP COLUMN file_name;
