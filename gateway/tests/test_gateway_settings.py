# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""The cloud credential stays inside the gateway; producers use a local one."""

import asyncio
import os
from unittest.mock import AsyncMock, patch

import pytest

import gateway

CLOUD = "c" * 64
LOCAL = "l" * 32


def environment(**overrides):
    values = {"EDGE_API_KEY": CLOUD, "GATEWAY_LOCAL_KEY": LOCAL}
    values.update(overrides)
    return {key: value for key, value in values.items() if value is not None}


def private_file(tmp_path, name, contents, mode=0o600):
    path = tmp_path / name
    path.write_text(contents)
    path.chmod(mode)
    return str(path)


def test_defaults_listen_on_loopback_with_both_credentials():
    settings = gateway.load_settings(environment())
    assert (settings.host, settings.port) == ("127.0.0.1", 8001)
    assert (settings.cloud_key, settings.local_key) == (CLOUD, LOCAL)


@pytest.mark.parametrize(
    ("overrides", "message"),
    [
        ({"GATEWAY_LOCAL_KEY": None}, "GATEWAY_LOCAL_KEY"),
        ({"GATEWAY_LOCAL_KEY": "l" * 31}, "at least 32 bytes"),
        ({"GATEWAY_LOCAL_KEY": CLOUD}, "must differ"),
        ({"EDGE_API_KEY": None}, "EDGE_API_KEY"),
        ({"GATEWAY_LOCAL_KEY": "l" * 16 + " " + "l" * 16}, "whitespace"),
        ({"GATEWAY_LOCAL_KEY": "l" * 32 + "\u00a0"}, "whitespace"),
        ({"EDGE_API_KEY": CLOUD + " "}, "whitespace"),
        ({"GATEWAY_HOST": "0.0.0.0"}, "loopback"),
        ({"GATEWAY_HOST": "192.168.0.10"}, "loopback"),
        ({"GATEWAY_HOST": "gateway.internal"}, "loopback"),
        ({"GATEWAY_PORT": "70000"}, "TCP port"),
        ({"GATEWAY_PORT": "65536"}, "TCP port"),
        ({"GATEWAY_PORT": "0"}, "TCP port"),
        ({"GATEWAY_PORT": "http"}, "TCP port"),
        ({"EDGE_MAX_IN_FLIGHT": "9"}, "EDGE_MAX_IN_FLIGHT"),
        ({"EDGE_MAX_IN_FLIGHT": "eight"}, "EDGE_MAX_IN_FLIGHT"),
    ],
)
def test_unsafe_or_incomplete_settings_refuse_to_start(overrides, message):
    with pytest.raises(ValueError, match=message):
        gateway.load_settings(environment(**overrides))


@pytest.mark.parametrize("host", ["127.0.0.1", "::1", "localhost", "127.0.0.2"])
def test_every_loopback_host_is_accepted(host):
    assert gateway.load_settings(environment(GATEWAY_HOST=host)).host == host


@pytest.mark.parametrize("port", [1, 65535])
def test_port_range_is_inclusive_of_its_ends(port):
    assert gateway.load_settings(environment(GATEWAY_PORT=str(port))).port == port


def test_cloud_url_and_retention_can_be_tuned():
    settings = gateway.load_settings(environment())
    assert settings.cloud_url == "wss://api.estaciona.tech/ws/edge"

    settings = gateway.load_settings(
        environment(
            SERVER_WS_URL="wss://staging.example/ws/edge",
            GATEWAY_SYNCED_RETENTION_HOURS="0.5",
            GATEWAY_METRICS_RETENTION_DAYS="30",
            GATEWAY_MAX_DB_BYTES="1048576",
        )
    )
    assert settings.cloud_url == "wss://staging.example/ws/edge"
    assert settings.retention == gateway.Retention(
        synced_hours=0.5, metrics_days=30, max_db_bytes=1048576
    )


def test_secrets_come_from_private_files_before_the_environment(tmp_path):
    settings = gateway.load_settings(
        environment(
            EDGE_API_KEY="ignored-because-a-file-is-named",
            EDGE_API_KEY_FILE=private_file(tmp_path, "cloud", CLOUD + "\n"),
            GATEWAY_LOCAL_KEY_FILE=private_file(tmp_path, "local", LOCAL + "\r\n"),
        )
    )
    assert (settings.cloud_key, settings.local_key) == (CLOUD, LOCAL)


