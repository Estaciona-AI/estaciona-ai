-- A back-channel logout ends sessions that exist; this mark also refuses a
-- sign-in whose ID token the provider issued before that logout, when the
-- callback is still in flight. Kept for the longest session lifetime.
CREATE TABLE oidc_logout_marks (
    issuer TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('sid', 'sub')),
    value TEXT NOT NULL,
    logged_out_at TIMESTAMPTZ NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (issuer, kind, value)
);
CREATE INDEX oidc_logout_marks_expiration ON oidc_logout_marks (expires_at);
