"""One-use human WebSocket tickets are opaque, scoped, and never reusable."""

import asyncio
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import pytest
import websockets
from websockets.exceptions import InvalidStatus

from test_delivery_integration import server as delivery_server
from test_session_integration import insert_user, token

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)
server = delivery_server


def http(server, path, bearer=None, payload=None, cookie=None):
    headers = {}
    data = None
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"
    if cookie:
        headers["Cookie"] = cookie
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    request = Request(f"http://{server.host}:8000{path}", headers=headers, data=data)
    try:
        response = urlopen(request, timeout=6)
    except HTTPError as error:
        response = error
    with response:
        raw = response.read().decode()
        try:
            body = json.loads(raw)
        except json.JSONDecodeError:
            body = raw
        return (
            response.status if hasattr(response, "status") else response.code,
            body,
            response.headers,
        )


def issue(server, bearer, destination="app"):
    return http(server, "/auth/ws-ticket", bearer, {"destination": destination})


def ws_url(server, endpoint, ticket):
    return f"ws://{server.host}:8000/ws/{endpoint}?ticket={ticket}"


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
@pytest.mark.parametrize(
    "origin", [None, "null", "http://localhost.evil", "http://localhost:5173"]
)
def test_denied_origin_preserves_ticket_for_allowed_browser(
    server, endpoint, role, origin
):
    user_id = insert_user(server, role=role)
    jwt = token(user_id, role, int(time.time()) + 120)
    status, result, _ = issue(server, jwt, endpoint)
    assert status == 201, result
    ticket = result["ticket"]
    digest = hashlib.sha256(ticket.encode()).digest()
    url = ws_url(server, endpoint, ticket)

    async def run():
        with pytest.raises(InvalidStatus) as denied:
            await websockets.connect(url, origin=origin)
        assert denied.value.response.status_code == 403
        assert server.query("SELECT 1 FROM ws_tickets WHERE digest=%s", (digest,)) == [
            (1,)
        ]

        async with websockets.connect(url, origin="http://localhost"):
            pass
        assert server.query("SELECT 1 FROM ws_tickets WHERE digest=%s", (digest,)) == []

    asyncio.run(run())


