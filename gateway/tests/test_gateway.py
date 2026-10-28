# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import sqlite3
import json
import asyncio
from datetime import datetime, timedelta
from unittest.mock import AsyncMock, patch

import pytest

from gateway import (
    init_db,
    init_metrics_db,
    save_event,
    finish_event,
    pending_deliveries,
    save_metric,
    update_metrics_forwarded_batch,
    is_authorized,
    handler,
    sync_loop,
    Retention,
)


def test_saved_events_are_pending_in_order_with_their_own_uuid(tmp_path):
    db_path = str(tmp_path / "test_fallback.db")
    init_db(db_path)
    first = save_event(db_path, "event_data_1")
    second = save_event(db_path, "event_data_2")
    events = pending_deliveries(db_path)
    assert [(uuid, text) for _, uuid, text in events] == [
        (first, "event_data_1"),
        (second, "event_data_2"),
    ]


def test_a_finished_event_leaves_the_queue_with_its_outcome(tmp_path):
    db_path = str(tmp_path / "test_fallback.db")
    init_db(db_path)
    event = save_event(db_path, "event_data_1")
    finish_event(db_path, event, "rejected", "invalid_local_payload")
    assert pending_deliveries(db_path) == []
    with sqlite3.connect(db_path) as conn:
        assert conn.execute(
            "SELECT synced, delivery_result, delivery_reason FROM fallback_events"
        ).fetchone() == (1, "rejected", "invalid_local_payload")


def test_save_and_update_metrics(tmp_path):
    db_path = str(tmp_path / "test_metrics.db")
    init_metrics_db(db_path)
    save_metric(
        db_path,
        "cam_01",
        "A-01",
        "occupied",
        "2026-06-05T12:00:00Z",
        "2026-06-05T12:00:01Z",
        "2026-06-05T12:00:02Z",
        event_uuid="metric-test-event",
    )
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()
    cursor.execute("SELECT edge_id, spot_id, status, cloud_ack FROM metrics")
    row = cursor.fetchone()
    assert row == ("cam_01", "A-01", "occupied", 0)
    conn.close()

    update_metrics_forwarded_batch(
        db_path, [("2026-06-05T12:00:03Z", "metric-test-event")]
    )
    conn = sqlite3.connect(db_path)
    cursor = conn.cursor()
    cursor.execute(
        "SELECT gateway_forwarded_ts, cloud_ack FROM metrics WHERE spot_id='A-01'"
    )
    row = cursor.fetchone()
    assert row == ("2026-06-05T12:00:03Z", 0)
    conn.close()


def test_is_authorized():
    expected_key = "secret_key"
    assert is_authorized({"Authorization": "Bearer secret_key"}, expected_key) is True
    assert is_authorized({"authorization": "Bearer secret_key"}, expected_key) is True
    assert is_authorized({"Authorization": "secret_key"}, expected_key) is False
    assert is_authorized({"Authorization": "Bearer wrong_key"}, expected_key) is False
    assert is_authorized({}, expected_key) is False


def test_handler_auth_success(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)

        mock_ws = AsyncMock()
        mock_ws.request.headers = {"Authorization": "Bearer secret_key"}
        mock_ws.recv.side_effect = [
            json.dumps(
                {
                    "type": "SPOT_UPDATE",
                    "camera_id": "cam-01",
                    "spot_id": "A-01",
                    "status": "occupied",
                    "timestamp": "2026-06-05T12:00:00Z",
                    "edge_sent_ts": "2026-06-05T12:00:01Z",
                }
            ),
            Exception("Closed"),
        ]

        sync_event = asyncio.Event()
        await handler(mock_ws, db_path, metrics_path, "secret_key", sync_event)

        events = pending_deliveries(db_path)
        assert len(events) == 1
        payload = json.loads(events[0][2])
        received = payload["gateway_received_at"]
        assert received.endswith("Z")
        assert datetime.fromisoformat(received).utcoffset() == timedelta(0)
        assert sync_event.is_set()
        with sqlite3.connect(metrics_path) as conn:
            assert conn.execute(
                "SELECT edge_id, spot_id, status, detection_ts, edge_sent_ts,"
                " gateway_received_ts FROM metrics"
            ).fetchall() == [
                (
                    "cam-01",
                    "A-01",
                    "occupied",
                    "2026-06-05T12:00:00Z",
                    "2026-06-05T12:00:01Z",
                    received,
                )
            ]

    asyncio.run(run())


