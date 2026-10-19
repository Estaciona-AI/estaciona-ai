# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import asyncio
import json
from unittest.mock import patch, AsyncMock

import pytest
import client
from delivery import LocalDelivery, LocalOutbox


@pytest.fixture(autouse=True)
def isolated_delivery(tmp_path, monkeypatch):
    monkeypatch.setattr(
        client,
        "DELIVERY",
        LocalDelivery(client.WS_URL, LocalOutbox(tmp_path / "outbox.db")),
    )


def acking_socket():
    ws = AsyncMock()

    async def recv():
        event = json.loads(ws.send.call_args.args[0])
        return json.dumps(
            {
                "type": "LOCAL_ACK",
                "version": 1,
                "event_id": event["event_id"],
                "outcome": "persisted",
            }
        )

    ws.recv.side_effect = recv
    return ws


def test_connect_ws_negotiates_local_delivery():
    async def run():
        with patch("websockets.connect", new_callable=AsyncMock) as connect:
            ws = AsyncMock()
            ws.recv.return_value = '{"type":"LOCAL_READY","delivery_version":1}'
            connect.return_value = ws
            assert await client.connect_ws({"Authorization": "Bearer token"}) is ws
            connect.assert_called_once_with(
                client.WS_URL,
                additional_headers={
                    "Authorization": "Bearer token",
                    "X-Estaciona-Local-Delivery": "1",
                },
                open_timeout=3,
            )

    asyncio.run(run())


def test_old_gateway_is_closed_without_dropping_pending_events():
    async def run():
        event_id = client.local_delivery().outbox.enqueue('{"type":"SPOT_UPDATE"}')
        with patch("websockets.connect", new_callable=AsyncMock) as connect:
            ws = AsyncMock()
            ws.recv.return_value = '{"type":"EDGE_READY","delivery_version":1}'
            connect.return_value = ws
            with pytest.raises(ValueError):
                await client.connect_ws({})
            ws.close.assert_awaited_once()
        assert client.local_delivery().outbox.first()["event_id"] == event_id

    asyncio.run(run())


def test_safe_send_success_requires_persistence_ack():
    async def run():
        ws = acking_socket()
        assert await client.safe_send(ws, '{"type":"SPOT_UPDATE"}', {}) is ws
        assert client.local_delivery().outbox.first() is None

    asyncio.run(run())


def test_safe_send_reconnect_uses_same_event_id():
    async def run():
        failed = AsyncMock()
        failed.recv.side_effect = OSError("lost ACK")
        recovered = acking_socket()
        client.local_delivery().connect = AsyncMock(return_value=recovered)
        assert await client.safe_send(failed, '{"type":"SPOT_UPDATE"}', {}) is recovered
        first = json.loads(failed.send.call_args.args[0])
        assert json.loads(recovered.send.call_args.args[0]) == first
        assert client.local_delivery().outbox.first() is None

    asyncio.run(run())


def test_safe_send_retries_queued_updates_after_reconnect_failure():
    async def run():
        failed = AsyncMock()
        failed.send.side_effect = OSError("offline")
        recovered = acking_socket()
        client.local_delivery().connect = AsyncMock(
            side_effect=[OSError("offline"), recovered]
        )
        assert (
            await client.safe_send(
                failed, '{"type":"SPOT_UPDATE","spot_id":"first"}', {}
            )
            is None
        )
        assert (
            await client.safe_send(
                None, '{"type":"SPOT_UPDATE","spot_id":"second"}', {}
            )
            is recovered
        )
        assert [
            json.loads(call.args[0])["payload"]["spot_id"]
            for call in recovered.send.call_args_list
        ] == ["first", "second"]
        assert client.local_delivery().outbox.first() is None

    asyncio.run(run())


def test_main_emits_initial_transition_and_heartbeat_in_microsecond_order(monkeypatch):
    import datetime
    import time
    from unittest.mock import Mock
    import numpy as np

    base = datetime.datetime(2026, 10, 19, 12, 0, 0, tzinfo=datetime.UTC)
    offsets = [0.1, 0.2, 0.7, 0.8, 30.9]
    clock = {"index": 0}

    def instant():
        return base + datetime.timedelta(seconds=offsets[clock["index"]])

    class Clock(datetime.datetime):
        @classmethod
        def now(cls, tz=None):
            return instant().astimezone(tz)

    real_gmtime = time.gmtime
    monkeypatch.setattr(client.datetime, "datetime", Clock)
    monkeypatch.setattr(client.time, "time", lambda: instant().timestamp())
    monkeypatch.setattr(
        client.time, "gmtime", lambda: real_gmtime(instant().timestamp())
    )
    monkeypatch.setattr(client, "EDGE_API_KEY", "disposable")
    monkeypatch.setattr(client, "VIDEO_PATH", "fixture.mp4")
    monkeypatch.setattr(client.sys, "argv", ["client.py"])
    monkeypatch.setattr(
        client, "load_spots", lambda _: {"A": [[0, 0], [5, 0], [5, 5], [0, 5]]}
    )
    capture = Mock()
    capture.isOpened.return_value = True
    capture.read.side_effect = lambda: (True, np.zeros((8, 8, 3), dtype=np.uint8))
    monkeypatch.setattr(client.cv2, "VideoCapture", lambda _: capture)
    for name in ("namedWindow", "resizeWindow", "imshow", "destroyAllWindows"):
        monkeypatch.setattr(client.cv2, name, Mock())

    def next_frame(_):
        if clock["index"] == len(offsets) - 1:
            return ord("q")
        clock["index"] += 1
        return 0

    monkeypatch.setattr(client.cv2, "waitKey", next_frame)
    detector = Mock()
    detector.detect.return_value = []
    monkeypatch.setattr(client, "load_detector", lambda *args, **kwargs: detector)
    monkeypatch.setattr(
        client, "compute_occupancy", Mock(side_effect=[0.0, 0.5, 0.5, 0.5, 0.5])
    )
    socket = AsyncMock()
    monkeypatch.setattr(client, "connect_ws", AsyncMock(return_value=socket))
    emitted = []

    async def send(ws, payload, headers):
        emitted.append(json.loads(payload))
        return ws

    monkeypatch.setattr(client, "safe_send", send)
    monkeypatch.setattr(client.asyncio, "sleep", AsyncMock())
    asyncio.run(client.main())
    assert [message["status"] for message in emitted] == [
        "free",
        "occupied",
        "occupied",
    ]
    timestamps = [message["timestamp"] for message in emitted]
    assert timestamps == [
        "2026-10-19T12:00:00.100000Z",
        "2026-10-19T12:00:00.800000Z",
        "2026-10-19T12:00:30.900000Z",
    ]
    assert timestamps == sorted(timestamps)
