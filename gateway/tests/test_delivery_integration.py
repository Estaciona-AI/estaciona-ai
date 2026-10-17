"""Opt-in integration against the built Rust server and a disposable PostgreSQL DB."""

import asyncio
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
        site.mkdir()
        (site / "config.json").write_text(
            json.dumps(
                [{"name": "Test lot", "path": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]}]
            )
        )
        (site / "spots_3d.json").write_text(
            json.dumps([{"id": "R02-A", "center3D": {"x": 1, "y": 0, "z": 1}}])
        )
        self.environment = {
            **os.environ,
            "DATABASE_URL": db_url,
            "SITE_DATA_DIR": str(site),
            "JWT_SECRET": "disposable-jwt",
            "PLATE_SECRET_PEPPER": "disposable-pepper",
        }
        self.environment.pop("EDGE_API_KEY", None)

    def query(self, statement, arguments=()):
        with closing(psycopg2.connect(self.db_url)) as conn, conn:
            with conn.cursor() as cursor:
                cursor.execute(statement, arguments)
                return cursor.fetchall() if cursor.description else None

    def start(self):
        if self.postgres_container:
            parsed = urlsplit(self.db_url)
            netloc = parsed.netloc.rsplit("@", 1)[0] + "@127.0.0.1:5432"
            database_url = urlunsplit(parsed._replace(netloc=netloc))
            args = [
                "docker",
                "run",
                "--detach",
                "--pull=never",
                "--cap-drop=ALL",
                "--security-opt",
                "no-new-privileges",
                "--user",
                "1000:1000",
                "--network",
                f"container:{self.postgres_container}",
                "--mount",
                f"type=bind,source={self.binary},target=/app/server,readonly",
                "--mount",
                f"type=bind,source={self.root / 'site'},target=/site,readonly",
                "--env",
                f"DATABASE_URL={database_url}",
                "--env",
                "SITE_DATA_DIR=/site",
                "--env",
                "JWT_SECRET=disposable-jwt",
                "--env",
                "PLATE_SECRET_PEPPER=disposable-pepper",
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
            # Refuse unrelated listeners; never signal them.
            with socket.socket() as probe:
                probe.bind(("0.0.0.0", 8000))
            self.process = subprocess.Popen(
                [self.binary],
                cwd=self.root / "server",
                env=self.environment,
                stdout=self.log,
                stderr=self.log,
            )
        deadline = time.monotonic() + 10
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
        instance.start()
        instance.query(
            "INSERT INTO edge_gateways (id, name, credential_digest) VALUES (%s, %s, %s)",
            (
                instance.gateway_id,
                "Delivery fixture",
                hashlib.sha256(KEY.encode()).digest(),
            ),
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
            "timestamp": "2026-10-11T12:00:00Z",
        },
    }


async def until(predicate):
    async def poll():
        while not predicate():
            await asyncio.sleep(0.01)

    await asyncio.wait_for(poll(), timeout=10)


async def receive_ready(connection):
    assert json.loads(await asyncio.wait_for(connection.recv(), timeout=3)) == {
        "type": "EDGE_READY",
        "delivery_version": 1,
    }


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
    event_id = gateway.save_event(outbox, json.dumps(message("unused")["payload"]))
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
