# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import json
from pathlib import Path

import pytest
from jsonschema import ValidationError
from referencing.exceptions import Unresolvable

from contract_validation import ContractValidator

ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture
def check():
    return ContractValidator(
        {
            str(path.relative_to(ROOT)): json.loads(path.read_text())
            for path in (ROOT / "v1").glob("*.schema.json")
        }
    )


def test_required_fields_and_uuid_formats_are_enforced(check):
    path = "v1/rest-api-error.schema.json"
    valid = {
        "code": "UNAUTHORIZED",
        "message": "Unauthorized",
        "request_id": "00000000-0000-4000-8000-000000000001",
    }
    check.validate(path, valid)
    for invalid in [{"code": "UNAUTHORIZED"}, {**valid, "request_id": "invalid"}]:
        with pytest.raises(ValidationError):
            check.validate(path, invalid)


def test_collections_validate_the_container_and_each_item(check):
    path = "v1/rest-reservation-response.schema.json"
    check.validate(path, [], collection=True)
    with pytest.raises(ValidationError):
        check.validate(path, {}, collection=True)
    with pytest.raises(ValidationError):
        check.validate(path, [{}], collection=True)


def test_cross_document_references_resolve_without_network():
    target = {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "urn:test:identity",
        "type": "string",
        "format": "uuid",
    }
    envelope = {
        "$schema": target["$schema"],
        "$id": "urn:test:envelope",
        "type": "object",
        "properties": {"id": {"$ref": target["$id"]}},
        "required": ["id"],
    }
    check = ContractValidator({"identity": target, "envelope": envelope})
    check.validate("envelope", {"id": "00000000-0000-4000-8000-000000000001"})
    with pytest.raises(ValidationError):
        check.validate("envelope", {"id": "not-a-uuid"})
    with pytest.raises(Unresolvable):
        ContractValidator({"envelope": envelope}).validate("envelope", {"id": "x"})


def test_wire_timestamps_and_message_shapes_are_enforced(check):
    message = {
        "type": "SPOT_UPDATE",
        "spot_id": "test-01",
        "camera_id": "test-camera",
        "status": "occupied",
        "confidence": 0.9,
        "timestamp": "2026-10-20T12:00:00Z",
    }
    path = "v1/edge-events.schema.json"
    check.validate(path, {**message, "additive_metadata": True})
    for invalid in [
        {**message, "timestamp": "yesterday"},
        {**message, "confidence": "high"},
        {"type": "UNKNOWN"},
    ]:
        with pytest.raises(ValidationError):
            check.validate(path, invalid)
