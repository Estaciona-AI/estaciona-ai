# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Optional OpenCV presentation; never required by the processing pipeline."""

import time

import cv2
import numpy as np

from pipeline import FrameResult


def draw_annotations(frame, result, spots, *, scale=1.0, compact_labels=False):
    """Render this result on a copy, preserving source coordinates and capture ownership."""
    if scale == 1.0:
        frame = frame.copy()
    else:
        frame = cv2.resize(
            frame, (round(frame.shape[1] * scale), round(frame.shape[0] * scale))
        )
    for index, observation in enumerate(result.spots, 1):
        color = (0, 0, 255) if observation.status == "occupied" else (0, 255, 0)
        polygon = np.rint(np.array(spots[observation.spot_id]) * scale).astype(np.int32)
        cv2.polylines(frame, [polygon], isClosed=True, color=color, thickness=2)
        cv2.putText(
            frame,
            str(index)
            if compact_labels
            else f"{observation.spot_id} {observation.ratio:.0%}",
            polygon[0],
            cv2.FONT_HERSHEY_SIMPLEX,
            0.45 if compact_labels else 0.6,
            color,
            1 if compact_labels else 2,
        )
    for box in result.detections:
        x1, y1, x2, y2 = (round(value * scale) for value in box)
        cv2.rectangle(frame, (x1, y1), (x2, y2), (255, 0, 0), 2)
    return frame


class OpenCVDisplay:
    title = "Estaciona AI - Vision"

    def __init__(self, spots: dict):
        self.spots = spots
        self._previous_time = time.monotonic()

    def open(self):
        cv2.namedWindow(self.title, cv2.WINDOW_NORMAL)
        cv2.resizeWindow(self.title, 1280, 720)

    def render(self, frame: np.ndarray, result: FrameResult) -> bool:
        frame = draw_annotations(frame, result, self.spots)
        current_time = time.monotonic()
        interval = current_time - self._previous_time
        self._previous_time = current_time
        fps = 1.0 / interval if interval > 0 else 0.0
        cv2.putText(
            frame,
            f"FPS: {fps:.1f}",
            (20, 50),
            cv2.FONT_HERSHEY_SIMPLEX,
            1.0,
            (0, 255, 255),
            2,
        )
        cv2.imshow(self.title, frame)
        return cv2.waitKey(1) & 0xFF == ord("q")

    def close(self):
        cv2.destroyAllWindows()
