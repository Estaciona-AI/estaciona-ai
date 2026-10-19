# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import os
import logging
import pandas as pd
import numpy as np
import torch
import torch.nn as nn
from datetime import datetime, timedelta, timezone
import time
from sqlalchemy import create_engine, text
from dotenv import load_dotenv
if __package__:
    from .history import physical_history
else:
    from history import physical_history
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


class TemporalAttentionForecast(nn.Module):
    def __init__(self, seq_len=168, pred_len=24, d_model=32):
        super().__init__()
        self.seq_len = seq_len
        self.pred_len = pred_len
        self.feature_proj = nn.Linear(1, d_model)
        self.attention = nn.MultiheadAttention(
            embed_dim=d_model, num_heads=4, batch_first=True
        )
        self.fc = nn.Sequential(
            nn.Linear(d_model * seq_len, 128), nn.ReLU(), nn.Linear(128, pred_len)
        )

    def forward(self, x):
        x = self.feature_proj(x)
        attn_out, _ = self.attention(x, x, x)
        flat = attn_out.reshape(attn_out.size(0), -1)
        return self.fc(flat)


def get_db_engine():
    db_url = os.environ.get("ML_DATABASE_URL") or os.environ.get("DATABASE_URL")
    if db_url and db_url.startswith("postgres://"):
        db_url = db_url.replace("postgres://", "postgresql://", 1)
    return create_engine(db_url)


class PredictiveEngine:
    def __init__(self):
        models_dir = os.path.join(os.path.dirname(__file__), "models")

        pt_path = os.path.join(models_dir, "occupancy_transformer.pt")
        checkpoint = torch.load(pt_path, weights_only=True)
        self.max_val = checkpoint["max_val"]

        self.transformer = TemporalAttentionForecast()
        self.transformer.load_state_dict(checkpoint["model_state_dict"])
        self.transformer.eval()

        self.engine = get_db_engine()

    def get_history(self, hours_back):
        end_time = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
        start_time = end_time - timedelta(hours=hours_back)
        frame = physical_history(self.engine, start_time, end_time)
        hours = pd.date_range(start=start_time, periods=hours_back, freq="h", tz="UTC")
        return (
            frame.set_index("timestamp")["occupancy"]
            .reindex(hours, fill_value=0)
            .astype(int)
            .tolist()
        )

    def get_capacity(self):
        with self.engine.connect() as connection:
            return int(
                connection.execute(
                    text("SELECT COUNT(*) FROM spots WHERE is_active")
                ).scalar_one()
            )

    def predict_trends(self):
        now = datetime.now(timezone.utc)
        capacity = self.get_capacity()

        # Get 192 hours to evaluate the last 24h performance and predict next 24h
        history_counts = self.get_history(192)

        # 1. Predict next 24h using the last 168h
        if not np.isfinite(self.max_val) or self.max_val <= 0:
            raise ValueError("Forecast normalization must be finite and positive")
        recent_168 = history_counts[-168:]
        data_norm = np.array(recent_168, dtype=np.float32) / self.max_val
        tensor_in = torch.tensor(data_norm).unsqueeze(0).unsqueeze(-1)

        t_start = time.perf_counter()
        with torch.no_grad():
            preds_norm = self.transformer(tensor_in).squeeze().numpy()
        inference_time_ms = (time.perf_counter() - t_start) * 1000

        if np.shape(preds_norm) != (24,) or not np.isfinite(preds_norm).all():
            raise ValueError("Forecast inference returned invalid values")
        preds_real = (
            (preds_norm * self.max_val).clip(min=0, max=capacity).astype(int).tolist()
        )

        forecast_array = []
        for i, val in enumerate(preds_real):
            forecast_time = (now + timedelta(hours=i + 1)).replace(
                minute=0, second=0, microsecond=0
            )
            forecast_array.append(
                {"timestamp": forecast_time.isoformat(), "occupancy": val}
            )

        # 2. Evaluate model health using the previous 168h to predict the last 24h
        eval_168 = history_counts[:168]
        actual_24 = history_counts[-24:]

        eval_norm = np.array(eval_168, dtype=np.float32) / self.max_val
        eval_tensor = torch.tensor(eval_norm).unsqueeze(0).unsqueeze(-1)

        with torch.no_grad():
            eval_preds_norm = self.transformer(eval_tensor).squeeze().numpy()

        eval_preds_real = (eval_preds_norm * self.max_val).clip(min=0)

        evaluation = evaluate_forecast(actual_24, eval_preds_real, inference_time_ms)

        payload = {
            "type": "TREND_PREDICTION_V2",
            "timestamp": now.isoformat(),
            "stay_duration": {"status": "unavailable", "reason": "not_measured"},
            "evaluation": evaluation,
            "next_24h_occupancy": forecast_array,
            "max_capacity": capacity,
        }

        return payload


if __name__ == "__main__":
    print("Estaciona AI - Inteligência Preditiva")
    engine = PredictiveEngine()
    print("Iniciando inferência para as próximas 24h...")
    import json

    print(json.dumps(engine.predict_trends(), indent=2))
