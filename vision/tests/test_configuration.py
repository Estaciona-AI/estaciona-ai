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
    descriptor = json.loads((profile[0].parent / "installation.json").read_text())
    organization = descriptor["organization"]["id"]
    installation = descriptor["installation"]["id"]
    assert (config.organization_id, config.installation_id) == (
        organization,
        installation,
    )
    assert config.scope == f"{organization}/{installation}/test"
    assert config.credential_env == "TEST_EDGE_KEY"
    assert (item.mapping.height, item.detector.device) == (64, "cpu")
    assert (item.source.kind, item.source.loop) == ("file", True)


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
    with pytest.raises(ValueError, match="duplicate JSON key"):
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


def write_mapping(profile, mapping, name="mapping.json"):
    (profile[0].parent / name).write_text(json.dumps(mapping))


def triangle(x=0):
    return [[x, 0], [x + 2, 0], [x, 2]]


def test_omitted_limits_and_calibration_use_documented_defaults(profile):
    del profile[1]["sources"]["video"]["loop"]
    config = load(save(profile))
    detector = config.inputs[0].detector
    assert (config.max_frame_age, config.stall_timeout, config.startup_timeout) == (
        2.0,
        30.0,
        120.0,
    )
    assert config.inputs[0].source.loop is False
    assert (detector.confidence, detector.gamma, detector.tile_overlap) == (
        0.1,
        1.4,
        0.25,
    )
    assert (detector.tile_size, detector.tile_scope) == (0, "frame")


@pytest.mark.parametrize(
    "key,accepted,refused",
    [
        ("max_frame_age", 0.1, 30.5),
        ("startup_timeout", 10, 601),
        ("startup_timeout", 600, 9.9),
        ("stall_timeout", 300, 300.5),
    ],
)
def test_runtime_limits_accept_their_bounds(profile, key, accepted, refused):
    profile[1][key] = accepted
    assert getattr(load(save(profile)), key) == accepted
    profile[1][key] = refused
    with pytest.raises(ValueError, match="numeric limit"):
        load(save(profile))


def test_frame_age_upper_bound_is_accepted(profile):
    profile[1].update(max_frame_age=30, stall_timeout=300)
    assert load(save(profile)).max_frame_age == 30


@pytest.mark.parametrize(
    "key,accepted,refused",
    [("gamma", 4, 4.01), ("confidence", 0.01, 0.009), ("tile_overlap", 0.5, 0.51)],
)
def test_detector_calibration_accepts_its_bounds(profile, key, accepted, refused):
    profile[1]["detectors"]["main"][key] = accepted
    assert getattr(load(save(profile)).inputs[0].detector, key) == accepted
    profile[1]["detectors"]["main"][key] = refused
    with pytest.raises(ValueError, match="numeric limit"):
        load(save(profile))


@pytest.mark.parametrize(
    "size,valid", [(256, True), (2048, True), (255, False), (2049, False)]
)
def test_tile_size_bounds(profile, size, valid):
    profile[1]["detectors"]["main"]["tile_size"] = size
    if valid:
        assert load(save(profile)).inputs[0].detector.tile_size == size
    else:
        with pytest.raises(ValueError, match="tile size"):
            load(save(profile))


def test_watchdog_boundary_is_exclusive(profile):
    # fps 0.2 samples every 5 s; with a 2 s frame age the stall must exceed 7 s.
    profile[1]["inputs"][0]["fps"] = 0.2
    profile[1]["stall_timeout"] = 7
    with pytest.raises(ValueError, match="sampling"):
        load(save(profile))
    profile[1]["stall_timeout"] = 7.01
    assert load(save(profile)).stall_timeout == 7.01
    profile[1]["inputs"][0]["fps"] = 30
    assert load(save(profile)).inputs[0].fps == 30


def test_demo_environment_is_valid(profile):
    path = profile[0].parent / "installation.json"
    descriptor = json.loads(path.read_text())
    descriptor["environment"] = "demo"
    path.write_text(json.dumps(descriptor))
    assert load(profile[0]).scope.endswith("/demo")


@pytest.mark.parametrize("version", [True, 2, "1", 1.0])
def test_runtime_version_must_be_integer_one(profile, version):
    profile[1]["version"] = version
    with pytest.raises(ValueError, match="runtime version"):
        load(save(profile))


@pytest.mark.parametrize("version", [True, 2, "1"])
def test_manifest_schema_version_must_be_integer_one(profile, version):
    path = profile[0].parent / "installation.json"
    descriptor = json.loads(path.read_text())
    descriptor["schema_version"] = version
    path.write_text(json.dumps(descriptor))
    with pytest.raises(ValueError, match="installation manifest"):
        load(profile[0])


