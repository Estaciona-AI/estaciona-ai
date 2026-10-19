-- Fail before dropping ownership if scoped UUIDs cannot form a global key.
-- A downgrade must never discard receipts to resolve that ambiguity.
ALTER TABLE edge_event_receipts
    ADD CONSTRAINT edge_event_receipts_pkey PRIMARY KEY (event_id);

DROP INDEX edge_event_receipts_legacy_event_id_key;

ALTER TABLE edge_event_receipts
    DROP CONSTRAINT edge_event_receipts_gateway_event_key,
    DROP COLUMN gateway_id;
