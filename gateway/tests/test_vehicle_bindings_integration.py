# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Optional LPR must be inert by default and require explicit vehicle approval."""

import asyncio
import json
import os
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen
from uuid import uuid4

import pytest
import websockets

from test_delivery_integration import KEY, message, receive_ready
from test_delivery_integration import server as delivery_server
from test_session_integration import enable_lpr, insert_user, token

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)


server = delivery_server


def request(server, path, signed=None, *, method="GET", payload=None):
    headers = {}
    if signed:
        headers["Authorization"] = f"Bearer {signed}"
    data = None
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    req = Request(
        f"http://{server.host}:8000{path}", headers=headers, method=method, data=data
    )
    try:
        response = urlopen(req, timeout=6)
    except HTTPError as error:
        response = error
    with response:
        raw = response.read().decode()
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            value = raw
        return response.status if hasattr(response, "status") else response.code, value


def signed(server, role="admin"):
    identity = insert_user(server, role=role)
    return identity, token(server, identity, role, int(time.time()) + 120)


def declaration(server, signed_token, user):
    status, binding = request(
        server,
        "/admin/vehicle-bindings",
        signed_token,
        method="POST",
        payload={"user_id": str(user), "plate": "ABC-1D23"},
    )
    assert status == 201, binding
    return binding


def test_disabled_lpr_cannot_affect_occupancy_or_accounts(server):
    _, admin = signed(server)
    assert request(server, "/admin/vehicle-bindings", admin)[0] == 404
    server.query("ALTER TABLE vehicles RENAME TO unavailable_optional_vehicles")
    status, me = request(server, "/me", signed(server, "user")[1])
    assert status == 200, me

    async def run():
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as edge:
            await receive_ready(edge)
            await edge.send(
                json.dumps(
                    {
                        "type": "CAR_DETECTED",
                        "plate": "UNVERIFIED",
                        "camera_id": "ungranted-camera",
                        "timestamp": "2026-10-17T12:00:00Z",
                    }
                )
            )
            event = message(str(uuid4()))
            await edge.send(json.dumps(event))
            ack = json.loads(await asyncio.wait_for(edge.recv(), timeout=3))
            assert ack["type"] == "EDGE_ACK" and ack["outcome"] == "applied"
            assert ack["event_id"] == event["event_id"]

    asyncio.run(run())
    assert server.query("SELECT status FROM spots WHERE id='R02-A'")[0][0] == "occupied"
    assert server.query("SELECT COUNT(*) FROM reservations")[0][0] == 0


def test_admin_approval_recovery_revocation_and_bounded_routes(server):
    enable_lpr(server)
    admin_id, admin = signed(server)
    _, user_token = signed(server, "user")
    first = insert_user(server)
    second = insert_user(server)
    body = {"user_id": str(first), "plate": "ABC1D23"}
    assert (
        request(server, "/admin/vehicle-bindings", method="POST", payload=body)[0]
        == 401
    )
    assert (
        request(
            server, "/admin/vehicle-bindings", user_token, method="POST", payload=body
        )[0]
        == 403
    )
    assert (
        request(
            server,
            "/admin/vehicle-bindings",
            admin,
            method="POST",
            payload={**body, "status": "approved"},
        )[0]
        == 422
    )
    one = declaration(server, admin, first)
    two = declaration(server, admin, second)
    assert one["vehicle_id"] == two["vehicle_id"]
    assert one["status"] == two["status"] == "pending"
    approve_one = f"/admin/vehicle-bindings/{one['id']}/approve"
    approve_two = f"/admin/vehicle-bindings/{two['id']}/approve"
    assert (
        request(server, approve_one, admin, method="POST", payload={"reason": ""})[0]
        == 400
    )
    assert (
        request(
            server, approve_one, admin, method="POST", payload={"reason": "x" * 17000}
        )[0]
        == 413
    )
    assert (
        request(
            server,
            approve_one,
            admin,
            method="POST",
            payload={"reason": "institutional review"},
        )[0]
        == 200
    )
    assert (
        request(
            server,
            approve_two,
            admin,
            method="POST",
            payload={"reason": "not an authorized transfer"},
        )[0]
        == 409
    )
    recovery = {"reason": "validated recovery", "expected_previous_binding": one["id"]}
    assert (
        request(server, approve_two, admin, method="POST", payload=recovery)[0] == 200
    )
    assert (
        request(server, approve_two, admin, method="POST", payload=recovery)[0] == 200
    )
    status, rows = request(
        server, "/admin/vehicle-bindings?status=approved&limit=1", admin
    )
    assert status == 200 and [row["id"] for row in rows] == [two["id"]]
    assert request(server, "/admin/vehicle-bindings?limit=101", admin)[0] == 400
    assert request(server, "/admin/vehicle-bindings?status=invalid", admin)[0] == 400
    assert (
        request(
            server,
            f"/admin/vehicle-bindings/{two['id']}/revoke",
            admin,
            method="POST",
            payload={"reason": "access withdrawn"},
        )[0]
        == 200
    )
    assert (
        server.query("SELECT COUNT(*) FROM vehicle_bindings WHERE status='approved'")[
            0
        ][0]
        == 0
    )
    assert server.query("SELECT COUNT(*) FROM vehicle_binding_audit")[0][0] == 6
    assert (
        server.query(
            "SELECT COUNT(*) FROM vehicle_binding_audit WHERE actor_id=%s",
            (str(admin_id),),
        )[0][0]
        == 6
    )
    server.query("UPDATE users SET role='user' WHERE id=%s", (str(admin_id),))
    assert (
        request(
            server,
            approve_one,
            admin,
            method="POST",
            payload={"reason": "role withdrawn"},
        )[0]
        == 403
    )


def test_an_undeclared_plate_never_authorizes_an_arrival(server):
    enable_lpr(server)
    signed(server, "user")
    assert server.query("SELECT COUNT(*) FROM vehicles")[0][0] == 0
    server.query(
        "INSERT INTO edge_camera_grants (gateway_id,camera_id,car_detected) VALUES (%s,'cam-01',TRUE)",
        (server.gateway_id,),
    )
    server.query(
        "INSERT INTO edge_camera_spots (gateway_id,camera_id,spot_id) VALUES (%s,'cam-01','R02-A')",
        (server.gateway_id,),
    )

    async def run():
        async with websockets.connect(
            server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
        ) as edge:
            await receive_ready(edge)
            await edge.send(
                json.dumps(
                    {
                        "type": "CAR_DETECTED",
                        "plate": "ABC1D23",
                        "camera_id": "cam-01",
                        "timestamp": "2026-10-17T12:00:00Z",
                    }
                )
            )
            event = message(str(uuid4()))
            await edge.send(json.dumps(event))
            ack = json.loads(await asyncio.wait_for(edge.recv(), timeout=3))
            assert ack["event_id"] == event["event_id"] and ack["outcome"] == "applied"

    asyncio.run(run())
    assert server.query("SELECT COUNT(*) FROM reservations")[0][0] == 0
