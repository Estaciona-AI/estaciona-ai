"""Opt-in session lifecycle checks against Rust and disposable PostgreSQL."""

import asyncio
import base64
from contextlib import contextmanager, suppress
import hashlib
import hmac
import json
import os
import time
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from uuid import uuid4

import pytest
import psycopg2
from psycopg2 import sql
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus

from test_delivery_integration import KEY, server as delivery_server

server = delivery_server

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)


def token(user_id, role, expires_at):
    """Sign the existing JWT schema using only a disposable fixture secret."""

    def encoded(value):
        return base64.urlsafe_b64encode(
            json.dumps(value, separators=(",", ":")).encode()
        ).rstrip(b"=")

    signed = b".".join(
        (
            encoded({"alg": "HS256", "typ": "JWT"}),
            encoded({"sub": str(user_id), "role": role, "exp": expires_at}),
        )
    )
    signature = hmac.new(b"disposable-jwt", signed, hashlib.sha256).digest()
    return (signed + b"." + base64.urlsafe_b64encode(signature).rstrip(b"=")).decode()


def insert_user(server, role="user", plate=None):
    user_id = uuid4()
    server.query(
        "INSERT INTO users (id, name, email, password_hash, role, plate) "
        "VALUES (%s, 'Session fixture', %s, 'unused-test-hash', %s, %s)",
        (str(user_id), f"{user_id}@example.invalid", role, plate),
    )
    return user_id


def websocket_url(server, endpoint, user_id, signed_token):
    query = {"token": signed_token}
    if endpoint == "app":
        query["user_id"] = str(user_id)
    return f"ws://{server.host}:8000/ws/{endpoint}?{urlencode(query)}"


def request_status(
    server, path, signed_token=None, method="GET", payload=None, timeout=3
):
    headers = {}
    if signed_token is not None:
        headers["Authorization"] = f"Bearer {signed_token}"
    data = None
    if payload is not None:
        data = json.dumps(payload).encode()
        headers["Content-Type"] = "application/json"
    request = Request(
        f"http://{server.host}:8000{path}",
        data=data,
        headers=headers,
        method=method,
    )
    try:
        with urlopen(request, timeout=timeout) as response:
            return response.status
    except HTTPError as error:
        return error.code


@contextmanager
def held_table_lock(server, table):
    connection = psycopg2.connect(server.db_url)
    try:
        with connection.cursor() as cursor:
            cursor.execute("BEGIN")
            cursor.execute("SET LOCAL lock_timeout = '3s'")
            cursor.execute(
                sql.SQL("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE").format(
                    sql.Identifier(table)
                )
            )
        yield
    finally:
        try:
            connection.rollback()
        finally:
            connection.close()


async def wait_for_lock_waiter(server, table, query_fragment):
    async def poll():
        while True:
            rows = server.query(
                "SELECT activity.pid FROM pg_stat_activity AS activity "
                "JOIN pg_locks AS locks ON locks.pid = activity.pid "
                "WHERE activity.datname = current_database() "
                "AND activity.wait_event_type = 'Lock' AND NOT locks.granted "
                "AND locks.relation = %s::regclass AND activity.query LIKE %s",
                (table, f"%{query_fragment}%"),
            )
            if rows:
                return rows[0][0]
            await asyncio.sleep(0.01)

    return await asyncio.wait_for(poll(), timeout=1.5)


async def wait_for_backend_transaction_to_finish(server, backend_pid):
    async def poll():
        while True:
            rows = server.query(
                "SELECT state FROM pg_stat_activity WHERE pid = %s", (backend_pid,)
            )
            # An idle-in-transaction backend can still commit a delayed write.
            if not rows or rows == [("idle",)]:
                return
            await asyncio.sleep(0.01)

    await asyncio.wait_for(poll(), timeout=3)


async def closure_time(connection):
    await connection.wait_closed()
    return time.time()


