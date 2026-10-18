CREATE TABLE ws_tickets (
    digest BYTEA PRIMARY KEY CHECK (octet_length(digest) = 32),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    destination TEXT NOT NULL CHECK (destination IN ('app', 'dashboard')),
    session_expires_at BIGINT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX ws_tickets_expiration ON ws_tickets(expires_at);
