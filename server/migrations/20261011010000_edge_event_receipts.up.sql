CREATE TABLE edge_event_receipts (
    event_id UUID PRIMARY KEY,
    payload JSONB NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('pending', 'applied', 'ignored', 'rejected')),
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