async def consume_snapshot(connection):
    # Drain the fixture's initial snapshot before observing an otherwise idle socket.
    assert json.loads(await asyncio.wait_for(connection.recv(), timeout=3)) == {
        "type": "SPOT_UPDATE",
        "spot_id": "R02-A",
        "status": "free",
    }


async def handshake_status(server, endpoint, user_id, signed_token):
    try:
        async with websockets.connect(
            websocket_url(server, endpoint, user_id, signed_token), close_timeout=0.5
        ):
            return 101
    except InvalidStatus as error:
        return error.response.status_code


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_recently_expired_token_is_rejected_at_websocket_handshake(
    server, endpoint, role
):
    user_id = insert_user(server, role)
    expired = token(user_id, role, int(time.time()) - 1)
    assert asyncio.run(handshake_status(server, endpoint, user_id, expired)) == 401


@pytest.mark.parametrize("path", ["/reservations", "/users/{user_id}"])
def test_recently_expired_token_is_rejected_by_rest(server, path):
    user_id = insert_user(server, "admin")
    expired = token(user_id, "admin", int(time.time()) - 1)
    assert request_status(server, path.format(user_id=user_id), expired) == 401


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_idle_websocket_closes_when_its_token_expires(server, endpoint, role):
    user_id = insert_user(server, role)

    async def run():
        expires_at = int(time.time()) + 3
        async with websockets.connect(
            websocket_url(server, endpoint, user_id, token(user_id, role, expires_at)),
            close_timeout=0.5,
        ) as connection:
            await consume_snapshot(connection)
            # No client message or broadcast wakes the server before expiration.
            closed = asyncio.create_task(closure_time(connection))
            await asyncio.sleep(0.1)
            assert not closed.done(), "a valid session must remain open before exp"
            closed_at = await asyncio.wait_for(
                closed, timeout=expires_at - time.time() + 2
            )
            assert expires_at - 0.2 <= closed_at <= expires_at + 2

    asyncio.run(run())


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_websocket_expires_during_continuous_broadcasts(server, endpoint, role):
    user_id = insert_user(server, role)

    async def run():
        async with websockets.connect(
            server.url,
            additional_headers={"Authorization": f"Bearer {KEY}"},
            close_timeout=0.5,
        ) as edge:
            assert json.loads(await asyncio.wait_for(edge.recv(), timeout=3)) == {
                "type": "EDGE_READY",
                "delivery_version": 1,
            }
            expires_at = int(time.time()) + 3
            async with websockets.connect(
                websocket_url(
                    server, endpoint, user_id, token(user_id, role, expires_at)
                ),
                close_timeout=0.5,
            ) as connection:
                await consume_snapshot(connection)
                frame = json.dumps(
                    {
                        "type": "SPOT_UPDATE",
                        "spot_id": "R02-A",
                        "status": "free",
                        "camera_id": "session-camera",
                        "confidence": 0.9,
                        "timestamp": "2026-10-11T12:00:00Z",
                    }
                )
                sent = 0

                async def publish():
                    nonlocal sent
                    while True:
                        await edge.send(frame)
                        sent += 1
                        await asyncio.sleep(1 / 60)

                async def receive_until_closed():
                    received = 0
                    try:
                        while True:
                            assert json.loads(await connection.recv()) == {
                                "type": "SPOT_UPDATE",
                                "spot_id": "R02-A",
                                "status": "free",
                            }
                            received += 1
                    except ConnectionClosed:
                        return received, time.time()

                publisher = asyncio.create_task(publish())
                try:
                    received, closed_at = await asyncio.wait_for(
                        receive_until_closed(), timeout=expires_at - time.time() + 2
                    )
                    assert sent >= 5 and received >= 5, (
                        "real broadcasts must reach the socket"
                    )
                    assert expires_at - 0.2 <= closed_at <= expires_at + 2
                finally:
                    publisher.cancel()
                    with suppress(asyncio.CancelledError):
                        await publisher

    asyncio.run(run())


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_websocket_expires_while_initial_snapshot_is_blocked(server, endpoint, role):
    user_id = insert_user(server, role)

    async def run():
        with held_table_lock(server, "spots"):
            expires_at = int(time.time()) + 4
            async with websockets.connect(
                websocket_url(
                    server, endpoint, user_id, token(user_id, role, expires_at)
                ),
                close_timeout=0.5,
            ) as connection:
                await wait_for_lock_waiter(
                    server, "spots", "SELECT id, status FROM spots"
                )
                # A blocked snapshot must neither postpone expiration nor send data.
                with pytest.raises(ConnectionClosed):
                    await asyncio.wait_for(
                        connection.recv(), timeout=expires_at - time.time() + 2
                    )
                assert expires_at - 0.2 <= time.time() <= expires_at + 2

    asyncio.run(run())


