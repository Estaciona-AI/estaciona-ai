-- Who a spot is kept for comes from the installation's map, not from
-- hard-coded spot names.
ALTER TABLE spots ADD COLUMN reserved_for TEXT
    CHECK (reserved_for IN ('pcd', 'elderly'));
