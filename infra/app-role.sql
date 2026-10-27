-- Runtime role for the server: reads and writes rows, never changes the schema.
-- Run as the database owner (the role that runs --installation-init and
-- --migrate) in the application database, after --installation-init. Safe to
-- repeat, and repeat it after a release adds tables owned by another role.
-- An owner without role powers (infra/db-init) cannot create or alter roles:
-- db-init then creates estaciona_app, and this script only checks it. A role
-- that can manage roles creates it here; set its password separately, outside
-- version control:
--   ALTER ROLE estaciona_app PASSWORD '...';
-- The server then uses this role in DATABASE_URL; --migrate keeps the owner.

DO $$
DECLARE
    manages_roles BOOLEAN;
BEGIN
    IF to_regclass('public._sqlx_migrations') IS NULL
        OR to_regclass('public.installation_binding') IS NULL THEN
        RAISE EXCEPTION 'Run --installation-init before granting the runtime role';
    END IF;
    SELECT rolsuper OR rolcreaterole INTO manages_roles
        FROM pg_roles WHERE rolname = current_user;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'estaciona_app') THEN
        IF NOT manages_roles THEN
            RAISE EXCEPTION 'Create estaciona_app first (infra/db-init does) or run as a role that can create roles';
        END IF;
        CREATE ROLE estaciona_app LOGIN;
    END IF;
    IF manages_roles THEN
        ALTER ROLE estaciona_app
            NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
    ELSIF EXISTS (
        SELECT 1 FROM pg_roles WHERE rolname = 'estaciona_app'
            AND (rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls)
    ) THEN
        RAISE EXCEPTION 'estaciona_app holds more than row privileges';
    END IF;
END
$$;

REVOKE CREATE ON SCHEMA public FROM PUBLIC;
REVOKE ALL ON SCHEMA public FROM estaciona_app;
GRANT USAGE ON SCHEMA public TO estaciona_app;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM estaciona_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO estaciona_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO estaciona_app;

-- Migration history and the installation binding change only through the owner.
REVOKE INSERT, UPDATE, DELETE ON public._sqlx_migrations FROM estaciona_app;
REVOKE INSERT, UPDATE, DELETE ON public.installation_binding FROM estaciona_app;
-- The administrative trail is append-only for the service.
REVOKE UPDATE, DELETE, TRUNCATE ON public.admin_audit_log FROM estaciona_app;

-- Tables that later migrations create as this owner get the same row access.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO estaciona_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO estaciona_app;
