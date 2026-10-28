# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Confirmed events and metrics expire; pending events and the budget hold."""

import asyncio
from contextlib import closing
import datetime
import importlib.util
import json
from pathlib import Path
import sqlite3
import time

import pytest
import websockets

import gateway

spec = importlib.util.spec_from_file_location(
    "vision_delivery", Path(__file__).resolve().parents[2] / "vision/src/delivery.py"
)
delivery_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delivery_module)

DAY = datetime.timedelta(days=1)


def databases(tmp_path):
    db, metrics = str(tmp_path / "gateway.db"), str(tmp_path / "metrics.db")
    gateway.init_db(db)
    gateway.init_metrics_db(metrics)
    return db, metrics


def aged(db, table, ids, age):
    stamp = (datetime.datetime.now(datetime.UTC) - age).strftime("%Y-%m-%d %H:%M:%S")
    with sqlite3.connect(db) as conn:
        conn.executemany(
            f"UPDATE {table} SET created_at = ? WHERE id = ?",
            [(stamp, row) for row in ids],
        )


def event(db, *, synced, age=datetime.timedelta(0)):
    gateway.save_event(db, json.dumps({"type": "SPOT_UPDATE", "spot_id": "A"}))
    with sqlite3.connect(db) as conn:
        row = conn.execute("SELECT MAX(id) FROM fallback_events").fetchone()[0]
        conn.execute(
            "UPDATE fallback_events SET synced = ? WHERE id = ?", (synced, row)
        )
    aged(db, "fallback_events", [row], age)
    return row


def remaining(db):
    with sqlite3.connect(db) as conn:
        return {row for (row,) in conn.execute("SELECT id FROM fallback_events")}


def test_only_confirmed_events_past_retention_are_deleted(tmp_path):
    db, _ = databases(tmp_path)
    old_pending = event(db, synced=0, age=30 * DAY)
    old_synced = event(db, synced=1, age=2 * DAY)
    recent_synced = event(db, synced=1, age=datetime.timedelta(hours=1))
    assert gateway.purge_synced_events(db, DAY) == 1
    assert remaining(db) == {old_pending, recent_synced}
    assert old_synced not in remaining(db)


def test_retention_ages_rows_in_utc_whatever_the_host_zone(tmp_path, monkeypatch):
    db, _ = databases(tmp_path)
    expired = event(db, synced=1, age=datetime.timedelta(hours=2))
    recent = event(db, synced=1, age=datetime.timedelta(minutes=30))
    monkeypatch.setenv("TZ", "America/Sao_Paulo")
    time.tzset()
    try:
        assert gateway.purge_synced_events(db, datetime.timedelta(hours=1)) == 1
    finally:
        monkeypatch.undo()
        time.tzset()
    assert remaining(db) == {recent}
    assert expired not in remaining(db)


def test_retention_deletes_in_bounded_batches(tmp_path, monkeypatch):
    db, _ = databases(tmp_path)
    monkeypatch.setattr(gateway, "RETENTION_BATCH", 3)
    rows = [event(db, synced=1, age=2 * DAY) for _ in range(10)]
    keep = event(db, synced=0, age=2 * DAY)
    assert gateway.purge_synced_events(db, DAY) == len(rows)
    assert remaining(db) == {keep}


def test_metrics_expire_by_age(tmp_path):
    _, metrics = databases(tmp_path)
    for spot in ("old", "new"):
        gateway.save_metric(metrics, "cam", spot, "free", "", "", "")
    with sqlite3.connect(metrics) as conn:
        old = conn.execute("SELECT id FROM metrics WHERE spot_id='old'").fetchone()[0]
    aged(metrics, "metrics", [old], 8 * DAY)
    assert gateway.purge_metrics(metrics, 7 * DAY) == 1
    with sqlite3.connect(metrics) as conn:
        assert conn.execute("SELECT spot_id FROM metrics").fetchall() == [("new",)]


def test_pending_deliveries_use_the_partial_index(tmp_path):
    db, _ = databases(tmp_path)
    with sqlite3.connect(db) as conn:
        plan = conn.execute(
            "EXPLAIN QUERY PLAN SELECT id, event_uuid, message_text FROM fallback_events "
            "WHERE synced = 0 AND id > ? ORDER BY id LIMIT ?",
            (0, 64),
        ).fetchall()
    assert any("fallback_pending" in row[-1] for row in plan), plan


