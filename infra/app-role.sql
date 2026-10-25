-- Runtime role for the server: reads and writes rows, never changes the schema.
-- Run as the database owner (the role that runs --installation-init and
-- --migrate) in the application database, after --installation-init. Safe to
-- repeat, and repeat it after a release adds tables owned by another role.
-- Set the password separately, outside version control:
--   ALTER ROLE estaciona_app PASSWORD '...';
-- The server then uses this role in DATABASE_URL; --migrate keeps the owner.

DO $$
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NULL
        OR to_regclass('public.installation_binding') IS NULL THEN
        RAISE EXCEPTION 'Run --installation-init before granting the runtime role';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'estaciona_app') THEN
        CREATE ROLE estaciona_app LOGIN;
    END IF;
END
$$;

ALTER ROLE estaciona_app
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;

-- Only the owner creates objects in public (PostgreSQL 15 default, also enforced
-- on databases created by older versions).
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM estaciona_app;
GRANT USAGE ON SCHEMA public TO estaciona_app;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM estaciona_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO estaciona_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO estaciona_app;

-- Migration history and the installation binding change only through the owner.
REVOKE INSERT, UPDATE, DELETE ON public._sqlx_migrations FROM estaciona_app;
REVOKE INSERT, UPDATE, DELETE ON public.installation_binding FROM estaciona_app;

-- Tables that later migrations create as this owner get the same row access.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO estaciona_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO estaciona_app;
