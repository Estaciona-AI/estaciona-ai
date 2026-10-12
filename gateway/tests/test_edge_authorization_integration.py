"""Gateway credentials and scopes against Rust and disposable PostgreSQL."""

import asyncio
from contextlib import suppress
import hashlib
import json
import os
import sqlite3
import time
from unittest.mock import patch
from uuid import uuid4

import pytest
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus

import gateway

from test_delivery_integration import (
    KEY,
    message,
    receive_ready,
    server as delivery_server,
    until,
)
from test_session_integration import held_table_lock

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


def add_other_spot(server):
    server.query(
        "INSERT INTO spots (id, status, parking_lot, x, y, z) "
        "VALUES ('R05-OTHER', 'free', 'Main', 99, 0, 99)"
    )


async def expect_policy_close(connection):
    with pytest.raises(ConnectionClosed) as failure:
        await asyncio.wait_for(connection.recv(), timeout=3)
    assert failure.value.rcvd is not None
    assert failure.value.rcvd.code == 1008


@pytest.mark.parametrize("forgery", ["camera", "spot", "mapped_spot"])
def test_forbidden_durable_event_is_terminal_and_next_authorized_event_applies(
    server, tmp_path, forgery
):
    add_other_spot(server)
    server.query(
        "CREATE TABLE effect_count (updates INTEGER NOT NULL); INSERT INTO effect_count VALUES (0); "
        "CREATE FUNCTION count_effect() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN "
        "UPDATE effect_count SET updates = updates + 1; RETURN NEW; END $$; "
        "CREATE TRIGGER count_effect BEFORE UPDATE ON spots FOR EACH ROW EXECUTE FUNCTION count_effect();"
    )
    payload = message("unused")["payload"]
    if forgery == "camera":
        payload["camera_id"] = "foreign-camera"
    elif forgery == "spot":
        payload["spot_id"] = "R05-OTHER"
    else:
        payload["spot_id"] = "foreign-alias"
        server.query(
            "INSERT INTO vision_spot_mappings (camera_id, vision_spot_name, dashboard_spot_id) "
            "VALUES ('test-camera', 'foreign-alias', 'R05-OTHER')"
        )
    outbox, metrics = str(tmp_path / "outbox.db"), str(tmp_path / "metrics.db")
    gateway.init_db(outbox)
    gateway.init_metrics_db(metrics)
    rejected_id = gateway.save_event(outbox, json.dumps(payload))
    accepted_id = gateway.save_event(outbox, json.dumps(message("unused")["payload"]))

    async def run():
        sync = asyncio.create_task(
            gateway.sync_loop(outbox, metrics, server.url, KEY, asyncio.Event())
        )
        try:
            await until(lambda: not gateway.pending_deliveries(outbox))
        finally:
            sync.cancel()
            with suppress(asyncio.CancelledError):
                await sync

    asyncio.run(run())
    with sqlite3.connect(outbox) as conn:
        rejected = conn.execute(
            "SELECT synced, delivery_result, delivery_reason FROM fallback_events "
            "WHERE event_uuid = ?",
            (rejected_id,),
        ).fetchone()
        assert rejected[:2] == (1, "rejected")
        assert rejected[2]
        assert conn.execute(
            "SELECT synced, delivery_result FROM fallback_events WHERE event_uuid = ?",
            (accepted_id,),
        ).fetchone() == (1, "applied")
    assert server.query("SELECT status FROM spots WHERE id = 'R05-OTHER'") == [
        ("free",)
    ]
    assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
        ("occupied",)
    ]
    assert server.query(
        "SELECT outcome, reason FROM edge_event_receipts WHERE gateway_id = %s AND event_id = %s",
        (server.gateway_id, rejected_id),
    ) == [("rejected", rejected[2])]

    # Replaying the same denied event returns its terminal receipt without new effects.
    async def replay():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            await connection.send(
                json.dumps(
                    {
                        "type": "EDGE_EVENT",
                        "version": 1,
                        "event_id": rejected_id,
                        "payload": payload,
                    }
                )
            )
            ack = json.loads(await asyncio.wait_for(connection.recv(), timeout=3))
            assert (ack["outcome"], ack["duplicate"], ack["reason"]) == (
                "rejected",
                True,
                rejected[2],
            )

    asyncio.run(replay())
    assert server.query("SELECT updates FROM effect_count") == [(1,)]


