# ==============================================================================
# Copyright (C) 2026 Guilherme Pedroza
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU Affero General Public License as
# published by the Free Software Foundation, either version 3 of the
# License, or (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU Affero General Public License for more details.
#
# You should have received a copy of the GNU Affero General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.
# ==============================================================================

import datetime

import json
import os
import sys
import time
import asyncio
import glob
from collections import deque
import websockets
from dotenv import load_dotenv

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))

os.environ["QT_LOGGING_RULES"] = "*=false"

import cv2
import numpy as np
from ultralytics import YOLO
import torch

WS_URL = os.environ.get("LOCAL_WS_URL", "ws://localhost:8001/ws/edge")
EDGE_API_KEY = os.environ.get("EDGE_API_KEY")

MODEL_PATH = os.environ.get("VISION_MODEL_PATH", "yolo26x.pt")
VIDEO_PATH = os.environ.get("VISION_STREAM_URL")
SPOTS_PATH = os.environ.get("VISION_SPOTS_PATH", "data/spots.json")
CAMERA_ID = os.environ.get("VISION_CAMERA_ID", "cam_01")
PENDING_UPDATES = deque()

VEHICLE_CLASSES = [2, 7]

TRIGGER_OCCUPIED_THRESHOLD = 0.35
STAY_OCCUPIED_THRESHOLD = 0.20

CONF_THRESHOLD = 0.10
INFERENCE_SIZE = 1312
BOX_SHRINK_FACTOR = 0.15
GAMMA_CORRECTION = 1.4
CLAHE_ENABLED = False


def preprocess_frame(frame, gamma=1.0, clahe_enabled=False) -> np.ndarray:
    processed = frame.copy()
    if gamma != 1.0:
        inv_gamma = 1.0 / gamma
        table = np.array(
            [((i / 255.0) ** inv_gamma) * 255 for i in np.arange(0, 256)]
        ).astype("uint8")
        processed = cv2.LUT(processed, table)
    if clahe_enabled:
        lab = cv2.cvtColor(processed, cv2.COLOR_BGR2LAB)
        l, a, b = cv2.split(lab)
        clahe = cv2.createCLAHE(clipLimit=2.0, tileGridSize=(8, 8))
        cl = clahe.apply(l)
        processed = cv2.cvtColor(cv2.merge((cl, a, b)), cv2.COLOR_LAB2BGR)
    return processed


