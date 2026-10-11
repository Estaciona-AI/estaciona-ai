import asyncio
from contextlib import suppress
import json
import sqlite3
from unittest.mock import AsyncMock, patch
from uuid import UUID, uuid4

import pytest

import gateway


def payload():
    return {
        "type": "SPOT_UPDATE",
        "spot_id": "A-01",
        "status": "occupied",
        "camera_id": "cam",
        "confidence": 0.9,
        "timestamp": "2026-10-11T12:00:00Z",
    }


def setup(tmp_path):
    db, metrics = str(tmp_path / "outbox.db"), str(tmp_path / "metrics.db")
    gateway.init_db(db)
    gateway.init_metrics_db(metrics)
    return db, metrics


def enqueue(db, metrics, message):
    event_id = gateway.save_event(db, json.dumps(message))
    gateway.save_metric(
        metrics,
        "cam",
        message.get("spot_id", "unknown"),
        message.get("status", "unknown"),
        "t0",
        "t1",
        "t2",
        event_id,
    )
    return event_id


async def until(predicate):
    async def poll():
        while not predicate():
            await asyncio.sleep(0)

    await asyncio.wait_for(poll(), timeout=2)


class Cloud:
    def __init__(self, on_send=None, version=1):
        self.responses = asyncio.Queue()
        self.responses.put_nowait(
            json.dumps({"type": "EDGE_READY", "delivery_version": version})
        )
        self.on_send, self.frames = on_send, []
        self.closed = asyncio.Event()

    async def send(self, text):
        frame = json.loads(text)
        self.frames.append(frame)
        if self.on_send:
            self.on_send(self, frame)

    async def recv(self):
        return await self.responses.get()

    async def close(self):
        self.closed.set()

    def ack(self, frame, outcome="applied", reason=None, **changes):
        message = {
            "type": "EDGE_ACK",
            "version": 1,
            "event_id": frame["event_id"],
            "outcome": outcome,
            "reason": reason,
            "duplicate": False,
        }
        message.update(changes)
        self.responses.put_nowait(json.dumps(message))


async def sync(db, metrics, cloud, predicate):
    with patch(
        "gateway.websockets.connect", new_callable=AsyncMock, return_value=cloud
    ):
        task = asyncio.create_task(
            gateway.sync_loop(db, metrics, "ws://test", "test", asyncio.Event())
        )
        try:
            await until(predicate)
        finally:
            task.cancel()
            with suppress(asyncio.CancelledError):
                await task


def test_backlog_migration_preserves_ids_and_synced_history(tmp_path):
    path = str(tmp_path / "legacy.db")
    with sqlite3.connect(path) as conn:
        conn.execute(
            "CREATE TABLE fallback_events (id INTEGER PRIMARY KEY AUTOINCREMENT, message_text TEXT NOT NULL, synced INTEGER DEFAULT 0, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)"
        )
        conn.executemany(
            "INSERT INTO fallback_events (message_text, synced) VALUES (?, ?)",
            [("pending", 0), ("history", 1)],
        )
    gateway.init_db(path)
    first = gateway.pending_deliveries(path)
    assert len(first) == 1
    UUID(first[0][1])
    gateway.init_db(path)
    assert gateway.pending_deliveries(path) == first
    with sqlite3.connect(path) as conn:
        assert conn.execute(
            "SELECT message_text, synced, event_uuid FROM fallback_events WHERE id = 2"
        ).fetchone() == ("history", 1, None)


