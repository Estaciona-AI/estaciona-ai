# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Synthetic consumers discover a real core; no customer panel or ML is mocked in."""

import json
import os
from urllib.parse import urlencode
from uuid import UUID, uuid4

import pytest

from test_delivery_integration import server as delivery_server
from test_rest_contract_integration import call

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"), reason="integration server required"
)


class Consumer:
    """Test-only client bootstrapped from the existing public discovery endpoint."""

    def __init__(self, server):
        self.server = server
        status, _, raw = call(server, "/api/v1/capabilities")
        assert status == 200
        self.capabilities = json.loads(raw)
        self.manifest = self.get_json(self.capabilities["paths"]["contract_manifest"])
        self.catalog = self.artifact(self.manifest["catalog"])
        assert self.manifest["version"] == self.catalog["version"]
        assert self.catalog["version"] == self.capabilities["contract_version"]
        self.operations = {item["id"]: item for item in self.catalog["http"]}
        self.sockets = {item["id"]: item for item in self.catalog["websockets"]}

    def get_json(self, path):
        status, _, body = call(self.server, path)
        assert status == 200, body
        return json.loads(body)

    def artifact(self, path):
        return self.get_json(f"/api/v1/contracts/{path}")

    def request(
        self, operation, *, payload=None, bearer=None, parameters=None, query=None
    ):
        operation = self.operations[operation]
        path = operation["path"].format(**(parameters or {}))
        if query:
            path += "?" + urlencode(query)
        result = call(
            self.server,
            path,
            operation["method"],
            payload,
            bearer,
            {"Origin": "http://localhost"},
        )
        status, headers, raw = result
        if 200 <= status < 300:
            response = operation["responses"][str(status)]
            if response["encoding"] == "empty":
                assert raw == b""
            elif response["encoding"] == "json":
                assert headers["Content-Type"].startswith("application/json")
                json.loads(raw)
            else:
                assert headers["Content-Type"].startswith("text/plain")
        return result

    def register_and_login(self):
        credentials = {
            "email": f"{uuid4()}@example.invalid",
            "password": "disposable-consumer-password",
        }
        status, _, raw = self.request(
            "register", payload={"name": "Synthetic consumer", **credentials}
        )
        assert status == 201, raw
        user = json.loads(raw)
        status, headers, raw = self.request("login", payload=credentials)
        assert status == 200, raw
        login = json.loads(raw)
        assert user["id"] == login["id"]
        assert "HttpOnly" in headers["Set-Cookie"]
        return user["id"], login["token"]


@pytest.fixture
def consumer(server):
    return Consumer(server)


def assert_error(status, headers, raw, expected_status, expected_code):
    assert status == expected_status, raw
    error = json.loads(raw)
    assert error["code"] == expected_code
    assert error["request_id"] == headers["X-Request-ID"]
    UUID(error["request_id"])
    assert headers["X-Estaciona-Api-Version"] == "1"


def test_consumer_resolves_all_advertised_schemas_from_live_server(consumer):
    paths = set()

    def collect(value):
        if isinstance(value, dict):
            for key, child in value.items():
                if key in {"schema", "request_schema", "error_schema"} and child:
                    paths.add(child)
                else:
                    collect(child)
        elif isinstance(value, list):
            for child in value:
                collect(child)

    collect(consumer.catalog)
    for path in paths:
        schema = consumer.artifact(path)
        assert schema["$schema"] == "https://json-schema.org/draft/2020-12/schema"
        assert schema["x-contract-version"] == consumer.manifest["version"]
    assert len(paths) >= 15
    assert consumer.capabilities["authentication"]["sso"] is False


def test_catalog_protected_operations_reject_anonymous_consumers(consumer):
    for name, operation in consumer.operations.items():
        if (
            operation["availability"] != "always"
            or operation["authentication"] == "public"
        ):
            continue
        payload = {
            "websocket_ticket": {"destination": "app"},
            "create_reservation": {"spot_id": "R02-A"},
            "update_spot_status": {"status": "free"},
            "save_config": [],
        }.get(name)
        result = consumer.request(
            name,
            payload=payload,
            parameters={"id": str(uuid4())},
            query={"user_id": str(uuid4())} if name == "recommend_spot" else None,
        )
        assert_error(*result, 401, "UNAUTHORIZED")
    for name, operation in consumer.operations.items():
        if operation["availability"] == "lpr_enabled":
            assert (
                consumer.request(
                    name,
                    parameters={"id": str(uuid4())},
                    payload={} if operation["method"] == "POST" else None,
                )[0]
                == 404
            )


def test_browser_consumer_login_reservation_ownership_and_logout(consumer):
    user_id, bearer = consumer.register_and_login()
    _, other_bearer = consumer.register_and_login()
    status, _, raw = consumer.request(
        "create_reservation", payload={"spot_id": "R02-A"}, bearer=bearer
    )
    assert status == 200, raw
    reservation = json.loads(raw)
    assert reservation["user_id"] == user_id
    assert reservation["status"] == "active"
    # A valid session belonging to another user must not manage the reservation.
    assert_error(
        *consumer.request(
            "cancel_reservation",
            bearer=other_bearer,
            parameters={"id": reservation["id"]},
        ),
        404,
        "NOT_FOUND",
    )
    assert_error(
        *consumer.request("list_reservations", bearer=bearer), 403, "FORBIDDEN"
    )
    status, headers, raw = consumer.request(
        "cancel_reservation", bearer=bearer, parameters={"id": reservation["id"]}
    )
    assert status == 200, raw
    assert headers["Content-Type"].startswith("text/plain")
    assert raw.decode() == "Reservation cancelled."
    assert consumer.server.query(
        "SELECT status FROM reservations WHERE id=%s", (reservation["id"],)
    ) == [("cancelled",)]
    status, _, raw = consumer.request("logout", bearer=bearer)
    assert status == 204 and raw == b""
    assert_error(
        *consumer.request("get_user", bearer=bearer, parameters={"id": user_id}),
        401,
        "UNAUTHORIZED",
    )
