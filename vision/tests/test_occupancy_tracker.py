# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import numpy as np
import pytest

import occupancy


def tracker():
    return occupancy.OccupancyTracker(
        {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}, (10, 10, 3)
    )


def observe(subject, monkeypatch, ratio, now):
    monkeypatch.setattr(occupancy, "spot_occupancy", lambda *args: ratio)
    return subject.update([], now=now)[0]


def test_initial_state_and_unchanged_frames_emit_only_once(monkeypatch):
    subject = tracker()
    first = observe(subject, monkeypatch, 0.35, 0.0)
    assert (first.spot_id, first.status, first.ratio, first.changed) == (
        "A",
        "occupied",
        0.35,
        True,
    )
    assert not observe(subject, monkeypatch, 0.35, 0.1).changed
    assert subject.confirmed_statuses() == {"A": "occupied"}


def test_occupied_requires_two_candidate_frames_and_half_second(monkeypatch):
    subject = tracker()
    observe(subject, monkeypatch, 0.0, 0.0)
    assert not observe(subject, monkeypatch, 0.5, 0.1).changed
    assert not observe(subject, monkeypatch, 0.5, 0.2).changed
    confirmed = observe(subject, monkeypatch, 0.5, 0.6)
    assert (confirmed.status, confirmed.changed) == ("occupied", True)


def test_free_requires_five_candidate_frames_and_three_seconds(monkeypatch):
    subject = tracker()
    observe(subject, monkeypatch, 0.5, 0.0)
    for now in (0.1, 0.2, 0.3, 0.4, 0.5):
        assert not observe(subject, monkeypatch, 0.0, now).changed
    confirmed = observe(subject, monkeypatch, 0.0, 3.1)
    assert (confirmed.status, confirmed.changed) == ("free", True)


def test_elapsed_time_alone_cannot_replace_candidate_frames(monkeypatch):
    subject = tracker()
    observe(subject, monkeypatch, 0.5, 0.0)
    observe(subject, monkeypatch, 0.0, 0.1)
    assert not observe(subject, monkeypatch, 0.0, 10.0).changed
    assert subject.confirmed_statuses() == {"A": "occupied"}


def test_candidate_interruption_restarts_confirmation(monkeypatch):
    subject = tracker()
    observe(subject, monkeypatch, 0.0, 0.0)
    observe(subject, monkeypatch, 0.5, 0.1)
    observe(subject, monkeypatch, 0.0, 0.7)
    assert not observe(subject, monkeypatch, 0.5, 1.0).changed
    assert not observe(subject, monkeypatch, 0.5, 1.1).changed
    assert observe(subject, monkeypatch, 0.5, 1.5).changed


@pytest.mark.parametrize("ratio", [0.20, 0.25, 0.349])
def test_hysteresis_preserves_existing_state(monkeypatch, ratio):
    occupied, free = tracker(), tracker()
    observe(occupied, monkeypatch, 0.5, 0.0)
    observe(free, monkeypatch, 0.0, 0.0)
    for now in (1.0, 2.0, 3.0, 4.0, 5.0, 6.0):
        assert observe(occupied, monkeypatch, ratio, now).status == "occupied"
        assert observe(free, monkeypatch, ratio, now).status == "free"


def test_real_masks_and_tracker_instances_are_independent():
    first, second = tracker(), tracker()
    assert first.update([(0, 0, 9, 9)], now=0)[0].status == "occupied"
    assert second.update([], now=0)[0].status == "free"
    assert first.confirmed_statuses() == {"A": "occupied"}
    snapshot = first.confirmed_statuses()
    snapshot["A"] = "free"
    assert first.confirmed_statuses() == {"A": "occupied"}
    assert np.count_nonzero(first.spot_masks["A"][0]) == 100


def test_windowed_ratio_matches_full_frame_computation():
    rng = np.random.default_rng(7)
    height, width = 120, 200
    spots = {}
    for index in range(12):
        x, y = int(rng.integers(0, width - 30)), int(rng.integers(0, height - 40))
        spots[f"S{index}"] = [
            (x, y),
            (x + 25, y + 3),
            (x + 28, y + 35),
            (x - 2, y + 30),
        ]
    spots["offscreen_edge"] = [(width - 5, 0), (width + 20, 0), (width + 20, 20)]
    subject = occupancy.OccupancyTracker(spots, (height, width, 3))
    for frame in range(20):
        detections = [
            (
                int(rng.integers(-20, width)),
                int(rng.integers(-20, height)),
                int(rng.integers(0, width + 20)),
                int(rng.integers(0, height + 20)),
            )
            for _ in range(int(rng.integers(0, 8)))
        ]
        observed = subject.update(detections, now=float(frame))
        for observation in observed:
            mask, area = subject.spot_masks[observation.spot_id]
            expected = occupancy.compute_occupancy(
                mask, area, detections, (height, width, 3)
            )
            assert observation.ratio == expected
