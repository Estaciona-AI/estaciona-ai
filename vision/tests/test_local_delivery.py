# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Transport tests deliberately do not import CUDA, a detector or OpenCV."""

import asyncio
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import sqlite3
import threading
from unittest.mock import AsyncMock

import pytest
from delivery import LocalOutbox, LocalDelivery


def test_batch_survives_restart_with_distinct_ids_and_ordered_payloads(tmp_path):
    path = tmp_path / "outbox.db"
    payloads = [{"spot_id": f"A-{index:02}"} for index in range(20)]
    ids = LocalOutbox(path).enqueue_many([json.dumps(value) for value in payloads])
    assert len(set(ids)) == len(payloads)
    restored = LocalOutbox(path)
    for event_id, payload in zip(ids, payloads, strict=True):
        event = restored.first()
        assert event["event_id"] == event_id
        assert event["payload"] == payload
        restored.acknowledge(event_id)
    assert restored.first() is None


@pytest.mark.parametrize("limit", ["count", "bytes"])
def test_full_batch_does_not_partially_insert_or_remove_pending_events(tmp_path, limit):
    text = '{"spot_id":"A"}'
    outbox = LocalOutbox(
        tmp_path / "outbox.db",
        max_events=2 if limit == "count" else 100,
        max_bytes=len(text.encode()) * 2 if limit == "bytes" else 1024,
    )
    previous = outbox.enqueue(text)
    with pytest.raises(BufferError):
        outbox.enqueue_many([text, text])
    assert outbox.pending_count() == 1
    assert outbox.first()["event_id"] == previous


@pytest.mark.parametrize(
    "invalid",
    ["invalid JSON", "[]", '{"x":"' + "x" * (512 * 1024) + '"}'],
    ids=["invalid-json", "non-object", "oversized"],
)
def test_invalid_batch_leaves_existing_events_unchanged(tmp_path, invalid):
    outbox = LocalOutbox(tmp_path / "outbox.db")
    previous = outbox.enqueue('{"spot_id":"previous"}')
    with pytest.raises((ValueError, BufferError)):
        outbox.enqueue_many(['{"spot_id":"new"}', invalid])
    assert outbox.pending_count() == 1
    assert outbox.first()["event_id"] == previous


def test_batch_sql_failure_rolls_back_all_its_events(tmp_path):
    outbox = LocalOutbox(tmp_path / "outbox.db")
    previous = outbox.enqueue('{"spot_id":"previous"}')
    with closing(outbox.connect()) as conn, conn:
        conn.execute(
            "CREATE TRIGGER refuse_second BEFORE INSERT ON local_outbox "
            "WHEN json_extract(NEW.payload, '$.spot_id') = 'refused' "
            "BEGIN SELECT RAISE(ABORT, 'test storage failure'); END"
        )
    with pytest.raises(sqlite3.IntegrityError, match="test storage failure"):
        outbox.enqueue_many(['{"spot_id":"new"}', '{"spot_id":"refused"}'])
    assert LocalOutbox(outbox.path).pending_count() == 1
    assert outbox.first()["event_id"] == previous


def test_concurrent_batches_cannot_overfill_the_outbox(tmp_path):
    path = tmp_path / "outbox.db"
    LocalOutbox(path, max_events=3)
    ready = threading.Barrier(2)

    def insert(prefix):
        outbox = LocalOutbox(path, max_events=3)
        ready.wait(timeout=2)
        try:
            return outbox.enqueue_many(
                [json.dumps({"spot_id": f"{prefix}-{index}"}) for index in range(2)]
            )
        except BufferError:
            return None

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(insert, ["A", "B"]))
    assert sum(result is not None for result in results) == 1
    assert LocalOutbox(path).pending_count() == 2


def test_batch_replay_keeps_pending_uuids_after_partial_ack_loss(tmp_path):
    async def run():
        outbox = LocalOutbox(tmp_path / "outbox.db")
        ids = outbox.enqueue_many([json.dumps({"spot_id": str(i)}) for i in range(3)])
        delivery = LocalDelivery("ws://unused", outbox)
        disconnected = AsyncMock()
        disconnected.recv.side_effect = [
            json.dumps(
                {
                    "type": "LOCAL_ACK",
                    "version": 1,
                    "event_id": ids[0],
                    "outcome": "persisted",
                }
            ),
            OSError("lost ACK"),
        ]
        delivery.connect = AsyncMock(side_effect=OSError("offline"))
        assert await delivery.send(disconnected, None, {}) is None
        restored = LocalOutbox(outbox.path)
        assert restored.first()["event_id"] == ids[1]
        assert restored.pending_count() == 2
        recovered = AsyncMock()

        async def ack():
            event = json.loads(recovered.send.call_args.args[0])
            return json.dumps(
                {
                    "type": "LOCAL_ACK",
                    "version": 1,
                    "event_id": event["event_id"],
                    "outcome": "duplicate",
                }
            )

        recovered.recv.side_effect = ack
        assert (
            await LocalDelivery("ws://unused", restored).send(recovered, None, {})
            is recovered
        )
        assert [
            json.loads(call.args[0])["event_id"]
            for call in recovered.send.call_args_list
        ] == ids[1:]
        assert restored.first() is None

    asyncio.run(run())


