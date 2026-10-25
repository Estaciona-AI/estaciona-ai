-- Local credentials are not restored: accounts need a new registration.
DROP TABLE oidc_login_attempts;
DROP INDEX auth_sessions_idp_session;
ALTER TABLE auth_sessions DROP COLUMN idp_session_id;
DROP INDEX users_oidc_identity;
ALTER TABLE users
    DROP CONSTRAINT users_role_known,
    DROP CONSTRAINT users_identity_complete,
    DROP COLUMN display_name_enc,
    DROP COLUMN username,
    DROP COLUMN oidc_subject,
    DROP COLUMN oidc_issuer,
    ADD COLUMN name VARCHAR(255),
    ADD COLUMN password_hash VARCHAR(255),
    ADD COLUMN email VARCHAR(255) UNIQUE;
