# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import hashlib
import json
import os
import logging
import sqlite3
from contextlib import closing
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
        deployment_context,
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
        deployment_context,
        get_db_engine,
        has_recent_history,
        predict_rates,
    )

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
        error = predicted - actual
        mae = float(np.mean(np.abs(error)))
        rmse = float(np.sqrt(np.mean(error**2)))
        if not np.isfinite([mae, rmse]).all():
            return {**unavailable, "reason": "non_finite_values"}
        metrics = {"mae": mae, "rmse": rmse, **timing}
        if np.ptp(actual) == 0:
            return {"status": "partial", "reason": "constant_target", **metrics}
        r2 = float(1 - np.sum(error**2) / np.sum((actual - actual.mean()) ** 2))
        if not np.isfinite(r2):
            return {**unavailable, "reason": "non_finite_values"}
        return {"status": "available", "r2_score": r2, **metrics}
    except Exception as error:
        logging.getLogger(__name__).warning(
            "Forecast evaluation unavailable: %s", type(error).__name__
        )
        return {**unavailable, "reason": "evaluation_failed"}


class StaleArtifact(ValueError):
    """The artifact was trained for another installation, site profile or
    capacity: retrain instead of serving it."""


class ForecastLedger:
    """Forecasts as issued, so they are scored only against later outcomes."""

    def __init__(self, path):
        self.path = str(path)
        with closing(sqlite3.connect(self.path)) as conn, conn:
            conn.execute(
                "CREATE TABLE IF NOT EXISTS forecasts "
                "(origin TEXT PRIMARY KEY, issued_at TEXT NOT NULL, rates TEXT NOT NULL)"
            )

    def record(self, origin, rates, issued_at):
        # The first forecast of an origin stands; a restart cannot revise it.
        with closing(sqlite3.connect(self.path)) as conn, conn:
            conn.execute(
                "INSERT OR IGNORE INTO forecasts VALUES (?, ?, ?)",
                (
                    origin.isoformat(),
                    issued_at.isoformat(),
                    json.dumps([float(rate) for rate in rates]),
                ),
            )
            conn.execute(
                "DELETE FROM forecasts WHERE origin < ?",
                ((origin - timedelta(days=7)).isoformat(),),
            )

    def issued(self, origin):
        with closing(sqlite3.connect(self.path)) as conn:
            row = conn.execute(
                "SELECT rates FROM forecasts WHERE origin = ?", (origin.isoformat(),)
            ).fetchone()
        return None if row is None else np.asarray(json.loads(row[0]), np.float64)


def load_occupancy_model(models_dir, context=None):
    """Load the LightGBM artifact only if it matches its manifest and, when
    given, the deployment it is about to serve."""
    with open(os.path.join(models_dir, MANIFEST_FILE), encoding="utf-8") as handle:
        manifest = json.load(handle)
    if context is not None and manifest.get("context") != context:
        raise StaleArtifact(
            "Forecast artifact was trained for another installation, "
            "site profile or capacity"
        )
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
    def __init__(self, models_dir=None, engine=None, ledger=None):
        models_dir = models_dir or os.path.join(os.path.dirname(__file__), "models")
        self.engine = engine or get_db_engine()
        self.ledger = ledger
        self.manifest, self.model = load_occupancy_model(
            models_dir, deployment_context(self.engine, self.get_capacity())
        )

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
        if deployment_context(self.engine, capacity) != self.manifest.get("context"):
            raise StaleArtifact("The deployment changed since the artifact was trained")

        # The last 24 h (scored against the forecast issued then) and the
        # lags of the forecast origin.
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
        evaluation = self._prospective_evaluation(
            rate[backtest_origin:forecast_origin], origin, capacity, inference_time_ms
        )
        if self.ledger is not None:
            self.ledger.record(origin, predicted, now)
        return self._payload(now, origin, counts, evaluation, capacity)

    def _prospective_evaluation(self, observed, origin, capacity, inference_time_ms):
        """Score the forecast issued 24 h ago against what happened since.

        Reconstructing it with the current artifact would score hours that
        artifact was trained on; only a forecast recorded when it was issued
        is out of sample.
        """
        issued = None
        if self.ledger is not None:
            issued = self.ledger.issued(origin - timedelta(hours=HORIZON))
        if issued is None:
            return {
                "status": "unavailable",
                "reason": "no_prior_forecast",
                "inference_time_ms": inference_time_ms,
            }
        if not np.isfinite(observed).all():
            return {
                "status": "unavailable",
                "reason": "invalid_evaluation_window",
                "inference_time_ms": inference_time_ms,
            }
        return evaluate_forecast(
            observed * capacity, issued.clip(0, 1) * capacity, inference_time_ms
        )

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
