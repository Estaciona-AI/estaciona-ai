# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Bounded overlapping crops in source coordinates, independent of the model."""

import math

import cv2
import numpy as np


def windows(width, height, size, overlap):
    if type(size) is not int or not 256 <= size <= 2048:
        raise ValueError("invalid tile size")
    if type(overlap) not in (int, float) or not 0.1 <= overlap <= 0.5:
        raise ValueError("invalid tile overlap")

    def axis(length):
        extent = min(size, length)
        count = math.ceil((length - extent) / (extent * (1 - overlap))) + 1
        return extent, np.linspace(0, length - extent, count).astype(int)

    w, xs = axis(width)
    h, ys = axis(height)
    if len(xs) * len(ys) > 64:
        raise ValueError("tile count exceeds 64; increase tile size")
    return [(int(x), int(y), w, h) for y in ys for x in xs]


def detect_tiled(frame, predict, confidence, size, overlap):
    height, width = frame.shape[:2]
    candidates = []
    for left, top, w, h in windows(width, height, size, overlap):
        for score, (x1, y1, x2, y2) in predict(
            frame[top : top + h, left : left + w], confidence
        ):
            # A neighboring crop supplies complete objects cut by an internal
            # edge. Physical image borders remain eligible for partial cars.
            if (
                (left > 0 and x1 <= 2)
                or (top > 0 and y1 <= 2)
                or (left + w < width and x2 >= w - 2)
                or (top + h < height and y2 >= h - 2)
            ):
                continue
            candidates.append((score, (x1 + left, y1 + top, x2 + left, y2 + top)))
    if not candidates:
        return []
    indices = cv2.dnn.NMSBoxes(
        [[x1, y1, x2 - x1, y2 - y1] for _, (x1, y1, x2, y2) in candidates],
        [score for score, _ in candidates],
        0.0,
        0.45,
    )
    kept = sorted(
        (candidates[int(i)] for i in np.asarray(indices).reshape(-1)),
        key=lambda item: -item[0],
    )
    # IoU misses a fragment of a car detected as a second, smaller vehicle:
    # drop a box when most of its own area lies inside a stronger, larger one.
    boxes = []
    for _, box in kept:
        area = _area(box)
        if not any(
            _intersection(box, other) >= COVERED_FRACTION * area and area < _area(other)
            for other in boxes
        ):
            boxes.append(box)
    return boxes


COVERED_FRACTION = 0.5


def _area(box):
    return (box[2] - box[0]) * (box[3] - box[1])


def _intersection(a, b):
    w = min(a[2], b[2]) - max(a[0], b[0])
    h = min(a[3], b[3]) - max(a[1], b[1])
    return max(w, 0) * max(h, 0)
