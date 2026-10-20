# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Per-input occupancy state, independent of transport and presentation."""

from dataclasses import dataclass

import cv2
import numpy as np

TRIGGER_OCCUPIED_THRESHOLD = 0.35
STAY_OCCUPIED_THRESHOLD = 0.20


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


@dataclass(frozen=True)
class SpotObservation:
    spot_id: str
    status: str
    ratio: float
    changed: bool


@dataclass
class _SpotState:
    confirmed: str
    candidate: str
    consecutive_frames: int
    last_change_time: float


class OccupancyTracker:
    def __init__(self, spots: dict, frame_shape: tuple):
        self.spot_masks = build_spots_masks(spots, frame_shape)
        self.frame_shape = frame_shape
        self._states: dict[str, _SpotState] = {}

    def confirmed_statuses(self) -> dict[str, str]:
        return {spot_id: state.confirmed for spot_id, state in self._states.items()}

    def update(self, detections, *, now: float) -> tuple[SpotObservation, ...]:
        observations = []
        for spot_id, (mask, area) in self.spot_masks.items():
            ratio = compute_occupancy(mask, area, detections, self.frame_shape)
            state = self._states.get(spot_id)
            threshold = (
                STAY_OCCUPIED_THRESHOLD
                if state is not None and state.confirmed == "occupied"
                else TRIGGER_OCCUPIED_THRESHOLD
            )
            raw_status = "occupied" if ratio >= threshold else "free"
            changed = state is None
            if state is None:
                state = _SpotState(raw_status, raw_status, 0, now)
                self._states[spot_id] = state

            if raw_status == state.confirmed:
                state.candidate = raw_status
                state.consecutive_frames = 0
            elif raw_status == state.candidate:
                state.consecutive_frames += 1
                threshold_frames, threshold_time = (
                    (2, 0.5) if raw_status == "occupied" else (5, 3.0)
                )
                if (
                    state.consecutive_frames >= threshold_frames
                    and now - state.last_change_time >= threshold_time
                ):
                    state.confirmed = raw_status
                    state.consecutive_frames = 0
                    state.last_change_time = now
                    changed = True
            else:
                state.candidate = raw_status
                state.consecutive_frames = 1
                state.last_change_time = now

            observations.append(
                SpotObservation(spot_id, state.confirmed, ratio, changed)
            )
        return tuple(observations)
