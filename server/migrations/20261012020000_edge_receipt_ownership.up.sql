ALTER TABLE edge_event_receipts
    ADD COLUMN gateway_id UUID REFERENCES edge_gateways(id),
    DROP CONSTRAINT edge_event_receipts_pkey,
    ADD CONSTRAINT edge_event_receipts_gateway_event_key UNIQUE (gateway_id, event_id);

-- Preserve historic uniqueness without guessing the producer of old receipts.
CREATE UNIQUE INDEX edge_event_receipts_legacy_event_id_key
    ON edge_event_receipts (event_id) WHERE gateway_id IS NULL;
