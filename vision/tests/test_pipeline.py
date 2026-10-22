# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from unittest.mock import Mock

import cv2
import numpy as np
import pytest

from pipeline import VisionPipeline, preprocess_frame


def test_pipeline_preserves_preprocessing_boxes_and_source_frame(monkeypatch):
    for name in ("namedWindow", "imshow", "waitKey", "polylines", "putText"):
        monkeypatch.setattr(
            cv2, name, Mock(side_effect=AssertionError("UI in pipeline"))
        )
    frame = np.full((100, 100, 3), 100, dtype=np.uint8)
    original = frame.copy()
    model = Mock()
    model.detect.return_value = [(10, 20, 50, 80)]
    pipeline = VisionPipeline(
        model, {"A": [[0, 0], [99, 0], [99, 99], [0, 99]]}, frame.shape
    )
    result = pipeline.process(frame, now=0.0)
    actual = model.detect.call_args.args[0]
    assert np.array_equal(actual, preprocess_frame(frame, gamma=1.4))
    assert model.detect.call_args.kwargs == {"confidence": 0.10}
    assert result.detections == ((16, 29, 44, 71),)
    assert result.spots[0].ratio == pytest.approx(29 * 43 / 10000)
    assert result.spots[0].changed
    assert not pipeline.process(frame, now=1.0).spots[0].changed
    assert np.array_equal(frame, original)


def test_pipeline_confirmations_use_monotonic_time(monkeypatch):
    model = Mock()
    model.detect.return_value = []
    frame = np.zeros((10, 10, 3), dtype=np.uint8)
    pipeline = VisionPipeline(
        model, {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}, frame.shape
    )
    times = iter([0.0, 0.25, 0.5, 0.75])
    monkeypatch.setattr("pipeline.time.monotonic", lambda: next(times))
    assert pipeline.process(frame).spots[0].status == "free"
    model.detect.return_value = [(0, 0, 9, 9)]
    assert not pipeline.process(frame).spots[0].changed
    assert not pipeline.process(frame).spots[0].changed
    result = pipeline.process(frame)
    assert (result.spots[0].status, result.spots[0].changed) == ("occupied", True)
    assert pipeline.confirmed_statuses() == {"A": "occupied"}


def test_expired_inference_does_not_change_occupancy(monkeypatch):
    model = Mock()
    model.detect.return_value = [(0, 0, 9, 9)]
    frame = np.zeros((10, 10, 3), dtype=np.uint8)
    pipeline = VisionPipeline(
        model, {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}, frame.shape
    )
    clock = iter([0.0, 2.1])
    monkeypatch.setattr("pipeline.time.monotonic", lambda: next(clock))
    assert pipeline.process(frame, expires_at=2.0) is None
    assert pipeline.confirmed_statuses() == {}


def test_reconnection_resets_candidates_but_preserves_confirmed_state():
    model = Mock()
    model.detect.return_value = []
    frame = np.zeros((10, 10, 3), dtype=np.uint8)
    pipeline = VisionPipeline(
        model, {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}, frame.shape
    )
    pipeline.process(frame, now=0.0)
    model.detect.return_value = [(0, 0, 9, 9)]
    pipeline.process(frame, now=0.1)
    pipeline.reset_candidates(now=10.0)
    assert pipeline.confirmed_statuses() == {"A": "free"}
    assert not pipeline.process(frame, now=10.1).spots[0].changed
    assert not pipeline.process(frame, now=10.2).spots[0].changed
    assert pipeline.process(frame, now=10.6).spots[0].changed


def test_expired_inference_breaks_pending_confirmation(monkeypatch):
    model = Mock()
    model.detect.return_value = []
    frame = np.zeros((10, 10, 3), dtype=np.uint8)
    pipeline = VisionPipeline(
        model, {"A": [[0, 0], [9, 0], [9, 9], [0, 9]]}, frame.shape
    )
    pipeline.process(frame, now=0)
    model.detect.return_value = [(0, 0, 9, 9)]
    pipeline.process(frame, now=1)
    monkeypatch.setattr("pipeline.time.monotonic", lambda: 5)
    assert pipeline.process(frame, now=2, expires_at=3) is None
    assert not pipeline.process(frame, now=20).spots[0].changed


def test_pipeline_applies_explicit_confidence_and_gamma_without_changing_geometry():
    frame = np.full((100, 100, 3), 80, np.uint8)
    model = Mock()
    model.detect.return_value = [(10, 20, 50, 80)]
    pipeline = VisionPipeline(model, {}, frame.shape, confidence=0.45, gamma=1.0)
    result = pipeline.process(frame, now=0.0)
    np.testing.assert_array_equal(model.detect.call_args.args[0], frame)
    assert model.detect.call_args.kwargs["confidence"] == 0.45
    assert result.detections == ((16, 29, 44, 71),)