@pytest.mark.parametrize(
    "role,path", [("user", "/users/{user_id}"), ("admin", "/reservations")]
)
def test_rest_authorization_times_out_while_users_is_locked(server, role, path):
    user_id = insert_user(server, role)
    signed_token = token(user_id, role, int(time.time()) + 60)

    async def run():
        with held_table_lock(server, "users"):
            started = time.monotonic()
            response = asyncio.create_task(
                asyncio.to_thread(
                    request_status,
                    server,
                    path.format(user_id=user_id),
                    signed_token,
                    timeout=5,
                )
            )
            await wait_for_lock_waiter(server, "users", "SELECT role FROM users")
            assert await asyncio.wait_for(response, timeout=3.5) == 503
            elapsed = time.monotonic() - started
            assert 1.8 <= elapsed <= 3.5, (
                "the two-second authorization timeout must be bounded"
            )

    asyncio.run(run())


def test_blocked_app_reservation_does_not_write_after_session_expiration(server):
    user_id = insert_user(server)

    async def run():
        expires_at = int(time.time()) + 4
        async with websockets.connect(
            websocket_url(server, "app", user_id, token(user_id, "user", expires_at)),
            close_timeout=0.5,
        ) as connection:
            await consume_snapshot(connection)
            with held_table_lock(server, "spots"):
                await connection.send(
                    json.dumps(
                        {
                            "type": "RESERVE_SPOT",
                            "spot_id": "R02-A",
                            "user_id": str(user_id),
                            "plate": "R03TEST",
                        }
                    )
                )
                backend_pid = await wait_for_lock_waiter(
                    server, "spots", "UPDATE spots SET status = 'reserved'"
                )
                closed_at = await asyncio.wait_for(
                    closure_time(connection), timeout=expires_at - time.time() + 2
                )
                assert expires_at - 0.2 <= closed_at <= expires_at + 2
            # Release the lock, then observe the actual transaction's completion.
            # A detached command would resume here and its committed effects count.
            await wait_for_backend_transaction_to_finish(server, backend_pid)
            assert server.query("SELECT COUNT(*) FROM reservations") == [(0,)]
            assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
                ("free",)
            ]

    asyncio.run(run())


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_deleted_user_is_rejected_at_websocket_handshake(server, endpoint, role):
    user_id = insert_user(server, role)
    signed_token = token(user_id, role, int(time.time()) + 60)
    server.query("DELETE FROM users WHERE id = %s", (str(user_id),))
    assert asyncio.run(handshake_status(server, endpoint, user_id, signed_token)) == 401


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
@pytest.mark.parametrize("change", ["delete", "role"])
def test_idle_websocket_revalidates_user_and_role(server, endpoint, role, change):
    user_id = insert_user(server, role)
    signed_token = token(user_id, role, int(time.time()) + 60)

    async def run():
        async with websockets.connect(
            websocket_url(server, endpoint, user_id, signed_token), close_timeout=0.5
        ) as connection:
            await consume_snapshot(connection)
            if change == "delete":
                server.query("DELETE FROM users WHERE id = %s", (str(user_id),))
            else:
                server.query(
                    "UPDATE users SET role = %s WHERE id = %s",
                    ("user" if role == "admin" else "viewer", str(user_id)),
                )
            # The approved five-second revalidation has 2.5 seconds of CI margin.
            await asyncio.wait_for(connection.wait_closed(), timeout=7.5)

    asyncio.run(run())


