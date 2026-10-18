# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

#

import os
import pandas as pd
import numpy as np
import lightgbm as lgb
import pickle
import torch
import torch.nn as nn
import torch.optim as optim
from torch.utils.data import DataLoader, TensorDataset
from sqlalchemy import create_engine
from dotenv import load_dotenv
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
    max_val = data.max() if data.max() > 0 else 1.0
    data_norm = data / max_val

    X, y = [], []
    for i in range(len(data_norm) - seq_len - pred_len):
        X.append(data_norm[i : i + seq_len])
        y.append(data_norm[i + seq_len : i + seq_len + pred_len])

    return torch.tensor(np.array(X)).unsqueeze(-1), torch.tensor(np.array(y)), max_val


def train_forecasting_model(df):
    ts_df = prepare_timeseries_data(df)

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


if __name__ == "__main__":
    print("Estaciona AI - Inteligência Preditiva")
    print("Copyright (C) 2026 Guilherme Pedroza")
    print("Licensed under Apache-2.0; provided AS IS, without warranties.")
    print("Redistribution is permitted under Apache License 2.0;")
    print("see LICENSE for conditions and attribution requirements.\n")
    df = load_and_preprocess_data()
    train_duration_model(df)
    train_forecasting_model(df)
