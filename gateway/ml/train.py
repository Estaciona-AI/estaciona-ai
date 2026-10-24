# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

#

import argparse
import hashlib
import json
import os
import warnings
from datetime import datetime, timedelta, timezone
import pandas as pd
import numpy as np
import lightgbm as lgb
import pickle
import torch
import torch.nn as nn
import torch.optim as optim
from torch.utils.data import DataLoader, TensorDataset
from sqlalchemy import create_engine, text
from dotenv import load_dotenv

if __package__:
    from .backtest import (
        BASELINES,
        HORIZON,
        WEEK,
        actual_windows,
        choose_serving,
        compare,
        origins_between,
        seasonal_naive,
        weekly_profile,
    )
    from .history import physical_history
    from .simulate import synthetic_rates
    from .site_calendar import calendar_frame, profile_from_env
else:
    from backtest import (
        BASELINES,
        HORIZON,
        WEEK,
        actual_windows,
        choose_serving,
        compare,
        origins_between,
        seasonal_naive,
        weekly_profile,
    )
    from history import physical_history
    from simulate import synthetic_rates
    from site_calendar import calendar_frame, profile_from_env
from sklearn.model_selection import train_test_split
from sklearn.metrics import mean_absolute_error, r2_score

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))


def get_db_engine():
    db_url = os.environ.get("ML_DATABASE_URL") or os.environ.get("DATABASE_URL")
    if db_url and db_url.startswith("postgres://"):
        db_url = db_url.replace("postgres://", "postgresql://", 1)
    return create_engine(db_url)


def load_and_preprocess_data():
    print("[1/6] Conectando ao banco e extraindo dados de ocupação...")
    engine = get_db_engine()

    query = """
        SELECT 
            h.id, h.spot_id, h.occupied_at, h.released_at,
            u.pcd_status, u.date_of_birth
        FROM user_occupancy_history h
        JOIN users u ON h.user_id = u.id
        WHERE h.released_at IS NOT NULL
    """

    df = pd.read_sql(query, engine)

    print("[2/6] Realizando Feature Engineering...")
    df["occupied_at"] = pd.to_datetime(df["occupied_at"])
    df["released_at"] = pd.to_datetime(df["released_at"])
    df["duration_mins"] = (
        df["released_at"] - df["occupied_at"]
    ).dt.total_seconds() / 60.0

    df["hour_of_day"] = df["occupied_at"].dt.hour
    df["day_of_week"] = df["occupied_at"].dt.dayofweek
    df["is_weekend"] = df["day_of_week"] >= 5

    df["birth_year"] = pd.to_datetime(df["date_of_birth"]).dt.year
    df["is_elderly"] = df["birth_year"] <= 1966

    print(f"Base processada com {len(df)} registros válidos.")
    return df


def train_duration_model(df):
    print("[3/6] Treinando modelo LightGBM para predição de tempo de permanência...")

    features = ["hour_of_day", "day_of_week", "is_weekend", "pcd_status", "is_elderly"]
    X = df[features].astype({"pcd_status": int, "is_elderly": int, "is_weekend": int})
    y = df["duration_mins"]

    X_train, X_test, y_train, y_test = train_test_split(
        X, y, test_size=0.2, random_state=42
    )

    params = {
        "objective": "regression",
        "metric": "mae",
        "boosting_type": "gbdt",
        "learning_rate": 0.05,
        "num_leaves": 31,
        "verbose": -1,
    }

    train_data = lgb.Dataset(X_train, label=y_train)
    test_data = lgb.Dataset(X_test, label=y_test, reference=train_data)

    model = lgb.train(params, train_data, num_boost_round=100, valid_sets=[test_data])

    preds = model.predict(X_test)
    mae = mean_absolute_error(y_test, preds)
    r2 = r2_score(y_test, preds)

    print(
        f"Modelo LightGBM treinado! Erro médio (MAE): {mae:.2f} minutos | R²: {r2:.2f}"
    )

    models_dir = os.path.join(os.path.dirname(__file__), "models")
    os.makedirs(models_dir, exist_ok=True)
    model_path = os.path.join(models_dir, "duration_model.pkl")

    with open(model_path, "wb") as f:
        pickle.dump(model, f)

    print(f"Pesos salvos em: {model_path}")
    return model


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


