# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import numpy as np
from client import preprocess_frame


def test_preprocess_frame_identity():
    frame = np.random.randint(0, 256, (100, 100, 3), dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=1.0, clahe_enabled=False)
    assert np.array_equal(frame, processed)
    assert processed.shape == (100, 100, 3)
    assert processed.dtype == np.uint8


def test_preprocess_frame_gamma_bright():
    frame = np.full((10, 10, 3), 100, dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=2.0, clahe_enabled=False)
    assert not np.array_equal(frame, processed)
    assert processed[0, 0, 0] > 100


def test_preprocess_frame_gamma_dark():
    frame = np.full((10, 10, 3), 100, dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=0.5, clahe_enabled=False)
    assert not np.array_equal(frame, processed)
    assert processed[0, 0, 0] < 100


def test_preprocess_frame_clahe():
    frame = np.random.randint(0, 256, (100, 100, 3), dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=1.0, clahe_enabled=True)
    assert not np.array_equal(frame, processed)
    assert processed.shape == (100, 100, 3)


def test_preprocess_frame_clahe_and_gamma():
    frame = np.random.randint(0, 256, (100, 100, 3), dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=1.5, clahe_enabled=True)
    assert processed.shape == (100, 100, 3)


def test_preprocess_frame_immutability():
    frame = np.full((10, 10, 3), 100, dtype=np.uint8)
    processed = preprocess_frame(frame, gamma=1.5, clahe_enabled=True)
    assert frame[0, 0, 0] == 100
    assert not np.array_equal(frame, processed)