def test_uncheckpointed_wal_counts_toward_the_budget(tmp_path):
    db, _ = databases(tmp_path)
    with closing(sqlite3.connect(db)) as writer:
        writer.execute("PRAGMA wal_autocheckpoint=0")
        with writer:
            writer.executemany(
                "INSERT INTO fallback_events (message_text) VALUES (?)",
                [("x" * 2000,)] * 50,
            )
        with_wal = gateway.used_bytes(db)
        writer.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        assert gateway.used_bytes(db) < with_wal


def test_purged_space_counts_as_room_again(tmp_path):
    db, metrics = databases(tmp_path)
    for _ in range(400):
        gateway.save_event(db, json.dumps({"padding": "x" * 2000}))
    with sqlite3.connect(db) as conn:
        conn.execute("UPDATE fallback_events SET synced = 1")
    aged(db, "fallback_events", remaining(db), 2 * DAY)
    full = gateway.used_bytes(db)
    retention = gateway.Retention(synced_hours=24, max_db_bytes=full // 2)
    # The file keeps its size; freed pages are what make room.
    assert gateway.has_room(db, metrics, retention)
    assert gateway.used_bytes(db) < full // 2
    assert remaining(db) == set()


def test_storage_full_refuses_the_batch_and_the_producer_keeps_it(tmp_path):
    async def run():
        db, metrics = databases(tmp_path)
        pending = event(db, synced=0, age=30 * DAY)
        outbox = delivery_module.LocalOutbox(tmp_path / "vision.db")
        event_id = outbox.enqueue(
            '{"type":"SPOT_UPDATE","spot_id":"A","status":"free"}'
        )
        sync = asyncio.Event()
        retention = gateway.Retention(max_db_bytes=1)

        async def handle(ws):
            await gateway.handler(ws, db, metrics, "local-test", sync, retention)

        headers = {"Authorization": "Bearer local-test"}
        async with websockets.serve(handle, "127.0.0.1", 0) as server:
            url = f"ws://127.0.0.1:{server.sockets[0].getsockname()[1]}"
            delivery = delivery_module.LocalDelivery(url, outbox)
            await delivery.send(None, None, headers)
        # Refused before any commit: nothing new stored, nothing acknowledged,
        # and the pending event is never purged to make room.
        assert remaining(db) == {pending}
        assert outbox.first() is not None
        assert not sync.is_set()

        retention = gateway.Retention()
        async with websockets.serve(handle, "127.0.0.1", 0) as server:
            url = f"ws://127.0.0.1:{server.sockets[0].getsockname()[1]}"
            delivery = delivery_module.LocalDelivery(url, outbox)
            ws = await delivery.send(None, None, headers)
            await ws.close()
        assert outbox.first() is None
        with sqlite3.connect(db) as conn:
            assert conn.execute(
                "SELECT COUNT(*) FROM fallback_events WHERE event_uuid = ?", (event_id,)
            ).fetchone() == (1,)

    asyncio.run(run())


@pytest.mark.parametrize(
    "name",
    [
        "GATEWAY_SYNCED_RETENTION_HOURS",
        "GATEWAY_METRICS_RETENTION_DAYS",
        "GATEWAY_MAX_DB_BYTES",
    ],
)
@pytest.mark.parametrize("value", ["0", "-1", "soon"])
def test_retention_settings_must_be_positive(name, value):
    environ = {"EDGE_API_KEY": "c" * 64, "GATEWAY_LOCAL_KEY": "l" * 32, name: value}
    with pytest.raises(ValueError, match=name):
        gateway.load_settings(environ)


def test_retention_settings_have_safe_defaults():
    settings = gateway.load_settings(
        {"EDGE_API_KEY": "c" * 64, "GATEWAY_LOCAL_KEY": "l" * 32}
    )
    assert settings.retention == gateway.Retention(24, 7, 2 * 1024**3)


def test_retention_loop_keeps_purging_after_a_failed_pass(monkeypatch, capsys):
    retention = gateway.Retention(synced_hours=1)
    passes, sleeps = [], []

    def apply(db, metrics, policy):
        passes.append((db, metrics, policy))
        if len(passes) == 1:
            raise sqlite3.OperationalError("database is locked")

    async def sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) == 2:
            raise asyncio.CancelledError

    monkeypatch.setattr(gateway, "apply_retention", apply)
    monkeypatch.setattr(gateway.asyncio, "sleep", sleep)
    with pytest.raises(asyncio.CancelledError):
        asyncio.run(gateway.retention_loop("events.db", "metrics.db", retention))

    assert passes == [("events.db", "metrics.db", retention)] * 2
    assert sleeps == [gateway.RETENTION_INTERVAL_SECONDS] * 2
    assert "RETENTION_FAILED reason=database is locked" in capsys.readouterr().out