def prepare_timeseries_data(df):
    print("[4/6] Agrupando histórico em série temporal (hora a hora)...")
    start_time = df["occupied_at"].min().floor("h")
    end_time = df["released_at"].max().ceil("h")

    hours = pd.date_range(start=start_time, end=end_time, freq="h")
    ts_df = pd.DataFrame({"timestamp": hours})

    occupancy_counts = []
    for h in hours:
        count = ((df["occupied_at"] <= h) & (df["released_at"] > h)).sum()
        occupancy_counts.append(count)

    ts_df["occupancy"] = occupancy_counts
    return ts_df


def create_sequences(ts_df, seq_len=168, pred_len=24):
    data = ts_df["occupancy"].values.astype(np.float32)
    if len(data) < seq_len + pred_len + 1 or not np.isfinite(data).all():
        raise ValueError(
            "Training requires sufficient finite observed physical history"
        )
    if not ts_df["timestamp"].diff().dropna().eq(pd.Timedelta(hours=1)).all():
        raise ValueError("Training physical history contains missing hours")
    max_val = data.max() if data.max() > 0 else 1.0
    data_norm = data / max_val

    X, y = [], []
    for i in range(len(data_norm) - seq_len - pred_len):
        X.append(data_norm[i : i + seq_len])
        y.append(data_norm[i + seq_len : i + seq_len + pred_len])

    return torch.tensor(np.array(X)).unsqueeze(-1), torch.tensor(np.array(y)), max_val


def train_forecasting_model():
    ts_df = physical_history(get_db_engine())
    if not ts_df.empty:
        # Unknown samples split the timeline; never fill them or join runs.
        ts_df = ts_df.loc[np.isfinite(ts_df["occupancy"])].sort_values("timestamp")
    if not ts_df.empty:
        runs = ts_df["timestamp"].diff().ne(pd.Timedelta(hours=1)).cumsum()
        ts_df = ts_df.loc[runs == runs.iloc[-1]].reset_index(drop=True)
    if len(ts_df) < 193:
        raise ValueError(
            "At least 193 hours of physical occupancy are required for training"
        )

    print("[5/6] Preparando tensores para o PyTorch tranformer...")
    seq_len = 168
    pred_len = 24

    X, y, max_val = create_sequences(ts_df, seq_len, pred_len)

    dataset = TensorDataset(X, y)
    loader = DataLoader(dataset, batch_size=32, shuffle=True)

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    print(
        f"[6/6] Treinando Rede Neural de Atenção (Aceleração: {device.type.upper()})..."
    )

    model = TemporalAttentionForecast(seq_len, pred_len).to(device)
    optimizer = optim.Adam(model.parameters(), lr=0.001)
    criterion = nn.MSELoss()

    model.train()
    epochs = 50

    for epoch in range(epochs):
        total_loss = 0
        for batch_X, batch_y in loader:
            batch_X, batch_y = batch_X.to(device), batch_y.to(device)
            optimizer.zero_grad()
            preds = model(batch_X)
            loss = criterion(preds, batch_y)
            loss.backward()
            optimizer.step()
            total_loss += loss.item()

        if (epoch + 1) % 5 == 0:
            print(
                f"    Epoch {epoch + 1}/{epochs} | Loss: {total_loss / len(loader):.4f}"
            )

    print("Treinamento finalizado.")

    models_dir = os.path.join(os.path.dirname(__file__), "models")
    os.makedirs(models_dir, exist_ok=True)

    torch.save(
        {"model_state_dict": model.state_dict(), "max_val": float(max_val)},
        os.path.join(models_dir, "occupancy_transformer.pt"),
    )
    print("Pesos do transformer salvos com sucesso.")


# Occupancy model: LightGBM on occupancy rate, one row per forecast horizon.
LOOKBACK = 4 * WEEK
SIM_VARIANTS = 16
SIM_DAYS = 730
SIM_SEED = 20261024
SIM_ORIGIN_STRIDE = 3
REAL_WEIGHT = 20.0
GATE_DAYS = 28
MODEL_FILE = "occupancy_lgbm.txt"
MANIFEST_FILE = "occupancy_lgbm.json"
MANIFEST_VERSION = 1
LGB_ROUNDS = 400
LGB_PARAMS = {
    "objective": "regression",
    "learning_rate": 0.05,
    "num_leaves": 63,
    "min_data_in_leaf": 50,
    "feature_fraction": 0.9,
    "deterministic": True,
    "force_row_wise": True,
    "num_threads": 4,
    "seed": SIM_SEED,
    "verbose": -1,
}
FEATURES = [
    "horizon",
    "hour",
    "weekday",
    "period",
    "sem_progress",
    "holiday",
    "holiday_last_week",
    "period_last_week",
    "last_rate",
    "same_hour_yesterday",
    "same_hour_last_week",
    "weekly_profile",
    "weekly_change",
    "mean_24h",
]