def test_secret_files_readable_by_others_or_owned_by_another_user_are_refused(
    tmp_path, monkeypatch
):
    shared = private_file(tmp_path, "shared", CLOUD, mode=0o640)
    with pytest.raises(ValueError, match="private regular file"):
        gateway.load_settings(environment(EDGE_API_KEY_FILE=shared))
    private = private_file(tmp_path, "private", CLOUD)
    monkeypatch.setattr(os, "geteuid", lambda: os.stat(private).st_uid + 1)
    with pytest.raises(ValueError, match="owned by this user"):
        gateway.load_settings(environment(EDGE_API_KEY_FILE=private))


def test_listener_accepts_only_the_local_key_and_the_cloud_gets_the_cloud_key(
    monkeypatch, tmp_path
):
    monkeypatch.chdir(tmp_path)
    for key, value in environment(EDGE_MAX_IN_FLIGHT="3").items():
        monkeypatch.setenv(key, value)
    monkeypatch.delenv("GATEWAY_HOST", raising=False)

    async def run():
        with (
            patch("websockets.serve") as serve,
            patch("gateway.sync_loop", new_callable=AsyncMock) as sync_loop,
            patch("gateway.retention_loop", new_callable=AsyncMock) as retention,
        ):
            serve.return_value.__aenter__ = AsyncMock()
            serve.return_value.__aexit__ = AsyncMock(return_value=False)
            task = asyncio.create_task(gateway.main())
            for _ in range(1000):
                if serve.called and sync_loop.called:
                    break
                await asyncio.sleep(0.01)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
        listener, host, port = serve.call_args.args
        assert (host, port) == ("127.0.0.1", 8001)
        db, metrics, cloud_url, cloud_key, sync_event = sync_loop.call_args.args
        assert (db, metrics) == ("local_fallback.db", "metrics.db")
        assert (cloud_url, cloud_key) == ("wss://api.estaciona.tech/ws/edge", CLOUD)
        assert sync_loop.call_args.kwargs == {"max_in_flight": 3}
        retention.assert_called_once_with(db, metrics, gateway.Retention())

        # The cloud credential is refused by the local listener.
        socket = AsyncMock()
        socket.request.headers = {"Authorization": f"Bearer {CLOUD}"}
        await listener(socket)
        socket.close.assert_awaited_once_with(1008, "Unauthorized")

        socket = AsyncMock()
        socket.request.headers = {"Authorization": f"Bearer {LOCAL}"}
        socket.recv.side_effect = [
            '{"type": "SPOT_UPDATE", "spot_id": "A-01", "status": "free"}',
            ConnectionError("producer left"),
        ]
        await listener(socket)
        socket.close.assert_not_awaited()
        # The listener stores where the sync loop reads and wakes that loop.
        assert len(gateway.pending_deliveries(db)) == 1
        assert sync_event.is_set()

        # It also enforces the storage budget.
        socket.recv.side_effect = [
            '{"type": "SPOT_UPDATE", "spot_id": "A-02", "status": "free"}'
        ]
        with patch("gateway.has_room", return_value=False) as has_room:
            await listener(socket)
        has_room.assert_called_once_with(db, metrics, gateway.Retention())
        socket.close.assert_awaited_once_with(1013, "Gateway storage full")

    asyncio.run(run())


@pytest.mark.parametrize(
    "header",
    [
        f"Bearer {LOCAL}x",
        f"Bearer {LOCAL[:-1]}",
        "",
        f"Basic {LOCAL}",
        f"bearer {LOCAL}",
        f"Bearer\t{LOCAL}",
        f"Bearer  {LOCAL}",
    ],
)
def test_authorization_rejects_near_misses(header):
    assert not gateway.is_authorized({"Authorization": header}, LOCAL)


def test_secret_files_are_bounded_at_exactly_the_limit(tmp_path):
    limit = gateway.MAX_SECRET_BYTES
    exact = private_file(tmp_path, "exact", "c" * limit)
    assert gateway.read_secret_file(exact) == "c" * limit
    larger = private_file(tmp_path, "larger", "c" * (limit + 1))
    with pytest.raises(ValueError, match=f"exceeds {limit} bytes"):
        gateway.read_secret_file(larger)
