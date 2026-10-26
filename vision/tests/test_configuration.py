# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import importlib
import json
from uuid import uuid4

import pytest


def load(path):
    module = importlib.import_module("configuration")
    return module.load_configuration(path)


@pytest.fixture
def profile(tmp_path):
    (tmp_path / "installation.json").write_text(
        json.dumps(
            {
                "schema_version": 1,
                "organization": {"id": str(uuid4())},
                "installation": {"id": str(uuid4())},
                "environment": "test",
            }
        )
    )
    (tmp_path / "model.onnx").write_bytes(b"fixture-not-an-actual-model")
    (tmp_path / "source.avi").write_bytes(b"fixture-not-an-actual-video")
    (tmp_path / "mapping.json").write_text(
        json.dumps(
            {
                "width": 64,
                "height": 64,
                "spots": {"A": [[0, 0], [20, 0], [20, 20], [0, 20]]},
            }
        )
    )
    data = {
        "version": 1,
        "installation_manifest": "installation.json",
        "state_dir": "state",
        "gateway_url": "ws://127.0.0.1:8001/ws/edge",
        "credential_env": "TEST_EDGE_KEY",
        "detectors": {
            "main": {"adapter": "rtdetr", "path": "model.onnx", "device": "cpu"}
        },
        "sources": {"video": {"kind": "file", "path": "source.avi", "loop": True}},
        "inputs": [
            {
                "input_id": "cam-01",
                "source": "video",
                "detector": "main",
                "mapping": "mapping.json",
            }
        ],
    }
    path = tmp_path / "runtime.json"
    path.write_text(json.dumps(data))
    return path, data


def save(profile):
    path, data = profile
    path.write_text(json.dumps(data))
    return path


def test_loads_relative_paths_identity_and_defaults(profile):
    config = load(profile[0])
    item = config.inputs[0]
    assert item.input_id == "cam-01"
    assert item.mapping.width == 64
    assert item.fps == 2.5
    assert item.detector.path == profile[0].parent / "model.onnx"
    assert item.source.path == profile[0].parent / "source.avi"
    assert config.state_dir == profile[0].parent / "state"
    assert config.environment == "test"
    assert not config.state_dir.exists()  # validation has no runtime side effects


@pytest.mark.parametrize(
    "mutation",
    [
        "duplicate_input",
        "duplicate_spot",
        "missing_source",
        "bad_model",
        "bad_device",
        "zero_fps",
        "nan_fps",
        "bool_fps",
        "unknown_field",
        "empty_inputs",
        "inline_secret",
    ],
)
def test_invalid_profiles_are_rejected(profile, mutation):
    path, data = profile
    if mutation == "duplicate_input":
        data["inputs"].append(dict(data["inputs"][0]))
    elif mutation == "duplicate_spot":
        data["inputs"].append(dict(data["inputs"][0], input_id="cam-02"))
    elif mutation == "missing_source":
        data["inputs"][0]["source"] = "missing"
    elif mutation == "bad_model":
        data["detectors"]["main"]["adapter"] = "python-plugin"
    elif mutation == "bad_device":
        data["detectors"]["main"]["device"] = "automatic"
    elif mutation in ("zero_fps", "nan_fps", "bool_fps"):
        data["inputs"][0]["fps"] = {
            "zero_fps": 0,
            "nan_fps": float("nan"),
            "bool_fps": True,
        }[mutation]
    elif mutation == "unknown_field":
        data["typo"] = 1
    elif mutation == "empty_inputs":
        data["inputs"] = []
    else:
        data["sources"]["video"] = {
            "kind": "rtsp",
            "url": "rtsp://user:secret@host/live",
        }
    with pytest.raises(ValueError):
        load(save(profile))


def test_rtsp_secret_is_only_resolved_from_environment(profile, monkeypatch):
    _, data = profile
    data["sources"]["video"] = {"kind": "rtsp", "env": "TEST_RTSP"}
    monkeypatch.setenv("TEST_RTSP", "rtsp://user:secret@localhost/live")
    config = load(save(profile))
    assert config.inputs[0].source.resolve() == "rtsp://user:secret@localhost/live"
    assert "user:secret" not in repr(config)
    monkeypatch.delenv("TEST_RTSP")
    with pytest.raises(ValueError, match="source environment"):
        config.inputs[0].source.resolve()


@pytest.mark.parametrize(
    "mapping",
    [
        {"width": 0, "height": 64, "spots": {"A": [[0, 0], [2, 0], [2, 2]]}},
        {"width": 64, "height": 64, "spots": {"A": [[0, 0], [80, 0], [2, 2]]}},
        {"width": 64, "height": 64, "spots": {"A": [[0, 0], [1, 1], [2, 2]]}},
        {"width": 64, "height": 64, "spots": {}},
    ],
)
def test_invalid_mapping_is_rejected(profile, mapping):
    (profile[0].parent / "mapping.json").write_text(json.dumps(mapping))
    with pytest.raises(ValueError):
        load(profile[0])


def test_rejects_duplicate_json_keys(profile):
    profile[0].write_text('{"version":1,"version":1}')
    with pytest.raises(ValueError):
        load(profile[0])