def calendar_for(profile, index):
    return calendar_frame(profile, index)


def build_features(rate, calendar, origins):
    """Features for each origin and horizon; only rates before the origin are read."""
    origins = np.asarray(origins, dtype=int)
    if len(origins) and origins.min() < LOOKBACK:
        raise ValueError("Origins need four weeks of lag positions")
    mean_24h = pd.Series(rate).rolling(24, min_periods=1).mean().to_numpy()
    columns = {name: calendar[name].to_numpy() for name in calendar.columns}
    frames = []
    for horizon in range(HORIZON):
        target = origins + horizon
        lags = np.stack([rate[target - WEEK * k] for k in range(1, 5)])
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)
            profile = np.nanmean(lags, axis=0)
        frames.append(
            pd.DataFrame(
                {
                    "horizon": horizon,
                    "hour": columns["hour"][target],
                    "weekday": columns["weekday"][target],
                    "period": columns["period"][target],
                    "sem_progress": columns["sem_progress"][target],
                    "holiday": columns["holiday"][target],
                    "holiday_last_week": columns["holiday"][target - WEEK],
                    "period_last_week": columns["period"][target - WEEK],
                    "last_rate": rate[origins - 1],
                    "same_hour_yesterday": rate[target - 24],
                    "same_hour_last_week": rate[target - WEEK],
                    "weekly_profile": profile,
                    "weekly_change": rate[origins - 1] - rate[origins - 1 - WEEK],
                    "mean_24h": mean_24h[origins - 1],
                    "origin": origins,
                    "target": target,
                }
            )
        )
    return pd.concat(frames, ignore_index=True)


def has_recent_history(rate, origin):
    """Enough recent observation for lags to describe the current situation."""
    day = rate[max(origin - 24, 0) : origin]
    week = rate[max(origin - WEEK, 0) : origin]
    return bool(np.isfinite(day).any() and np.isfinite(week).sum() >= WEEK / 2)


def synthetic_training_rates(profile, capacity, index, variants=SIM_VARIANTS):
    return synthetic_rates(profile, capacity, index, variants, SIM_SEED)


def _training_rows(rate, calendar, origins, weight):
    frame = build_features(rate, calendar, origins)
    target = rate[frame["target"].to_numpy()]
    keep = np.isfinite(target)
    return frame.loc[keep, FEATURES], target[keep], np.full(keep.sum(), weight)


def fit_model(profile, index, real_rate, sims, rounds=LGB_ROUNDS):
    """Train on randomized campuses plus real history weighted more heavily.

    Fine-tuning on a short real window alone forgets regimes it has not seen
    (holidays, vacations); pooling keeps them.
    """
    calendar = calendar_for(profile, index)
    origins = origins_between(index, index[0], index[-1])
    parts = [
        _training_rows(rate, calendar, origins[::SIM_ORIGIN_STRIDE], 1.0)
        for rate in sims
    ]
    if np.isfinite(real_rate).any():
        parts.append(_training_rows(real_rate, calendar, origins, REAL_WEIGHT))
    features = pd.concat([part[0] for part in parts], ignore_index=True)
    target = np.concatenate([part[1] for part in parts])
    weight = np.concatenate([part[2] for part in parts])
    dataset = lgb.Dataset(features, target, weight=weight, free_raw_data=True)
    return lgb.train(LGB_PARAMS, dataset, num_boost_round=rounds)


def predict_rates(model, rate, calendar, origins):
    frame = build_features(rate, calendar, origins)
    predicted = model.predict(frame[FEATURES]).clip(0, 1)
    out = np.full((len(origins), HORIZON), np.nan)
    rows = np.searchsorted(np.asarray(origins), frame["origin"].to_numpy())
    out[rows, frame["target"].to_numpy() - frame["origin"].to_numpy()] = predicted
    return out


def baseline_rates(name, rate, origins):
    method = {"seasonal_naive": seasonal_naive, "weekly_profile": weekly_profile}[name]
    return method(rate, origins)


