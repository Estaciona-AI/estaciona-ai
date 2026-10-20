# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Frame processing without capture, presentation or delivery side effects."""

from dataclasses import dataclass
import time

import cv2
import numpy as np

from occupancy import OccupancyTracker, SpotObservation

CONF_THRESHOLD = 0.10
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


@dataclass(frozen=True)
class FrameResult:
    detections: tuple[tuple[int, int, int, int], ...]
    spots: tuple[SpotObservation, ...]


class VisionPipeline:
    def __init__(self, detector, spots: dict, frame_shape: tuple):
        self.detector = detector
        self._occupancy = OccupancyTracker(spots, frame_shape)

    def confirmed_statuses(self) -> dict[str, str]:
        return self._occupancy.confirmed_statuses()

    def reset_candidates(self, *, now: float):
        self._occupancy.reset_candidates(now=now)

    def process(
        self,
        frame: np.ndarray,
        *,
        now: float | None = None,
        expires_at: float | None = None,
    ) -> FrameResult | None:
        if expires_at is not None:
            current = time.monotonic()
            if current > expires_at:
                self.reset_candidates(now=current)
                return None
        inference_frame = preprocess_frame(
            frame, gamma=GAMMA_CORRECTION, clahe_enabled=CLAHE_ENABLED
        )
        detections = []
        for x1, y1, x2, y2 in self.detector.detect(
            inference_frame, confidence=CONF_THRESHOLD
        ):
            shrink_w = int((x2 - x1) * BOX_SHRINK_FACTOR)
            shrink_h = int((y2 - y1) * BOX_SHRINK_FACTOR)
            detections.append(
                (x1 + shrink_w, y1 + shrink_h, x2 - shrink_w, y2 - shrink_h)
            )
        if expires_at is not None:
            current = time.monotonic()
            if current > expires_at:
                self.reset_candidates(now=current)
                return None
        observations = self._occupancy.update(
            detections, now=time.monotonic() if now is None else now
        )
        return FrameResult(tuple(detections), observations)