@pytest.mark.parametrize("forgery", ["camera", "spot"])
def test_forbidden_legacy_spot_update_closes_without_mutation(server, forgery):
    add_other_spot(server)
    payload = message("unused")["payload"]
    payload["camera_id" if forgery == "camera" else "spot_id"] = (
        "foreign-camera" if forgery == "camera" else "R05-OTHER"
    )

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            await connection.send(json.dumps(payload))
            await expect_policy_close(connection)
        assert server.query("SELECT id, status FROM spots ORDER BY id") == [
            ("R02-A", "free"),
            ("R05-OTHER", "free"),
        ]
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            assert (await deliver(connection))["outcome"] == "applied"

    asyncio.run(run())
    assert server.query("SELECT status FROM spots WHERE id = 'R05-OTHER'") == [
        ("free",)
    ]
    assert server.query("SELECT COUNT(*) FROM edge_event_receipts") == [(1,)]


def test_same_uuid_from_another_gateway_cannot_reveal_the_first_receipt(server):
    add_other_spot(server)
    server.query(
        "CREATE TABLE effect_count (updates INTEGER NOT NULL); INSERT INTO effect_count VALUES (0); "
        "CREATE FUNCTION count_effect() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN "
        "UPDATE effect_count SET updates = updates + 1; RETURN NEW; END $$; "
        "CREATE TRIGGER count_effect BEFORE UPDATE ON spots FOR EACH ROW EXECUTE FUNCTION count_effect();"
    )
    other_key = "33" * 32
    other_id = provision_gateway(server, other_key, "other-camera", "R05-OTHER")
    shared_event_id = str(uuid4())

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            first = await deliver(connection, shared_event_id)
            assert (first["outcome"], first["duplicate"]) == ("applied", False)
        async with edge_connection(server, other_key) as connection:
            await receive_ready(connection)
            forbidden = await deliver(connection, shared_event_id)
            assert (forbidden["outcome"], forbidden["duplicate"]) == ("rejected", False)
            assert forbidden["reason"]
            allowed = await deliver(
                connection, camera_id="other-camera", spot_id="R05-OTHER"
            )
            assert (allowed["outcome"], allowed["duplicate"]) == ("applied", False)

    asyncio.run(run())
    assert server.query(
        "SELECT gateway_id::text, outcome FROM edge_event_receipts WHERE event_id = %s "
        "ORDER BY outcome",
        (shared_event_id,),
    ) == [(server.gateway_id, "applied"), (other_id, "rejected")]
    assert server.query("SELECT updates FROM effect_count") == [(2,)]


def test_authentication_database_failure_keeps_outbox_pending_until_retry(
    server, tmp_path
):
    outbox, metrics = str(tmp_path / "outbox.db"), str(tmp_path / "metrics.db")
    gateway.init_db(outbox)
    gateway.init_metrics_db(metrics)
    event_id = gateway.save_event(outbox, json.dumps(message("unused")["payload"]))
    gateway.save_metric(
        metrics, "test-camera", "R02-A", "occupied", "t0", "t1", "t2", event_id
    )

    async def run():
        failed = asyncio.Event()
        statuses = []
        real_connect = websockets.connect

        async def observe_actual_handshake(*args, **kwargs):
            try:
                return await real_connect(*args, **kwargs)
            except InvalidStatus as error:
                statuses.append(error.response.status_code)
                failed.set()
                raise

        sync = None
        with held_table_lock(server, "edge_gateways"):
            try:
                with (
                    patch("gateway.websockets.connect", observe_actual_handshake),
                    patch("gateway.RETRY_DELAY_SECONDS", 60),
                ):
                    sync = asyncio.create_task(
                        gateway.sync_loop(
                            outbox, metrics, server.url, KEY, asyncio.Event()
                        )
                    )
                    await asyncio.wait_for(failed.wait(), timeout=5)
                    assert statuses == [503]
                    assert gateway.pending_deliveries(outbox)[0][1] == event_id
                    with sqlite3.connect(metrics) as conn:
                        assert conn.execute(
                            "SELECT cloud_ack FROM metrics"
                        ).fetchone() == (0,)
                    assert server.query("SELECT COUNT(*) FROM edge_event_receipts") == [
                        (0,)
                    ]
            finally:
                if sync is not None:
                    sync.cancel()
                    with suppress(asyncio.CancelledError):
                        await sync
        retry = asyncio.create_task(
            gateway.sync_loop(outbox, metrics, server.url, KEY, asyncio.Event())
        )
        try:
            await until(lambda: not gateway.pending_deliveries(outbox))
        finally:
            retry.cancel()
            with suppress(asyncio.CancelledError):
                await retry

    asyncio.run(run())
    assert server.query(
        "SELECT outcome FROM edge_event_receipts WHERE gateway_id = %s AND event_id = %s",
        (server.gateway_id, event_id),
    ) == [("applied",)]
    with sqlite3.connect(metrics) as conn:
        assert conn.execute(
            "SELECT cloud_ack, delivery_result FROM metrics"
        ).fetchone() == (1, "applied")


