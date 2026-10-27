# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
import uuid
from datetime import timedelta
from pathlib import Path

import numpy as np
import pandas as pd
import pytest

from ml import backtest, history, site_calendar, train
from ml.inference import PredictiveEngine

from forecast_db import history_engine


def hourly_index(days, start="2026-01-05"):
    return pd.date_range(start, periods=days * 24, freq="h", tz="UTC")


def test_registering_a_spot_keeps_the_earlier_rate_history():
    hours = pd.date_range("2026-10-01", periods=3, freq="h", tz="UTC")
    rows = [(spot, hour, "occupied") for hour in hours for spot in ("a", "b")]
    rows += [("c", hours[-1], "free")]  # Spot c joins the map in the last hour.
    revisions = [(hours[0], ["a", "b"]), (hours[-1], ["a", "b", "c"])]
    engine = history_engine(["a", "b", "c"], rows=rows, revisions=revisions)
    frame = history.physical_history(engine, end=hours[-1] + timedelta(hours=1))
    assert frame["rate"].tolist() == pytest.approx([1.0, 1.0, 2 / 3])
    # Each hour is complete for the map in force, so its count is exact.
    assert frame["occupancy"].tolist() == [2, 2, 2]
    assert frame["expected_spots"].tolist() == [2, 2, 3]
    engine.dispose()


def test_a_stable_tenth_of_the_lot_is_not_the_lot_rate():
    """Review R04: 40 of 400 mapped spots reporting is not 100% occupancy."""
    spots = [f"s{index}" for index in range(400)]
    hours = pd.date_range("2026-10-01", periods=24, freq="h", tz="UTC")
    rows = [(spot, hour, "occupied") for hour in hours for spot in spots[:40]]
    engine = history_engine(spots, rows=rows, revisions=[(hours[0], spots)])
    frame = history.physical_history(engine, end=hours[-1] + timedelta(hours=1))
    assert frame["observed_spots"].tolist() == [40] * 24
    assert frame["rate"].isna().all()
    assert frame["occupancy"].isna().all()
    engine.dispose()


def lost_camera_engine():
    """Ten mapped spots; from the second day only five keep reporting."""
    spots = [f"s{index}" for index in range(10)]
    hours = pd.date_range("2026-10-01", periods=48, freq="h", tz="UTC")
    rows = [(spot, hour, "free") for hour in hours[:24] for spot in spots]
    rows += [(spot, hour, "occupied") for hour in hours[24:] for spot in spots[:5]]
    return history_engine(spots, rows=rows, revisions=[(hours[0], spots)]), hours


def test_a_camera_lost_before_the_window_still_lowers_coverage():
    engine, hours = lost_camera_engine()
    end = hours[-1] + timedelta(hours=1)
    # A window that starts after the loss must not take five spots as the lot.
    late = history.physical_history(engine, start=hours[24], end=end)
    assert len(late) == 24
    assert late["rate"].isna().all()
    engine.dispose()


def test_training_and_inference_windows_agree_on_every_shared_hour():
    engine, hours = lost_camera_engine()
    end = hours[-1] + timedelta(hours=1)
    whole = history.physical_history(engine, end=end).set_index("timestamp")
    for start in (hours[12], hours[24], hours[36]):
        part = history.physical_history(engine, start=start, end=end)
        shared = whole.loc[part["timestamp"], "rate"].to_numpy()
        np.testing.assert_array_equal(part["rate"].to_numpy(), shared)
    engine.dispose()


def test_only_spots_of_the_map_in_force_count():
    hours = pd.date_range("2026-10-01", periods=1, freq="h", tz="UTC")
    rows = [(spot, hours[0], "occupied") for spot in ("a", "b", "unmapped")]
    engine = history_engine(["a", "b"], rows=rows, revisions=[(hours[0], ["a", "b"])])
    frame = history.physical_history(engine, end=hours[0] + timedelta(hours=1))
    assert frame[["observed_spots", "expected_spots"]].values.tolist() == [[2, 2]]
    assert frame["rate"].tolist() == [1.0]
    engine.dispose()


@pytest.mark.parametrize("observed, has_rate", [(8, False), (9, True), (10, True)])
def test_hours_need_most_monitored_spots_to_count(observed, has_rate):
    spots = [str(index) for index in range(10)]
    hours = pd.date_range("2026-10-01", periods=2, freq="h", tz="UTC")
    rows = [(spot, hours[0], "free") for spot in spots]
    rows += [(spot, hours[1], "occupied") for spot in spots[:observed]]
    engine = history_engine(spots, rows=rows)
    frame = history.physical_history(engine, end=hours[-1] + timedelta(hours=1))
    assert np.isfinite(frame["rate"].iloc[1]) == has_rate
    engine.dispose()


def test_features_never_read_rates_at_or_after_the_origin():
    rng = np.random.default_rng(0)
    index = hourly_index(35)
    calendar = site_calendar.calendar_frame(site_calendar.default_profile(10), index)
    rate = rng.random(len(index))
    origin = train.LOOKBACK + 50
    before = train.build_features(rate, calendar, [origin])
    rate[origin:] = rng.random(len(index) - origin)
    after = train.build_features(rate, calendar, [origin])
    pd.testing.assert_frame_equal(before, after)
    assert list(before[train.FEATURES].columns) == train.FEATURES