def test_uuid_survives_restart(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(db, metrics, payload())
    UUID(event_id)
    gateway.init_db(db)
    assert gateway.pending_deliveries(db)[0][1] == event_id


def test_metrics_failure_does_not_leave_persisted_event_waiting(tmp_path):
    db, metrics = setup(tmp_path)
    with sqlite3.connect(metrics) as conn:
        conn.execute(
            "CREATE TRIGGER fail_metric BEFORE INSERT ON metrics BEGIN SELECT RAISE(FAIL, 'injected metrics failure'); END"
        )

    async def run():
        producer = AsyncMock()
        producer.request.headers = {"Authorization": "Bearer test"}
        producer.recv.side_effect = [json.dumps(payload()), Exception("Closed")]
        ready = asyncio.Event()
        await gateway.handler(producer, db, metrics, "test", ready)
        assert len(gateway.pending_deliveries(db)) == 1
        assert ready.is_set()

    asyncio.run(run())


def test_ack_confirms_only_one_of_two_events_for_the_same_spot(tmp_path):
    db, metrics = setup(tmp_path)
    first, second = enqueue(db, metrics, payload()), enqueue(db, metrics, payload())

    def send(cloud, frame):
        if frame["event_id"] == first:
            cloud.ack(frame)

    cloud = Cloud(send)
    asyncio.run(sync(db, metrics, cloud, lambda: len(cloud.frames) == 2))
    assert [row[1] for row in gateway.pending_deliveries(db)] == [second]
    with sqlite3.connect(metrics) as conn:
        assert dict(conn.execute("SELECT event_uuid, cloud_ack FROM metrics")) == {
            first: 1,
            second: 0,
        }


def test_bad_acks_cannot_confirm_an_event(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(db, metrics, payload())

    def send(cloud, frame):
        cloud.responses.put_nowait("not-json")
        for changes in [
            {"event_id": str(uuid4())},
            {"version": 2},
            {"version": True},
            {"outcome": "unknown"},
            {"duplicate": "yes"},
        ]:
            cloud.ack(frame, **changes)
        cloud.responses.put_nowait(
            json.dumps({"type": "EDGE_ACK", "version": 1, "event_id": event_id})
        )

    async def run():
        cloud = Cloud(send)
        with patch("gateway.ACK_TIMEOUT_SECONDS", 0.02):
            await sync(db, metrics, cloud, cloud.closed.is_set)

    asyncio.run(run())
    assert gateway.pending_deliveries(db)[0][1] == event_id
    with sqlite3.connect(metrics) as conn:
        assert conn.execute("SELECT cloud_ack FROM metrics").fetchone()[0] == 0


def test_incompatible_server_keeps_queue(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(db, metrics, payload())
    cloud = Cloud(version=2)
    asyncio.run(sync(db, metrics, cloud, cloud.closed.is_set))
    assert gateway.pending_deliveries(db)[0][1] == event_id
    assert cloud.frames == []


def test_rejection_is_diagnostic_and_does_not_block_next_event(tmp_path):
    db, metrics = setup(tmp_path)
    rejected, applied = enqueue(db, metrics, payload()), enqueue(db, metrics, payload())

    def send(cloud, frame):
        cloud.ack(frame, "rejected", "unknown_spot") if frame[
            "event_id"
        ] == rejected else cloud.ack(frame)

    asyncio.run(
        sync(db, metrics, Cloud(send), lambda: not gateway.pending_deliveries(db))
    )
    with sqlite3.connect(db) as conn:
        results = {
            event_id: (result, reason)
            for event_id, result, reason in conn.execute(
                "SELECT event_uuid, delivery_result, delivery_reason FROM fallback_events"
            )
        }
    assert results == {
        rejected: ("rejected", "unknown_spot"),
        applied: ("applied", None),
    }


@pytest.mark.parametrize(
    "invalid",
    [
        "null_character",
        "null_key",
        "nested_null",
        "nonfinite_number",
        "overflow_number",
    ],
)
def test_unrepresentable_json_is_rejected_without_blocking_queue(tmp_path, invalid):
    db, metrics = setup(tmp_path)
    message = payload()
    if invalid in ("null_character", "null_key", "nested_null"):
        if invalid == "null_character":
            message["camera_id"] = "cam\0"
        elif invalid == "null_key":
            message["key\0"] = "value"
        else:
            message["extra"] = [{"name": "nested\0"}]
        reason = "invalid_local_payload"
    else:
        message["confidence"] = float("nan" if invalid == "nonfinite_number" else "inf")
        reason = "invalid_local_json"
    rejected = enqueue(db, metrics, message)
    if invalid == "overflow_number":
        with sqlite3.connect(db) as conn:
            conn.execute(
                "UPDATE fallback_events SET message_text = replace(message_text, 'Infinity', '1e400') WHERE event_uuid = ?",
                (rejected,),
            )
    applied = enqueue(db, metrics, payload())
    cloud = Cloud(lambda cloud, frame: cloud.ack(frame))
    asyncio.run(sync(db, metrics, cloud, lambda: not gateway.pending_deliveries(db)))
    assert [frame["event_id"] for frame in cloud.frames] == [applied]
    with sqlite3.connect(db) as conn:
        assert conn.execute(
            "SELECT delivery_result, delivery_reason FROM fallback_events WHERE event_uuid = ?",
            (rejected,),
        ).fetchone() == ("rejected", reason)
    with sqlite3.connect(metrics) as conn:
        assert dict(conn.execute("SELECT event_uuid, cloud_ack FROM metrics")) == {
            rejected: 0,
            applied: 1,
        }


def test_timeout_retry_preserves_uuid_and_payload(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(db, metrics, payload())

    async def run():
        first, second = (
            Cloud(),
            Cloud(lambda cloud, frame: cloud.ack(frame, duplicate=True)),
        )
        with (
            patch(
                "gateway.websockets.connect",
                new_callable=AsyncMock,
                side_effect=[first, second],
            ),
            patch("gateway.ACK_TIMEOUT_SECONDS", 0.02),
            patch("gateway.RETRY_DELAY_SECONDS", 0),
        ):
            task = asyncio.create_task(
                gateway.sync_loop(db, metrics, "ws://test", "test", asyncio.Event())
            )
            try:
                await until(lambda: not gateway.pending_deliveries(db))
            finally:
                task.cancel()
                with suppress(asyncio.CancelledError):
                    await task
        assert first.frames[0]["event_id"] == second.frames[0]["event_id"] == event_id
        assert first.frames[0]["payload"] == second.frames[0]["payload"] == payload()

    asyncio.run(run())


def test_failure_after_metrics_commit_keeps_outbox_replayable(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(db, metrics, payload())
    with patch(
        "gateway.finish_event", side_effect=sqlite3.OperationalError("injected")
    ):
        try:
            gateway.acknowledge_event(db, metrics, event_id, "applied", None)
        except sqlite3.OperationalError:
            pass
        else:
            raise AssertionError("failure must propagate")
    assert gateway.pending_deliveries(db)[0][1] == event_id
    gateway.acknowledge_event(db, metrics, event_id, "applied", None)
    assert gateway.pending_deliveries(db) == []
    with sqlite3.connect(metrics) as conn:
        assert conn.execute(
            "SELECT cloud_ack, delivery_result FROM metrics"
        ).fetchone() == (1, "applied")


def test_legacy_delivery_does_not_fabricate_ack(tmp_path):
    db, metrics = setup(tmp_path)
    event_id = enqueue(
        db,
        metrics,
        {"type": "PATH_UPDATE", "from_node": "a", "to_node": "b", "is_active": True},
    )
    asyncio.run(sync(db, metrics, Cloud(), lambda: not gateway.pending_deliveries(db)))
    with sqlite3.connect(db) as conn:
        assert (
            conn.execute(
                "SELECT delivery_result FROM fallback_events WHERE event_uuid = ?",
                (event_id,),
            ).fetchone()[0]
            == "legacy_forwarded"
        )
    with sqlite3.connect(metrics) as conn:
        assert conn.execute("SELECT cloud_ack FROM metrics").fetchone()[0] == 0