def test_handler_auth_fail(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)

        mock_ws = AsyncMock()
        mock_ws.request.headers = {"Authorization": "Bearer wrong_key"}
        sync_event = asyncio.Event()

        await handler(mock_ws, db_path, metrics_path, "secret_key", sync_event)

        mock_ws.close.assert_called_once()
        assert len(pending_deliveries(db_path)) == 0

    asyncio.run(run())


def test_malformed_frame_does_not_discard_valid_batch(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        mock_ws = AsyncMock()
        mock_ws.request.headers = {"Authorization": "Bearer secret_key"}
        valid = json.dumps({"type": "SPOT_UPDATE", "spot_id": "A-01", "status": "free"})
        mock_ws.recv.side_effect = [
            valid,
            "{invalid",
            "[1]",
            valid,
            Exception("Closed"),
        ]

        await handler(mock_ws, db_path, metrics_path, "secret_key", asyncio.Event())

        assert len(pending_deliveries(db_path)) == 2

    asyncio.run(run())


def test_sync_loop_sends_to_cloud(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)

        payload = {
            "type": "SPOT_UPDATE",
            "spot_id": "A-01",
            "status": "occupied",
            "camera_id": "cam_01",
            "confidence": 0.9,
            "timestamp": "2026-06-05T12:00:00Z",
            "edge_sent_ts": "2026-06-05T12:00:01Z",
        }
        event_uuid = save_event(db_path, json.dumps(payload))
        save_metric(
            metrics_path,
            "cam_01",
            "A-01",
            "occupied",
            payload["timestamp"],
            payload["edge_sent_ts"],
            "2026-06-05T12:00:02Z",
            event_uuid=event_uuid,
        )

        mock_cloud_ws = AsyncMock()
        responses = asyncio.Queue()
        responses.put_nowait(json.dumps({"type": "EDGE_READY", "delivery_version": 1}))

        async def send(frame):
            event = json.loads(frame)
            responses.put_nowait(
                json.dumps(
                    {
                        "type": "EDGE_ACK",
                        "version": 1,
                        "event_id": event["event_id"],
                        "outcome": "applied",
                        "reason": None,
                        "duplicate": False,
                    }
                )
            )

        mock_cloud_ws.send.side_effect = send
        mock_cloud_ws.recv.side_effect = responses.get
        with patch("websockets.connect", new_callable=AsyncMock) as mock_connect:
            mock_connect.return_value = mock_cloud_ws

            sync_event = asyncio.Event()
            task = asyncio.create_task(
                sync_loop(
                    db_path,
                    metrics_path,
                    "wss://cloud/ws",
                    "secret_key",
                    sync_event,
                )
            )
            for _ in range(1000):
                if not pending_deliveries(db_path):
                    break
                await asyncio.sleep(0.01)
            task.cancel()
            try:
                await task
            except asyncio.CancelledError:
                pass

            sent_str = mock_cloud_ws.send.call_args[0][0]
            sent_payload = json.loads(sent_str)
            assert sent_payload["type"] == "EDGE_EVENT"
            assert sent_payload["event_id"] == event_uuid
            assert sent_payload["payload"] == payload
            assert "gateway_forwarded_at" in sent_payload

            assert len(pending_deliveries(db_path)) == 0

            conn = sqlite3.connect(metrics_path)
            cursor = conn.cursor()
            cursor.execute(
                "SELECT gateway_forwarded_ts, cloud_ack FROM metrics WHERE spot_id='A-01'"
            )
            row = cursor.fetchone()
            assert row[0] is not None
            assert row[1] == 1
            conn.close()

    asyncio.run(run())


def test_send_without_cloud_ack_preserves_pending_event(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        save_event(
            db_path,
            json.dumps(
                {
                    "type": "SPOT_UPDATE",
                    "spot_id": "A-01",
                    "status": "occupied",
                    "camera_id": "cam_01",
                    "confidence": 0.9,
                    "timestamp": "2026-06-05T12:00:00Z",
                }
            ),
        )
        sent = asyncio.Event()
        ready_sent = False

        async def send(_message):
            sent.set()

        async def recv():
            nonlocal ready_sent
            if not ready_sent:
                ready_sent = True
                return json.dumps({"type": "EDGE_READY", "delivery_version": 1})
            await asyncio.Future()

        cloud = AsyncMock()
        cloud.send.side_effect = send
        cloud.recv.side_effect = recv
        with patch("websockets.connect", new_callable=AsyncMock, return_value=cloud):
            task = asyncio.create_task(
                sync_loop(
                    db_path, metrics_path, "wss://cloud/ws", "test", asyncio.Event()
                )
            )
            try:
                await asyncio.wait_for(sent.wait(), timeout=1)
                await asyncio.sleep(0.01)
            finally:
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass

        assert len(pending_deliveries(db_path)) == 1

    asyncio.run(run())


def test_continuous_input_commits_without_waiting_for_idle(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        ws = AsyncMock()
        ws.request.headers = {"Authorization": "Bearer secret_key"}
        sync_event = asyncio.Event()
        reads = 0
        paused = asyncio.Event()

        async def recv():
            nonlocal reads
            reads += 1
            if reads > 64:
                paused.set()
                await asyncio.Future()
            return json.dumps({"type": "SPOT_UPDATE", "spot_id": str(reads)})

        ws.recv.side_effect = recv
        task = asyncio.create_task(
            handler(ws, db_path, metrics_path, "secret_key", sync_event)
        )
        try:
            await asyncio.wait_for(paused.wait(), 2)
            assert len(pending_deliveries(db_path)) == 64
            assert sync_event.is_set()
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(run())


def test_large_frames_flush_before_next_receive(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        ws = AsyncMock()
        ws.request.headers = {"Authorization": "Bearer secret_key"}
        frame = json.dumps({"type": "SPOT_UPDATE", "padding": "x" * (256 * 1024)})
        reads = 0
        persisted_before_second_receive = False

        async def recv():
            nonlocal reads, persisted_before_second_receive
            reads += 1
            if reads == 1:
                return frame
            if reads == 2:
                persisted_before_second_receive = len(pending_deliveries(db_path)) == 1
            raise RuntimeError("closed")

        ws.recv.side_effect = recv
        await handler(ws, db_path, metrics_path, "secret_key", asyncio.Event())
        assert persisted_before_second_receive

    asyncio.run(run())


def test_continuous_slow_input_has_a_batch_deadline(tmp_path):
    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        ws = AsyncMock()
        ws.request.headers = {"Authorization": "Bearer secret_key"}

        # A frame every 4 ms never reaches the existing 10 ms idle timeout.
        async def recv():
            await asyncio.sleep(0.004)
            return '{"type":"SPOT_UPDATE","spot_id":"A-01"}'

        ws.recv.side_effect = recv
        sync_event = asyncio.Event()
        task = asyncio.create_task(
            handler(ws, db_path, metrics_path, "secret_key", sync_event)
        )
        try:
            await asyncio.wait_for(sync_event.wait(), 0.20)
            assert pending_deliveries(db_path)
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    asyncio.run(run())


def test_local_delivery_replay_has_one_persisted_event_and_ack_after_commit(tmp_path):
    from uuid import uuid4

    async def run():
        db_path = str(tmp_path / "fallback.db")
        metrics_path = str(tmp_path / "metrics.db")
        init_db(db_path)
        init_metrics_db(metrics_path)
        event_id = str(uuid4())
        payload = {"type": "SPOT_UPDATE", "spot_id": "A-01", "status": "free"}
        frame = json.dumps(
            {
                "type": "LOCAL_EVENT",
                "version": 1,
                "event_id": event_id,
                "payload": payload,
            }
        )
        for expected in ["persisted", "duplicate"]:
            ws = AsyncMock()
            ws.request.headers = {
                "Authorization": "Bearer key",
                "X-Estaciona-Local-Delivery": "1",
            }
            ws.recv.side_effect = [frame, RuntimeError("closed")]
            acks = []

            async def send(text):
                message = json.loads(text)
                if message["type"] == "LOCAL_ACK":
                    # A separate connection must already see the event.
                    rows = pending_deliveries(db_path)
                    assert len(rows) == 1
                    assert json.loads(rows[0][2])["spot_id"] == "A-01"
                    acks.append(message)

            ws.send.side_effect = send
            await handler(ws, db_path, metrics_path, "key", asyncio.Event())
            assert acks == [
                {
                    "type": "LOCAL_ACK",
                    "version": 1,
                    "event_id": event_id,
                    "outcome": expected,
                }
            ]
        with sqlite3.connect(metrics_path) as conn:
            assert conn.execute("SELECT COUNT(*) FROM metrics").fetchone()[0] == 1

    asyncio.run(run())


def test_local_uuid_conflict_cannot_replace_persisted_payload(tmp_path):
    from uuid import uuid4

    async def run():
        db, metrics = str(tmp_path / "fallback.db"), str(tmp_path / "metrics.db")
        init_db(db)
        init_metrics_db(metrics)
        event_id = str(uuid4())
        for status in ["free", "occupied"]:
            ws = AsyncMock()
            ws.request.headers = {
                "Authorization": "Bearer key",
                "X-Estaciona-Local-Delivery": "1",
            }
            ws.recv.side_effect = [
                json.dumps(
                    {
                        "type": "LOCAL_EVENT",
                        "version": 1,
                        "event_id": event_id,
                        "payload": {"type": "SPOT_UPDATE", "status": status},
                    }
                ),
                OSError("closed"),
            ]
            await handler(ws, db, metrics, "key", asyncio.Event())
            ack = json.loads(ws.send.call_args.args[0])
            assert ack["outcome"] == ("persisted" if status == "free" else "rejected")
        assert json.loads(pending_deliveries(db)[0][2])["status"] == "free"

    asyncio.run(run())


def local_socket(frames, local=True):
    ws = AsyncMock()
    ws.request.headers = {"Authorization": "Bearer key"}
    if local:
        ws.request.headers["X-Estaciona-Local-Delivery"] = "1"
    ws.recv.side_effect = [*frames, OSError("closed")]
    return ws


def local_event(event_id, payload, version=1):
    return {
        "type": "LOCAL_EVENT",
        "version": version,
        "event_id": event_id,
        "payload": payload,
    }


SPOT = {"type": "SPOT_UPDATE", "spot_id": "A-01", "status": "free"}
EVENT_ID = "6f1c2a9e-3b7d-4e8a-9c41-2d5f8b0a7e13"


@pytest.mark.parametrize(
    "frame,local,reason",
    [
        (
            local_event(EVENT_ID, SPOT, version=2),
            True,
            "Invalid local delivery envelope",
        ),
        (
            local_event(EVENT_ID, SPOT, version="1"),
            True,
            "Invalid local delivery envelope",
        ),
        (
            local_event(EVENT_ID, SPOT, version=True),
            True,
            "Invalid local delivery envelope",
        ),
        (local_event(EVENT_ID, [SPOT]), True, "Invalid local delivery envelope"),
        (local_event(EVENT_ID, SPOT), False, "Invalid local delivery envelope"),
        (local_event("not-a-uuid", SPOT), True, "Invalid event UUID"),
        (local_event(None, SPOT), True, "Invalid event UUID"),
        (SPOT, True, "Local delivery envelope required"),
    ],
)
def test_invalid_local_delivery_closes_with_policy_violation(
    tmp_path, frame, local, reason
):
    async def run():
        db, metrics = str(tmp_path / "fallback.db"), str(tmp_path / "metrics.db")
        init_db(db)
        init_metrics_db(metrics)
        ws = local_socket([json.dumps(frame)], local=local)

        await handler(ws, db, metrics, "key", asyncio.Event())

        ws.close.assert_awaited_once_with(1008, reason)
        assert pending_deliveries(db) == []
        assert not any(
            json.loads(call.args[0])["type"] == "LOCAL_ACK"
            for call in ws.send.await_args_list
        )

    asyncio.run(run())


def test_local_replay_with_reordered_keys_is_a_duplicate(tmp_path):
    async def run():
        db, metrics = str(tmp_path / "fallback.db"), str(tmp_path / "metrics.db")
        init_db(db)
        init_metrics_db(metrics)
        outcomes = []
        for payload in (SPOT, dict(reversed(SPOT.items()))):
            ws = local_socket([json.dumps(local_event(EVENT_ID, payload))])
            await handler(ws, db, metrics, "key", asyncio.Event())
            outcomes.append(json.loads(ws.send.await_args.args[0])["outcome"])
        assert outcomes == ["persisted", "duplicate"]
        assert len(pending_deliveries(db)) == 1

    asyncio.run(run())


def test_full_storage_refuses_the_batch_without_acknowledging_it(tmp_path):
    async def run():
        db, metrics = str(tmp_path / "fallback.db"), str(tmp_path / "metrics.db")
        init_db(db)
        init_metrics_db(metrics)
        ws = local_socket([json.dumps(local_event(EVENT_ID, SPOT))])
        sync_event = asyncio.Event()

        await handler(ws, db, metrics, "key", sync_event, Retention(max_db_bytes=1))

        ws.close.assert_awaited_once_with(1013, "Gateway storage full")
        assert pending_deliveries(db) == []
        assert [
            json.loads(call.args[0])["type"] for call in ws.send.await_args_list
        ] == ["LOCAL_READY"]
        assert not sync_event.is_set()

    asyncio.run(run())
