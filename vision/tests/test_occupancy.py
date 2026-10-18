# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import cv2
import numpy as np
from client import compute_occupancy


def test_compute_occupancy_zero_area():
    mask = np.zeros((100, 100), dtype=np.uint8)
    ratio = compute_occupancy(mask, 0.0, [(10, 10, 20, 20)], (100, 100))
    assert ratio == 0.0


def test_compute_occupancy_no_detections():
    mask = np.zeros((100, 100), dtype=np.uint8)
    cv2.rectangle(mask, (10, 10), (20, 20), 255, -1)
    area = float(np.count_nonzero(mask))
    ratio = compute_occupancy(mask, area, [], (100, 100))
    assert ratio == 0.0


def test_compute_occupancy_perfect_overlap():
    mask = np.zeros((100, 100), dtype=np.uint8)
    cv2.rectangle(mask, (10, 10), (20, 20), 255, -1)
    area = float(np.count_nonzero(mask))
    ratio = compute_occupancy(mask, area, [(10, 10, 20, 20)], (100, 100))
    assert ratio == 1.0


def test_compute_occupancy_partial_overlap():
    mask = np.zeros((100, 100), dtype=np.uint8)
    cv2.rectangle(mask, (10, 10), (20, 20), 255, -1)
    area = float(np.count_nonzero(mask))
    ratio = compute_occupancy(mask, area, [(10, 10, 15, 20)], (100, 100))
    assert abs(ratio - 0.5) < 0.1


def test_compute_occupancy_no_overlap():
    mask = np.zeros((100, 100), dtype=np.uint8)
    cv2.rectangle(mask, (10, 10), (20, 20), 255, -1)
    area = float(np.count_nonzero(mask))
    ratio = compute_occupancy(mask, area, [(40, 40, 50, 50)], (100, 100))
    assert ratio == 0.0


def test_compute_occupancy_multiple_detections():
    mask = np.zeros((100, 100), dtype=np.uint8)
    cv2.rectangle(mask, (10, 10), (20, 20), 255, -1)
    area = float(np.count_nonzero(mask))
    ratio = compute_occupancy(
        mask, area, [(10, 10, 15, 20), (15, 10, 20, 20)], (100, 100)
    )
    assert abs(ratio - 1.0) < 0.05
