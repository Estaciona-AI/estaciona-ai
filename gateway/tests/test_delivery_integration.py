# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import datetime
from functools import lru_cache

"""Opt-in integration against the built Rust server and a disposable PostgreSQL DB."""

import asyncio
import base64
from contextlib import closing, suppress
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import time
from unittest.mock import patch
from urllib.request import urlopen
from urllib.parse import urlsplit, urlunsplit
from uuid import uuid4

import psycopg2
from psycopg2 import sql
import pytest
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus

import gateway

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)
KEY = "11" * 32
APP_ROLE_SCRIPT = Path(__file__).parents[2] / "infra" / "app-role.sql"
IDENTITY = {
    "OIDC_ISSUER_URL": "http://127.0.0.1:9/realms/integration",
    "OIDC_CLIENT_ID": "integration",
    "OIDC_CLIENT_SECRET": "integration-client-secret",
    "OIDC_REDIRECT_URL": "http://localhost:8000/auth/callback",
    "APP_URL": "http://localhost",
}
# How long a server command or startup may take before the harness gives up;
# a wait, not a performance assertion.
SERVER_COMMAND_TIMEOUT = 120
# Optional server admission limits a test may lower.
WS_LIMITS = (
    "WS_HANDSHAKE_CONCURRENCY",
    "WS_MAX_EDGE_CONNECTIONS",
    "WS_MAX_APP_CONNECTIONS",
    "WS_MAX_DASHBOARD_CONNECTIONS",
)
IMAGE = (
    "postgres@sha256:bbbf67eadd2381a2738a4a2e36d4c3936c36edd82d80e7ef00411194385d3829"
)


