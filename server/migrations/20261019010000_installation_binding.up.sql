-- Explicit ownership of this dedicated database. Never infer or seed identity.
CREATE TABLE public.installation_binding (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    organization_id UUID NOT NULL CHECK (organization_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    installation_id UUID NOT NULL CHECK (installation_id <> '00000000-0000-0000-0000-000000000000'::uuid),
    environment TEXT NOT NULL CHECK (environment IN ('production', 'demo', 'test')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
