-- Older servers do not consult this registry. Dropping unexpired revocations
-- would make their signed tokens valid again. Rollback requires an explicit
-- signing-key rotation/reconciliation, or waiting for all sessions to expire.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM auth_sessions WHERE expires_at > NOW()) THEN
        RAISE EXCEPTION 'Cannot downgrade while registered sessions are unexpired; rotate the JWT signing key and reconcile sessions explicitly, or wait for expiry';
    END IF;
END
$$;
-- Never turn tickets from revoked sessions into unrevokable legacy tickets.
DELETE FROM ws_tickets;
ALTER TABLE ws_tickets DROP COLUMN session_id;
DROP TABLE auth_sessions;
