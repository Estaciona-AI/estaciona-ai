ALTER TABLE spots ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT TRUE;
CREATE TABLE site_map_revisions (
    revision BIGSERIAL PRIMARY KEY,
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
    manifest JSONB NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