def gate(profile, index, real_rate, sims):
    """Rolling backtest on the last real weeks, refitted without them."""
    holdout_start = index[-1] - timedelta(days=GATE_DAYS)
    holdout = origins_between(index, holdout_start, index[-1], step=6)
    known = real_rate.copy()
    known[np.asarray(index >= holdout_start)] = np.nan
    if np.isfinite(known).sum() < WEEK or len(holdout) == 0:
        return {"status": "not_evaluated", "reason": "insufficient_real_history"}
    candidate = fit_model(profile, index, known, sims)
    calendar = calendar_for(profile, index)
    predictions = {"model": predict_rates(candidate, real_rate, calendar, holdout)}
    for name in BASELINES:
        predictions[name] = baseline_rates(name, real_rate, holdout)
    scores = compare(actual_windows(real_rate, holdout), predictions)
    if scores["points"] == 0:
        return {"status": "not_evaluated", "reason": "no_comparable_points"}
    return {"status": "evaluated", "holdout_days": GATE_DAYS, "scores": scores}


def _write_atomic(path, data):
    temporary = f"{path}.tmp"
    with open(temporary, "wb") as handle:
        handle.write(data)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def train_occupancy_model(engine=None, models_dir=None, variants=SIM_VARIANTS):
    engine = engine or get_db_engine()
    models_dir = models_dir or os.path.join(os.path.dirname(__file__), "models")
    end = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    index = pd.date_range(
        end - timedelta(days=SIM_DAYS), end, freq="h", tz="UTC", inclusive="left"
    )
    with engine.connect() as connection:
        capacity = int(
            connection.execute(
                text("SELECT COUNT(*) FROM spots WHERE is_active")
            ).scalar_one()
        )
    if capacity == 0:
        raise ValueError("Training requires registered active spots")
    profile = profile_from_env(capacity)
    history = physical_history(engine, index[0], end)
    real_rate = (
        history.set_index("timestamp")["rate"].reindex(index).to_numpy(dtype=np.float64)
    )
    real_hours = int(np.isfinite(real_rate).sum())

    print(f"[1/3] Simulando {variants} campi para o prior sintético...")
    sims = synthetic_training_rates(profile, capacity, index, variants)
    serving, evaluation = (
        "model",
        {"status": "not_evaluated", "reason": "insufficient_real_history"},
    )
    if real_hours >= GATE_DAYS * 24:
        print("[2/3] Comparando com baselines nas últimas semanas reais...")
        evaluation = gate(profile, index, real_rate, sims)
        if evaluation["status"] == "evaluated":
            serving = choose_serving(evaluation["scores"])
    print(f"[3/3] Treinando LightGBM com {real_hours} horas reais...")
    model = fit_model(profile, index, real_rate, sims)

    os.makedirs(models_dir, exist_ok=True)
    model_text = model.model_to_string().encode()
    manifest = {
        "manifest_version": MANIFEST_VERSION,
        "model": "lightgbm_direct_24h_rate",
        "model_sha256": hashlib.sha256(model_text).hexdigest(),
        "features": FEATURES,
        "trained_at": datetime.now(timezone.utc).isoformat(),
        "data_end": end.isoformat(),
        "capacity_at_training": capacity,
        "real_hours": real_hours,
        "source": "blended" if real_hours else "synthetic_prior",
        "profile": {"specified": profile.specified, "sha256": profile.digest()},
        "simulator": {"variants": variants, "days": SIM_DAYS, "seed": SIM_SEED},
        "evaluation": evaluation,
        "serving": serving,
    }
    # The manifest names the model hash, so it is written last.
    _write_atomic(os.path.join(models_dir, MODEL_FILE), model_text)
    _write_atomic(
        os.path.join(models_dir, MANIFEST_FILE),
        json.dumps(manifest, indent=2, allow_nan=False).encode(),
    )
    print(f"Modelo salvo; servindo: {serving}; fonte: {manifest['source']}.")
    return manifest


if __name__ == "__main__":
    print("Estaciona AI - Inteligência Preditiva")
    print("Copyright (C) 2026 Guilherme Pedroza")
    print("Licensed under Apache-2.0; provided AS IS, without warranties.")
    print("Redistribution is permitted under Apache License 2.0;")
    print("see LICENSE for conditions and attribution requirements.\n")
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--legacy-attention",
        action="store_true",
        help="also train the legacy attention checkpoint",
    )
    args = parser.parse_args()
    df = load_and_preprocess_data()
    if not df.empty:
        train_duration_model(df)
    train_occupancy_model()
    if args.legacy_attention:
        train_forecasting_model()
