"""Versioned HTTP routes share sessions, limits and state with legacy consumers."""

import asyncio
import datetime
import json
import os
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from uuid import UUID, uuid4

import pytest
import websockets

from test_delivery_integration import server as delivery_server
from test_session_integration import insert_user, token

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"), reason="integration server required"
)


def call(server, path, method="GET", payload=None, bearer=None, headers=None):
    request_headers = dict(headers or {})
    if bearer:
        request_headers["Authorization"] = f"Bearer {bearer}"
    data = None
    if payload is not None:
        data = json.dumps(payload).encode()
        request_headers["Content-Type"] = "application/json"
    request = Request(
        f"http://{server.host}:8000{path}",
        method=method,
        data=data,
        headers=request_headers,
    )
    try:
        response = urlopen(request, timeout=7)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.headers, response.read()


def test_discovery_exposes_actual_capabilities_and_immutable_schemas(server):
    status, headers, body = call(server, "/api/v1/capabilities")
    assert status == 200
    capabilities = json.loads(body)
    assert capabilities["tenancy_mode"] == "single_installation"
    assert capabilities["authentication"]["sso"] is False
    assert capabilities["authentication"]["browser_websocket"] == "single_use_ticket"
    assert headers["X-Estaciona-Api-Version"] == "1"
    UUID(headers["X-Request-ID"])
    status, _, body = call(server, capabilities["paths"]["contract_manifest"])
    assert status == 200
    manifest = json.loads(body)
    assert manifest["version"] == capabilities["contract_version"]
    for item in manifest["rest"]["schemas"].values():
        status, _, body = call(server, f"/api/v1/contracts/{item['path']}")
        assert status == 200
        assert json.loads(body)["$id"] == item["id"]
    for path in ["v1/fixtures.json", "v1/unknown.schema.json", "../Cargo.toml"]:
        status, _, _ = call(server, f"/api/v1/contracts/{path}")
        assert status == 404


def test_versioned_errors_preserve_legacy_responses_and_method_headers(server):
    status, headers, body = call(
        server, "/api/v1/reservations", headers={"X-Request-ID": "untrusted"}
    )
    assert status == 401
    error = json.loads(body)
    assert error["code"] == "UNAUTHORIZED"
    assert error["request_id"] == headers["X-Request-ID"]
    UUID(error["request_id"])
    status, headers, body = call(server, "/reservations")
    assert status == 401
    assert not headers.get("X-Estaciona-Api-Version")
    assert headers["Content-Type"].startswith("text/plain")
    for path, method, expected in [
        ("/api/v1/missing", "GET", "NOT_FOUND"),
        ("/api/v1/login", "GET", "METHOD_NOT_ALLOWED"),
    ]:
        status, headers, body = call(server, path, method)
        assert json.loads(body)["code"] == expected
        if status == 405:
            assert "POST" in headers["Allow"]


def test_versioned_registration_login_reservation_and_logout_share_state(server):
    email = f"{uuid4()}@example.invalid"
    status, _, body = call(
        server,
        "/api/v1/users",
        "POST",
        {
            "name": "Contract fixture",
            "email": email,
            "password": "disposable-password",
        },
    )
    assert status in (200, 201), body
    user = json.loads(body)
    status, headers, body = call(
        server,
        "/api/v1/login",
        "POST",
        {
            "email": email,
            "password": "disposable-password",
        },
    )
    assert status == 200, body
    login = json.loads(body)
    assert login["id"] == user["id"]
    assert "HttpOnly" in headers["Set-Cookie"]
    bearer = login["token"]
    status, _, body = call(
        server, "/api/v1/reservations", "POST", {"spot_id": "R02-A"}, bearer
    )
    assert status == 200, body
    reservation = json.loads(body)
    assert reservation["user_id"] == user["id"]
    created = datetime.datetime.fromisoformat(reservation["created_at"])
    expires = datetime.datetime.fromisoformat(reservation["expires_at"])
    assert (expires - created).total_seconds() == pytest.approx(900, abs=2)
    status, _, body = call(server, "/reservations", bearer=bearer)
    assert status == 403  # Listing all reservations remains administrator-only.
    assert (
        server.query(
            "SELECT user_id FROM reservations WHERE id=%s", (reservation["id"],)
        )[0][0]
        == user["id"]
    )
    assert call(server, f"/users/{user['id']}", bearer=bearer)[0] == 200
    assert call(server, "/api/v1/auth/logout", "POST", bearer=bearer)[0] in (200, 204)
    for path in ["/reservations", "/api/v1/reservations"]:
        assert call(server, path, bearer=bearer)[0] == 401


def test_versioned_ticket_uses_existing_websocket_and_role_checks(server):
    user_id = insert_user(server)
    bearer = token(server, user_id, "user", int(time.time()) + 60)
    origin = {"Origin": "http://localhost"}
    status, _, body = call(
        server,
        "/api/v1/auth/ws-ticket",
        "POST",
        {"destination": "dashboard"},
        bearer,
        origin,
    )
    assert status == 403
    status, _, body = call(
        server, "/api/v1/auth/ws-ticket", "POST", {"destination": "app"}, bearer, origin
    )
    assert status == 201, body
    ticket = json.loads(body)["ticket"]

    async def connect():
        async with websockets.connect(
            f"ws://{server.host}:8000/ws/app?ticket={ticket}", origin="http://localhost"
        ) as socket:
            await socket.send(json.dumps({"type": "RESERVE_SPOT", "spot_id": "R02-A"}))
            while True:
                message = json.loads(await asyncio.wait_for(socket.recv(), 3))
                if message["type"] == "RESERVATION_CONFIRMED":
                    assert message["spot_id"] == "R02-A"
                    return

    asyncio.run(connect())


def test_auth_rate_budget_is_shared_between_versioned_and_legacy_routes(server):
    # Malformed but bounded inputs consume the same login bucket, without hashing.
    statuses = []
    for index in range(16):
        path = "/login" if index % 2 else "/api/v1/login"
        status, headers, body = call(
            server, path, "POST", {"email": "", "password": ""}
        )
        statuses.append(status)
        if status == 429:
            assert headers["Retry-After"] == "1"
            if path.startswith("/api/v1"):
                assert json.loads(body)["code"] == "TOO_MANY_REQUESTS"
    assert 429 in statuses


def test_versioned_extractors_limits_and_cors_use_the_public_error_contract(server):
    for path, payload, expected in [
        ("/api/v1/login", {"email": "x", "password": "a" * 17000}, 413),
        ("/api/v1/reservations", {"spot_id": "R02-A", "expires_at": "tomorrow"}, 422),
    ]:
        status, headers, body = call(
            server, path, "POST", payload, headers={"Origin": "http://localhost"}
        )
        assert status == expected
        error = json.loads(body)
        assert error["request_id"] == headers["X-Request-ID"]
        assert "x-request-id" in headers["Access-Control-Expose-Headers"].lower()
    assert call(server, "/api/v1/users/not-a-uuid")[0] == 400
