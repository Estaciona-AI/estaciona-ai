CREATE TABLE auth_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX auth_sessions_expiration ON auth_sessions(expires_at);
CREATE INDEX auth_sessions_user ON auth_sessions(user_id);
-- Tickets issued before session registration cannot be safely assigned an owner session.
DELETE FROM ws_tickets;
ALTER TABLE ws_tickets ADD COLUMN session_id UUID NOT NULL REFERENCES auth_sessions(id) ON DELETE CASCADE;
CREATE INDEX ws_tickets_session ON ws_tickets(session_id);
