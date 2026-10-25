-- Receipts written before the keyed fingerprint (11-19/10/2026) kept the raw
-- CAR_DETECTED payload or an unkeyed SHA-256 of it. Keep the outcome, drop the
-- plate. A replay of one of these events is answered as an event_id conflict.
UPDATE edge_event_receipts
SET payload = '{"redacted": "car_detected"}'::jsonb
WHERE payload->'json'->>'type' = 'CAR_DETECTED'
   OR payload->>'json_text' LIKE '%CAR_DETECTED%'
   OR payload ? 'sha256';
