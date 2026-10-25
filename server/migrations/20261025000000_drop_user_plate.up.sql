-- Plates are out of the first installation. Registration kept a duplicate digest;
-- the dormant LPR tables (vehicles, bindings, audit) stay for the module's return.
ALTER TABLE users DROP COLUMN plate;
