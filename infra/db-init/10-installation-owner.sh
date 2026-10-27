#!/bin/sh
# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0
#
# Runs once, when the database volume is created. The installation's tables
# belong to estaciona_owner, a role without superuser powers that owns only
# this database: it runs --installation-init, --migrate and infra/app-role.sql.
# The runtime role estaciona_app is created here too, since the owner cannot.
# The container superuser (POSTGRES_USER) stays for maintenance only.
set -eu
: "${ESTACIONA_OWNER_PASSWORD:?Set ESTACIONA_OWNER_PASSWORD in env/postgres.env}"
: "${ESTACIONA_APP_PASSWORD:?Set ESTACIONA_APP_PASSWORD in env/postgres.env}"
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
    -v owner_password="$ESTACIONA_OWNER_PASSWORD" \
    -v app_password="$ESTACIONA_APP_PASSWORD" -v database="$POSTGRES_DB" <<'SQL'
CREATE ROLE estaciona_owner LOGIN PASSWORD :'owner_password'
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
-- The server's role; infra/app-role.sql grants it row access later.
CREATE ROLE estaciona_app LOGIN PASSWORD :'app_password'
    NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
ALTER DATABASE :"database" OWNER TO estaciona_owner;
ALTER SCHEMA public OWNER TO estaciona_owner;
SQL
