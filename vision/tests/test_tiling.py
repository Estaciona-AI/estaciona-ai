# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import numpy as np
import pytest

from tiling import detect_tiled

# Two crops of a 900 x 600 frame. LEFT touches the left, top and bottom borders
# and its right edge is internal. RIGHT is offset in both axes: its left, top
# and bottom edges are internal and its right edge is the frame border.
LEFT, RIGHT = (0, 0, 512, 600), (388, 44, 512, 512)
FRAME = np.zeros((600, 900, 3), np.uint8)


def run(left=(), right=()):
    per_crop = {LEFT: list(left), RIGHT: list(right)}

    def predict(crops, confidence):
        assert confidence == 0.3
        assert [crop.shape[:2] for crop in crops] == [(600, 512), (512, 512)]
        return [per_crop[LEFT], per_crop[RIGHT]]

    return detect_tiled(FRAME, predict, 0.3, [LEFT, RIGHT])


def in_frame(box):
    return (box[0] + 388, box[1] + 44, box[2] + 388, box[3] + 44)


def test_crop_boxes_are_translated_to_frame_coordinates():
    assert run(right=[(0.9, (100, 50, 200, 150))]) == [(488, 94, 588, 194)]


@pytest.mark.parametrize(
    "box,kept",
    [
        ((2, 50, 100, 150), False),  # cut by the internal left edge
        ((3, 50, 100, 150), True),
        ((100, 2, 200, 100), False),  # cut by the internal top edge
        ((100, 3, 200, 100), True),
        ((400, 50, 512, 150), True),  # the right edge is the frame border
        ((100, 400, 200, 510), False),  # cut by the internal bottom edge
        ((100, 400, 200, 509), True),
    ],
)
def test_internal_edges_drop_cut_objects_but_frame_borders_keep_them(box, kept):
    assert run(right=[(0.9, box)]) == ([in_frame(box)] if kept else [])


@pytest.mark.parametrize(
    "box,kept",
    [
        ((0, 0, 100, 100), True),  # frame corner
        ((100, 500, 200, 600), True),  # frame bottom
        ((400, 100, 510, 200), False),  # cut by the internal right edge
        ((400, 100, 509, 200), True),
    ],
)
def test_left_crop_keeps_frame_borders_and_drops_its_internal_right_edge(box, kept):
    assert run(left=[(0.9, box)]) == ([box] if kept else [])


def test_a_cut_object_does_not_hide_later_objects_of_the_same_crop():
    found = run(right=[(0.9, (0, 50, 100, 150)), (0.8, (200, 200, 300, 300))])
    assert found == [in_frame((200, 200, 300, 300))]


def test_same_object_seen_by_two_crops_is_reported_once():
    # Frame x 420..480 lies in the overlap of both crops.
    found = run(
        left=[(0.7, (420, 200, 480, 260)), (0.9, (100, 100, 200, 200))],
        right=[(0.8, (32, 156, 92, 216))],
    )
    assert found == [(100, 100, 200, 200), (420, 200, 480, 260)]


@pytest.mark.parametrize(
    "first,second",
    [
        ((300, 100, 320, 140), (330, 100, 350, 140)),
        ((100, 300, 140, 320), (100, 330, 140, 350)),
    ],
)
def test_adjacent_cars_without_overlap_are_both_kept(first, second):
    assert run(left=[(0.9, first), (0.8, second)]) == [first, second]


def test_equal_boxes_overlapping_less_than_nms_threshold_are_both_kept():
    # IoU 0.43 is below the 0.45 threshold and neither box is larger.
    found = run(left=[(0.9, (100, 100, 200, 200)), (0.8, (140, 100, 240, 200))])
    assert found == [(100, 100, 200, 200), (140, 100, 240, 200)]


@pytest.mark.parametrize(
    "car,fragment",
    [
        ((100, 100, 300, 280), (295, 150, 305, 160)),
        ((100, 300, 300, 500), (295, 310, 305, 320)),
    ],
)
def test_fragment_with_half_its_area_inside_a_larger_car_is_dropped(car, fragment):
    assert run(left=[(0.9, car), (0.8, fragment)]) == [car]


@pytest.mark.parametrize(
    "car,fragment",
    [
        ((200, 100, 300, 200), (400, 100, 402, 150)),
        ((100, 200, 200, 300), (100, 400, 150, 402)),
    ],
)
def test_fragment_beside_a_car_without_overlap_is_kept(car, fragment):
    assert run(left=[(0.9, car), (0.8, fragment)]) == [car, fragment]


def test_predictions_must_match_the_requested_crops():
    with pytest.raises(ValueError):
        detect_tiled(FRAME, lambda crops, confidence: [[]], 0.3, [LEFT, RIGHT])


@pytest.mark.parametrize("size,overlap", [(256, 0.1), (2048, 0.5), (256, 0.5)])
def test_tile_size_and_overlap_bounds_are_accepted(size, overlap):
    from tiling import windows

    assert windows(1024, 512, size, overlap)


@pytest.mark.parametrize("size,overlap", [(2049, 0.25), (512, 0.09), (512, 0.51)])
def test_tile_size_and_overlap_beyond_bounds_are_refused(size, overlap):
    from tiling import windows

    with pytest.raises(ValueError, match="invalid tile"):
        windows(4096, 2048, size, overlap)


def test_at_most_64_tiles():
    from tiling import windows

    # 256 px tiles with half overlap advance 128 px: 8 x 8 fits, 13 x 5 does not.
    assert len(windows(1152, 1152, 256, 0.5)) == 64
    with pytest.raises(ValueError, match="exceeds 64"):
        windows(1792, 768, 256, 0.5)