def test_outbox_survives_restart_with_the_same_event_id(tmp_path):
    path = tmp_path / "outbox.db"
    first = LocalOutbox(path)
    event_id = first.enqueue('{"type":"SPOT_UPDATE","spot_id":"A-01"}')
    restored = LocalOutbox(path)
    assert restored.first()["event_id"] == event_id
    restored.acknowledge(event_id)
    assert restored.first() is None


def test_outbox_refuses_growth_instead_of_silently_dropping(tmp_path):
    outbox = LocalOutbox(tmp_path / "outbox.db", max_events=1)
    event_id = outbox.enqueue('{"type":"SPOT_UPDATE"}')
    with pytest.raises(BufferError):
        outbox.enqueue('{"type":"SPOT_UPDATE"}')
    assert outbox.first()["event_id"] == event_id


def test_missing_ack_keeps_the_same_event_for_retry(tmp_path):
    async def run():
        outbox = LocalOutbox(tmp_path / "outbox.db")
        delivery = LocalDelivery("ws://unused", outbox)
        ws = AsyncMock()
        ws.recv.side_effect = OSError("lost ACK")
        delivery.connect = AsyncMock(side_effect=OSError("offline"))
        assert await delivery.send(ws, '{"type":"SPOT_UPDATE"}', {}) is None
        event = outbox.first()
        assert event["event_id"] == json.loads(ws.send.call_args.args[0])["event_id"]
        recovered = AsyncMock()
        recovered.recv.return_value = json.dumps(
            {
                "type": "LOCAL_ACK",
                "version": 1,
                "event_id": event["event_id"],
                "outcome": "persisted",
            }
        )
        assert await delivery.send(recovered, None, {}) is recovered
        assert outbox.first() is None

    asyncio.run(run())


def test_wrong_ack_does_not_remove_pending_event(tmp_path):
    async def run():
        outbox = LocalOutbox(tmp_path / "outbox.db")
        delivery = LocalDelivery("ws://unused", outbox)
        ws = AsyncMock()
        ws.recv.return_value = (
            '{"type":"LOCAL_ACK","version":1,"event_id":"wrong","outcome":"persisted"}'
        )
        delivery.connect = AsyncMock(side_effect=OSError("offline"))
        assert await delivery.send(ws, '{"type":"SPOT_UPDATE"}', {}) is None
        assert outbox.first() is not None

    asyncio.run(run())


def test_outbox_bounds_aggregate_bytes_and_frame_size(tmp_path):
    outbox = LocalOutbox(tmp_path / "outbox.db", max_bytes=20)
    event_id = outbox.enqueue('{"a":"12345"}')
    with pytest.raises(BufferError):
        outbox.enqueue('{"a":"12345"}')
    assert outbox.first()["event_id"] == event_id
    with pytest.raises(BufferError):
        LocalOutbox(tmp_path / "large.db").enqueue(
            json.dumps({"padding": "x" * (512 * 1024)})
        )


def test_full_outbox_drains_before_accepting_the_next_transition(tmp_path):
    async def run():
        outbox = LocalOutbox(tmp_path / "outbox.db", max_events=1)
        old = outbox.enqueue('{"type":"SPOT_UPDATE","spot_id":"A"}')
        delivery = LocalDelivery("ws://unused", outbox)
        ws = AsyncMock()

        async def ack():
            event = json.loads(ws.send.call_args.args[0])
            return json.dumps(
                {
                    "type": "LOCAL_ACK",
                    "version": 1,
                    "event_id": event["event_id"],
                    "outcome": "persisted",
                }
            )

        ws.recv.side_effect = ack
        assert await delivery.send(ws, '{"type":"SPOT_UPDATE","spot_id":"B"}', {}) is ws
        events = [json.loads(call.args[0]) for call in ws.send.call_args_list]
        assert [event["payload"]["spot_id"] for event in events] == ["A", "B"]
        assert events[0]["event_id"] == old
        assert outbox.first() is None

    asyncio.run(run())


def test_full_offline_outbox_waits_without_dropping_durable_events(tmp_path):
    async def run():
        outbox = LocalOutbox(tmp_path / "outbox.db", max_events=1)
        old = outbox.enqueue('{"type":"SPOT_UPDATE","spot_id":"A"}')
        delivery = LocalDelivery("ws://unused", outbox)
        attempted = asyncio.Event()

        async def offline(headers):
            attempted.set()
            raise OSError("offline")

        delivery.connect = offline
        task = asyncio.create_task(
            delivery.send(None, '{"type":"SPOT_UPDATE","spot_id":"B"}', {})
        )
        await asyncio.wait_for(attempted.wait(), 1)
        assert not task.done()
        assert outbox.first()["event_id"] == old
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert LocalOutbox(outbox.path).first()["event_id"] == old

    asyncio.run(run())
