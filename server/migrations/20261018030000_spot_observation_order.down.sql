-- Downgrade deliberately loses observation ordering history.
ALTER TABLE spots DROP COLUMN observation_timestamp;
