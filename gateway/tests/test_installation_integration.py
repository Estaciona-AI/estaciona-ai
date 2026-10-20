# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Exercise installation provisioning and startup with the real Rust binary."""

from contextlib import closing
import json
import os
from pathlib import Path
import subprocess
import socket
from urllib.parse import urlsplit, urlunsplit
from uuid import uuid4

import psycopg2
from psycopg2 import sql
import pytest

from test_delivery_integration import Server, server as delivery_server

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


def test_mismatched_identity_cannot_create_administrator(unstarted_server):
    server = unstarted_server
    assert provision(server).returncode == 0
    manifest = server.root / "site" / "installation.json"
    value = json.loads(manifest.read_text())
    value["installation"]["id"] = str(uuid4())
    manifest.write_text(json.dumps(value))
    password = server.root / "password"
    password.write_text("synthetic-password\n")
    password.chmod(0o600)
    result = command(
        server,
        "--create-admin",
        "synthetic-admin@example.invalid",
        "--password-file",
        password,
    )
    assert result.returncode == 1, result.stdout + result.stderr
    assert server.query("SELECT count(*) FROM users")[0][0] == 0


def database_snapshot(server):
    tables = server.query(
        "SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname='public' ORDER BY tablename"
    )
    return {
        table: sorted(
            row[0]
            for row in server.query(
                sql.SQL("SELECT row_to_json(t)::text FROM {} t").format(
                    sql.Identifier(table)
                )
            )
        )
        for (table,) in tables
    }


server = delivery_server


@pytest.mark.parametrize(
    "difference",
    [
        "organization",
        "installation",
        "environment",
        "unbound",
        "missing-schema",
        "missing-descriptor",
    ],
)
def test_startup_and_all_administration_refuse_wrong_binding_without_writes(
    server, difference
):
    server.stop()
    owner = str(uuid4())
    server.query(
        "INSERT INTO users(id,email,password_hash) VALUES (%s,'owner@example.invalid','preserved')",
        (owner,),
    )
    server.query(
        "INSERT INTO reservations(id,user_id,spot_id,status,expires_at) VALUES (%s,%s,'R02-A','active',NOW()+INTERVAL '1 hour')",
        (str(uuid4()), owner),
    )
    server.query(
        "INSERT INTO user_occupancy_history(user_id,spot_id) VALUES (%s,'R02-A')",
        (owner,),
    )
    manifest = server.root / "site" / "installation.json"
    value = json.loads(manifest.read_text())
    if difference in ("organization", "installation"):
        value[difference]["id"] = str(uuid4())
    elif difference == "environment":
        value["environment"] = "production"
    elif difference == "unbound":
        server.query("DELETE FROM installation_binding")
    elif difference == "missing-schema":
        server.query("DROP TABLE installation_binding")
    manifest.write_text(json.dumps(value))
    if difference == "missing-descriptor":
        manifest.unlink()
    # If a denied startup reaches migrations or reconciliation, these sentinels change.
    server.query("DROP TABLE physical_occupancy_hourly")
    server.query("DELETE FROM _sqlx_migrations WHERE version=20261019000000")
    (server.root / "site" / "spots_3d.json").write_text(
        json.dumps([{"id": "WRONG-SITE", "center3D": {"x": 9, "y": 0, "z": 9}}])
    )
    password = server.root / "password"
    password.write_text("synthetic-password\n")
    password.chmod(0o600)
    policy = server.root / "policy.json"
    policy.write_text(
        json.dumps({"path_update": False, "trend_prediction": False, "cameras": []})
    )
    secret = server.root / "new-credential"
    before = database_snapshot(server)
    site_directory = server.root / "site"
    files_before = {
        path.name: path.read_bytes()
        for path in site_directory.iterdir()
        if path.is_file()
    }
    operations = [
        (),
        ("--create-admin", "new-admin@example.invalid", "--password-file", password),
        ("--edge-create", "wrong-installation", "--secret-file", secret),
        ("--edge-rotate", server.gateway_id, "--secret-file", secret),
        ("--edge-revoke", server.gateway_id),
        ("--edge-policy", server.gateway_id, policy),
    ]
    for operation in operations:
        result = command(server, *operation)
        assert result.returncode == (2 if difference == "missing-descriptor" else 1), (
            result.stdout + result.stderr
        )
        assert database_snapshot(server) == before
        assert {
            path.name: path.read_bytes()
            for path in site_directory.iterdir()
            if path.is_file()
        } == files_before
        assert not secret.exists()
    with socket.socket() as probe:
        probe.settimeout(0.2)
        assert probe.connect_ex((server.host, 8000)) != 0


def test_renaming_labels_preserves_identity_and_restart(server):
    server.stop()
    before = server.query(
        "SELECT organization_id,installation_id,environment,created_at FROM installation_binding"
    )
    manifest = server.root / "site" / "installation.json"
    value = json.loads(manifest.read_text())
    value["organization"].update(slug="renamed-org", name="Renamed organization")
    value["installation"].update(slug="renamed-site", name="Renamed campus")
    manifest.write_text(json.dumps(value))
    server.start()
    assert (
        server.query(
            "SELECT organization_id,installation_id,environment,created_at FROM installation_binding"
        )
        == before
    )
    assert provision(server, "check").returncode == 0