def test_manifest_size_limit_is_16_kib(profile):
    path = profile[0].parent / "installation.json"
    descriptor = json.dumps(json.loads(path.read_text()))
    path.write_text(descriptor + " " * (16 * 1024 - len(descriptor)))
    assert load(profile[0]).environment == "test"
    path.write_text(descriptor + " " * (16 * 1024 + 1 - len(descriptor)))
    with pytest.raises(ValueError, match="too large"):
        load(profile[0])


def test_gateway_port_zero_is_refused(profile):
    profile[1]["gateway_url"] = "ws://127.0.0.1:0/ws/edge"
    with pytest.raises(ValueError, match="gateway URL"):
        load(save(profile))


@pytest.mark.parametrize("field", ["sources", "detectors"])
def test_profile_collections_must_be_objects(profile, field):
    profile[1][field] = list(profile[1][field].values())
    with pytest.raises(ValueError, match="profile collections"):
        load(save(profile))


@pytest.mark.parametrize("field", ["sources", "detectors"])
def test_profile_collections_hold_at_most_64(profile, field):
    entry = profile[1][field]["video" if field == "sources" else "main"]
    profile[1][field].update({f"extra-{i}": dict(entry) for i in range(63)})
    assert load(save(profile)).inputs[0].input_id == "cam-01"
    profile[1][field]["extra-63"] = dict(entry)
    with pytest.raises(ValueError, match="profile collections"):
        load(save(profile))


def test_at_most_64_inputs(profile):
    template = profile[1]["inputs"][0]
    profile[1]["inputs"] = []
    for i in range(65):
        write_mapping(
            profile,
            {"width": 64, "height": 64, "spots": {f"S{i}": triangle()}},
            f"m{i}.json",
        )
        profile[1]["inputs"].append(
            dict(template, input_id=f"cam-{i}", mapping=f"m{i}.json")
        )
    with pytest.raises(ValueError, match="1 to 64 inputs"):
        load(save(profile))
    profile[1]["inputs"].pop()
    assert len(load(save(profile)).inputs) == 64


def test_polygon_vertices_from_3_to_64(profile):
    many = [[i, 0] for i in range(62)] + [[61, 10], [0, 10]]
    write_mapping(
        profile, {"width": 64, "height": 64, "spots": {"T": triangle(), "M": many}}
    )
    spots = load(profile[0]).inputs[0].mapping.spots
    assert (len(spots["T"]), len(spots["M"])) == (3, 64)
    for points in (triangle()[:2], many + [[0, 5]]):
        write_mapping(profile, {"width": 64, "height": 64, "spots": {"X": points}})
        with pytest.raises(ValueError, match="invalid polygon"):
            load(profile[0])


@pytest.mark.parametrize("points", ["abc", {"a": 1, "b": 2, "c": 3}])
def test_polygon_must_be_a_list(profile, points):
    write_mapping(profile, {"width": 64, "height": 64, "spots": {"X": points}})
    with pytest.raises(ValueError, match="invalid polygon"):
        load(profile[0])


def test_polygon_points_stay_inside_the_reference_frame(profile):
    edge = [[0, 0], [63, 0], [63, 63]]
    write_mapping(profile, {"width": 64, "height": 64, "spots": {"E": edge}})
    assert load(profile[0]).inputs[0].mapping.spots["E"] == edge
    for outside in (
        [[0, 0], [64, 0], [63, 63]],
        [[0, 0], [63, 0], [63, 64]],
        [[0, 0], [2, 0], [0, -1]],
    ):
        write_mapping(profile, {"width": 64, "height": 64, "spots": {"E": outside}})
        with pytest.raises(ValueError, match="outside reference frame"):
            load(profile[0])


@pytest.mark.parametrize(
    "points",
    [
        [[0, 0], [2, 1], [4, 2]],
        [[0, 0], [4, 2], [2, 1], [6, 3]],
        [[5, 5], [5, 5], [5, 5]],
    ],
)
def test_degenerate_polygon_is_refused(profile, points):
    write_mapping(profile, {"width": 64, "height": 64, "spots": {"Z": points}})
    with pytest.raises(ValueError, match="zero-area"):
        load(profile[0])


@pytest.mark.parametrize(
    "width,height,valid",
    [
        (8192, 2048, True),
        (2048, 8192, True),
        (8193, 1, False),
        (1, 8193, False),
        (8192, 2049, False),
        (4097, 4096, False),
    ],
)
def test_mapping_dimensions_and_pixel_budget(profile, width, height, valid):
    write_mapping(
        profile, {"width": width, "height": height, "spots": {"A": triangle()}}
    )
    if valid:
        assert load(profile[0]).inputs[0].mapping.width == width
    else:
        with pytest.raises(ValueError, match="dimensions"):
            load(profile[0])


def test_spot_collection_holds_at_most_10000(profile):
    spots = {f"S{i}": triangle() for i in range(10000)}
    write_mapping(profile, {"width": 64, "height": 64, "spots": spots})
    assert len(load(profile[0]).inputs[0].mapping.spots) == 10000
    spots["S10000"] = triangle()
    write_mapping(profile, {"width": 64, "height": 64, "spots": spots})
    with pytest.raises(ValueError, match="spot collection"):
        load(profile[0])


