# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Consumers can discover operations without depending on a client repository."""

import json
from pathlib import Path

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[1]


def catalog():
    manifest = json.loads((ROOT / "v1/manifest.json").read_text())
    assert manifest["version"] == "1.4.0"
    assert manifest["wire_version"] == 1
    return json.loads((ROOT / manifest["catalog"]).read_text())


def test_catalog_covers_current_rest_operations_and_conditional_lpr():
    operations = catalog()["http"]
    assert len({item["id"] for item in operations}) == len(operations)
    assert len({(item["method"], item["path"]) for item in operations}) == len(
        operations
    )
    expected = {
        ("GET", "/health"),
        ("GET", "/ready"),
        ("GET", "/api/v1/capabilities"),
        ("GET", "/api/v1/contracts/{path}"),
        ("GET", "/api/v1/me"),
        ("PUT", "/api/v1/me/eligibility"),
        ("GET", "/api/v1/me/export"),
        ("DELETE", "/api/v1/me"),
        ("GET", "/api/v1/users/{id}"),
        ("POST", "/api/v1/auth/logout"),
        ("POST", "/api/v1/auth/ws-ticket"),
        ("POST", "/api/v1/reservations"),
        ("GET", "/api/v1/reservations"),
        ("PUT", "/api/v1/reservations/{id}/cancel"),
        ("POST", "/api/v1/reservations/{id}/confirm"),
        ("PUT", "/api/v1/reservations/{id}/extend"),
        ("GET", "/api/v1/reservations/recommend"),
        ("PUT", "/api/v1/spots/{id}/status"),
        ("POST", "/api/v1/config"),
        ("GET", "/api/v1/admin/vehicle-bindings"),
        ("POST", "/api/v1/admin/vehicle-bindings"),
        ("POST", "/api/v1/admin/vehicle-bindings/{id}/approve"),
        ("POST", "/api/v1/admin/vehicle-bindings/{id}/revoke"),
    }
    assert {(item["method"], item["path"]) for item in operations} == expected
    for item in operations:
        assert item["authentication"] in catalog()["authentication"]
        assert item["authorization"]
        assert item["responses"]
        assert item["request_encoding"] in {"none", "json"}
        if "/admin/vehicle-bindings" in item["path"]:
            assert item["availability"] == "lpr_enabled"
        else:
            assert item["availability"] == "always"
        if item["path"].startswith("/api/v1/"):
            assert item["error_schema"] == "v1/rest-api-error.schema.json"


def test_catalog_schema_references_resolve_and_formats_are_explicit():
    def visit(value):
        if isinstance(value, dict):
            for key, child in value.items():
                if key in {"schema", "error_schema", "request_schema"}:
                    if child is not None:
                        path = ROOT / child
                        assert path.is_file(), child
                        Draft202012Validator.check_schema(json.loads(path.read_text()))
                else:
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    document = catalog()
    visit(document)
    for operation in document["http"]:
        for response in operation["responses"].values():
            assert response["encoding"] in {"json", "text", "empty"}
            assert "schema" in response
    assert document["scope"] == "single_installation_per_process_and_database"
    assert document["installation_configuration"]["source"] == "operator_package"
    assert document["installation_configuration"]["public_endpoint"] is None


def test_websocket_catalog_keeps_edge_and_browser_authority_separate():
    sockets = {item["id"]: item for item in catalog()["websockets"]}
    assert set(sockets) == {"app", "dashboard", "edge"}
    assert sockets["app"]["authentication"] == "single_use_ticket"
    assert sockets["dashboard"]["authorization"] == "admin"
    assert sockets["dashboard"]["receive"] is None
    assert sockets["edge"]["authentication"] == "individual_gateway_bearer"
    assert sockets["edge"]["authorization"] == "explicit_gateway_grants"
    assert sockets["edge"]["receive"]["schema"] == "v1/gateway-to-server.schema.json"
    assert sockets["app"]["receive"]["schema"] == "v1/app-to-server.schema.json"
    local = catalog()["local_transport"]
    assert local["authentication"] == "local_shared_key"
    assert local["cloud_credential_reused"] is True