@pytest.mark.parametrize("change", ["delete", "role"])
def test_app_command_rechecks_principal_before_next_periodic_validation(server, change):
    user_id = insert_user(server)
    signed_token = token(user_id, "user", int(time.time()) + 60)

    async def run():
        async with websockets.connect(
            websocket_url(server, "app", user_id, signed_token), close_timeout=0.5
        ) as connection:
            await consume_snapshot(connection)
            if change == "delete":
                server.query("DELETE FROM users WHERE id = %s", (str(user_id),))
            else:
                server.query(
                    "UPDATE users SET role = 'viewer' WHERE id = %s", (str(user_id),)
                )
            await connection.send(
                json.dumps(
                    {
                        "type": "RESERVE_SPOT",
                        "spot_id": "R02-A",
                        "user_id": str(user_id),
                        "plate": "R03TEST",
                    }
                )
            )
            try:
                # Less than the five-second tick: the command itself must recheck.
                await asyncio.wait_for(connection.wait_closed(), timeout=1.5)
            finally:
                assert server.query("SELECT COUNT(*) FROM reservations") == [(0,)]
                assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [
                    ("free",)
                ]

    asyncio.run(run())


@pytest.mark.parametrize(
    "method,path,payload",
    [
        ("GET", "/users/{user_id}", None),
        ("GET", "/reservations/recommend?user_id={user_id}", None),
        (
            "POST",
            "/reservations",
            {"spot_id": "R02-A", "expires_at": "2099-01-01T00:00:00Z"},
        ),
        ("PUT", "/reservations/{reservation_id}/cancel", None),
        ("PUT", "/reservations/{reservation_id}/extend", None),
        ("POST", "/reservations/{reservation_id}/confirm", None),
    ],
)
def test_protected_rest_rejects_a_deleted_user(server, method, path, payload):
    user_id = insert_user(server)
    signed_token = token(user_id, "user", int(time.time()) + 60)
    server.query("DELETE FROM users WHERE id = %s", (str(user_id),))
    path = path.format(user_id=user_id, reservation_id=uuid4())
    assert request_status(server, path, signed_token, method, payload) == 401


@pytest.mark.parametrize(
    "path,method,payload",
    [
        ("/reservations", "GET", None),
        ("/spots/R02-A/status", "PUT", {"status": "occupied"}),
    ],
)
def test_protected_rest_rejects_a_demoted_admin(server, path, method, payload):
    user_id = insert_user(server, "admin")
    signed_token = token(user_id, "admin", int(time.time()) + 60)
    server.query("UPDATE users SET role = 'user' WHERE id = %s", (str(user_id),))
    assert request_status(server, path, signed_token, method, payload) in (401, 403)
    assert server.query("SELECT status FROM spots WHERE id = 'R02-A'") == [("free",)]


def test_admin_rest_distinguishes_missing_authentication_from_insufficient_role(server):
    user_id = insert_user(server)
    assert request_status(server, "/reservations") == 401
    assert (
        request_status(
            server, "/reservations", token(user_id, "user", int(time.time()) + 60)
        )
        == 403
    )


def test_dashboard_handshake_rejects_authenticated_non_admin(server):
    user_id = insert_user(server)
    signed_token = token(user_id, "user", int(time.time()) + 60)
    assert (
        asyncio.run(handshake_status(server, "dashboard", user_id, signed_token)) == 401
    )


