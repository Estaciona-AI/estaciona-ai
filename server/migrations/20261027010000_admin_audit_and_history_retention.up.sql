-- Administrative actions of this installation: who did what to which target.
-- No foreign key: the trail outlives the accounts it mentions. The runtime
-- role may only append (infra/app-role.sql).
CREATE TABLE admin_audit_log (
    id BIGSERIAL PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    actor_user_id UUID NOT NULL,
    action TEXT NOT NULL CHECK (action ~ '^[a-z_]{1,64}$'),
    target TEXT,
    details JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX admin_audit_log_occurred ON admin_audit_log (occurred_at);

-- Personal parking history expires by age (see sessions::cleanup_expired).
CREATE INDEX user_occupancy_history_occupied_at ON user_occupancy_history (occupied_at);
