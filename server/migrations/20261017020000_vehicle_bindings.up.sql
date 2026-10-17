CREATE TABLE vehicles (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    plate_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE vehicle_bindings (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    vehicle_id UUID NOT NULL REFERENCES vehicles(id),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','approved','revoked')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (vehicle_id, user_id)
);
CREATE UNIQUE INDEX one_approved_vehicle_binding
    ON vehicle_bindings(vehicle_id) WHERE status = 'approved';
CREATE INDEX vehicle_bindings_user ON vehicle_bindings(user_id);
CREATE INDEX vehicle_bindings_status ON vehicle_bindings(status, id);
-- Audit facts retain opaque IDs after account/binding deletion. No plaintext plate.
CREATE TABLE vehicle_binding_audit (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    binding_id UUID NOT NULL,
    vehicle_id UUID NOT NULL,
    subject_user_id UUID NOT NULL,
    actor_id UUID,
    action TEXT NOT NULL CHECK (action IN ('requested','approved','revoked','reassigned')),
    reason TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX vehicle_binding_audit_binding ON vehicle_binding_audit(binding_id, occurred_at);
INSERT INTO vehicles (plate_hash) SELECT plate FROM users WHERE plate IS NOT NULL;
INSERT INTO vehicle_bindings (vehicle_id, user_id)
    SELECT vehicles.id, users.id FROM users JOIN vehicles ON vehicles.plate_hash = users.plate;
INSERT INTO vehicle_binding_audit (binding_id, vehicle_id, subject_user_id, action, reason)
    SELECT id, vehicle_id, user_id, 'requested', 'legacy_unverified' FROM vehicle_bindings;
-- A declaration is not exclusive ownership. Only approved bindings are unique.
ALTER TABLE users DROP CONSTRAINT users_plate_key;
