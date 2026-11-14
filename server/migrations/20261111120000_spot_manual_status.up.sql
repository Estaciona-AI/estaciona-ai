-- When an administrator set the spot's status by hand. A newer reading, a
-- reservation or its release clears it; until then apps are told the status
-- is manual, and it does not go stale like a reading.
ALTER TABLE spots ADD COLUMN manual_since TIMESTAMPTZ;