class Server:
    def __init__(self, binary, db_url, root):
        self.binary, self.db_url, self.root = binary, db_url, root
        self.gateway_id = str(uuid4())
        self.process = None
        self.container_id = None
        self.postgres_container = os.environ.get("ESTACIONA_TEST_POSTGRES_CONTAINER")
        self.host = "127.0.0.1"
        if self.postgres_container:
            network_mode = subprocess.check_output(
                [
                    "docker",
                    "inspect",
                    self.postgres_container,
                    "--format",
                    "{{.HostConfig.NetworkMode}}",
                ],
                text=True,
            ).strip()
            if network_mode != "host":
                self.host = subprocess.check_output(
                    [
                        "docker",
                        "inspect",
                        self.postgres_container,
                        "--format",
                        "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
                    ],
                    text=True,
                ).strip()
        self.url = f"ws://{self.host}:8000/ws/edge"
        self.log = (root / "server.log").open("a")
        (root / "server").mkdir()
        site = root / "site"
        site.mkdir(exist_ok=True)
        (site / "config.json").write_text(
            json.dumps(
                [{"name": "Test lot", "path": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]}]
            )
        )
        (site / "spots_3d.json").write_text(
            json.dumps([{"id": "R02-A", "center3D": {"x": 1, "y": 0, "z": 1}}])
        )
        (site / "installation.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "organization": {
                        "id": str(uuid4()),
                        "slug": "test-org",
                        "name": "Synthetic organization",
                    },
                    "installation": {
                        "id": str(uuid4()),
                        "slug": "test-site",
                        "name": "Synthetic installation",
                    },
                    "environment": "test",
                }
            )
        )
        self.personal_data_keys = root / "personal-data.keys"
        self.personal_data_keys.write_text(
            "1:" + base64.b64encode(os.urandom(32)).decode() + "\n"
        )
        self.personal_data_keys.chmod(0o600)
        self.environment = {
            **os.environ,
            "DATABASE_URL": db_url,
            "SITE_DATA_DIR": str(site),
            "JWT_SECRET": "disposable-jwt-secret-for-integration",
            "LPR_ENABLED": "false",
            "PLATE_SECRET_PEPPER": "disposable-plate-pepper-for-integration",
            "PERSONAL_DATA_KEYS_FILE": str(self.personal_data_keys),
            # Sign-in is covered by the Rust tests; here the provider stays
            # unreachable, as during an outage: everything else keeps working.
            **IDENTITY,
            "WS_ALLOWED_ORIGINS": "http://localhost",
        }
        self.environment.pop("EDGE_API_KEY", None)

    def use_runtime_role(self):
        """Serve with the DML-only role of infra/app-role.sql, as in production."""
        password = uuid4().hex
        with closing(psycopg2.connect(self.db_url)) as conn, conn:
            with conn.cursor() as cursor:
                cursor.execute(APP_ROLE_SCRIPT.read_text())
                cursor.execute("ALTER ROLE estaciona_app PASSWORD %s", (password,))
        parsed = urlsplit(self.db_url)
        netloc = f"estaciona_app:{password}@{parsed.hostname}:{parsed.port or 5432}"
        self.environment["DATABASE_URL"] = urlunsplit(parsed._replace(netloc=netloc))

    def query(self, statement, arguments=()):
        with closing(psycopg2.connect(self.db_url)) as conn, conn:
            with conn.cursor() as cursor:
                cursor.execute(statement, arguments)
                return cursor.fetchall() if cursor.description else None

    def start(self):
        if self.postgres_container:
            parsed = urlsplit(self.environment["DATABASE_URL"])
            netloc = (
                parsed.netloc.rsplit("@", 1)[0] + f"@127.0.0.1:{parsed.port or 5432}"
            )
            database_url = urlunsplit(parsed._replace(netloc=netloc))
            args = [
                "docker",
                "run",
                "--detach",
                "--pull=never",
                "--cap-drop=ALL",
                "--security-opt",
                "no-new-privileges",
                # The owner of the private fixture files, as SECRETS_DIR in production.
                "--user",
                f"{os.getuid()}:{os.getgid()}",
                "--network",
                f"container:{self.postgres_container}",
                "--mount",
                f"type=bind,source={self.binary},target=/app/server,readonly",
                "--mount",
                f"type=bind,source={self.root / 'site'},target=/site,readonly",
                "--mount",
                f"type=bind,source={self.personal_data_keys},target=/run/secrets/personal-data.keys,readonly",
                "--env",
                "PERSONAL_DATA_KEYS_FILE=/run/secrets/personal-data.keys",
                "--env",
                f"DATABASE_URL={database_url}",
                "--env",
                "SITE_DATA_DIR=/site",
                "--env",
                f"LPR_ENABLED={self.environment['LPR_ENABLED']}",
                "--env",
                "JWT_SECRET=disposable-jwt-secret-for-integration",
                "--env",
                "PLATE_SECRET_PEPPER=disposable-plate-pepper-for-integration",
                *[
                    argument
                    for key in IDENTITY
                    for argument in ("--env", f"{key}={self.environment[key]}")
                ],
                "--env",
                f"WS_ALLOWED_ORIGINS={self.environment['WS_ALLOWED_ORIGINS']}",
                *[
                    argument
                    for key in WS_LIMITS
                    if key in self.environment
                    for argument in ("--env", f"{key}={self.environment[key]}")
                ],
                "--entrypoint",
                "/app/server",
                IMAGE,
            ]
            # Only the compatibility rejection test injects a legacy global key.
            if "EDGE_API_KEY" in self.environment:
                args[2:2] = [
                    "--env",
                    f"EDGE_API_KEY={self.environment['EDGE_API_KEY']}",
                ]
            self.container_id = subprocess.check_output(args, text=True).strip()
        else:
            # Refuse unrelated listeners; never signal them. SO_REUSEADDR lets
            # the previous test's connections linger in TIME_WAIT, while Linux
            # still refuses the bind when anything listens on the port.
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                probe.bind(("0.0.0.0", 8000))
            self.process = subprocess.Popen(
                [self.binary],
                cwd=self.root / "server",
                env=self.environment,
                stdout=self.log,
                stderr=self.log,
            )
        deadline = time.monotonic() + SERVER_COMMAND_TIMEOUT
        while time.monotonic() < deadline:
            if self.process is not None and self.process.poll() is not None:
                pytest.fail((self.root / "server.log").read_text())
            try:
                with urlopen(
                    f"http://{self.host}:8000/health", timeout=0.2
                ) as response:
                    if response.status == 200:
                        return
            except OSError:
                time.sleep(0.02)
        pytest.fail(
            "Rust server did not become healthy: "
            + (self.root / "server.log").read_text()
        )

    def stop(self):
        if self.container_id is not None:
            subprocess.run(
                ["docker", "rm", "--force", self.container_id],
                check=True,
                capture_output=True,
            )
            self.container_id = None
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.process = None