def test_ticket_is_bearer_only_scoped_to_app_and_single_use(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, _, _ = http(
        server,
        "/auth/ws-ticket",
        payload={"destination": "app"},
        cookie=f"estaciona_token={jwt}",
    )
    assert status == 401

    status, result, headers = issue(server, jwt)
    assert status == 201, result
    ticket = result["ticket"]
    assert len(ticket) == 64 and ticket != jwt
    assert headers["Cache-Control"] == "no-store"
    digest = hashlib.sha256(ticket.encode()).digest()
    stored = server.query(
        "SELECT user_id,destination,session_expires_at FROM ws_tickets WHERE digest=%s",
        (digest,),
    )
    assert stored == [(str(user_id), "app", int(time.time()) + 120)]

    async def run():
        async with websockets.connect(
            ws_url(server, "app", ticket), origin="http://localhost"
        ):
            pass
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(
                ws_url(server, "app", ticket), origin="http://localhost"
            )
        assert error.value.response.status_code == 401

    asyncio.run(run())


def test_wrong_destination_burns_ticket_and_dashboard_needs_admin(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    assert issue(server, jwt, "dashboard")[0] == 403

    admin_id = insert_user(server, role="admin")
    admin = token(admin_id, "admin", int(time.time()) + 120)
    status, result, _ = issue(server, admin, "app")
    assert status == 201, result
    ticket = result["ticket"]

    async def run():
        with pytest.raises(InvalidStatus) as wrong:
            await websockets.connect(
                ws_url(server, "dashboard", ticket), origin="http://localhost"
            )
        assert wrong.value.response.status_code == 401
        with pytest.raises(InvalidStatus) as replay:
            await websockets.connect(
                ws_url(server, "app", ticket), origin="http://localhost"
            )
        assert replay.value.response.status_code == 401

    asyncio.run(run())


def test_ticket_issue_rejects_unknown_fields_and_oversized_json(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, _, _ = http(
        server,
        "/auth/ws-ticket",
        jwt,
        {"destination": "app", "unexpected": "field"},
    )
    assert status == 400

    status, _, _ = http(
        server,
        "/auth/ws-ticket",
        jwt,
        {"destination": "app", "padding": "x" * 2048},
    )
    assert status == 413


def test_legacy_jwt_query_parameter_is_not_accepted(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)

    async def run():
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(
                f"ws://{server.host}:8000/ws/app?token={jwt}", origin="http://localhost"
            )
        assert error.value.response.status_code == 400

    asyncio.run(run())


def test_role_change_invalidates_an_issued_dashboard_ticket(server):
    admin_id = insert_user(server, role="admin")
    admin = token(admin_id, "admin", int(time.time()) + 120)
    status, result, _ = issue(server, admin, "dashboard")
    assert status == 201, result
    server.query("UPDATE users SET role='user' WHERE id=%s", (str(admin_id),))

    async def run():
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(
                ws_url(server, "dashboard", result["ticket"]), origin="http://localhost"
            )
        assert error.value.response.status_code == 403

    asyncio.run(run())


def test_ticket_operations_fail_closed_when_ticket_store_is_unavailable(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, result, _ = issue(server, jwt)
    assert status == 201, result
    server.query("DROP TABLE ws_tickets")
    assert issue(server, jwt)[0] == 503

    async def run():
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(
                ws_url(server, "app", result["ticket"]), origin="http://localhost"
            )
        assert error.value.response.status_code == 503

    asyncio.run(run())


def test_revoked_or_expired_sessions_cannot_use_issued_ticket(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, result, _ = issue(server, jwt)
    assert status == 201, result
    digest = hashlib.sha256(result["ticket"].encode()).digest()
    server.query("DELETE FROM users WHERE id=%s", (str(user_id),))
    assert server.query("SELECT 1 FROM ws_tickets WHERE digest=%s", (digest,)) == []

    admin_id = insert_user(server, role="admin")
    admin = token(admin_id, "admin", int(time.time()) + 120)
    status, expired, _ = issue(server, admin, "dashboard")
    assert status == 201, expired
    expired_digest = hashlib.sha256(expired["ticket"].encode()).digest()
    server.query(
        "UPDATE ws_tickets SET expires_at=NOW()-INTERVAL '1 second' WHERE digest=%s",
        (expired_digest,),
    )

    async def run():
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(
                ws_url(server, "dashboard", expired["ticket"]),
                origin="http://localhost",
            )
        assert error.value.response.status_code == 401

    asyncio.run(run())


def test_at_most_five_unconsumed_tickets_per_user(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    for _ in range(5):
        status, result, _ = issue(server, jwt)
        assert status == 201, result
    status, _result, _ = issue(server, jwt)
    assert status == 429


def test_concurrent_ticket_issuance_respects_per_user_limit(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    with ThreadPoolExecutor(max_workers=8) as pool:
        statuses = list(pool.map(lambda _: issue(server, jwt)[0], range(8)))
    assert statuses.count(201) == 5
    assert statuses.count(429) == 3


def test_ticket_remains_single_use_after_server_restart(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, result, _ = issue(server, jwt)
    assert status == 201, result
    url = ws_url(server, "app", result["ticket"])
    server.stop()
    server.start()

    async def run():
        async with websockets.connect(url, origin="http://localhost"):
            pass
        with pytest.raises(InvalidStatus) as error:
            await websockets.connect(url, origin="http://localhost")
        assert error.value.response.status_code == 401

    asyncio.run(run())


def test_concurrent_handshakes_allow_exactly_one_use(server):
    user_id = insert_user(server)
    jwt = token(user_id, "user", int(time.time()) + 120)
    status, result, _ = issue(server, jwt)
    assert status == 201, result
    url = ws_url(server, "app", result["ticket"])

    async def connect():
        try:
            async with websockets.connect(url, origin="http://localhost"):
                return 101
        except InvalidStatus as error:
            return error.response.status_code

    async def run():
        return await asyncio.gather(connect(), connect())

    assert sorted(asyncio.run(run())) == [101, 401]


def test_ticket_expiry_never_exceeds_the_original_jwt(server):
    user_id = insert_user(server)
    session_expiry = int(time.time()) + 3
    jwt = token(user_id, "user", session_expiry)
    status, result, _ = issue(server, jwt)
    assert status == 201, result
    digest = hashlib.sha256(result["ticket"].encode()).digest()
    stored = server.query(
        "SELECT session_expires_at,EXTRACT(EPOCH FROM expires_at) "
        "FROM ws_tickets WHERE digest=%s",
        (digest,),
    )
    assert len(stored) == 1
    session_expiry_db, ticket_expiry = stored[0]
    assert session_expiry_db == session_expiry
    assert session_expiry_db - 30 <= ticket_expiry <= session_expiry_db
