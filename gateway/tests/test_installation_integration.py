# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Exercise installation provisioning and startup with the real Rust binary."""

from contextlib import closing
import json
import os
from pathlib import Path
import subprocess
from urllib.parse import urlsplit, urlunsplit
from uuid import uuid4

import psycopg2
from psycopg2 import sql
import pytest

from test_delivery_integration import Server

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL for real Rust integration",
)


def descriptor():
    return {
        "schema_version": 1,
        "organization": {
            "id": str(uuid4()),
            "slug": "synthetic-org",
            "name": "Test org",
        },
        "installation": {
            "id": str(uuid4()),
            "slug": "synthetic-site",
            "name": "Test site",
        },
        "environment": "test",
    }


@pytest.fixture
def unstarted_server(tmp_path):
    admin_url = os.environ["DATABASE_URL"]
    database = "estaciona_installation_" + uuid4().hex
    db_url = urlunsplit(urlsplit(admin_url)._replace(path="/" + database))
    with closing(psycopg2.connect(admin_url)) as conn:
        conn.autocommit = True
        with conn.cursor() as cursor:
            cursor.execute(
                sql.SQL("CREATE DATABASE {}").format(sql.Identifier(database))
            )
    instance = Server(
        str(Path(os.environ["ESTACIONA_TEST_SERVER"]).resolve()), db_url, tmp_path
    )
    (tmp_path / "site" / "installation.json").write_text(json.dumps(descriptor()))
    try:
        yield instance
    finally:
        instance.stop()
        instance.log.close()
        with closing(psycopg2.connect(admin_url)) as conn:
            conn.autocommit = True
            with conn.cursor() as cursor:
                cursor.execute(
                    sql.SQL("DROP DATABASE {} WITH (FORCE)").format(
                        sql.Identifier(database)
                    )
                )


def command(server, *args, environment=None):
    return subprocess.run(
        [server.binary, *map(str, args)],
        cwd=server.root / "server",
        env=server.environment if environment is None else environment,
        capture_output=True,
        text=True,
        timeout=15,
    )


def provision(server, operation="init", *extra):
    return command(
        server,
        "--installation-" + operation,
        "--manifest",
        server.root / "site" / "installation.json",
        *extra,
    )


def test_check_is_read_only_and_init_is_explicit_and_idempotent(unstarted_server):
    server = unstarted_server
    assert provision(server, "check").returncode == 1
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None
    for _ in range(2):
        result = provision(server)
        assert result.returncode == 0, result.stderr
    assert provision(server, "check").returncode == 0
    assert server.query("SELECT count(*) FROM installation_binding")[0][0] == 1


def test_invalid_input_and_unavailable_database_have_safe_exit_codes(unstarted_server):
    server = unstarted_server
    result = command(
        server, "--installation-adopt", "--manifest", "missing", "secret-argument"
    )
    assert result.returncode == 2
    assert "secret-argument" not in result.stdout + result.stderr
    manifest = server.root / "site" / "installation.json"
    manifest.write_text('{"sensitive-field":"do-not-echo"}')
    result = provision(server)
    assert result.returncode == 2
    assert "do-not-echo" not in result.stdout + result.stderr
    manifest.write_text(json.dumps(descriptor()))
    env = {
        **server.environment,
        "DATABASE_URL": "postgres://fake:password-not-for-logs@127.0.0.1:1/nonexistent",
    }
    result = command(
        server, "--installation-check", "--manifest", manifest, environment=env
    )
    assert result.returncode == 1
    assert "password-not-for-logs" not in result.stdout + result.stderr


def test_init_refuses_legacy_before_migrations(unstarted_server):
    server = unstarted_server
    server.query("CREATE TABLE dashboard_admins (data TEXT)")
    server.query("INSERT INTO dashboard_admins VALUES ('preserved')")
    assert provision(server).returncode == 1
    assert server.query("SELECT data FROM dashboard_admins") == [("preserved",)]
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None


def test_adopt_requires_confirmation_and_preserves_current_business_data(
    unstarted_server,
):
    server = unstarted_server
    assert provision(server).returncode == 0
    server.query("DELETE FROM installation_binding")
    server.query(
        "INSERT INTO users (id,email,password_hash) VALUES (%s,'synthetic@example.invalid','preserved')",
        (str(uuid4()),),
    )
    assert provision(server).returncode == 1
    assert (
        provision(server, "adopt", "--confirm-database", "wrong-database").returncode
        == 1
    )
    database = urlsplit(server.db_url).path.removeprefix("/")
    for _ in range(2):
        result = provision(server, "adopt", "--confirm-database", database)
        assert result.returncode == 0, result.stderr
    assert server.query("SELECT password_hash FROM users") == [("preserved",)]


def test_contract_export_and_legacy_password_rejection_need_no_database(
    unstarted_server,
):
    server = unstarted_server
    env = {
        key: value
        for key, value in server.environment.items()
        if key not in ("DATABASE_URL", "SITE_DATA_DIR")
    }
    result = command(
        server, "--export-contracts", server.root / "contracts", environment=env
    )
    assert result.returncode == 0, result.stderr
    result = command(
        server,
        "--create-admin",
        "synthetic@example.invalid",
        "password-not-for-logs",
        environment=env,
    )
    assert result.returncode == 2
    assert "password-not-for-logs" not in result.stdout + result.stderr
