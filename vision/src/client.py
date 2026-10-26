# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import argparse
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
from detector import load_detector
from pipeline import VisionPipeline, preprocess_frame as preprocess_frame
from presentation import OpenCVDisplay
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


async def main(argv=None):
    parser = argparse.ArgumentParser(description="Estaciona AI vision client")
    parser.add_argument("spots_path", nargs="?", default=SPOTS_PATH)
    parser.add_argument(
        "--headless",
        action="store_true",
        help="Process without OpenCV display or overlays",
    )
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    if not EDGE_API_KEY:
        raise RuntimeError("EDGE_API_KEY is required")
    if not VIDEO_PATH:
        raise RuntimeError(
            "VISION_STREAM_URL is required: video file, directory or RTSP URL"
        )
    spots_path = args.spots_path

    headers = {"Authorization": f"Bearer {EDGE_API_KEY}"}
    websocket = None
    cap = None
    display = None
    try:
        websocket = await connect_ws(headers)

        spots = load_spots(spots_path)

        device = os.environ.get("VISION_DEVICE", "cuda")
        model = load_detector(
            MODEL_PATH,
            device=device,
            expected_sha256=os.environ.get("VISION_MODEL_SHA256") or None,
        )

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
            print(f"Error: could not open input {CAMERA_ID}")
            return

        ret, frame = cap.read()
        if not ret:
            print("Error: could not read first frame.")
            return

        pipeline = VisionPipeline(model, spots, frame.shape)
        cap.set(cv2.CAP_PROP_POS_FRAMES, 0)

        if not args.headless:
            display = OpenCVDisplay(spots)
            display.open()

        print(f"Loaded {len(spots)} parking spots from {spots_path}")

        last_retry_time = 0.0
        last_sync_time = time.monotonic()

        while True:
            if (
                local_delivery().outbox.first() is not None
                and time.monotonic() - last_retry_time >= 5
            ):
                last_retry_time = time.monotonic()
                websocket = await safe_send(websocket, None, headers)

            if (
                time.monotonic() - last_sync_time >= 30
                and local_delivery().outbox.first() is None
            ):
                last_sync_time = time.monotonic()
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
                        websocket = await safe_send(
                            websocket, json.dumps(payload), headers
                        )

            ret, frame = cap.read()
            if not ret:
                cap.release()
                current_video_idx = (current_video_idx + 1) % len(video_list)
                print(f"[VISION] Reopening input {CAMERA_ID}")
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

            if display is not None and display.render(frame, result):
                break

            await asyncio.sleep(0.01)

    finally:
        try:
            if cap is not None:
                cap.release()
        finally:
            try:
                if display is not None:
                    display.close()
            finally:
                if websocket is not None:
                    await websocket.close()


if __name__ == "__main__":
    asyncio.run(main())