def test_production_rejects_file_source(profile):
    path = profile[0].parent / "installation.json"
    descriptor = json.loads(path.read_text())
    descriptor["environment"] = "production"
    path.write_text(json.dumps(descriptor))
    with pytest.raises(ValueError, match="production"):
        load(profile[0])


def test_watchdog_must_allow_sampling_interval_and_processing_age(profile):
    profile[1]["inputs"][0]["fps"] = 0.1
    profile[1]["stall_timeout"] = 5
    with pytest.raises(ValueError, match="sampling"):
        load(save(profile))


def test_process_group_defaults_to_isolated_inputs(profile):
    assert load(profile[0]).inputs_per_process == 1


@pytest.mark.parametrize("size", [1, 2, 3, 4])
def test_process_group_size_is_explicit(profile, size):
    profile[1]["inputs_per_process"] = size
    assert load(save(profile)).inputs_per_process == size


@pytest.mark.parametrize("size", [0, 5, True, 2.0, "2", None])
def test_invalid_process_group_size_is_rejected(profile, size):
    profile[1]["inputs_per_process"] = size
    with pytest.raises(ValueError):
        load(save(profile))


def test_detector_calibration_is_explicit_and_per_profile(profile):
    profile[1]["detectors"]["main"].update(
        confidence=0.45, gamma=1.0, tile_size=512, tile_overlap=0.25
    )
    detector = load(save(profile)).inputs[0].detector
    assert (
        detector.confidence,
        detector.gamma,
        detector.tile_size,
        detector.tile_overlap,
    ) == (0.45, 1.0, 512, 0.25)


@pytest.mark.parametrize(
    "key,value",
    [
        ("confidence", True),
        ("confidence", float("nan")),
        ("confidence", 1.1),
        ("gamma", 0),
        ("tile_size", 512.5),
        ("tile_size", True),
        ("tile_size", 128),
        ("tile_overlap", 0.9),
        ("tile_scope", "everything"),
        ("tile_scope", "spots"),
        ("provider", "openvino"),
        ("provider", "tensorrt"),
        ("precision", "fp16"),
        ("precision", "int8"),
        ("engine_cache", "missing-engines"),
    ],
)
def test_invalid_detector_calibration_rejected(profile, key, value):
    profile[1]["detectors"]["main"][key] = value
    with pytest.raises(ValueError):
        load(save(profile))


def test_spot_tile_scope_is_explicit_and_requires_tiling(profile):
    assert load(save(profile)).inputs[0].detector.tile_scope == "frame"
    profile[1]["detectors"]["main"].update(tile_size=512, tile_scope="spots")
    assert load(save(profile)).inputs[0].detector.tile_scope == "spots"


def test_tensorrt_profile_requires_cuda_and_engine_cache(profile):
    path, data = profile
    base = path.parent
    detector = load(save(profile)).inputs[0].detector
    assert (detector.provider, detector.precision, detector.engine_cache) == (
        "default",
        "fp32",
        None,
    )
    (base / "engines").mkdir()
    data["detectors"]["main"].update(
        device="cuda", provider="tensorrt", precision="fp16", engine_cache="engines"
    )
    detector = load(save(profile)).inputs[0].detector
    assert (detector.provider, detector.precision) == ("tensorrt", "fp16")
    assert detector.engine_cache == (base / "engines").resolve()
    data["detectors"]["main"]["device"] = "cpu"
    with pytest.raises(ValueError):
        load(save(profile))


@pytest.mark.parametrize(
    "url",
    [
        "ws://10.0.0.5:8001/ws/edge",
        "ws://0.0.0.0:8001/ws/edge",
        "ws://gateway.internal:8001/ws/edge",
        "http://127.0.0.1:8001/ws/edge",
    ],
)
def test_plain_gateway_url_must_stay_on_this_host(profile, url):
    profile[1]["gateway_url"] = url
    with pytest.raises(ValueError, match="gateway URL"):
        load(save(profile))


@pytest.mark.parametrize(
    "url",
    [
        "ws://localhost:8001/ws/edge",
        "ws://[::1]:8001/ws/edge",
        "ws://127.0.0.2:8001/ws/edge",
        "wss://gateway.example:8443/ws/edge",
    ],
)
def test_loopback_or_tls_gateway_urls_are_accepted(profile, url):
    profile[1]["gateway_url"] = url
    assert load(save(profile)).gateway_url == url


@pytest.mark.parametrize("sha256", ["AB" * 32, "ab" * 31, 42, "zz" * 32])
def test_detector_digest_must_be_lowercase_sha256(profile, sha256):
    profile[1]["detectors"]["main"]["sha256"] = sha256
    with pytest.raises(ValueError, match="sha256"):
        load(save(profile))


def test_detector_digest_is_optional_and_kept(profile):
    assert load(profile[0]).inputs[0].detector.sha256 is None
    profile[1]["detectors"]["main"]["sha256"] = "ab" * 32
    assert load(save(profile)).inputs[0].detector.sha256 == "ab" * 32
