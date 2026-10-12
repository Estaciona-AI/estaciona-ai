"""Gateway credentials and scopes against Rust and disposable PostgreSQL."""

import asyncio
import hashlib
import json
import os
import time
from uuid import uuid4

import pytest
import websockets
from websockets.exceptions import InvalidStatus

from test_delivery_integration import (
    KEY,
    message,
    receive_ready,
    server as delivery_server,
)

server = delivery_server

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)


def test_revoked_credential_cannot_use_a_matching_legacy_global_key(server):
    server.query("UPDATE edge_gateways SET revoked_at = NOW()")
    server.environment["EDGE_API_KEY"] = KEY
    server.stop()
    server.start()

    async def run():
        with pytest.raises(InvalidStatus) as failure:
            async with websockets.connect(
                server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
            ):
                raise AssertionError("revoked credential must not upgrade")
        assert failure.value.response.status_code == 401

    asyncio.run(run())
    assert server.query("SELECT COUNT(*) FROM edge_event_receipts") == [(0,)]


def edge_connection(server, key=KEY):
    return websockets.connect(
        server.url,
        additional_headers={"Authorization": f"Bearer {key}"},
        close_timeout=0.5,
    )


async def deliver(connection, event_id=None, **changes):
    frame = message(event_id or str(uuid4()))
    frame["payload"].update(changes)
    await connection.send(json.dumps(frame))
    ack = json.loads(await asyncio.wait_for(connection.recv(), timeout=3))
    assert ack["type"] == "EDGE_ACK"
    assert ack["event_id"] == frame["event_id"]
    return ack


def test_individual_credential_authenticates_without_global_key(server):
    assert "EDGE_API_KEY" not in server.environment
    event_id = str(uuid4())

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            ack = await deliver(connection, event_id)
            assert (ack["outcome"], ack["duplicate"]) == ("applied", False)

    asyncio.run(run())
    assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
        ("occupied",)
    ]
    assert server.query(
        "SELECT outcome FROM edge_event_receipts WHERE event_id = %s",
        (event_id,),
    ) == [("applied",)]


@pytest.mark.parametrize("change", ["rotation", "revocation"])
def test_idle_edge_closes_within_seven_seconds_when_credential_changes(server, change):
    rotated_key = "22" * 32

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            if change == "rotation":
                server.query(
                    "UPDATE edge_gateways SET credential_digest = %s WHERE id = %s",
                    (hashlib.sha256(rotated_key.encode()).digest(), server.gateway_id),
                )
            else:
                server.query(
                    "UPDATE edge_gateways SET revoked_at = NOW() WHERE id = %s",
                    (server.gateway_id,),
                )
            started = time.monotonic()
            await asyncio.wait_for(connection.wait_closed(), timeout=7)
            assert time.monotonic() - started < 7
        with pytest.raises(InvalidStatus) as failure:
            async with edge_connection(server):
                raise AssertionError("old credential must not upgrade")
        assert failure.value.response.status_code == 401
        if change == "rotation":
            server.stop()
            server.start()
            async with edge_connection(server, rotated_key) as connection:
                await receive_ready(connection)
                assert (await deliver(connection))["outcome"] == "applied"
            assert server.query(
                "SELECT COUNT(*) FROM edge_camera_spots WHERE gateway_id = %s",
                (server.gateway_id,),
            ) == [(2,)]

    asyncio.run(run())
