-- Accounts live in the central identity provider. This database keeps the
-- provider subject, the generated username and the encrypted display name;
-- e-mail addresses and password hashes are dropped. Rows created before have
-- no subject and cannot sign in again (no real users existed on 25/10/2026).
ALTER TABLE users
    DROP COLUMN email,
    DROP COLUMN password_hash,
    DROP COLUMN name,
    ADD COLUMN oidc_issuer TEXT,
    ADD COLUMN oidc_subject TEXT,
    ADD COLUMN username TEXT,
    ADD COLUMN display_name_enc BYTEA,
    ADD CONSTRAINT users_identity_complete CHECK ((oidc_issuer IS NULL) = (oidc_subject IS NULL)),
    ADD CONSTRAINT users_role_known CHECK (role IN ('user', 'admin'));
CREATE UNIQUE INDEX users_oidc_identity ON users (oidc_issuer, oidc_subject);

-- Back-channel logout names the provider session to end.
ALTER TABLE auth_sessions ADD COLUMN idp_session_id TEXT;
CREATE INDEX auth_sessions_idp_session ON auth_sessions (idp_session_id)
    WHERE idp_session_id IS NOT NULL;

-- A sign-in in progress: single use, short-lived, keyed by the digest of the
-- state that the browser also holds in a cookie.
CREATE TABLE oidc_login_attempts (
    state_digest BYTEA PRIMARY KEY CHECK (octet_length(state_digest) = 32),
    pkce_verifier TEXT NOT NULL,
    nonce TEXT NOT NULL,
    return_path TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX oidc_login_attempts_expiration ON oidc_login_attempts (expires_at);
