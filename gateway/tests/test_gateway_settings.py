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
        ({"GATEWAY_HOST": "0.0.0.0"}, "loopback"),
        ({"GATEWAY_HOST": "192.168.0.10"}, "loopback"),
        ({"GATEWAY_HOST": "gateway.internal"}, "loopback"),
        ({"GATEWAY_PORT": "70000"}, "TCP port"),
        ({"EDGE_MAX_IN_FLIGHT": "9"}, "EDGE_MAX_IN_FLIGHT"),
    ],
)
def test_unsafe_or_incomplete_settings_refuse_to_start(overrides, message):
    with pytest.raises(ValueError, match=message):
        gateway.load_settings(environment(**overrides))


@pytest.mark.parametrize("host", ["127.0.0.1", "::1", "localhost", "127.0.0.2"])
def test_every_loopback_host_is_accepted(host):
    assert gateway.load_settings(environment(GATEWAY_HOST=host)).host == host


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
        ):
            serve.return_value.__aenter__ = AsyncMock()
            serve.return_value.__aexit__ = AsyncMock(return_value=False)
            task = asyncio.create_task(gateway.main())
            await asyncio.sleep(0.05)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
        listener, host, port = serve.call_args.args
        assert (host, port) == ("127.0.0.1", 8001)
        assert sync_loop.call_args.args[3] == CLOUD
        assert sync_loop.call_args.kwargs["max_in_flight"] == 3

        # The cloud credential is refused by the local listener.
        socket = AsyncMock()
        socket.request.headers = {"Authorization": f"Bearer {CLOUD}"}
        await listener(socket)
        socket.close.assert_awaited_once_with(1008, "Unauthorized")

        socket = AsyncMock()
        socket.request.headers = {"Authorization": f"Bearer {LOCAL}"}
        socket.recv.side_effect = ConnectionError("producer left")
        await listener(socket)
        socket.close.assert_not_awaited()

    asyncio.run(run())


@pytest.mark.parametrize("header", [f"Bearer {LOCAL}x", f"Bearer {LOCAL[:-1]}", ""])
def test_authorization_rejects_near_misses(header):
    assert not gateway.is_authorized({"Authorization": header}, LOCAL)