def test_recent_history_rule():
    rate = np.full(train.LOOKBACK, np.nan)
    origin = train.LOOKBACK
    assert not train.has_recent_history(rate, origin)
    rate[origin - 84 :] = 0.5
    assert train.has_recent_history(rate, origin)
    rate[origin - 24 :] = np.nan
    assert not train.has_recent_history(rate, origin)


@pytest.mark.parametrize(
    "model, expected",
    [(1.0, "model"), (2.5, "seasonal_naive"), (3.5, "seasonal_naive")],
)
def test_serving_requires_beating_every_baseline(model, expected):
    scores = {
        "model": {"nmae_pct": model},
        "seasonal_naive": {"nmae_pct": 2.0},
        "weekly_profile": {"nmae_pct": 3.0},
    }
    assert backtest.choose_serving(scores) == expected


def test_comparison_scores_only_points_every_method_covers():
    actual = np.array([[0.5, 0.5, np.nan]])
    scores = backtest.compare(
        actual,
        {"model": np.array([[0.5, 0.4, 0.1]]), "other": np.array([[np.nan, 0.6, 0.1]])},
    )
    assert scores["points"] == 1
    assert scores["model"]["nmae_pct"] == pytest.approx(10.0)


def campus_engine(days, capacity=6):
    """A small lot with a weekday class pattern ending at the current hour."""
    end = pd.Timestamp.now(tz="UTC").floor("h")
    hours = pd.date_range(end - timedelta(days=days), end, freq="h", inclusive="left")
    spots = [f"s{index}" for index in range(capacity)]
    local = hours.tz_convert("America/Sao_Paulo")
    busy = (local.weekday < 5) & (local.hour >= 8) & (local.hour < 18)
    rows = [
        (spot, hour, "occupied" if is_busy and index < 4 else "free")
        for hour, is_busy in zip(hours, busy)
        for index, spot in enumerate(spots)
    ]
    return history_engine(spots, rows=rows)


@pytest.fixture
def small_training(monkeypatch):
    monkeypatch.setattr(train, "SIM_DAYS", 63)
    monkeypatch.setattr(train, "LGB_ROUNDS", 20)
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)


def test_training_publishes_a_verifiable_artifact_and_serves_v2(
    tmp_path, small_training
):
    jsonschema = pytest.importorskip("jsonschema")
    engine = campus_engine(days=42)
    manifest = train.train_occupancy_model(engine, str(tmp_path), variants=2)
    assert manifest["source"] == "blended"
    assert manifest["real_hours"] == 42 * 24
    assert manifest["evaluation"]["status"] == "evaluated"
    assert manifest["serving"] in ("model",) + backtest.BASELINES
    assert manifest["profile"]["specified"] is False

    payload = PredictiveEngine(str(tmp_path), engine).predict_trends()
    forecast = payload["next_24h_occupancy"]
    origin = pd.Timestamp.now(tz="UTC").floor("h")
    assert forecast[0]["timestamp"] == origin.isoformat()
    assert all(0 <= item["occupancy"] <= 6 for item in forecast)
    assert payload["evaluation"]["status"] in ("available", "partial")

    schema_path = Path(__file__).parents[2] / "shared/v1/gateway-to-server.schema.json"
    schema = json.loads(schema_path.read_text())
    envelope = {
        "type": "EDGE_EVENT",
        "version": 1,
        "event_id": str(uuid.uuid4()),
        "payload": json.loads(json.dumps(payload, allow_nan=False)),
    }
    jsonschema.Draft202012Validator(schema).validate(envelope)
    engine.dispose()


def test_prior_without_real_data_is_labelled_and_never_scored(tmp_path, small_training):
    engine = history_engine([f"s{index}" for index in range(6)])
    manifest = train.train_occupancy_model(engine, str(tmp_path), variants=2)
    assert manifest["source"] == "synthetic_prior"
    assert manifest["real_hours"] == 0
    assert manifest["evaluation"] == {
        "status": "not_evaluated",
        "reason": "insufficient_real_history",
    }
    # Without recent observations the forecast refuses instead of inventing them.
    with pytest.raises(ValueError, match="history"):
        PredictiveEngine(str(tmp_path), engine).predict_trends()
    engine.dispose()


def test_tampered_or_incompatible_artifacts_are_refused(tmp_path, small_training):
    engine = history_engine([f"s{index}" for index in range(6)])
    train.train_occupancy_model(engine, str(tmp_path), variants=1)
    manifest_path = tmp_path / train.MANIFEST_FILE
    original = json.loads(manifest_path.read_text())

    manifest_path.write_text(json.dumps({**original, "features": ["hour"]}))
    with pytest.raises(ValueError, match="features"):
        PredictiveEngine(str(tmp_path), engine)

    manifest_path.write_text(json.dumps(original))
    with open(tmp_path / train.MODEL_FILE, "a") as handle:
        handle.write("\n")
    with pytest.raises(ValueError, match="manifest"):
        PredictiveEngine(str(tmp_path), engine)
    engine.dispose()
