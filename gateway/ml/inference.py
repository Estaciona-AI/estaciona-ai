# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import os
import logging
import lightgbm as lgb
import pandas as pd
import numpy as np
from datetime import datetime, timedelta, timezone
import time
from sqlalchemy import text
from dotenv import load_dotenv

if __package__:
    from .backtest import BASELINES, HORIZON
    from .history import physical_history
    from .site_calendar import profile_from_env
    from .train import (
        FEATURES,
        LOOKBACK,
        MANIFEST_FILE,
        MANIFEST_VERSION,
        MODEL_FILE,
        baseline_rates,
        calendar_for,
        get_db_engine,
        has_recent_history,
        predict_rates,
    )
else:
    from backtest import BASELINES, HORIZON
    from history import physical_history
    from site_calendar import profile_from_env
    from train import (
        FEATURES,
        LOOKBACK,
        MANIFEST_FILE,
        MANIFEST_VERSION,
        MODEL_FILE,
        baseline_rates,
        calendar_for,
        get_db_engine,
        has_recent_history,
        predict_rates,
    )

from sklearn.metrics import mean_absolute_error, mean_squared_error, r2_score

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))


def evaluate_forecast(actual, predicted, inference_time_ms):
    timing = {"inference_time_ms": float(inference_time_ms)}
    unavailable = {"status": "unavailable", **timing}
    try:
        actual = np.asarray(actual, dtype=np.float64)
        predicted = np.asarray(predicted, dtype=np.float64)
        if actual.shape != (24,) or predicted.shape != (24,):
            return {**unavailable, "reason": "invalid_evaluation_window"}
        if not np.isfinite(actual).all() or not np.isfinite(predicted).all():
            return {**unavailable, "reason": "non_finite_values"}
        mae = float(mean_absolute_error(actual, predicted))
        rmse = float(np.sqrt(mean_squared_error(actual, predicted)))
        if not np.isfinite([mae, rmse]).all():
            return {**unavailable, "reason": "non_finite_values"}
        metrics = {"mae": mae, "rmse": rmse, **timing}
        if np.ptp(actual) == 0:
            return {"status": "partial", "reason": "constant_target", **metrics}
        r2 = float(r2_score(actual, predicted, force_finite=False))
        if not np.isfinite(r2):
            return {**unavailable, "reason": "non_finite_values"}
        return {"status": "available", "r2_score": r2, **metrics}
    except Exception as error:
        logging.getLogger(__name__).warning(
            "Forecast evaluation unavailable: %s", type(error).__name__
        )
        return {**unavailable, "reason": "evaluation_failed"}


def load_occupancy_model(models_dir):
    """Load the LightGBM artifact only if it matches its manifest."""
    with open(os.path.join(models_dir, MANIFEST_FILE), encoding="utf-8") as handle:
        manifest = json.load(handle)
    if manifest.get("manifest_version") != MANIFEST_VERSION:
        raise ValueError("Unsupported forecast manifest version")
    if manifest.get("features") != FEATURES:
        raise ValueError("Forecast artifact features do not match this code")
    if manifest.get("serving") not in ("model",) + BASELINES:
        raise ValueError("Forecast manifest names an unknown serving method")
    with open(os.path.join(models_dir, MODEL_FILE), "rb") as handle:
        model_text = handle.read()
    if hashlib.sha256(model_text).hexdigest() != manifest.get("model_sha256"):
        raise ValueError("Forecast model does not match its manifest")
    return manifest, lgb.Booster(model_str=model_text.decode())


class PredictiveEngine:
    def __init__(self, models_dir=None, engine=None):
        models_dir = models_dir or os.path.join(os.path.dirname(__file__), "models")
        self.manifest, self.model = load_occupancy_model(models_dir)
        self.engine = engine or get_db_engine()

    def get_capacity(self):
        with self.engine.connect() as connection:
            return int(
                connection.execute(
                    text("SELECT COUNT(*) FROM spots WHERE is_active")
                ).scalar_one()
            )

    def _rates(self, rate, calendar, origin):
        predicted = predict_rates(self.model, rate, calendar, [origin])[0]
        serving = self.manifest["serving"]
        if serving != "model":
            # The model fills hours the baseline cannot cover after gaps.
            baseline = baseline_rates(serving, rate, [origin])[0]
            predicted = np.where(np.isfinite(baseline), baseline, predicted)
        return predicted

    def predict_trends(self):
        now = datetime.now(timezone.utc)
        origin = now.replace(minute=0, second=0, microsecond=0)
        capacity = self.get_capacity()
        if capacity <= 0:
            raise ValueError("Forecast requires registered active spots")

        # Lags for a backtest origin 24 h ago, then the forecast origin itself.
        start = origin - timedelta(hours=LOOKBACK + HORIZON)
        index = pd.date_range(start, periods=LOOKBACK + 2 * HORIZON, freq="h", tz="UTC")
        frame = physical_history(self.engine, start, origin)
        rate = (
            frame.set_index("timestamp")["rate"]
            .reindex(index)
            .to_numpy(dtype=np.float64)
        )
        backtest_origin, forecast_origin = LOOKBACK, LOOKBACK + HORIZON
        if not has_recent_history(rate, forecast_origin):
            raise ValueError(
                "Insufficient observed physical history: recent coverage required"
            )
        calendar = calendar_for(profile_from_env(capacity), index)

        t_start = time.perf_counter()
        predicted = self._rates(rate, calendar, forecast_origin)
        inference_time_ms = (time.perf_counter() - t_start) * 1000
        if np.shape(predicted) != (HORIZON,) or not np.isfinite(predicted).all():
            raise ValueError("Forecast inference returned invalid values")
        counts = np.rint(predicted.clip(0, 1) * capacity).astype(int).tolist()

        actual = rate[backtest_origin:forecast_origin] * capacity
        if has_recent_history(rate, backtest_origin) and np.isfinite(actual).all():
            backtest = self._rates(rate, calendar, backtest_origin) * capacity
            evaluation = evaluate_forecast(actual, backtest, inference_time_ms)
        else:
            evaluation = {
                "status": "unavailable",
                "reason": "invalid_evaluation_window",
                "inference_time_ms": inference_time_ms,
            }
        return self._payload(now, origin, counts, evaluation, capacity)

    def _payload(self, now, origin, counts, evaluation, capacity):
        return {
            "type": "TREND_PREDICTION_V2",
            "timestamp": now.isoformat(),
            "stay_duration": {"status": "unavailable", "reason": "not_measured"},
            "evaluation": evaluation,
            "next_24h_occupancy": [
                {
                    "timestamp": (origin + timedelta(hours=i)).isoformat(),
                    "occupancy": value,
                }
                for i, value in enumerate(counts)
            ],
            "max_capacity": capacity,
        }


if __name__ == "__main__":
    print("Estaciona AI - Inteligência Preditiva")
    engine = PredictiveEngine()
    print("Iniciando inferência para as próximas 24h...")
    import json

    print(json.dumps(engine.predict_trends(), indent=2))