def test_bound_administration_does_not_require_map_files(unstarted_server):
    server = unstarted_server
    assert provision(server).returncode == 0
    for name in ("config.json", "spots_3d.json"):
        (server.root / "site" / name).unlink()
    for name in (
        "JWT_SECRET",
        "PLATE_SECRET_PEPPER",
        "WS_ALLOWED_ORIGINS",
        "LPR_ENABLED",
    ):
        server.environment.pop(name, None)

    password = server.root / "password"
    password.write_text("synthetic-password\n")
    password.chmod(0o600)
    result = command(
        server,
        "--create-admin",
        "new-admin@example.invalid",
        "--password-file",
        password,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    secret = server.root / "credential"
    result = command(
        server, "--edge-create", "synthetic-gateway", "--secret-file", secret
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert secret.exists()
    assert server.query("SELECT role FROM users") == [("admin",)]


def test_startup_database_failure_does_not_echo_credentials(unstarted_server):
    server = unstarted_server
    env = {
        **server.environment,
        "DATABASE_URL": "postgres://fake:password-not-for-logs@127.0.0.1:1/nonexistent",
    }
    result = command(server, environment=env)
    assert result.returncode == 1
    assert "password-not-for-logs" not in result.stdout + result.stderr


def test_unbound_startup_does_not_create_schema(unstarted_server):
    server = unstarted_server
    result = command(server)
    assert result.returncode == 1
    assert "--installation-init" in result.stderr
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None


@pytest.mark.parametrize(
    "setting",
    [
        "config.json",
        "spots_3d.json",
        "reservation_policy.json",
        "JWT_SECRET",
        "PLATE_SECRET_PEPPER",
        "WS_ALLOWED_ORIGINS",
        "LPR_ENABLED",
        "DATABASE_URL",
    ],
)
def test_invalid_runtime_configuration_is_refused_before_migrations(
    unstarted_server, setting
):
    server = unstarted_server
    assert provision(server).returncode == 0
    server.query("DROP TABLE physical_occupancy_hourly")
    server.query("DELETE FROM _sqlx_migrations WHERE version=20261019000000")
    env = dict(server.environment)
    if setting.endswith(".json"):
        (server.root / "site" / setting).write_text("do-not-echo-this-input")
    else:
        env[setting] = {
            "JWT_SECRET": "",
            "PLATE_SECRET_PEPPER": "",
            "WS_ALLOWED_ORIGINS": "https://do-not-echo.example/path",
            "LPR_ENABLED": "do-not-echo-this-input",
            "DATABASE_URL": "postgres://synthetic:do-not-echo-this-input@127.0.0.1:bad/test",
        }[setting]
    before = database_snapshot(server)
    directory = server.root / "site"
    files = {
        path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()
    }
    result = command(server, environment=env)
    assert result.returncode == 2, result.stdout + result.stderr
    assert "do-not-echo" not in result.stdout + result.stderr
    assert database_snapshot(server) == before
    assert {
        path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()
    } == files
    with socket.socket() as probe:
        probe.settimeout(0.2)
        assert probe.connect_ex((server.host, 8000)) != 0


@pytest.mark.parametrize("relative", [False, True])
@pytest.mark.parametrize("database_setting", [None, "do-not-echo-invalid-database-url"])
def test_offline_package_validation_is_read_only_and_does_not_require_runtime(
    unstarted_server, relative, database_setting
):
    server = unstarted_server
    directory = server.root / "site"
    manifest = directory / "client-manifest.json"
    (directory / "installation.json").rename(manifest)
    (server.root / ".env").write_text(
        "DATABASE_URL=do-not-echo-from-dotenv\nLPR_ENABLED=invalid\n"
    )
    env = {
        key: value
        for key, value in server.environment.items()
        if key
        not in (
            "DATABASE_URL",
            "SITE_DATA_DIR",
            "JWT_SECRET",
            "PLATE_SECRET_PEPPER",
            "WS_ALLOWED_ORIGINS",
            "LPR_ENABLED",
        )
    }
    if database_setting is not None:
        env["DATABASE_URL"] = database_setting
    before = {
        path.relative_to(server.root): path.read_bytes()
        for path in server.root.rglob("*")
        if path.is_file()
    }
    result = subprocess.run(
        [
            server.binary,
            "--installation-validate",
            "--manifest",
            manifest.name if relative else str(manifest),
        ],
        cwd=directory if relative else server.root / "server",
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "do-not-echo" not in result.stdout + result.stderr
    assert {
        path.relative_to(server.root): path.read_bytes()
        for path in server.root.rglob("*")
        if path.is_file()
    } == before
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None
    check = command(server, "--installation-check", "--manifest", manifest)
    assert check.returncode == 1
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None
    with socket.socket() as probe:
        probe.settimeout(0.2)
        assert probe.connect_ex((server.host, 8000)) != 0


@pytest.mark.parametrize("invalid", ["missing-map", "invalid-policy", "extra-argument"])
def test_offline_package_validation_refuses_invalid_input_without_writes(
    unstarted_server, invalid
):
    server = unstarted_server
    directory = server.root / "site"
    if invalid == "missing-map":
        (directory / "spots_3d.json").unlink()
    elif invalid == "invalid-policy":
        (directory / "reservation_policy.json").write_text('{"secret":"do-not-echo"}')
    args = ["--installation-validate", "--manifest", directory / "installation.json"]
    if invalid == "extra-argument":
        args.append("do-not-echo")
    env = {
        key: value
        for key, value in server.environment.items()
        if key
        not in (
            "DATABASE_URL",
            "SITE_DATA_DIR",
            "JWT_SECRET",
            "PLATE_SECRET_PEPPER",
            "WS_ALLOWED_ORIGINS",
            "LPR_ENABLED",
        )
    }
    before = {
        path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()
    }
    result = command(server, *args, environment=env)
    assert result.returncode == 2
    assert "do-not-echo" not in result.stdout + result.stderr
    assert {
        path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()
    } == before
    assert server.query("SELECT to_regclass('public._sqlx_migrations')")[0][0] is None