def load_spots(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def build_spots_masks(
    spots: dict, frame_shape: tuple
) -> dict[str, tuple[np.ndarray, float]]:
    height, width = frame_shape[:2]
    masks = {}

    for spot_id, points in spots.items():
        mask = np.zeros((height, width), dtype=np.uint8)
        polygon = np.array(points, dtype=np.int32)
        cv2.fillPoly(mask, [polygon], 255)
        area = float(cv2.countNonZero(mask))
        masks[spot_id] = (mask, area)

    return masks


def compute_occupancy(
    spot_mask: np.ndarray,
    spot_area: float,
    detections: list[tuple[int, int, int, int]],
    frame_shape: tuple,
) -> float:
    if spot_area == 0:
        return 0.0

    height, width = frame_shape[:2]
    vehicles_mask = np.zeros((height, width), dtype=np.uint8)

    for x1, y1, x2, y2 in detections:
        cv2.rectangle(vehicles_mask, (x1, y1), (x2, y2), 255, -1)

    overlap = cv2.bitwise_and(spot_mask, vehicles_mask)
    overlap_area = float(cv2.countNonZero(overlap))

    return overlap_area / spot_area


async def connect_ws(headers):
    try:
        ws = await websockets.connect(
            WS_URL, additional_headers=headers, open_timeout=3
        )
        print(f"[CONN] Conectado ao gateway: {WS_URL}")
        return ws
    except Exception as e:
        print(f"[CONN] Falha ao conectar ao gateway: {e}")
        raise


async def safe_send(ws, payload_str, headers):
    if payload_str is not None:
        PENDING_UPDATES.append(payload_str)
    try:
        if ws is None:
            ws = await connect_ws(headers)
        while PENDING_UPDATES:
            await ws.send(PENDING_UPDATES[0])
            PENDING_UPDATES.popleft()
        return ws
    except Exception:
        print("[CONN] Conexao perdida, reconectando...")
        try:
            ws = await connect_ws(headers)
            while PENDING_UPDATES:
                await ws.send(PENDING_UPDATES[0])
                PENDING_UPDATES.popleft()
            return ws
        except Exception as e:
            print(f"[CONN] Falha ao reenviar ({e})")
            return None


async def main():
    if not EDGE_API_KEY:
        raise RuntimeError("EDGE_API_KEY is required")
    if not VIDEO_PATH:
        raise RuntimeError(
            "VISION_STREAM_URL is required: video file, directory or RTSP URL"
        )
    spots_path = sys.argv[1] if len(sys.argv) > 1 else SPOTS_PATH

    headers = {"Authorization": f"Bearer {EDGE_API_KEY}"}
    websocket = await connect_ws(headers)

    spots = load_spots(spots_path)

    device = "cuda" if torch.cuda.is_available() else "cpu"
    print(f"\n[VISION DEVICE] Rodando em: {device.upper()}")
    if device == "cpu":
        print(
            "[AVISO] CUDA nao esta disponivel. O modelo rodara no processador (CPU), o que causara engasgos!"
        )

    model = YOLO(MODEL_PATH)

    video_list = []
    if os.path.isdir(VIDEO_PATH):
        video_list = sorted(glob.glob(os.path.join(VIDEO_PATH, "*.mp4")))
        if not video_list:
            print(f"Nenhum video .mp4 encontrado em {VIDEO_PATH}")
            return
    else:
        video_list = [VIDEO_PATH]

    current_video_idx = 0
    cap = cv2.VideoCapture(video_list[current_video_idx])

    if not cap.isOpened():
        print(f"Error: could not open {video_list[current_video_idx]}")
        return

    ret, frame = cap.read()
    if not ret:
        print("Error: could not read first frame.")
        return

    spot_masks = build_spots_masks(spots, frame.shape)
    cap.set(cv2.CAP_PROP_POS_FRAMES, 0)

    debounce_map = {}

    cv2.namedWindow("Estaciona AI - Vision", cv2.WINDOW_NORMAL)
    cv2.resizeWindow("Estaciona AI - Vision", 1280, 720)

    print(f"Loaded {len(spots)} parking spots from {spots_path}")

    prev_time = time.time()
    last_retry_time = 0.0
    last_sync_time = time.time()

    while True:
        if PENDING_UPDATES and time.monotonic() - last_retry_time >= 5:
            last_retry_time = time.monotonic()
            websocket = await safe_send(websocket, None, headers)

        if time.time() - last_sync_time >= 30:
            last_sync_time = time.time()
            print("[VISION] Resyncing all spots state to gateway (30s heartbeat)")
            for spot_id, state in debounce_map.items():
                if state["confirmed"]:
                    payload = {
                        "type": "SPOT_UPDATE",
                        "spot_id": spot_id,
                        "status": state["confirmed"],
                        "camera_id": CAMERA_ID,
                        "confidence": 1.0,
                        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                        "edge_sent_at": datetime.datetime.now(datetime.UTC)
                        .isoformat(timespec="milliseconds")
                        .replace("+00:00", "Z"),
                    }
                    websocket = await safe_send(websocket, json.dumps(payload), headers)

        ret, frame = cap.read()
        if not ret:
            cap.release()
            current_video_idx = (current_video_idx + 1) % len(video_list)
            print(
                f"[VISION] Fim do video. Trocando para: {video_list[current_video_idx]}"
            )
            cap = cv2.VideoCapture(video_list[current_video_idx])
            continue

        inference_frame = preprocess_frame(
            frame, gamma=GAMMA_CORRECTION, clahe_enabled=CLAHE_ENABLED
        )

        results = model.predict(
            inference_frame,
            classes=VEHICLE_CLASSES,
            conf=CONF_THRESHOLD,
            imgsz=INFERENCE_SIZE,
            device=device,
            half=(device == "cuda"),
            verbose=False,
        )

        detections: list[tuple[int, int, int, int]] = []
        for result in results:
            if result.boxes is None:
                continue
            for box in result.boxes.xyxy:
                x1, y1, x2, y2 = int(box[0]), int(box[1]), int(box[2]), int(box[3])

                w = x2 - x1
                h = y2 - y1
                shrink_w = int(w * BOX_SHRINK_FACTOR)
                shrink_h = int(h * BOX_SHRINK_FACTOR)

                sx1 = x1 + shrink_w
                sy1 = y1 + shrink_h
                sx2 = x2 - shrink_w
                sy2 = y2 - shrink_h

                detections.append((sx1, sy1, sx2, sy2))

        for spot_id, (mask, area) in spot_masks.items():
            ratio = compute_occupancy(mask, area, detections, frame.shape)

            if (
                spot_id in debounce_map
                and debounce_map[spot_id]["confirmed"] == "occupied"
            ):
                is_occupied = ratio >= STAY_OCCUPIED_THRESHOLD
            else:
                is_occupied = ratio >= TRIGGER_OCCUPIED_THRESHOLD

            raw_status = "occupied" if is_occupied else "free"

            if spot_id not in debounce_map:
                debounce_map[spot_id] = {
                    "confirmed": raw_status,
                    "candidate": raw_status,
                    "consecutive_frames": 0,
                    "last_change_time": time.time(),
                }

                payload = {
                    "type": "SPOT_UPDATE",
                    "spot_id": spot_id,
                    "status": raw_status,
                    "camera_id": CAMERA_ID,
                    "confidence": float(ratio) if raw_status == "occupied" else 1.0,
                    "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "edge_sent_at": datetime.datetime.now(datetime.UTC)
                    .isoformat(timespec="milliseconds")
                    .replace("+00:00", "Z"),
                }
                websocket = await safe_send(websocket, json.dumps(payload), headers)

            state = debounce_map[spot_id]

            if raw_status == state["confirmed"]:
                state["candidate"] = raw_status
                state["consecutive_frames"] = 0
            else:
                if raw_status == state["candidate"]:
                    state["consecutive_frames"] += 1
                    elapsed_time = time.time() - state["last_change_time"]

                    if raw_status == "occupied":
                        threshold_frames = 2
                        threshold_time = 0.5
                    else:
                        threshold_frames = 5
                        threshold_time = 3.0

                    if (
                        state["consecutive_frames"] >= threshold_frames
                        and elapsed_time >= threshold_time
                    ):
                        state["confirmed"] = raw_status
                        state["consecutive_frames"] = 0
                        state["last_change_time"] = time.time()

                        payload = {
                            "type": "SPOT_UPDATE",
                            "spot_id": spot_id,
                            "status": raw_status,
                            "camera_id": CAMERA_ID,
                            "confidence": float(ratio)
                            if raw_status == "occupied"
                            else 1.0,
                            "timestamp": time.strftime(
                                "%Y-%m-%dT%H:%M:%SZ", time.gmtime()
                            ),
                            "edge_sent_at": datetime.datetime.now(datetime.UTC)
                            .isoformat(timespec="milliseconds")
                            .replace("+00:00", "Z"),
                        }
                        websocket = await safe_send(
                            websocket, json.dumps(payload), headers
                        )

                else:
                    state["candidate"] = raw_status
                    state["consecutive_frames"] = 1
                    state["last_change_time"] = time.time()

            is_confirmed_occupied = state["confirmed"] == "occupied"
            color = (0, 0, 255) if is_confirmed_occupied else (0, 255, 0)

            status = f"{ratio:.0%}"

            polygon = np.array(spots[spot_id], dtype=np.int32)
            cv2.polylines(frame, [polygon], isClosed=True, color=color, thickness=2)
            cv2.putText(
                frame,
                f"{spot_id} {status}",
                polygon[0],
                cv2.FONT_HERSHEY_SIMPLEX,
                0.6,
                color,
                2,
            )

        for x1, y1, x2, y2 in detections:
            cv2.rectangle(frame, (x1, y1), (x2, y2), (255, 0, 0), 2)

        curr_time = time.time()
        time_diff = curr_time - prev_time
        fps = 1.0 / time_diff if time_diff > 0 else 0.0
        prev_time = curr_time
        cv2.putText(
            frame,
            f"FPS: {fps:.1f}",
            (20, 50),
            cv2.FONT_HERSHEY_SIMPLEX,
            1.0,
            (0, 255, 255),
            2,
        )

        cv2.imshow("Estaciona AI - Vision", frame)

        if cv2.waitKey(1) & 0xFF == ord("q"):
            break

        await asyncio.sleep(0.01)

    cap.release()
    cv2.destroyAllWindows()
    if websocket:
        await websocket.close()


if __name__ == "__main__":
    asyncio.run(main())
