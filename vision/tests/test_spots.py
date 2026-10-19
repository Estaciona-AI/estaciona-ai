# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
import numpy as np
from client import load_spots, build_spots_masks


def test_load_spots_valid(tmp_path):
    data = {
        "A-01": [[10, 10], [20, 10], [20, 20], [10, 20]],
        "A-02": [[30, 30], [40, 30], [40, 40], [30, 40]],
    }
    file_path = tmp_path / "spots.json"
    with open(file_path, "w") as f:
        json.dump(data, f)

    loaded = load_spots(str(file_path))
    assert loaded == data


def test_load_spots_empty(tmp_path):
    file_path = tmp_path / "empty.json"
    with open(file_path, "w") as f:
        json.dump({}, f)

    loaded = load_spots(str(file_path))
    assert loaded == {}


def test_build_spots_masks_simple():
    spots = {"A-01": [[0, 0], [10, 0], [10, 10], [0, 10]]}
    frame_shape = (100, 100, 3)
    masks = build_spots_masks(spots, frame_shape)

    assert "A-01" in masks
    mask, area = masks["A-01"]
    assert mask.shape == (100, 100)
    assert mask.dtype == np.uint8
    assert area > 0.0
    assert np.count_nonzero(mask) == int(area)
    assert mask[5, 5] == 255
    assert mask[50, 50] == 0


def test_build_spots_masks_multiple():
    spots = {
        "A-01": [[0, 0], [10, 0], [10, 10], [0, 10]],
        "A-02": [[20, 20], [30, 20], [30, 30], [20, 30]],
    }
    frame_shape = (100, 100, 3)
    masks = build_spots_masks(spots, frame_shape)

    assert len(masks) == 2
    assert "A-01" in masks
    assert "A-02" in masks

    mask1, area1 = masks["A-01"]
    mask2, area2 = masks["A-02"]

    assert mask1[5, 5] == 255
    assert mask1[25, 25] == 0
    assert mask2[5, 5] == 0
    assert mask2[25, 25] == 255


def test_build_spots_masks_empty():
    masks = build_spots_masks({}, (100, 100, 3))
    assert masks == {}


def test_build_spots_masks_triangle():
    spots = {"A-01": [[0, 0], [10, 0], [0, 10]]}
    frame_shape = (100, 100, 3)
    masks = build_spots_masks(spots, frame_shape)
    mask, area = masks["A-01"]
    assert area > 0.0
    assert mask[1, 1] == 255
    assert mask[9, 9] == 0