def test_rest_fails_closed_when_principal_lookup_fails(server):
    user_id = insert_user(server, "admin")
    signed_token = token(user_id, "admin", int(time.time()) + 60)
    # Only this fixture's disposable database is changed; PostgreSQL keeps running.
    server.query("ALTER TABLE users RENAME TO session_test_unavailable_users")
    try:
        assert request_status(server, "/reservations", signed_token) == 503
    finally:
        server.query("ALTER TABLE session_test_unavailable_users RENAME TO users")


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_websocket_handshake_fails_closed_when_principal_lookup_fails(
    server, endpoint, role
):
    user_id = insert_user(server, role)
    signed_token = token(user_id, role, int(time.time()) + 60)
    server.query("ALTER TABLE users RENAME TO session_test_unavailable_users")
    try:
        assert (
            asyncio.run(handshake_status(server, endpoint, user_id, signed_token))
            == 503
        )
    finally:
        server.query("ALTER TABLE session_test_unavailable_users RENAME TO users")


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_idle_websocket_fails_closed_when_principal_lookup_fails(
    server, endpoint, role
):
    user_id = insert_user(server, role)
    signed_token = token(user_id, role, int(time.time()) + 60)

    async def run():
        async with websockets.connect(
            websocket_url(server, endpoint, user_id, signed_token), close_timeout=0.5
        ) as connection:
            await consume_snapshot(connection)
            server.query("ALTER TABLE users RENAME TO session_test_unavailable_users")
            try:
                await asyncio.wait_for(connection.wait_closed(), timeout=7.5)
            finally:
                server.query(
                    "ALTER TABLE session_test_unavailable_users RENAME TO users"
                )

    asyncio.run(run())


@pytest.mark.parametrize("termination", ["expiration", "client_close"])
def test_older_socket_does_not_remove_the_newer_private_channel(server, termination):
    # A known plate and active reservation make CAR_DETECTED send a private route.
    hashed_plate = hmac.new(
        b"disposable-pepper", b"R03TEST", hashlib.sha256
    ).hexdigest()
    user_id = insert_user(server, plate=hashed_plate)

    async def run():
        expires_at = int(time.time()) + (3 if termination == "expiration" else 60)
        async with websockets.connect(
            websocket_url(server, "app", user_id, token(user_id, "user", expires_at)),
            close_timeout=0.5,
        ) as older:
            await consume_snapshot(older)
            async with websockets.connect(
                websocket_url(
                    server,
                    "app",
                    user_id,
                    token(user_id, "user", int(time.time()) + 60),
                ),
                close_timeout=0.5,
            ) as newer:
                await consume_snapshot(newer)
                server.query(
                    "INSERT INTO reservations (id, user_id, spot_id, expires_at) "
                    "VALUES (%s, %s, 'R02-A', NOW() + INTERVAL '15 minutes')",
                    (str(uuid4()), str(user_id)),
                )
                if termination == "expiration":
                    await asyncio.wait_for(
                        older.wait_closed(), timeout=expires_at - time.time() + 2
                    )
                else:
                    await older.close()
                # Give the older handler time to finish its server-side cleanup.
                await asyncio.sleep(0.1)
                async with websockets.connect(
                    server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
                ) as edge:
                    assert json.loads(
                        await asyncio.wait_for(edge.recv(), timeout=3)
                    ) == {
                        "type": "EDGE_READY",
                        "delivery_version": 1,
                    }
                    await edge.send(
                        json.dumps(
                            {
                                "type": "CAR_DETECTED",
                                "plate": "R03TEST",
                                "camera_id": "session-camera",
                                "timestamp": "2026-10-11T12:00:00Z",
                            }
                        )
                    )
                    assert json.loads(
                        await asyncio.wait_for(newer.recv(), timeout=3)
                    ) == {
                        "type": "NAVIGATION_START",
                        "spot_id": "R02-A",
                        "route": ["session-camera", "R02-A"],
                    }

    asyncio.run(run())
