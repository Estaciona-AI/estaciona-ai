# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from dataclasses import replace
import json
import cv2
import numpy as np
import pytest
from benchmark_corpus import build_corpus
from benchmark_profiles import artifact
from configuration import Mapping
from test_worker import config


@pytest.fixture
def corpus(tmp_path):
    cfg = config(tmp_path)
    mapping = Mapping(64, 48, {"A": [[1, 1], [20, 1], [20, 20], [1, 20]]})
    cfg = replace(cfg, inputs=(replace(cfg.inputs[0], mapping=mapping),))
    video = tmp_path / "video.avi"
    writer = cv2.VideoWriter(str(video), cv2.VideoWriter_fourcc(*"MJPG"), 10, (64, 48))
    assert writer.isOpened()
    for i in range(20):
        writer.write(np.full((48, 64, 3), i * 10, np.uint8))
    writer.release()
    path = build_corpus(
        cfg, video, tmp_path / "corpus", input_id="cam-A", frames=3, stride=2, warmup=1
    )
    return path, json.loads(path.read_text()), cfg, video


def test_corpus_freezes_source_mapping_frames_and_draft_labels(corpus):
    path, data, cfg, video = corpus
    assert data["video"] == artifact(video)
    assert [r["frame_index"] for r in data["frames"]] == [2, 10, 18]
    assert data["reviewed"] is False
    assert data["sampling"] == {"stride": 2, "warmup_frames": 1, "measured_frames": 9}
    assert all(r["labels"] == {"A": "uncertain"} for r in data["frames"])
    assert all(len(r["frame_sha256"]) == 64 for r in data["frames"])
    assert (path.parent / "index.html").is_file()
    assert len(list(path.parent.glob("frame-*.jpg"))) == 3
    with pytest.raises(FileExistsError):
        build_corpus(
            cfg, video, path.parent, input_id="cam-A", frames=3, stride=2, warmup=1
        )