@pytest.fixture
def server(tmp_path):
    binary = str(Path(os.environ["ESTACIONA_TEST_SERVER"]).resolve())
    admin_url = os.environ["DATABASE_URL"]
    database = "estaciona_delivery_" + uuid4().hex
    db_url = urlunsplit(urlsplit(admin_url)._replace(path="/" + database))
    with closing(psycopg2.connect(admin_url)) as conn:
        conn.autocommit = True
        with conn.cursor() as cursor:
            cursor.execute(
                sql.SQL("CREATE DATABASE {}").format(sql.Identifier(database))
            )
    instance = Server(binary, db_url, tmp_path)
    try:
        initialized = subprocess.run(
            [
                binary,
                "--installation-init",
                "--manifest",
                str(tmp_path / "site" / "installation.json"),
            ],
            cwd=tmp_path / "server",
            env=instance.environment,
            capture_output=True,
            text=True,
            # Applying every migration takes longer on a loaded machine; the
            # command bounds itself (store::MIGRATION_TIMEOUT).
            timeout=SERVER_COMMAND_TIMEOUT,
        )
        assert initialized.returncode == 0, initialized.stdout + initialized.stderr
        instance.use_runtime_role()
        instance.start()
        instance.query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES (%s, %s, %s)",
            (
                instance.gateway_id,
                "Delivery fixture",
                hashlib.sha256(KEY.encode()).digest(),
            ),
        )
        # A camera saw the mapped spot moments ago: free and reservable. A spot
        # without a recent observation is unknown and cannot be reserved.
        instance.query(
            "UPDATE spots SET observation_timestamp = NOW() - INTERVAL '30 seconds'"
        )
        for camera_id in ("test-camera", "session-camera"):
            instance.query(
                "INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) "
                "VALUES (%s, %s, TRUE, TRUE)",
                (instance.gateway_id, camera_id),
            )
            instance.query(
                "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) "
                "VALUES (%s, %s, 'R02-A')",
                (instance.gateway_id, camera_id),
            )
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


@lru_cache(maxsize=1024)
def observation_timestamp(event_id):
    # A replay is the same observation, including its original source time.
    return datetime.datetime.now(datetime.UTC).isoformat()


def message(event_id):
    return {
        "type": "EDGE_EVENT",
        "version": 1,
        "event_id": event_id,
        "payload": {
            "type": "SPOT_UPDATE",
            "spot_id": "R02-A",
            "status": "occupied",
            "camera_id": "test-camera",
            "confidence": 0.9,
            "timestamp": observation_timestamp(event_id),
        },
    }


async def until(predicate, timeout=10):
    async def poll():
        while not predicate():
            await asyncio.sleep(0.01)

    await asyncio.wait_for(poll(), timeout=timeout)


async def receive_ready(connection):
    ready = json.loads(await asyncio.wait_for(connection.recv(), timeout=3))
    assert ready["type"] == "EDGE_READY"
    assert ready["delivery_version"] == 1
    active = ["SPOT_UPDATE", "PATH_UPDATE", "TREND_PREDICTION", "TREND_PREDICTION_V2"]
    assert ready["durable_events"] in (active, [*active, "CAR_DETECTED"])


