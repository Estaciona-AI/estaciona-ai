# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from unittest.mock import Mock

import cv2
import numpy as np

from occupancy import SpotObservation
from pipeline import FrameResult
from presentation import OpenCVDisplay


def test_display_draws_on_a_copy_and_preserves_frame(monkeypatch):
    monkeypatch.setattr(cv2, "namedWindow", Mock())
    monkeypatch.setattr(cv2, "resizeWindow", Mock())
    show, close = Mock(), Mock()
    monkeypatch.setattr(cv2, "imshow", show)
    monkeypatch.setattr(cv2, "waitKey", lambda _: ord("q"))
    monkeypatch.setattr(cv2, "destroyAllWindows", close)
    source = np.zeros((100, 100, 3), dtype=np.uint8)
    display = OpenCVDisplay({"A": [[10, 10], [90, 10], [90, 90], [10, 90]]})
    result = FrameResult(
        ((20, 20, 80, 80),), (SpotObservation("A", "occupied", 0.5, True),)
    )
    display.open()
    assert display.render(source, result)
    shown = show.call_args.args[1]
    assert not np.shares_memory(source, shown)
    assert not np.any(source)
    assert np.any(shown)
    assert tuple(shown[10, 10]) == (0, 0, 255)
    assert tuple(shown[20, 20]) == (255, 0, 0)
    display.close()
    close.assert_called_once()
