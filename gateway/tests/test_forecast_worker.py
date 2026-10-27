# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import asyncio
import json
import sqlite3
from contextlib import closing
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

import pytest
from sqlalchemy import text

import gateway
from ml import train, worker
from forecast_db import history_engine
from test_forecast_model import campus_engine, small_training

__all__ = ["small_training"]


def utc(*args):
    return datetime(*args, tzinfo=timezone.utc)


@pytest.mark.parametrize(
    "now,expected",
    [
        (utc(2026, 10, 24, 13, 42, 10), utc(2026, 10, 24, 14, 5)),
        (utc(2026, 10, 24, 14, 4, 59), utc(2026, 10, 24, 14, 5)),
        (utc(2026, 10, 24, 14, 5), utc(2026, 10, 24, 15, 5)),
        (utc(2026, 10, 24, 23, 30), utc(2026, 10, 25, 0, 5)),
    ],
)
def test_inference_runs_once_each_hour_after_it_closes(now, expected):
    assert worker.next_inference(now) == expected


@pytest.mark.parametrize(
    "now,expected",
    [
        # 03:30 in São Paulo is 06:30 UTC.
        (utc(2026, 10, 24, 5, 0), utc(2026, 10, 24, 6, 30)),
        (utc(2026, 10, 24, 6, 30), utc(2026, 10, 25, 6, 30)),
        (utc(2026, 10, 24, 7, 0), utc(2026, 10, 25, 6, 30)),
    ],
)
def test_retrain_runs_daily_at_the_site_local_time(now, expected):
    assert worker.next_retrain(now, "America/Sao_Paulo") == expected


@pytest.fixture
def trainings(monkeypatch, small_training):
    calls = []
    real = worker.train_occupancy_model

    def train_quickly(engine, models_dir):
        calls.append(models_dir)
        return real(engine, models_dir, variants=1)

    monkeypatch.setattr(worker, "train_occupancy_model", train_quickly)
    return calls


def test_model_is_trained_only_when_missing_or_invalid(tmp_path, trainings):
    engine = history_engine([f"s{index}" for index in range(6)])
    first = worker.ensure_model(engine, str(tmp_path))
    assert len(trainings) == 1
    assert first.manifest["source"] == "synthetic_prior"

    worker.ensure_model(engine, str(tmp_path))
    assert len(trainings) == 1

    with open(tmp_path / train.MODEL_FILE, "a") as handle:
        handle.write("\n")
    worker.ensure_model(engine, str(tmp_path))
    assert len(trainings) == 2

    # A spot added to the map changes capacity: never serve the old artifact.
    with engine.begin() as connection:
        connection.execute(text("INSERT INTO spots VALUES ('s6', TRUE)"))
    retrained = worker.ensure_model(engine, str(tmp_path))
    assert len(trainings) == 3
    assert retrained.manifest["context"]["capacity"] == 7
    engine.dispose()


def test_a_deployment_change_while_running_retrains_before_publishing(
    tmp_path, trainings, monkeypatch
):
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)
    engine = campus_engine(days=42)
    first = worker.ensure_model(engine, str(tmp_path))
    profile = tmp_path / "profile.json"
    profile.write_text(json.dumps({"timezone": "America/Manaus"}))
    monkeypatch.setenv("FORECAST_PROFILE", str(profile))
    # The loop finds the stored artifact stale, retrains, then publishes.
    published, _, _ = run_loop(
        engine, str(tmp_path), str(tmp_path / "outbox.db"), utc(2026, 10, 24, 12, 0), 1
    )
    assert len(trainings) == 2
    assert len(published) == 1
    manifest = json.loads((tmp_path / train.MANIFEST_FILE).read_text())
    assert (
        manifest["context"]["profile_sha256"]
        != (first.manifest["context"]["profile_sha256"])
    )
    engine.dispose()


class Stop(Exception):
    pass


def run_loop(engine, models_dir, outbox, start, cycles):
    """Run the loop on a simulated clock that each sleep advances."""
    gateway.init_db(outbox)
    clock = [start]
    sleeps = []
    sync_event = asyncio.Event()

    async def sleep(seconds):
        sleeps.append(seconds)
        if len(sleeps) == cycles:
            raise Stop
        clock[0] += timedelta(seconds=seconds)

    async def inline(function, *args):
        return function(*args)

    # In-memory SQLite is private to each thread, so the thread hops run inline.
    with patch.object(asyncio, "to_thread", inline), pytest.raises(Stop):
        asyncio.run(
            worker.forecast_loop(
                engine,
                models_dir,
                outbox,
                sync_event,
                clock=lambda: clock[0],
                sleep=sleep,
            )
        )
    with closing(sqlite3.connect(outbox)) as connection:
        rows = connection.execute(
            "SELECT message_text FROM fallback_events WHERE synced = 0 ORDER BY id"
        ).fetchall()
    return [json.loads(text) for (text,) in rows], sleeps, sync_event


def test_loop_publishes_hourly_and_retrains_daily(tmp_path, trainings, monkeypatch):
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)
    engine = campus_engine(days=42)
    # 02:50 in São Paulo: publish now and at 03:05, retrain at 03:30 sharp
    # (not at the next publication), then publish at 04:05.
    published, sleeps, sync_event = run_loop(
        engine, str(tmp_path), str(tmp_path / "outbox.db"), utc(2026, 10, 24, 5, 50), 4
    )
    assert sleeps == [15 * 60, 25 * 60, 35 * 60, 3600]
    # Waking up to retrain publishes nothing off the hourly schedule.
    assert [payload["type"] for payload in published] == ["TREND_PREDICTION_V2"] * 3
    assert len(trainings) == 2  # Missing artifact at start, then the daily retrain.
    assert sync_event.is_set()
    engine.dispose()


def test_failed_retrain_keeps_serving_the_previous_model(
    tmp_path, trainings, monkeypatch
):
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)
    engine = campus_engine(days=42)
    worker.ensure_model(engine, str(tmp_path))

    failures = []

    def fail(engine, models_dir):
        failures.append(models_dir)
        raise RuntimeError("retrain fixture failure")

    monkeypatch.setattr(worker, "train_occupancy_model", fail)
    # Publish at 03:20 in São Paulo, fail the 03:30 retrain, publish at 04:05
    # with the previous model.
    published, sleeps, _ = run_loop(
        engine, str(tmp_path), str(tmp_path / "outbox.db"), utc(2026, 10, 24, 6, 20), 3
    )
    assert sleeps[:2] == [10 * 60, 35 * 60]
    assert len(failures) == 1
    assert len(published) == 2
    engine.dispose()


def test_prediction_failure_publishes_nothing_and_keeps_running(tmp_path, trainings):
    # A synthetic prior exists, but no camera has reported: nothing to forecast from.
    engine = history_engine([f"s{index}" for index in range(6)])
    published, sleeps, sync_event = run_loop(
        engine, str(tmp_path), str(tmp_path / "outbox.db"), utc(2026, 10, 24, 12, 0), 2
    )
    assert published == []
    assert len(sleeps) == 2
    assert not sync_event.is_set()
    engine.dispose()
