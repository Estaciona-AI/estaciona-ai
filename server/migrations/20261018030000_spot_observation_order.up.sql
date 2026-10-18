-- Watermark belongs to the spot row and is serialized by its existing lock.
ALTER TABLE spots ADD COLUMN observation_timestamp TIMESTAMPTZ;
