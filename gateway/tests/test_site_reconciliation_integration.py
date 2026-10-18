"""Real restart applies maps without exposing retired spots to clients."""

import asyncio
import json
import os
import time
from datetime import datetime, timedelta, timezone

import pytest
import websockets

from test_delivery_integration import receive_ready, server as delivery_server
from test_edge_authorization_integration import deliver, edge_connection
from test_session_integration import insert_user, request_status, token, websocket_url

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"),
    reason="set ESTACIONA_TEST_SERVER and DATABASE_URL for real Rust integration",
)


def test_restart_retires_spot_without_deleting_history_or_exposing_it_in_snapshots(
    server,
):
    owner = insert_user(server)
    admin = insert_user(server, role="admin")
    server.query(
        "INSERT INTO user_occupancy_history (user_id,spot_id) VALUES (%s,'R02-A')",
        (str(owner),),
    )
    server.stop()
    (server.root / "site" / "spots_3d.json").write_text(
        json.dumps([{"id": "R07-B", "center3D": {"x": 2, "y": 0, "z": 1}}])
    )
    server.start()
    signed = token(owner, "user", int(time.time()) + 60)
    admin_token = token(admin, "admin", int(time.time()) + 60)

    async def run():
        async with (
            websockets.connect(
                websocket_url(server, "app", owner, signed),
                origin="http://localhost",
                close_timeout=0.5,
            ) as app,
            websockets.connect(
                websocket_url(server, "dashboard", admin, admin_token),
                origin="http://localhost",
                close_timeout=0.5,
            ) as dashboard,
            edge_connection(server) as edge,
        ):
            for client in (app, dashboard):
                assert json.loads(await asyncio.wait_for(client.recv(), timeout=3)) == {
                    "type": "SPOT_UPDATE",
                    "spot_id": "R07-B",
                    "status": "free",
                }
            await receive_ready(edge)
            ack = await deliver(edge)
            assert (ack["outcome"], ack["reason"]) == ("rejected", "inactive_spot")
            for client in (app, dashboard):
                with pytest.raises(asyncio.TimeoutError):
                    await asyncio.wait_for(client.recv(), timeout=0.15)

    asyncio.run(run())
    assert (
        request_status(
            server,
            "/reservations",
            signed,
            "POST",
            {
                "spot_id": "R02-A",
                "expires_at": (
                    datetime.now(timezone.utc) + timedelta(minutes=15)
                ).isoformat(),
            },
        )
        == 409
    )
    assert (
        request_status(
            server, "/spots/R02-A/status", admin_token, "PUT", {"status": "free"}
        )
        == 404
    )
    assert server.query(
        "SELECT COUNT(*) FROM user_occupancy_history WHERE spot_id='R02-A' AND released_at IS NULL"
    ) == [(1,)]
    assert server.query("SELECT id,is_active FROM spots ORDER BY id") == [
        ("R02-A", False),
        ("R07-B", True),
    ]
    assert server.query("SELECT COUNT(*) FROM site_map_revisions") == [(2,)]