def test_missing_required_field_is_refused(profile):
    del profile[1]["credential_env"]
    with pytest.raises(ValueError, match="missing or unknown"):
        load(save(profile))


def test_malformed_gateway_url_is_refused(profile):
    profile[1]["gateway_url"] = "ws://[::1/ws/edge"
    with pytest.raises(ValueError, match="gateway URL"):
        load(save(profile))


def test_fps_upper_bound(profile):
    profile[1]["inputs"][0]["fps"] = 30.5
    with pytest.raises(ValueError, match="numeric limit"):
        load(save(profile))


@pytest.mark.parametrize(
    "url,valid",
    [
        ("rtsps://camera.local/live", True),
        ("rtsp://camera.local:554/live", True),
        ("http://camera.local/live", False),
        ("rtsp:///live", False),
        ("rtsp://camera.local:0/live", False),
        ("rtsp://[::1/live", False),
        ("", False),
    ],
)
def test_rtsp_source_url_is_validated_when_resolved(profile, monkeypatch, url, valid):
    profile[1]["sources"]["video"] = {"kind": "rtsp", "env": "TEST_RTSP"}
    monkeypatch.setenv("TEST_RTSP", url)
    source = load(save(profile)).inputs[0].source
    assert (source.kind, source.env) == ("rtsp", "TEST_RTSP")
    if valid:
        assert source.resolve() == url
    else:
        with pytest.raises(ValueError, match="source environment"):
            source.resolve()


@pytest.mark.parametrize("dimensions", [(64.0, 64), (64, 64.0)])
def test_mapping_dimensions_must_be_integers(profile, dimensions):
    width, height = dimensions
    write_mapping(
        profile, {"width": width, "height": height, "spots": {"A": triangle()}}
    )
    with pytest.raises(ValueError, match="dimensions"):
        load(profile[0])


@pytest.mark.parametrize("point", [[1, 2, 3], [1.5, 2], [1]])
def test_polygon_points_are_integer_pairs(profile, point):
    write_mapping(
        profile, {"width": 64, "height": 64, "spots": {"A": triangle() + [point]}}
    )
    with pytest.raises(ValueError, match="outside reference frame"):
        load(profile[0])


@pytest.mark.parametrize(
    "name,limit", [("mapping.json", 2 * 1024 * 1024), ("runtime.json", 512 * 1024)]
)
def test_configuration_files_have_size_limits(profile, name, limit):
    path = profile[0].parent / name
    content = path.read_text()
    path.write_text(content + " " * (limit - len(content)))
    assert load(profile[0]).inputs[0].input_id == "cam-01"
    path.write_text(content + " " * (limit + 1 - len(content)))
    with pytest.raises(ValueError, match="too large"):
        load(profile[0])


@pytest.mark.parametrize(
    "suffix,valid",
    [
        ("/live stream", True),
        ("/live\nstream", False),
        ("/live\x1fstream", False),
        ("/" + "a" * (4096 - len("rtsp://camera.local/")), True),
        ("/" + "a" * (4097 - len("rtsp://camera.local/")), False),
    ],
)
def test_rtsp_source_value_has_no_control_characters_and_bounded_length(
    profile, monkeypatch, suffix, valid
):
    profile[1]["sources"]["video"] = {"kind": "rtsp", "env": "TEST_RTSP"}
    monkeypatch.setenv("TEST_RTSP", "rtsp://camera.local" + suffix)
    source = load(save(profile)).inputs[0].source
    if valid:
        assert source.resolve().endswith(suffix)
    else:
        with pytest.raises(ValueError, match="source environment"):
            source.resolve()


@pytest.mark.parametrize(
    "state_dir,valid", [("", False), ("a/" * 2048, True), ("a/" * 2048 + "b", False)]
)
def test_local_paths_are_non_empty_and_bounded(profile, state_dir, valid):
    profile[1]["state_dir"] = state_dir
    if valid:
        assert load(save(profile)).state_dir.name == "a"
    else:
        with pytest.raises(ValueError, match="invalid local path"):
            load(save(profile))


@pytest.mark.parametrize(
    "field,message",
    [("credential_env", "environment reference"), ("input_id", "spot ID")],
)
def test_references_must_be_strings(profile, field, message):
    if field == "input_id":
        profile[1]["inputs"][0]["input_id"] = 5
    else:
        profile[1][field] = 5
    with pytest.raises(ValueError, match=message):
        load(save(profile))


def test_input_ids_are_unique_even_with_distinct_mappings(profile):
    write_mapping(
        profile, {"width": 64, "height": 64, "spots": {"B": triangle()}}, "other.json"
    )
    profile[1]["inputs"].append(dict(profile[1]["inputs"][0], mapping="other.json"))
    with pytest.raises(ValueError, match="duplicate input ID"):
        load(save(profile))