@pytest.mark.parametrize("rotate_credential", [False, True])
def test_gateway_recovers_a_lost_ack_after_both_processes_restart(
    server, tmp_path, rotate_credential
):
    server.query(
        "CREATE TABLE effect_count (updates INTEGER NOT NULL); INSERT INTO effect_count VALUES (0); CREATE FUNCTION count_effect() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN UPDATE effect_count SET updates = updates + 1; RETURN NEW; END $$; CREATE TRIGGER count_effect BEFORE UPDATE ON spots FOR EACH ROW EXECUTE FUNCTION count_effect();"
    )
    outbox, metrics = str(tmp_path / "outbox.db"), str(tmp_path / "metrics.db")
    gateway.init_db(outbox)
    gateway.init_metrics_db(metrics)
    event_id = gateway.save_event(outbox, json.dumps(message(str(uuid4()))["payload"]))
    gateway.save_metric(metrics, "cam", "R02-A", "occupied", "t0", "t1", "t2", event_id)
    frames, acks = [], []
    current_key = KEY

    async def run():
        nonlocal current_key
        dropped = asyncio.Event()

        async def proxy(client):
            async with websockets.connect(
                server.url,
                additional_headers={"Authorization": f"Bearer {current_key}"},
            ) as upstream:
                await client.send(await upstream.recv())
                frame = await client.recv()
                frames.append(json.loads(frame))
                await upstream.send(frame)
                ack = (
                    await upstream.recv()
                )  # Real ACK exists only after the PostgreSQL commit.
                acks.append(json.loads(ack))
                if len(acks) == 1:
                    await client.close()
                    dropped.set()
                else:
                    await client.send(ack)
                    await client.wait_closed()

        async with websockets.serve(proxy, "127.0.0.1", 0) as proxy_server:
            port = proxy_server.sockets[0].getsockname()[1]
            proxy_url = f"ws://127.0.0.1:{port}"
            with patch("gateway.RETRY_DELAY_SECONDS", 60):
                first = asyncio.create_task(
                    gateway.sync_loop(outbox, metrics, proxy_url, KEY, asyncio.Event())
                )
                try:
                    await asyncio.wait_for(dropped.wait(), timeout=10)
                    assert gateway.pending_deliveries(outbox)[0][1] == event_id
                    with sqlite3.connect(metrics) as conn:
                        assert (
                            conn.execute("SELECT cloud_ack FROM metrics").fetchone()[0]
                            == 0
                        )
                finally:
                    first.cancel()
                    with suppress(asyncio.CancelledError):
                        await first
            server.stop()
            if rotate_credential:
                current_key = "77" * 32
                server.query(
                    "UPDATE edge_gateways SET credential_digest = %s WHERE id = %s",
                    (hashlib.sha256(current_key.encode()).digest(), server.gateway_id),
                )
            server.start()
            gateway.init_db(outbox)
            second = asyncio.create_task(
                gateway.sync_loop(
                    outbox, metrics, proxy_url, current_key, asyncio.Event()
                )
            )
            try:
                await until(lambda: not gateway.pending_deliveries(outbox))
            finally:
                second.cancel()
                with suppress(asyncio.CancelledError):
                    await second

    asyncio.run(run())
    assert len(frames) == 2
    assert frames[0]["event_id"] == frames[1]["event_id"] == event_id
    assert frames[0]["payload"] == frames[1]["payload"]
    assert [ack["duplicate"] for ack in acks] == [False, True]
    assert server.query("SELECT updates FROM effect_count") == [(1,)]
    assert server.query(
        "SELECT outcome FROM edge_event_receipts WHERE gateway_id = %s AND event_id = %s",
        (server.gateway_id, event_id),
    ) == [("applied",)]
    with sqlite3.connect(metrics) as conn:
        assert conn.execute(
            "SELECT cloud_ack, delivery_result FROM metrics"
        ).fetchone() == (1, "applied")


