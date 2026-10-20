# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import datetime

import json
import os
import sys
import time
import asyncio
import glob
from pathlib import Path
import hashlib
from delivery import LocalDelivery, LocalOutbox
from dotenv import load_dotenv

load_dotenv(os.path.join(os.path.dirname(__file__), "../../.env"))

os.environ["QT_LOGGING_RULES"] = "*=false"

import cv2
import numpy as np
from detector import load_detector
from pipeline import VisionPipeline, preprocess_frame as preprocess_frame
from occupancy import (
    build_spots_masks as build_spots_masks,
    compute_occupancy as compute_occupancy,
)

WS_URL = os.environ.get("LOCAL_WS_URL", "ws://localhost:8001/ws/edge")
EDGE_API_KEY = os.environ.get("EDGE_API_KEY")

MODEL_PATH = os.environ.get("VISION_MODEL_PATH", "models/rtdetr-hgnetv2-l.onnx")
VIDEO_PATH = os.environ.get("VISION_STREAM_URL")
SPOTS_PATH = os.environ.get("VISION_SPOTS_PATH", "data/spots.json")
CAMERA_ID = os.environ.get("VISION_CAMERA_ID", "cam_01")
DELIVERY = None


def local_delivery():
    global DELIVERY
    if DELIVERY is None:
        camera_key = hashlib.sha256(CAMERA_ID.encode()).hexdigest()[:16]
        path = os.environ.get(
            "VISION_OUTBOX_PATH", str(Path("data") / f"vision-outbox-{camera_key}.db")
        )
        DELIVERY = LocalDelivery(WS_URL, LocalOutbox(path))
    return DELIVERY


def load_spots(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def observation_timestamp():
    return (
        datetime.datetime.now(datetime.UTC)
        .isoformat(timespec="microseconds")
        .replace("+00:00", "Z")
    )


async def connect_ws(headers):
    return await local_delivery().connect(headers)


async def safe_send(ws, payload_str, headers):
    return await local_delivery().send(ws, payload_str, headers)


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

    device = os.environ.get("VISION_DEVICE", "cuda")
    model = load_detector(MODEL_PATH, device=device)

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

    pipeline = VisionPipeline(model, spots, frame.shape)
    cap.set(cv2.CAP_PROP_POS_FRAMES, 0)

    cv2.namedWindow("Estaciona AI - Vision", cv2.WINDOW_NORMAL)
    cv2.resizeWindow("Estaciona AI - Vision", 1280, 720)

    print(f"Loaded {len(spots)} parking spots from {spots_path}")

    prev_time = time.time()
    last_retry_time = 0.0
    last_sync_time = time.time()

    while True:
        if (
            local_delivery().outbox.first() is not None
            and time.monotonic() - last_retry_time >= 5
        ):
            last_retry_time = time.monotonic()
            websocket = await safe_send(websocket, None, headers)

        if (
            time.time() - last_sync_time >= 30
            and local_delivery().outbox.first() is None
        ):
            last_sync_time = time.time()
            print("[VISION] Resyncing all spots state to gateway (30s heartbeat)")
            for spot_id, status in pipeline.confirmed_statuses().items():
                if status:
                    payload = {
                        "type": "SPOT_UPDATE",
                        "spot_id": spot_id,
                        "status": status,
                        "camera_id": CAMERA_ID,
                        "confidence": 1.0,
                        "timestamp": observation_timestamp(),
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

        result = pipeline.process(frame)
        for observation in result.spots:
            spot_id, ratio = observation.spot_id, observation.ratio
            if observation.changed:
                payload = {
                    "type": "SPOT_UPDATE",
                    "spot_id": spot_id,
                    "status": observation.status,
                    "camera_id": CAMERA_ID,
                    "confidence": float(ratio)
                    if observation.status == "occupied"
                    else 1.0,
                    "timestamp": observation_timestamp(),
                    "edge_sent_at": datetime.datetime.now(datetime.UTC)
                    .isoformat(timespec="milliseconds")
                    .replace("+00:00", "Z"),
                }
                websocket = await safe_send(websocket, json.dumps(payload), headers)

            is_confirmed_occupied = observation.status == "occupied"
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

        for x1, y1, x2, y2 in result.detections:
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
