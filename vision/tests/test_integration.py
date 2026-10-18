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
