# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Real local websocket + independent SQLite files, without loading ML runtimes."""

import asyncio
import importlib.util
import json
from pathlib import Path
import sqlite3

import websockets

import gateway

spec = importlib.util.spec_from_file_location(
    "vision_delivery", Path(__file__).resolve().parents[2] / "vision/src/delivery.py"
)
delivery_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delivery_module)


def test_lost_local_ack_reconnects_without_duplicate_and_survives_source_restart(
    tmp_path,
):
    async def run():
        db, metrics = str(tmp_path / "gateway.db"), str(tmp_path / "metrics.db")
        gateway.init_db(db)
        gateway.init_metrics_db(metrics)
        outbox_path = tmp_path / "vision.db"
        event_id = delivery_module.LocalOutbox(outbox_path).enqueue(
            '{"type":"SPOT_UPDATE","spot_id":"A-01","status":"free"}'
        )
        outbox = delivery_module.LocalOutbox(outbox_path)
        sync = asyncio.Event()
        drop = True

        class LostAck:
            def __init__(self, ws):
                self.ws, self.request = ws, ws.request

            async def recv(self):
                return await self.ws.recv()

            async def close(self, *args):
                return await self.ws.close(*args)

            async def send(self, text):
                nonlocal drop
                if json.loads(text)["type"] == "LOCAL_ACK" and drop:
                    drop = False
                    await self.ws.close()
                    raise OSError("injected lost ACK after SQLite commit")
                await self.ws.send(text)

        async def handle(ws):
            await gateway.handler(LostAck(ws), db, metrics, "local-test", sync)

        async with websockets.serve(handle, "127.0.0.1", 0) as server:
            url = f"ws://127.0.0.1:{server.sockets[0].getsockname()[1]}"
            delivery = delivery_module.LocalDelivery(url, outbox)
            ws = await delivery.send(None, None, {"Authorization": "Bearer local-test"})
            assert ws is not None
            await ws.close()
        assert outbox.first() is None
        with sqlite3.connect(db) as conn:
            assert conn.execute(
                "SELECT event_uuid FROM fallback_events"
            ).fetchall() == [(event_id,)]
        assert sync.is_set()

    asyncio.run(run())


def test_failed_gateway_commit_preserves_vision_event_for_retry(tmp_path):
    async def run():
        db, metrics = str(tmp_path / "gateway.db"), str(tmp_path / "metrics.db")
        gateway.init_db(db)
        gateway.init_metrics_db(metrics)
        with sqlite3.connect(db) as conn:
            conn.execute(
                "CREATE TRIGGER fail_insert BEFORE INSERT ON fallback_events BEGIN SELECT RAISE(ABORT,'injected disk failure'); END"
            )
        outbox = delivery_module.LocalOutbox(tmp_path / "vision.db")

        async def handle(ws):
            await gateway.handler(ws, db, metrics, "local-test", asyncio.Event())

        async with websockets.serve(handle, "127.0.0.1", 0) as server:
            delivery = delivery_module.LocalDelivery(
                f"ws://127.0.0.1:{server.sockets[0].getsockname()[1]}", outbox
            )
            assert (
                await delivery.send(
                    None,
                    '{"type":"SPOT_UPDATE"}',
                    {"Authorization": "Bearer local-test"},
                )
                is None
            )
            event_id = outbox.first()["event_id"]
            assert gateway.pending_deliveries(db) == []
            with sqlite3.connect(db) as conn:
                conn.execute("DROP TRIGGER fail_insert")
            ws = await delivery.send(None, None, {"Authorization": "Bearer local-test"})
            assert ws is not None
            await ws.close()
        assert outbox.first() is None
        with sqlite3.connect(db) as conn:
            assert conn.execute(
                "SELECT event_uuid FROM fallback_events"
            ).fetchall() == [(event_id,)]

    asyncio.run(run())
