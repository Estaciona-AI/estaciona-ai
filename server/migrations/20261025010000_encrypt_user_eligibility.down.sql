-- Envelopes cannot be decrypted in SQL; declared values are not restored.
ALTER TABLE users
    DROP COLUMN date_of_birth_enc,
    DROP COLUMN pcd_status_enc,
    ADD COLUMN date_of_birth DATE,
    ADD COLUMN pcd_status BOOLEAN DEFAULT FALSE;
