# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""WebSocket admission against the real server: slow or excess handshakes
are refused before they reach the database pool, connections are capped per
kind and released on close, and oversized messages close the socket."""

import asyncio
import os
import time

import pytest
import websockets
from websockets.exceptions import ConnectionClosed, InvalidStatus
from websockets.protocol import State

from test_delivery_integration import KEY, receive_ready, server as delivery_server
from test_session_integration import held_table_lock, insert_user, token
from test_ws_tickets_integration import issue, ws_url

pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL to run real Rust integration",
)

server = delivery_server


def restart_with(server, **limits):
    server.stop()
    server.environment.update({key: str(value) for key, value in limits.items()})
    server.start()


def edge(server):
    return websockets.connect(
        server.url, additional_headers={"Authorization": f"Bearer {KEY}"}
    )


def test_edge_connections_are_capped_and_released_on_close(server):
    restart_with(server, WS_MAX_EDGE_CONNECTIONS=1)

    async def run():
        async with edge(server) as first:
            await receive_ready(first)
            with pytest.raises(InvalidStatus) as refused:
                async with edge(server):
                    pass
            assert refused.value.response.status_code == 503
            assert refused.value.response.headers["Retry-After"] == "1"
        # The slot returns once the first socket is gone.
        deadline = time.monotonic() + 5
        while True:
            try:
                async with edge(server) as again:
                    await receive_ready(again)
                    return
            except InvalidStatus:
                if time.monotonic() > deadline:
                    raise
                await asyncio.sleep(0.05)

    asyncio.run(run())


def test_slow_authentication_cannot_take_every_handshake_slot(server):
    restart_with(server, WS_HANDSHAKE_CONCURRENCY=1)

    async def run():
        # The first handshake waits on the locked credential table, holding
        # the only authentication slot, as a slow database would.
        stuck = asyncio.create_task(edge(server).__aenter__())
        await asyncio.sleep(0.5)
        assert not stuck.done()
        started = time.monotonic()
        with pytest.raises(InvalidStatus) as refused:
            async with edge(server):
                pass
        assert refused.value.response.status_code == 503
        # Refused at once, without waiting for the database.
        assert time.monotonic() - started < 1
        stuck.cancel()
        with pytest.raises((asyncio.CancelledError, Exception)):
            await stuck

    with held_table_lock(server, "edge_gateways"):
        asyncio.run(run())

    async def recovered():
        async with edge(server) as connection:
            await receive_ready(connection)

    asyncio.run(recovered())


async def alive(connection):
    await asyncio.wait_for(await connection.ping(), timeout=5)
    await asyncio.sleep(0.2)
    assert connection.state is State.OPEN


async def closed_by_server(connection):
    """The server drops the socket; it may not send a close frame first."""
    with pytest.raises(ConnectionClosed):
        while True:
            await asyncio.wait_for(connection.recv(), timeout=5)


def test_an_oversized_edge_message_closes_the_socket(server):
    async def run():
        async with edge(server) as connection:
            await receive_ready(connection)
            # Up to the limit an unparseable frame is only ignored.
            await connection.send("x" * (64 * 1024))
            await alive(connection)
            await connection.send("x" * (64 * 1024 + 1))
            await closed_by_server(connection)

    asyncio.run(run())


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_an_oversized_client_message_closes_the_socket(server, endpoint, role):
    user = insert_user(server, role=role)
    jwt = token(server, user, role, int(time.time()) + 120)
    status, ticket, _ = issue(server, jwt, endpoint)
    assert status == 201, ticket

    async def run():
        async with websockets.connect(
            ws_url(server, endpoint, ticket["ticket"]), origin="http://localhost"
        ) as connection:
            await connection.send("x" * (16 * 1024))
            await alive(connection)
            await connection.send("x" * (16 * 1024 + 1))
            await closed_by_server(connection)

    asyncio.run(run())


@pytest.mark.parametrize("endpoint,role", [("app", "user"), ("dashboard", "admin")])
def test_a_client_ping_is_answered_and_keeps_the_socket_open(server, endpoint, role):
    """Non-browser clients (websockets' keepalive) send pings every 20 s."""
    user = insert_user(server, role=role)
    jwt = token(server, user, role, int(time.time()) + 120)
    status, ticket, _ = issue(server, jwt, endpoint)
    assert status == 201, ticket

    async def run():
        async with websockets.connect(
            ws_url(server, endpoint, ticket["ticket"]),
            origin="http://localhost",
            ping_interval=None,
        ) as connection:
            for _ in range(3):
                await alive(connection)

    asyncio.run(run())
