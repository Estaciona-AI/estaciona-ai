# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
from datetime import datetime, timedelta, timezone
from unittest.mock import patch

import numpy as np
import pandas as pd
import pytest
from sqlalchemy import create_engine, text

from ml import history, inference, train
from ml.backtest import HORIZON
from ml.inference import PredictiveEngine, evaluate_forecast

WINDOW = train.LOOKBACK + HORIZON  # hours before the forecast origin
BACKTEST = slice(train.LOOKBACK, train.LOOKBACK + HORIZON)


class FixedRate:
    def __init__(self, rate):
        self.rate = rate

    def predict(self, features):
        return np.full(len(features), self.rate)


class LastRate:
    """Persistence: every horizon repeats the last observed rate."""

    def predict(self, features):
        return features["last_rate"].to_numpy()


@pytest.fixture(autouse=True)
def generic_profile(monkeypatch):
    monkeypatch.delenv("FORECAST_PROFILE", raising=False)


def forecast_predictor(rates=None, capacity=400, model=None):
    """A predictor over WINDOW hourly rates ending at the current hour."""
    rates = np.full(WINDOW, 0.25) if rates is None else np.asarray(rates, float)
    origin = pd.Timestamp.now(tz="UTC").floor("h")
    frame = pd.DataFrame(
        {
            "timestamp": pd.date_range(
                origin - pd.Timedelta(hours=len(rates)), periods=len(rates), freq="h"
            ),
            "rate": rates,
        }
    )
    predictor = PredictiveEngine.__new__(PredictiveEngine)
    predictor.engine = None
    predictor.manifest = {"serving": "model"}
    predictor.model = model or FixedRate(0.5)
    predictor.get_capacity = lambda: capacity
    predictor.history_frame = frame
    return predictor


def predict(predictor):
    with patch.object(
        inference, "physical_history", return_value=predictor.history_frame
    ):
        return predictor.predict_trends()


def spots_engine(active, retired=()):
    engine = create_engine("sqlite://")
    with engine.begin() as connection:
        connection.execute(
            text("CREATE TABLE spots (id TEXT PRIMARY KEY,is_active BOOLEAN)")
        )
        connection.execute(
            text(
                "CREATE TABLE physical_occupancy_hourly (spot_id TEXT,hour_start TEXT,status TEXT)"
            )
        )
        for spot in active:
            connection.execute(
                text("INSERT INTO spots VALUES (:id,TRUE)"), {"id": spot}
            )
        for spot in retired:
            connection.execute(
                text("INSERT INTO spots VALUES (:id,FALSE)"), {"id": spot}
            )
    return engine


@pytest.mark.parametrize("spot_count", [3, 35])
def test_forecast_capacity_comes_from_registered_spots(spot_count):
    engine = spots_engine([str(index) for index in range(spot_count)], ["retired"])
    predictor = forecast_predictor()
    predictor.engine = engine
    del predictor.get_capacity
    payload = predict(predictor)
    assert payload["max_capacity"] == spot_count
    assert len(payload["next_24h_occupancy"]) == 24
    assert all(
        0 <= item["occupancy"] <= spot_count for item in payload["next_24h_occupancy"]
    )
    engine.dispose()


def test_forecast_refuses_a_lot_without_active_spots():
    with pytest.raises(ValueError, match="active spots"):
        predict(forecast_predictor(capacity=0))


def test_forecast_marks_unmeasured_stay_duration_instead_of_fabricating_zero():
    payload = predict(forecast_predictor())
    assert payload["type"] == "TREND_PREDICTION_V2"
    assert payload["stay_duration"] == {
        "status": "unavailable",
        "reason": "not_measured",
    }
    assert "avg_stay_duration_mins" not in payload
    assert "stay_duration_distribution" not in payload
    assert "model_health" not in payload


def test_constant_targets_have_real_errors_but_no_fabricated_r2():
    evaluation = predict(forecast_predictor())["evaluation"]
    assert evaluation["status"] == "partial"
    assert evaluation["reason"] == "constant_target"
    assert "r2_score" not in evaluation
    assert evaluation["mae"] > 0
    assert evaluation["rmse"] > 0


def test_negative_r2_is_a_valid_result_not_clamped_to_zero():
    rates = np.full(WINDOW, 0.25)
    rates[BACKTEST] = np.linspace(0.0, 0.2, HORIZON)
    evaluation = predict(forecast_predictor(rates))["evaluation"]
    assert evaluation["status"] == "available"
    assert evaluation["r2_score"] < 0


def test_r2_matches_its_definition():
    actual = np.arange(24.0)
    predicted = actual + np.tile([1.0, -1.0], 12)
    evaluation = evaluate_forecast(actual, predicted, 1.0)
    expected = 1 - 24 / np.sum((actual - actual.mean()) ** 2)
    assert evaluation["r2_score"] == pytest.approx(expected)
    assert evaluation["mae"] == pytest.approx(1.0)
    assert evaluation["rmse"] == pytest.approx(1.0)


def test_evaluation_failure_is_not_reported_as_perfect_zero_error():
    evaluation = evaluate_forecast(["not a number"] * 24, [0] * 24, 1.0)
    assert evaluation == {
        "status": "unavailable",
        "reason": "evaluation_failed",
        "inference_time_ms": 1.0,
    }


def test_perfect_constant_predictions_keep_actual_zero_errors():
    rates = np.full(WINDOW, 0.5)  # FixedRate predicts 0.5 as well.
    evaluation = predict(forecast_predictor(rates))["evaluation"]
    assert evaluation["status"] == "partial"
    assert evaluation["mae"] == 0.0
    assert evaluation["rmse"] == 0.0


