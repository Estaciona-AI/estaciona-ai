"""Real login sessions: logout revokes one session and all its WS tickets."""

import asyncio
from contextlib import suppress
import json
import os
from uuid import uuid4

import pytest
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus

from test_delivery_integration import server as delivery_server
from test_ws_tickets_integration import http, issue, ws_url

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="requires real Rust server and PostgreSQL",
)


def login_pair(server, role):
    email = f"{uuid4()}@example.invalid"
    password = "disposable-r03b-password"
    status, user, _ = http(
        server, "/users", payload={"name": "R03b", "email": email, "password": password}
    )
    assert status == 201, user
    user_id = server.query("SELECT id FROM users WHERE email=%s", (email,))[0][0]
    server.query("UPDATE users SET role=%s WHERE id=%s", (role, user_id))
    tokens = []
    for _ in range(2):
        status, result, _ = http(
            server, "/login", payload={"email": email, "password": password}
        )
        assert status == 200, result
        tokens.append(result["token"])
    return user_id, tokens


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_logout_revokes_only_current_session_including_idle_socket_and_ticket(
    server, endpoint, role
):
    user_id, (first, second) = login_pair(server, role)
    assert first != second
    status, initial, _ = issue(server, first, endpoint)
    assert status == 201, initial
    status, unused, _ = issue(server, first, endpoint)
    assert status == 201, unused
    status, other, _ = issue(server, second, endpoint)
    assert status == 201, other

    async def run():
        async with websockets.connect(
            ws_url(server, endpoint, initial["ticket"]),
            origin="http://localhost",
            close_timeout=0.5,
        ) as connection:
            status, _, headers = http(server, "/auth/logout", first, {})
            assert status == 204
            cookie = headers.get("Set-Cookie", "")
            assert "Max-Age=0" in cookie and "HttpOnly" in cookie and "Secure" in cookie
            assert issue(server, first, endpoint)[0] == 401
            assert http(server, f"/users/{user_id}", first)[0] == 401
            if endpoint == "app":
                with suppress(ConnectionClosed):
                    await connection.send(
                        json.dumps(
                            {
                                "type": "RESERVE_SPOT",
                                "spot_id": "R02-A",
                                "user_id": str(user_id),
                                "plate": "unused",
                            }
                        )
                    )
            await asyncio.wait_for(connection.wait_closed(), timeout=8)
            with pytest.raises(InvalidStatus):
                await websockets.connect(
                    ws_url(server, endpoint, unused["ticket"]),
                    origin="http://localhost",
                )
        async with websockets.connect(
            ws_url(server, endpoint, other["ticket"]), origin="http://localhost"
        ):
            pass

    asyncio.run(run())
    assert http(server, f"/users/{user_id}", second)[0] == 200
    assert server.query("SELECT COUNT(*) FROM reservations") == [(0,)]
    server.stop()
    server.start()
    assert http(server, f"/users/{user_id}", first)[0] == 401
    assert http(server, f"/users/{user_id}", second)[0] == 200


def test_logout_requires_bearer_and_is_idempotent(server):
    _, (first, _) = login_pair(server, "user")
    assert (
        http(server, "/auth/logout", payload={}, cookie=f"estaciona_token={first}")[0]
        == 401
    )
    assert issue(server, first)[0] == 201
    assert http(server, "/auth/logout", first, {})[0] == 204
    assert http(server, "/auth/logout", first, {})[0] == 204
