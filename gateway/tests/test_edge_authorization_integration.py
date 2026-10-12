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
        "SELECT gateway_id::text, outcome FROM edge_event_receipts WHERE event_id = %s",
        (event_id,),
    ) == [(server.gateway_id, "applied")]


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


def provision_gateway(server, key, camera_id, spot_id):
    gateway_id = str(uuid4())
    server.query(
        "INSERT INTO edge_gateways (id, name, credential_digest) VALUES (%s, %s, %s)",
        (gateway_id, "Additional test gateway", hashlib.sha256(key.encode()).digest()),
    )
    server.query(
        "INSERT INTO edge_camera_grants (gateway_id, camera_id, spot_update, car_detected) "
        "VALUES (%s, %s, TRUE, TRUE)",
        (gateway_id, camera_id),
    )
    server.query(
        "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) VALUES (%s, %s, %s)",
        (gateway_id, camera_id, spot_id),
    )
    return gateway_id


def test_same_uuid_is_deduplicated_only_within_its_gateway_owner(server):
    other_key = "44" * 32
    other_id = provision_gateway(server, other_key, "test-camera", "R02-A")
    event_id = str(uuid4())
    server.query(
        "CREATE TABLE effect_count (updates INTEGER NOT NULL); INSERT INTO effect_count VALUES (0); "
        "CREATE FUNCTION count_effect() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN "
        "UPDATE effect_count SET updates = updates + 1; RETURN NEW; END $$; "
        "CREATE TRIGGER count_effect BEFORE UPDATE ON spots FOR EACH ROW EXECUTE FUNCTION count_effect();"
    )

    async def run():
        for key in (KEY, other_key):
            async with edge_connection(server, key) as connection:
                await receive_ready(connection)
                first = await deliver(connection, event_id)
                assert (first["outcome"], first["duplicate"]) == ("applied", False)
                repeated = await deliver(connection, event_id)
                assert (repeated["outcome"], repeated["duplicate"]) == ("applied", True)

    asyncio.run(run())
    assert server.query("SELECT updates FROM effect_count") == [(2,)]
    assert set(
        server.query(
            "SELECT gateway_id::text, outcome FROM edge_event_receipts WHERE event_id = %s",
            (event_id,),
        )
    ) == {(server.gateway_id, "applied"), (other_id, "applied")}


def test_legacy_receipt_blocks_reapplication_until_reconciled(server):
    event_id = str(uuid4())
    legacy_payload = message(event_id)["payload"]
    server.query(
        "INSERT INTO edge_event_receipts (event_id, payload, outcome, gateway_id) "
        "VALUES (%s, %s::jsonb, 'ignored', NULL)",
        (event_id, json.dumps(legacy_payload)),
    )

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            ack = await deliver(connection, event_id)
            assert (ack["outcome"], ack["duplicate"], ack["reason"]) == (
                "rejected",
                False,
                "legacy_event_receipt",
            )

    asyncio.run(run())
    assert server.query(
        "SELECT gateway_id::text, outcome FROM edge_event_receipts WHERE event_id = %s "
        "ORDER BY gateway_id NULLS FIRST",
        (event_id,),
    ) == [(None, "ignored"), (server.gateway_id, "rejected")]
    assert server.query(
        "SELECT payload, outcome FROM edge_event_receipts "
        "WHERE event_id = %s AND gateway_id IS NULL",
        (event_id,),
    ) == [(legacy_payload, "ignored")]
    assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [("free",)]
