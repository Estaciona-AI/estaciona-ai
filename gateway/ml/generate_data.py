# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

#

import os
import uuid
import random
import datetime
import pandas as pd
import numpy as np
from sqlalchemy import create_engine, text
from dotenv import load_dotenv

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))

DB_URL = os.environ.get("ML_DATABASE_URL") or os.environ.get("DATABASE_URL")
if DB_URL and DB_URL.startswith("postgres://"):
    DB_URL = DB_URL.replace("postgres://", "postgresql://", 1)

engine = create_engine(DB_URL)


def generate_users(num_users=800):
    users = []
    for i in range(num_users):
        user_id = str(uuid.uuid4())
        is_pcd = random.random() < 0.05
        is_elderly = random.random() < 0.15

        if is_elderly:
            dob = datetime.date(
                random.randint(1940, 1965), random.randint(1, 12), random.randint(1, 28)
            )
        else:
            dob = datetime.date(
                random.randint(1970, 2007), random.randint(1, 12), random.randint(1, 28)
            )

        plate = f"{chr(random.randint(65, 90))}{chr(random.randint(65, 90))}{chr(random.randint(65, 90))}{random.randint(0, 9)}{chr(random.randint(65, 90))}{random.randint(0, 9)}{random.randint(0, 9)}"
        email = f"user_{i}_{user_id[:8]}@mock.estaciona.tech"
        name = f"User {i}"

        users.append(
            {
                "id": user_id,
                "plate": plate,
                "email": email,
                "name": name,
                "date_of_birth": dob,
                "pcd_status": is_pcd,
                "role": "user",
            }
        )
    return pd.DataFrame(users)


def get_base_rate(hour, is_weekend):
    if is_weekend:
        if 10 <= hour <= 15:
            return 8.0
        if 18 <= hour <= 22:
            return 6.0
        return 1.0

    if 6 <= hour <= 9:
        return 20.0
    if 12 <= hour <= 14:
        return 12.0
    if 17 <= hour <= 19:
        return 10.0
    if 9 < hour < 17:
        return 5.0
    return 0.5


def generate_occupancy(engine, users_df, days=730):
    with engine.connect() as conn:
        spots_result = conn.execute(text("SELECT id FROM spots")).fetchall()
        spot_ids = [r[0] for r in spots_result]

    special_spots = {"A-01", "A-02", "A-03", "A-04"}
    normal_spots = list(set(spot_ids) - special_spots)
    special_spots = list(special_spots)

    start_date = datetime.datetime.now(datetime.UTC) - datetime.timedelta(days=days)
    current_time = start_date
    end_time = datetime.datetime.now(datetime.UTC)

    records = []
    active_parkings = {}

    while current_time < end_time:
        released = [s for s, t in active_parkings.items() if t <= current_time]
        for s in released:
            del active_parkings[s]

        available_normal = [s for s in normal_spots if s not in active_parkings]
        available_special = [s for s in special_spots if s not in active_parkings]

        local_time = current_time - datetime.timedelta(hours=3)
        is_weekend = local_time.weekday() >= 5
        rate = get_base_rate(local_time.hour, is_weekend)

        if 23 <= local_time.hour or local_time.hour < 6:
            if len(active_parkings) >= 5:
                rate = 0.0
            else:
                rate = 0.05

        arrivals = np.random.poisson(rate / 4)

        for _ in range(arrivals):
            if not available_normal and not available_special:
                break

            user = users_df.sample(1).iloc[0]
            is_special_user = user["pcd_status"] or user["date_of_birth"].year <= 1966

            spot = None
            if is_special_user and available_special:
                spot = available_special.pop(random.randrange(len(available_special)))
            elif available_normal:
                spot = available_normal.pop(random.randrange(len(available_normal)))
            elif available_special:
                spot = available_special.pop(random.randrange(len(available_special)))

            if not spot:
                continue

            base_duration = 180 if is_special_user else 120
            duration_mins = max(15, int(np.random.normal(base_duration, 45)))

            released_at = current_time + datetime.timedelta(minutes=duration_mins)
            active_parkings[spot] = released_at

            records.append(
                {
                    "id": str(uuid.uuid4()),
                    "user_id": user["id"],
                    "spot_id": spot,
                    "occupied_at": current_time,
                    "released_at": released_at,
                }
            )

        current_time += datetime.timedelta(minutes=15)

    return pd.DataFrame(records)


def main():
    print("Gerando usuarios falsos...")
    users_df = generate_users(800)

    with engine.begin() as conn:
        for _, row in users_df.iterrows():
            conn.execute(
                text(
                    "INSERT INTO users (id, plate, email, name, date_of_birth, pcd_status, role) VALUES (:id, :plate, :email, :name, :date_of_birth, :pcd_status, :role) ON CONFLICT DO NOTHING"
                ),
                row.to_dict(),
            )

    print("Usuarios inseridos. Gerando histórico falso realista (2 anos)...")
    history_df = generate_occupancy(engine, users_df, days=730)

    print(f"Gerados {len(history_df)} registros. Limpando tabela e inserindo lotes...")

    with engine.begin() as conn:
        conn.execute(text("DELETE FROM user_occupancy_history"))

        batch_size = 5000
        for i in range(0, len(history_df), batch_size):
            batch = history_df.iloc[i : i + batch_size]
            conn.execute(
                text(
                    "INSERT INTO user_occupancy_history (id, user_id, spot_id, occupied_at, released_at) VALUES (:id, :user_id, :spot_id, :occupied_at, :released_at)"
                ),
                batch.to_dict("records"),
            )
            print(f"Inseridos {i + len(batch)} / {len(history_df)}")

    print("População do marco zero concluida com sucesso.")


if __name__ == "__main__":
    print("Estaciona AI - Inteligência Preditiva")
    print("Copyright (C) 2026 Guilherme Pedroza")
    print("Licensed under Apache-2.0; provided AS IS, without warranties.")
    print("Redistribution is permitted under Apache License 2.0;")
    print("see LICENSE for conditions and attribution requirements.\n")
    main()
