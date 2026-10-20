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