def test_failed_commit_has_no_ack_and_same_id_can_retry(server):
    server.query(
        "CREATE FUNCTION reject_receipt() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected commit-path failure'; END $$; CREATE TRIGGER reject_receipt BEFORE UPDATE ON edge_event_receipts FOR EACH ROW EXECUTE FUNCTION reject_receipt();"
    )
    event_id = str(uuid4())

    async def run():
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as conn:
            await receive_ready(conn)
            await conn.send(json.dumps(message(event_id)))
            with pytest.raises(ConnectionClosed):
                await asyncio.wait_for(conn.recv(), timeout=3)
        assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
            ("free",)
        ]
        assert server.query("SELECT COUNT(*) FROM edge_event_receipts") == [(0,)]
        server.query("DROP TRIGGER reject_receipt ON edge_event_receipts")
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as conn:
            await receive_ready(conn)
            await conn.send(json.dumps(message(event_id)))
            ack = json.loads(await asyncio.wait_for(conn.recv(), timeout=3))
            assert (ack["event_id"], ack["outcome"], ack["duplicate"]) == (
                event_id,
                "applied",
                False,
            )

    asyncio.run(run())


def test_unauthorized_edge_has_no_protocol_or_receipt(server):
    async def run():
        with pytest.raises(InvalidStatus) as failure:
            async with websockets.connect(
                server.url,
                additional_headers={"Authorization": "Bearer invalid-test-key"},
            ):
                raise AssertionError("unauthorized connection must not upgrade")
        assert failure.value.response.status_code == 401

    asyncio.run(run())
    assert server.query("SELECT COUNT(*) FROM edge_event_receipts") == [(0,)]


@pytest.mark.parametrize(
    "kind", ["PATH_UPDATE", "TREND_PREDICTION", "TREND_PREDICTION_V2"]
)
def test_durable_projection_ack_survives_real_server_restart(server, kind):
    server.query(
        "UPDATE edge_gateways SET path_update=TRUE,trend_prediction=TRUE WHERE id=%s",
        (server.gateway_id,),
    )
    payload = (
        {
            "type": "PATH_UPDATE",
            "from_node": "cam-01",
            "to_node": "path_node_1",
            "is_active": False,
        }
        if kind == "PATH_UPDATE"
        else {
            "type": "TREND_PREDICTION",
            "timestamp": "2026-10-18T12:00:00Z",
            "avg_stay_duration_mins": 40.0,
            "stay_duration_distribution": [1, 2],
            "max_capacity": 400,
            "model_health": {
                "r2_score": 0.9,
                "mae": 1.0,
                "rmse": 2.0,
                "inference_time_ms": 10.0,
            },
            "next_24h_occupancy": [
                {"timestamp": "2026-10-18T13:00:00Z", "occupancy": 100}
            ],
        }
    )
    if kind == "TREND_PREDICTION_V2":
        payload = {
            "type": kind,
            "timestamp": payload["timestamp"],
            "max_capacity": 400,
            "evaluation": {
                "status": "unavailable",
                "reason": "evaluation_failed",
                "inference_time_ms": 10.0,
            },
            "stay_duration": {"status": "unavailable", "reason": "not_measured"},
            "next_24h_occupancy": payload["next_24h_occupancy"],
        }
    frame = {
        "type": "EDGE_EVENT",
        "version": 1,
        "event_id": str(uuid4()),
        "payload": payload,
    }

    async def send():
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as connection:
            await receive_ready(connection)
            await connection.send(json.dumps(frame))
            return json.loads(await asyncio.wait_for(connection.recv(), timeout=5))

    first = asyncio.run(send())
    assert first["outcome"] == "applied", first
    assert first["duplicate"] is False
    server.stop()
    server.start()
    replay = asyncio.run(send())
    assert replay["outcome"] == "applied", replay
    assert replay["duplicate"] is True
    assert server.query(
        "SELECT COUNT(*) FROM edge_event_receipts WHERE event_id=%s",
        (frame["event_id"],),
    ) == [(1,)]
    if kind.startswith("TREND_PREDICTION"):
        from test_session_integration import insert_user, token
        from test_ws_tickets_integration import issue, ws_url

        admin = insert_user(server, role="admin")
        signed = token(server, admin, "admin", int(time.time()) + 3600)
        status, ticket, _ = issue(server, signed, "dashboard")
        assert status == 201, ticket

        async def read_restored_forecast():
            async with websockets.connect(
                ws_url(server, "dashboard", ticket["ticket"]),
                origin="http://localhost",
            ) as dashboard:
                for _ in range(4):
                    message = json.loads(
                        await asyncio.wait_for(dashboard.recv(), timeout=3)
                    )
                    if message.get("type") == kind:
                        assert message == payload
                        return
                pytest.fail("restored forecast was not delivered to the dashboard")

        asyncio.run(read_restored_forecast())