@pytest.mark.parametrize("change", ["operation", "spot_grant"])
def test_next_durable_event_observes_changed_camera_grants(server, change):
    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            if change == "operation":
                server.query(
                    "UPDATE edge_camera_grants SET spot_update = FALSE "
                    "WHERE gateway_id = %s AND camera_id = 'test-camera'",
                    (server.gateway_id,),
                )
            else:
                server.query(
                    "DELETE FROM edge_camera_spots WHERE gateway_id = %s "
                    "AND camera_id = 'test-camera' AND spot_id = 'R02-A'",
                    (server.gateway_id,),
                )
            denied = await deliver(connection)
            assert (denied["outcome"], denied["duplicate"]) == ("rejected", False)
            assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
                ("free",)
            ]
            if change == "operation":
                server.query(
                    "UPDATE edge_camera_grants SET spot_update = TRUE "
                    "WHERE gateway_id = %s AND camera_id = 'test-camera'",
                    (server.gateway_id,),
                )
            else:
                server.query(
                    "INSERT INTO edge_camera_spots (gateway_id, camera_id, spot_id) "
                    "VALUES (%s, 'test-camera', 'R02-A')",
                    (server.gateway_id,),
                )
            assert (await deliver(connection))["outcome"] == "applied"

    asyncio.run(run())


def test_rotated_credential_replays_its_existing_receipt_without_another_effect(server):
    event_id = str(uuid4())
    rotated_key = "55" * 32
    server.query(
        "CREATE TABLE effect_count (updates INTEGER NOT NULL); INSERT INTO effect_count VALUES (0); "
        "CREATE FUNCTION count_effect() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN "
        "UPDATE effect_count SET updates = updates + 1; RETURN NEW; END $$; "
        "CREATE TRIGGER count_effect BEFORE UPDATE ON spots FOR EACH ROW EXECUTE FUNCTION count_effect();"
    )

    async def run():
        async with edge_connection(server) as connection:
            await receive_ready(connection)
            ack = await deliver(connection, event_id)
            assert (ack["outcome"], ack["duplicate"]) == ("applied", False)
        server.query(
            "UPDATE edge_gateways SET credential_digest = %s WHERE id = %s",
            (hashlib.sha256(rotated_key.encode()).digest(), server.gateway_id),
        )
        with pytest.raises(InvalidStatus) as failure:
            async with edge_connection(server):
                raise AssertionError("old credential must not upgrade")
        assert failure.value.response.status_code == 401
        async with edge_connection(server, rotated_key) as connection:
            await receive_ready(connection)
            ack = await deliver(connection, event_id)
            assert (ack["outcome"], ack["duplicate"]) == ("applied", True)

    asyncio.run(run())
    assert server.query("SELECT updates FROM effect_count") == [(1,)]
    assert server.query(
        "SELECT gateway_id::text, outcome FROM edge_event_receipts WHERE event_id = %s",
        (event_id,),
    ) == [(server.gateway_id, "applied")]
