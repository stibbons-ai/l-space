-- Set before a file seek is sent, cleared only after its outcome is known safe. If the
-- process or cleanup fails, controls and replacement Progress must never trust this row,
-- even after reopening. Its stream remains available while the seek is in flight.
ALTER TABLE handoffs ADD COLUMN seeking INTEGER NOT NULL DEFAULT 0 CHECK (seeking IN (0, 1));
