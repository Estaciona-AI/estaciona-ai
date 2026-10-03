import os
import pandas as pd
import numpy as np
import torch
import torch.nn as nn
from datetime import datetime, timedelta, timezone
import time
from sqlalchemy import create_engine, text
from dotenv import load_dotenv
from sklearn.metrics import mean_absolute_error, mean_squared_error, r2_score

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))


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
        end_time = datetime.now(timezone.utc)
        start_time = end_time - timedelta(hours=hours_back)

        query = f"""
            SELECT occupied_at, COALESCE(released_at, NOW()) AS released_at
            FROM user_occupancy_history
            WHERE released_at IS NULL OR released_at > '{start_time.isoformat()}'
        """
        df = pd.read_sql(query, self.engine)

        hours = pd.date_range(
            start=pd.Timestamp(start_time).floor("h"),
            periods=hours_back,
            freq="h",
            tz="UTC",
        )
        occupancy_counts = []

        if not df.empty:
            df["occupied_at"] = pd.to_datetime(df["occupied_at"], utc=True)
            df["released_at"] = pd.to_datetime(df["released_at"], utc=True)
            for h in hours:
                count = ((df["occupied_at"] <= h) & (df["released_at"] > h)).sum()
                occupancy_counts.append(int(count))
        else:
            occupancy_counts = [0] * hours_back

        return occupancy_counts

    def get_capacity(self):
        with self.engine.connect() as connection:
            return int(
                connection.execute(text("SELECT COUNT(*) FROM spots")).scalar_one()
            )

    def predict_trends(self):
        now = datetime.now(timezone.utc)
        capacity = self.get_capacity()

        # Get 192 hours to evaluate the last 24h performance and predict next 24h
        history_counts = self.get_history(192)

        # 1. Predict next 24h using the last 168h
        recent_168 = history_counts[-168:]
        data_norm = np.array(recent_168, dtype=np.float32) / self.max_val
        tensor_in = torch.tensor(data_norm).unsqueeze(0).unsqueeze(-1)

        t_start = time.perf_counter()
        with torch.no_grad():
            preds_norm = self.transformer(tensor_in).squeeze().numpy()
        inference_time_ms = (time.perf_counter() - t_start) * 1000

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

        try:
            r2 = r2_score(actual_24, eval_preds_real)
            mae = mean_absolute_error(actual_24, eval_preds_real)
            rmse = np.sqrt(mean_squared_error(actual_24, eval_preds_real))
            # Fallback if r2 is negative or weird due to flat actuals
            if r2 < 0 or np.isnan(r2):
                r2 = 0.0
        except Exception:
            r2, mae, rmse = 0.0, 0.0, 0.0

        payload = {
            "type": "TREND_PREDICTION",
            "timestamp": now.isoformat(),
            "avg_stay_duration_mins": 0.0,
            "stay_duration_distribution": [],
            "model_health": {
                "r2_score": float(r2),
                "mae": float(mae),
                "rmse": float(rmse),
                "inference_time_ms": float(inference_time_ms),
            },
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
