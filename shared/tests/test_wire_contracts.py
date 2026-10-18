"""Cross-language artifacts must be useful schemas, not a dictionary of unchecked definitions."""

import json
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator, FormatChecker
from referencing import Registry, Resource

ROOT = Path(__file__).resolve().parents[1]


def registry():
    documents = [
        json.loads(path.read_text()) for path in (ROOT / "v1").glob("*.schema.json")
    ]
    return Registry().with_resources(
        (doc["$id"], Resource.from_contents(doc)) for doc in documents
    )


def validator(name):
    schema = json.loads((ROOT / "v1" / f"{name}.schema.json").read_text())
    Draft202012Validator.check_schema(schema)
    return Draft202012Validator(
        schema, registry=registry(), format_checker=FormatChecker()
    )


def test_root_rejects_non_messages():
    schema = json.loads((ROOT / "messages.json").read_text())
    check = Draft202012Validator(
        schema, registry=registry(), format_checker=FormatChecker()
    )
    for value in [{}, 42, None, {"hello": "world"}, {"type": "NOT_A_MESSAGE"}]:
        assert not check.is_valid(value)


@pytest.mark.parametrize(
    "name",
    [
        "app-to-server",
        "server-to-app",
        "gateway-to-server",
        "server-to-gateway",
        "vision-to-gateway",
        "gateway-to-vision",
    ],
)
def test_channels_reject_empty_messages(name):
    assert not validator(name).is_valid({})


def test_reserve_command_does_not_require_caller_identity_or_plate():
    validator("app-to-server").validate({"type": "RESERVE_SPOT", "spot_id": "A-01"})


def test_forecast_and_navigation_unavailable_are_present():
    fixtures = json.loads((ROOT / "v1/fixtures.json").read_text())
    messages = fixtures["server-to-app"]
    assert {msg["type"] for msg in messages} >= {
        "TREND_PREDICTION",
        "NAVIGATION_UNAVAILABLE",
    }
    for message in messages:
        validator("server-to-app").validate(message)


def test_all_serialized_rust_fixtures_obey_their_channel():
    fixtures = json.loads((ROOT / "v1/fixtures.json").read_text())
    for channel, messages in fixtures.items():
        check = validator(channel)
        for message in messages:
            check.validate(message)


@pytest.mark.parametrize(
    "channel",
    [
        "gateway-to-server",
        "server-to-gateway",
        "vision-to-gateway",
        "gateway-to-vision",
    ],
)
def test_envelope_version_must_be_integer_one(channel):
    fixtures = json.loads((ROOT / "v1/fixtures.json").read_text())
    for message in fixtures[channel]:
        field = "delivery_version" if "delivery_version" in message else "version"
        for invalid in [0, 2, True, "1"]:
            assert not validator(channel).is_valid({**message, field: invalid})


def test_invalid_confidence_and_dates_are_rejected():
    fixtures = json.loads((ROOT / "v1/fixtures.json").read_text())
    sample = fixtures["edge-events"][0]
    for invalid in [-0.01, 1.01, "0.8", None]:
        assert not validator("edge-events").is_valid({**sample, "confidence": invalid})
    assert not validator("edge-events").is_valid({**sample, "timestamp": "yesterday"})
    envelope = fixtures["gateway-to-server"][0]
    assert not validator("gateway-to-server").is_valid(
        {**envelope, "event_id": "not-a-uuid"}
    )


def test_directions_allow_additive_metadata_and_keep_legacy_refs():
    fixtures = json.loads((ROOT / "v1/fixtures.json").read_text())
    for channel, messages in fixtures.items():
        for message in messages:
            validator(channel).validate({**message, "future_metadata": {"sample": 1}})
    root = json.loads((ROOT / "messages.json").read_text())
    assert "EdgeToServer_SPOT_UPDATE" in root["$defs"]
    assert "ServerToApp_RESERVATION_REJECTED" in root["$defs"]
    check = Draft202012Validator(
        {"$ref": root["$id"] + "#/$defs/EdgeToServer_SPOT_UPDATE"},
        registry=registry().with_resource(root["$id"], Resource.from_contents(root)),
    )
    check.validate(fixtures["edge-events"][0])


def load_module(name, path):
    import importlib.util

    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_actual_python_outbox_and_gateway_encoder_match_rust_contracts(tmp_path):
    delivery = load_module(
        "contract_vision_delivery", ROOT.parent / "vision/src/delivery.py"
    )
    protocol = load_module(
        "contract_gateway_protocol", ROOT.parent / "gateway/protocol.py"
    )
    fixture = json.loads((ROOT / "v1/fixtures.json").read_text())["edge-events"][0]
    outbox = delivery.LocalOutbox(tmp_path / "vision.db")
    event_id = outbox.enqueue(json.dumps(fixture))
    local = outbox.first()
    validator("vision-to-gateway").validate(local)
    cloud = json.loads(
        protocol.encode_edge_event(event_id, local["payload"], "2026-10-01T12:00:01Z")
    )
    validator("gateway-to-server").validate(cloud)
    assert cloud["event_id"] == local["event_id"]
    assert cloud["payload"] == local["payload"]
    with pytest.raises(ValueError):
        protocol.encode_edge_event("invalid", fixture, "now")
    with pytest.raises(ValueError):
        protocol.encode_edge_event(event_id, {"confidence": float("nan")}, "now")
