# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Transport tests deliberately do not import CUDA, a detector or OpenCV."""

import asyncio
import json
from unittest.mock import AsyncMock

import pytest
from delivery import LocalOutbox, LocalDelivery


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
