# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Restore rehearsal: the database backup holds personal fields only
encrypted; restored together with the separately kept keys it serves the same
people again, and with any other key it serves nobody's data."""

import base64
from contextlib import closing
import json
import os
import shutil
import subprocess
import time
from urllib.error import HTTPError
from urllib.parse import urlsplit, urlunsplit
from urllib.request import Request, urlopen

import psycopg2
from psycopg2 import sql
import pytest

from test_delivery_integration import Server, server as delivery_server
from test_session_integration import insert_user, token

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)

server = delivery_server

ELIGIBILITY = {"date_of_birth": "1950-03-04", "pcd_status": True}


def call(target, method, path, bearer, payload=None):
    request = Request(
        f"http://{target.host}:8000{path}",
        method=method,
        headers={
            "Authorization": f"Bearer {bearer}",
            "Content-Type": "application/json",
        },
        data=None if payload is None else json.dumps(payload).encode(),
    )
    try:
        response = urlopen(request, timeout=6)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.read().decode()


def database_command(container, *args, data=None):
    user = urlsplit(os.environ["DATABASE_URL"]).username
    return subprocess.run(
        ["docker", "exec", "-i", container, *args[:1], "-U", user, *args[1:]],
        input=data,
        capture_output=True,
        check=True,
    ).stdout


def test_database_and_keys_restored_together_bring_the_people_back(server, tmp_path):
    container = server.postgres_container
    if not container:
        pytest.skip("the rehearsal runs pg_dump inside the PostgreSQL container")
    person = insert_user(server)
    signed = token(server, person, "user", int(time.time()) + 600)
    status, body = call(server, "PUT", "/api/v1/me/eligibility", signed, ELIGIBILITY)
    assert status == 200, body
    stored = server.query(
        "SELECT encode(date_of_birth_enc, 'escape') FROM users WHERE id = %s",
        (str(person),),
    )
    assert "1950" not in stored[0][0], "the database holds no clear value"

    database = urlsplit(server.db_url).path.lstrip("/")
    backup = database_command(container, "pg_dump", "--format=custom", database)
    server.stop()

    restored = database + "_restored"
    admin_url = os.environ["DATABASE_URL"]
    with closing(psycopg2.connect(admin_url)) as conn:
        conn.autocommit = True
        with conn.cursor() as cursor:
            cursor.execute(
                sql.SQL("CREATE DATABASE {}").format(sql.Identifier(restored))
            )
    try:
        database_command(container, "pg_restore", "--dbname", restored, data=backup)
        url = urlunsplit(urlsplit(server.db_url)._replace(path="/" + restored))
        (tmp_path / "restored").mkdir()
        copy = Server(server.binary, url, tmp_path / "restored")
        # Same installation package; the keys come from their own custody.
        shutil.copy(server.root / "site" / "installation.json", copy.root / "site")
        shutil.copy(server.personal_data_keys, copy.personal_data_keys)
        copy.use_runtime_role()
        copy.start()
        try:
            status, body = call(copy, "GET", "/api/v1/me", signed)
            assert status == 200, body
            account = json.loads(body)
            assert (account["date_of_birth"], account["pcd_status"]) == (
                "1950-03-04",
                True,
            )
            copy.stop()

            # The database alone, with any other key, gives nothing away.
            copy.personal_data_keys.write_text(
                "1:" + base64.b64encode(os.urandom(32)).decode() + "\n"
            )
            copy.start()
            status, body = call(copy, "GET", "/api/v1/me", signed)
            assert status >= 500 and "1950" not in body, (status, body)
        finally:
            copy.stop()
            copy.log.close()
    finally:
        with closing(psycopg2.connect(admin_url)) as conn:
            conn.autocommit = True
            with conn.cursor() as cursor:
                cursor.execute(
                    sql.SQL("DROP DATABASE IF EXISTS {} WITH (FORCE)").format(
                        sql.Identifier(restored)
                    )
                )
