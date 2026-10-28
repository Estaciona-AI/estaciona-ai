# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Real sessions: logout revokes one session and all its WS tickets."""

import asyncio
from contextlib import suppress
import json
import os
import time

import pytest
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus

from test_delivery_integration import server as delivery_server
from test_session_integration import insert_user, token
from test_ws_tickets_integration import http, issue, ws_url

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="requires real Rust server and PostgreSQL",
)


def login_pair(server, role):
    """Two sessions of one account, as two sign-ins at the identity provider."""
    user_id = insert_user(server, role=role)
    expires = int(time.time()) + 3600
    return user_id, [token(server, user_id, role, expires) for _ in range(2)]


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
            status, body, headers = http(server, "/auth/logout", first, {})
            assert status == 200, body
            # The provider is unreachable in this harness, so no logout URL.
            assert body == {"end_session_url": None}
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


def test_cookie_logout_needs_the_app_origin_and_logout_is_idempotent(server):
    _, (first, second) = login_pair(server, "user")
    cookie = f"__Host-estaciona_session={first}"
    for origin in (None, "https://evil.example"):
        status = http(server, "/auth/logout", payload={}, cookie=cookie, origin=origin)[
            0
        ]
        assert status == 403, origin
    assert issue(server, first)[0] == 201
    status = http(
        server, "/auth/logout", payload={}, cookie=cookie, origin="http://localhost"
    )[0]
    assert status == 200
    assert issue(server, first)[0] == 401
    assert http(server, "/auth/logout", second, {})[0] == 200
    assert http(server, "/auth/logout", second, {})[0] == 200