def test_freshness_survives_restart_and_rejects_future_poisoning(server):
    now = datetime.datetime.now(datetime.UTC)
    latest = message(str(uuid4()))
    latest["payload"]["timestamp"] = (now - datetime.timedelta(seconds=2)).isoformat()

    async def deliver(event):
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as conn:
            await receive_ready(conn)
            await conn.send(json.dumps(event))
            return json.loads(await asyncio.wait_for(conn.recv(), 3))

    assert asyncio.run(deliver(latest))["outcome"] == "applied"
    server.stop()
    server.start()
    older = message(str(uuid4()))
    older["payload"].update(
        status="free", timestamp=(now - datetime.timedelta(seconds=3)).isoformat()
    )
    ack = asyncio.run(deliver(older))
    assert (ack["outcome"], ack["reason"]) == ("ignored", "stale_observation")
    assert asyncio.run(deliver(older))["duplicate"] is True
    assert server.query("SELECT status FROM spots WHERE id='R02-A'") == [("occupied",)]
    future = message(str(uuid4()))
    future["payload"]["timestamp"] = (now + datetime.timedelta(days=1)).isoformat()
    assert asyncio.run(deliver(future))["reason"] == "future_timestamp"
    newer = message(str(uuid4()))
    newer["payload"]["status"] = "free"
    assert asyncio.run(deliver(newer))["outcome"] == "applied"
    assert server.query("SELECT status FROM spots WHERE id='R02-A'") == [("free",)]


def test_a_replay_after_retention_is_answered_as_a_duplicate(server, tmp_path, capsys):
    """Deleting a confirmed event is safe: the server keeps the receipt."""
    outbox, metrics = str(tmp_path / "outbox.db"), str(tmp_path / "metrics.db")
    gateway.init_db(outbox)
    gateway.init_metrics_db(metrics)
    payload = json.dumps(message(str(uuid4()))["payload"])
    event_id = gateway.save_event(outbox, payload)

    async def deliver():
        task = asyncio.create_task(
            gateway.sync_loop(outbox, metrics, server.url, KEY, asyncio.Event())
        )
        try:
            await until(lambda: not gateway.pending_deliveries(outbox))
        finally:
            task.cancel()
            with suppress(asyncio.CancelledError):
                await task

    asyncio.run(deliver())
    assert (
        f"event_id={event_id} outcome=applied duplicate=False"
        in capsys.readouterr().out
    )
    with sqlite3.connect(outbox) as conn:
        conn.execute("UPDATE fallback_events SET created_at = '2000-01-01 00:00:00'")
    assert gateway.purge_synced_events(outbox, datetime.timedelta(days=1)) == 1

    # The producer never saw its local ACK and replays the same event.
    with sqlite3.connect(outbox) as conn:
        assert conn.execute("SELECT COUNT(*) FROM fallback_events").fetchone() == (0,)
        conn.execute(
            "INSERT INTO fallback_events (message_text, event_uuid) VALUES (?, ?)",
            (payload, event_id),
        )
    asyncio.run(deliver())
    assert (
        f"event_id={event_id} outcome=applied duplicate=True" in capsys.readouterr().out
    )
    assert server.query(
        "SELECT COUNT(*) FROM edge_event_receipts WHERE event_id=%s", (event_id,)
    ) == [(1,)]
