-- Refuse to make retired spots operational implicitly during downgrade.
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM spots WHERE NOT is_active) THEN
        RAISE EXCEPTION 'Restore a map containing all retained spots before downgrade';
    END IF;
END $$;
DROP TABLE site_map_revisions;
ALTER TABLE spots DROP COLUMN is_active;
