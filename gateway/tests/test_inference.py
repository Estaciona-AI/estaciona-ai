# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from unittest.mock import patch

import pytest
import torch
from sqlalchemy import create_engine, text

from ml.inference import PredictiveEngine


class FixedForecast(torch.nn.Module):
    def forward(self, values):
        return torch.full((values.shape[0], 24), 2.0)


@pytest.mark.parametrize("spot_count", [0, 3, 35])
def test_forecast_capacity_comes_from_registered_spots(spot_count):
    # A real SQL table exercises capacity lookup without depending on a checkpoint.
    engine = create_engine("sqlite://")
    with engine.begin() as connection:
        connection.execute(
            text(
                "CREATE TABLE spots (id TEXT PRIMARY KEY, is_active BOOLEAN NOT NULL DEFAULT TRUE)"
            )
        )
        for index in range(spot_count):
            connection.execute(
                text("INSERT INTO spots (id) VALUES (:id)"), {"id": str(index)}
            )

        connection.execute(
            text("INSERT INTO spots (id,is_active) VALUES ('retired',FALSE)")
        )

    predictor = PredictiveEngine.__new__(PredictiveEngine)
    predictor.engine = engine
    predictor.max_val = 44.0  # Training normalization is independent of site capacity.
    predictor.transformer = FixedForecast()
    with patch.object(predictor, "get_history", return_value=[0] * 192):
        payload = predictor.predict_trends()

    assert payload["max_capacity"] == spot_count
    assert len(payload["next_24h_occupancy"]) == 24
    assert all(
        0 <= item["occupancy"] <= spot_count for item in payload["next_24h_occupancy"]
    )
    engine.dispose()


def forecast_predictor(history=None):
    predictor = PredictiveEngine.__new__(PredictiveEngine)
    predictor.max_val = 44.0
    predictor.transformer = FixedForecast()
    predictor.get_capacity = lambda: 400
    predictor.get_history = lambda _: [0] * 192 if history is None else history
    return predictor


def test_forecast_marks_unmeasured_stay_duration_instead_of_fabricating_zero():
    payload = forecast_predictor().predict_trends()
    assert payload["type"] == "TREND_PREDICTION_V2"
    assert payload["stay_duration"] == {
        "status": "unavailable",
        "reason": "not_measured",
    }
    assert "avg_stay_duration_mins" not in payload
    assert "stay_duration_distribution" not in payload
    assert "model_health" not in payload


def test_constant_targets_have_real_errors_but_no_fabricated_r2():
    payload = forecast_predictor().predict_trends()
    evaluation = payload["evaluation"]
    assert evaluation["status"] == "partial"
    assert evaluation["reason"] == "constant_target"
    assert "r2_score" not in evaluation
    assert evaluation["mae"] > 0
    assert evaluation["rmse"] > 0


def test_negative_r2_is_a_valid_result_not_clamped_to_zero():
    payload = forecast_predictor(list(range(192))).predict_trends()
    assert payload["evaluation"]["status"] == "available"
    assert payload["evaluation"]["r2_score"] < 0


def test_evaluation_failure_is_not_reported_as_perfect_zero_error():
    with patch(
        "ml.inference.mean_absolute_error", side_effect=ValueError("fixture failure")
    ):
        payload = forecast_predictor().predict_trends()
    evaluation = payload["evaluation"]
    assert evaluation["status"] == "unavailable"
    assert evaluation["reason"] == "evaluation_failed"
    assert not {"mae", "rmse", "r2_score"} & evaluation.keys()
    assert len(payload["next_24h_occupancy"]) == 24


def test_perfect_constant_predictions_keep_actual_zero_errors():
    history = [88] * 192  # FixedForecast emits 2 * max_val = 88.
    evaluation = forecast_predictor(history).predict_trends()["evaluation"]
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
    import json
    from ml.inference import evaluate_forecast

    evaluation = evaluate_forecast(actual, predicted, 1.0)
    assert evaluation == {
        "status": "unavailable",
        "reason": reason,
        "inference_time_ms": 1.0,
    }
    json.dumps(evaluation, allow_nan=False)


def test_non_finite_future_prediction_is_not_queued_as_a_forecast():
    predictor = forecast_predictor()

    class InvalidForecast(torch.nn.Module):
        def forward(self, values):
            return torch.full((values.shape[0], 24), float("nan"))

    predictor.transformer = InvalidForecast()
    with pytest.raises(ValueError, match="invalid values"):
        predictor.predict_trends()
