# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib
import json
from pathlib import Path
import sys

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/rtsp-lab"))
from configuration import load_configuration


@pytest.fixture
def assets(tmp_path):
    video, model, spots = (
        tmp_path / name for name in ("video.mp4", "model.onnx", "spots.json")
    )
    video.write_bytes(b"video fixture")
    model.write_bytes(b"model fixture")
    spots.write_text(
        json.dumps(
            {f"A-{i:02}": [[i, 0], [i + 1, 0], [i + 1, 5], [i, 5]] for i in range(44)}
        )
    )
    return video, model, spots


@pytest.mark.parametrize("count", [20, 25, 30, 35])
def test_scenario_has_400_distinct_destinations_and_explicit_origins(
    assets, tmp_path, monkeypatch, count
):
    module = importlib.import_module("scenarios")
    monkeypatch.setattr(
        module,
        "probe_video",
        lambda _: {"width": 64, "height": 48, "codec": "h264", "fps": "30/1"},
    )
    out = tmp_path / f"profile-{count}"
    video, model, spots = assets
    module.generate(out, video=video, model=model, spots=spots, inputs=count)
    config = load_configuration(out / "runtime.json")
    manifest = json.loads((out / "scenario.json").read_text())
    assert len(config.inputs) == count
    assert sum(len(item.mapping.spots) for item in config.inputs) == 400
    assert len({spot for item in config.inputs for spot in item.mapping.spots}) == 400
    assert (
        max(len(item.mapping.spots) for item in config.inputs)
        - min(len(item.mapping.spots) for item in config.inputs)
        <= 1
    )
    assert manifest["simulation"] is True
    assert len(manifest["assignments"]) == 400
    assert {entry["source_spot_id"] for entry in manifest["assignments"]} <= set(
        json.loads(spots.read_text())
    )
    assert all(item.source.env == "ESTACIONA_LAB_RTSP" for item in config.inputs)
    assert config.credential_env == "ESTACIONA_LAB_EDGE_KEY"
    assert config.environment == "demo"
    assert config.state_dir.is_relative_to(out)


def test_generator_refuses_overwrite_and_too_few_polygons(
    assets, tmp_path, monkeypatch
):
    module = importlib.import_module("scenarios")
    monkeypatch.setattr(
        module,
        "probe_video",
        lambda _: {"width": 64, "height": 48, "codec": "h264", "fps": "30/1"},
    )
    out = tmp_path / "existing"
    out.mkdir()
    keep = out / "keep"
    keep.write_text("user data")
    video, model, spots = assets
    with pytest.raises(ValueError):
        module.generate(out, video=video, model=model, spots=spots, inputs=20)
    assert keep.read_text() == "user data"
    with pytest.raises(ValueError):
        module.generate(
            tmp_path / "invalid", video=video, model=model, spots=spots, inputs=1
        )
    assert not (tmp_path / "invalid").exists()