@pytest.mark.parametrize(
    "actual,predicted,reason",
    [
        ([0] * 23, [0] * 24, "invalid_evaluation_window"),
        ([0] * 24, [float("nan")] * 24, "non_finite_values"),
        ([float("inf")] * 24, [0] * 24, "non_finite_values"),
    ],
)
def test_invalid_backtest_has_no_numeric_quality_claim(actual, predicted, reason):
    evaluation = evaluate_forecast(actual, predicted, 1.0)
    assert evaluation == {
        "status": "unavailable",
        "reason": reason,
        "inference_time_ms": 1.0,
    }
    json.dumps(evaluation, allow_nan=False)


def test_non_finite_future_prediction_is_not_queued_as_a_forecast():
    predictor = forecast_predictor(model=FixedRate(float("nan")))
    with pytest.raises(ValueError, match="invalid values"):
        predict(predictor)


def test_history_reads_physical_samples_without_personal_history():
    engine = spots_engine(["a", "b"], ["retired"])
    hour = pd.Timestamp.now(tz="UTC").floor("h") - pd.Timedelta(hours=1)
    with engine.begin() as connection:
        for spot, status in [("a", "occupied"), ("b", "free"), ("retired", "occupied")]:
            connection.execute(
                text(
                    "INSERT INTO physical_occupancy_hourly VALUES (:spot,:hour,:status)"
                ),
                {"spot": spot, "hour": hour.isoformat(), "status": status},
            )
    frame = history.physical_history(engine)
    assert frame["rate"].tolist() == [0.5]
    engine.dispose()


@pytest.mark.parametrize(
    "rates", [[], [0.25] * 24, [float("nan")] * WINDOW], ids=["empty", "short", "nan"]
)
def test_forecast_refuses_missing_recent_history(rates):
    with pytest.raises(ValueError, match="history"):
        predict(forecast_predictor(rates))


def test_empty_physical_history_is_unknown_instead_of_zero():
    engine = spots_engine(["a"])
    predictor = forecast_predictor()
    predictor.engine = engine
    with pytest.raises(ValueError, match="history"):
        predictor.predict_trends()
    engine.dispose()


def test_forecast_can_predict_without_claiming_an_unavailable_backtest():
    rates = np.full(WINDOW, 0.25)
    rates[train.LOOKBACK] = np.nan  # One unobserved hour in the backtest window.
    payload = predict(forecast_predictor(rates))
    assert len(payload["next_24h_occupancy"]) == 24
    assert payload["evaluation"]["status"] == "unavailable"
    assert payload["evaluation"]["reason"] == "invalid_evaluation_window"


def frozen_clock(*instants):
    readings = iter(instants)

    class Clock(datetime):
        @classmethod
        def now(cls, tz=None):
            cls.current = next(readings, cls.current)
            return cls.current.astimezone(tz)

    Clock.current = instants[0]
    return Clock


def hourly_history_engine(last_hour):
    # Spot a is always occupied; spot b only in the most recent stored hour.
    engine = spots_engine(["a", "b"])
    with engine.begin() as connection:
        for offset in range(200):
            hour = (pd.Timestamp(last_hour) - pd.Timedelta(hours=offset)).isoformat()
            connection.execute(
                text(
                    "INSERT INTO physical_occupancy_hourly VALUES ('a',:hour,'occupied'),('b',:hour,:b)"
                ),
                {"hour": hour, "b": "occupied" if offset == 0 else "free"},
            )
    predictor = PredictiveEngine.__new__(PredictiveEngine)
    predictor.engine = engine
    predictor.manifest = {"serving": "model"}
    predictor.model = LastRate()
    return predictor


def test_first_forecast_hour_follows_the_last_observed_hour():
    predictor = hourly_history_engine("2026-10-24T12:00:00+00:00")
    now = datetime(2026, 10, 24, 13, 42, 10, tzinfo=timezone.utc)
    with patch("ml.inference.datetime", frozen_clock(now)):
        forecast = predictor.predict_trends()["next_24h_occupancy"]

    # Hour 12 is the latest complete input, so the model's first step is hour 13.
    assert forecast[0] == {"timestamp": "2026-10-24T13:00:00+00:00", "occupancy": 2}
    origin = datetime(2026, 10, 24, 13, tzinfo=timezone.utc)
    assert [item["timestamp"] for item in forecast] == [
        (origin + timedelta(hours=step)).isoformat() for step in range(24)
    ]
    predictor.engine.dispose()


def test_history_and_labels_share_one_origin_across_an_hour_boundary():
    predictor = hourly_history_engine("2026-10-24T12:00:00+00:00")
    before = datetime(2026, 10, 24, 12, 59, 59, 999000, tzinfo=timezone.utc)
    after = datetime(2026, 10, 24, 13, 0, 0, 1000, tzinfo=timezone.utc)
    with (
        patch("ml.inference.datetime", frozen_clock(before, after)),
        patch.object(
            inference, "physical_history", wraps=inference.physical_history
        ) as physical_history,
    ):
        forecast = predictor.predict_trends()["next_24h_occupancy"]

    history_end = physical_history.call_args.args[2]
    assert forecast[0]["timestamp"] == history_end.isoformat()
    predictor.engine.dispose()


def test_forecast_rounds_to_the_nearest_spot_instead_of_truncating():
    predictor = forecast_predictor(capacity=4, model=FixedRate(0.4))  # 1.6 spots
    forecast = predict(predictor)["next_24h_occupancy"]
    assert {item["occupancy"] for item in forecast} == {2}


def test_forecast_never_falls_back_to_the_server_owner_credential(monkeypatch):
    monkeypatch.delenv("ML_DATABASE_URL", raising=False)
    monkeypatch.setenv("DATABASE_URL", "postgresql://owner@db/estaciona")
    with pytest.raises(ValueError, match="ML_DATABASE_URL"):
        train.get_db_engine()
