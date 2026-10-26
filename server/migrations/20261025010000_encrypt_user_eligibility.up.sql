-- Eligibility for reserved spots is personal and, for disability, sensitive.
-- The server stores AES-256-GCM envelopes; the key never reaches the database.
-- Values kept in clear are discarded: no real users existed (25/10/2026).
ALTER TABLE users
    DROP COLUMN date_of_birth,
    DROP COLUMN pcd_status,
    ADD COLUMN date_of_birth_enc BYTEA,
    ADD COLUMN pcd_status_enc BYTEA;
