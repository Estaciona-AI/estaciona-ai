-- Read-only role for the forecast worker (gateway/ml/worker.py).
-- Run as the database owner in the application database; safe to repeat.
-- Set the password separately, outside version control:
--   ALTER ROLE estaciona_forecast_reader PASSWORD '...';
-- The worker refuses to start if this role can write to any table.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'estaciona_forecast_reader') THEN
        CREATE ROLE estaciona_forecast_reader LOGIN;
    END IF;
END
$$;

ALTER ROLE estaciona_forecast_reader
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
ALTER ROLE estaciona_forecast_reader SET default_transaction_read_only = on;

REVOKE ALL ON ALL TABLES IN SCHEMA public FROM estaciona_forecast_reader;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM estaciona_forecast_reader;
GRANT SELECT ON TABLE spots, physical_occupancy_hourly, site_map_revisions
    TO estaciona_forecast_reader;
