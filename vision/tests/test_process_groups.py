# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from dataclasses import replace
import multiprocessing
import time

from test_runtime import wait_for
from test_worker import config
from worker import Status


def grouped_fixture(config, items, health, previews, measurement):
    from runtime import _report

    for block in health:
        _report(block, Status.ACTIVE, True, 1)
    while True:
        for item, block in zip(items, health):
            if item.input_id != "stalled":
                _report(block, Status.ACTIVE, True, 2)
        time.sleep(0.02)


def configuration(tmp_path, names):
    cfg = config(tmp_path)
    return replace(
        cfg,
        inputs_per_process=2,
        inputs=tuple(replace(cfg.inputs[0], input_id=name) for name in names),
        stall_timeout=0.25,
        startup_timeout=5,
    )


def test_groups_share_pid_but_keep_health_preview_and_restart_boundaries(
    tmp_path, monkeypatch
):
    import process_groups
    from runtime import Supervisor

    monkeypatch.setattr(process_groups, "group_entry", grouped_fixture)
    supervisor = Supervisor(
        configuration(tmp_path, ("healthy", "stalled", "other")), inspect_port=0
    )
    pids = set()
    try:

        def ready():
            supervisor.step()
            return all(
                s.snapshot() is not None and s.snapshot()[1] == Status.ACTIVE
                for s in supervisor.slots
            )

        wait_for(ready)
        first, second, third = supervisor.slots
        assert first.process.pid == second.process.pid != third.process.pid
        pids = {s.process.pid for s in supervisor.slots}
        assert first.health is not second.health
        channels = dict(supervisor.inspector.channels)
        old_pid, other_pid = first.process.pid, third.process.pid

        def restarted():
            supervisor.step()
            return first.process is not None and first.process.pid != old_pid

        wait_for(restarted)
        assert first.process.pid == second.process.pid
        assert third.process.pid == other_pid
        assert all(
            supervisor.inspector.channels[s.item.input_id]
            is not channels[s.item.input_id]
            for s in (first, second)
        )
        assert supervisor.inspector.channels["other"] is channels["other"]
    finally:
        supervisor.close()
    assert all(s.process is None for s in supervisor.slots)
    assert not any(c.pid in pids for c in multiprocessing.active_children())


def test_grouped_delivery_replays_existing_outboxes_without_crossing_ids(
    tmp_path, monkeypatch
):
    import asyncio
    import json
    import websockets
    import process_groups
    from runtime import Supervisor
    from configuration import Mapping
    from delivery import LocalOutbox
    from storage import input_directory
    from group_fixtures import measured_fixture_group

    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    monkeypatch.setattr(process_groups, "group_entry", measured_fixture_group)
    cfg = configuration(tmp_path, ("cam-A", "cam-B"))
    cfg = replace(
        cfg,
        stall_timeout=30,
        inputs=tuple(
            replace(
                item,
                mapping=Mapping(
                    10, 10, {item.input_id: [[0, 0], [9, 0], [9, 9], [0, 9]]}
                ),
            )
            for item in cfg.inputs
        ),
    )
    queued = {}
    outboxes = {}
    for item in cfg.inputs:
        outbox = LocalOutbox(
            input_directory(cfg.state_dir, cfg.scope, item.input_id) / "outbox.db"
        )
        outbox.enqueue(
            json.dumps(
                {
                    "type": "SPOT_UPDATE",
                    "camera_id": item.input_id,
                    "spot_id": item.input_id,
                    "status": "occupied",
                }
            )
        )
        queued[item.input_id] = outbox.first()["event_id"]
        outboxes[item.input_id] = outbox
    received = []
    lost = False

    async def scenario():
        async def gateway(ws):
            nonlocal lost
            await ws.send(json.dumps({"type": "LOCAL_READY", "delivery_version": 1}))
            async for text in ws:
                event = json.loads(text)
                received.append(event)
                if not lost:
                    lost = True
                    await ws.close()
                    return
                await ws.send(
                    json.dumps(
                        {
                            "type": "LOCAL_ACK",
                            "version": 1,
                            "event_id": event["event_id"],
                            "outcome": "persisted",
                        }
                    )
                )

        async with websockets.serve(gateway, "127.0.0.1", 0) as listener:
            local = replace(
                cfg,
                gateway_url=f"ws://127.0.0.1:{listener.sockets[0].getsockname()[1]}/ws/edge",
            )
            supervisor = Supervisor(local)
            try:
                deadline = time.monotonic() + 12
                while time.monotonic() < deadline:
                    supervisor.step()
                    ids = {event["event_id"] for event in received}
                    if (
                        set(queued.values()) <= ids
                        and len(ids) >= 4
                        and all(box.first() is None for box in outboxes.values())
                    ):
                        break
                    await asyncio.sleep(0.02)
                else:
                    raise AssertionError(
                        "grouped delivery did not drain isolated outboxes"
                    )
                assert (
                    supervisor.slots[0].process.pid == supervisor.slots[1].process.pid
                )
            finally:
                supervisor.close()

    asyncio.run(scenario())
    assert len(received) > len({event["event_id"] for event in received})
    for event in received:
        payload = event["payload"]
        assert payload["camera_id"] == payload["spot_id"]


def test_group_lease_conflict_disables_only_owner_and_preserves_peer(
    tmp_path, monkeypatch
):
    import process_groups
    from runtime import Supervisor
    from storage import FileLease, input_directory
    from group_fixtures import measured_fixture_group

    monkeypatch.setattr(process_groups, "group_entry", measured_fixture_group)
    cfg = configuration(tmp_path, ("blocked", "peer"))
    lease = input_directory(cfg.state_dir, cfg.scope, "blocked") / "worker.lock"
    supervisor = Supervisor(
        cfg,
        measurement=(
            tmp_path,
            multiprocessing.get_context("spawn").RawValue("d", 0),
            multiprocessing.get_context("spawn").RawValue("d", float("inf")),
            False,
        ),
    )
    try:
        with FileLease(lease):

            def conflict():
                supervisor.step()
                return supervisor.slots[0].disabled

            wait_for(conflict)
            assert supervisor.slots[0].last_status == Status.INVALID
            assert not supervisor.slots[1].disabled

            def peer_active():
                supervisor.step()
                snap = supervisor.slots[1].snapshot()
                return snap is not None and snap[1] == Status.ACTIVE

            wait_for(peer_active)
    finally:
        supervisor.close()


def test_group_spawn_failure_restores_environment_and_releases_channels(
    tmp_path, monkeypatch
):
    import os
    from runtime import Supervisor

    supervisor = Supervisor(configuration(tmp_path, ("a", "b")), inspect_port=0)
    original = supervisor.context
    monkeypatch.setenv("OPENBLAS_NUM_THREADS", "7")
    monkeypatch.delenv("OMP_NUM_THREADS", raising=False)

    class BrokenProcess:
        closed = False

        def start(self):
            assert os.environ["OPENBLAS_NUM_THREADS"] == "1"
            raise OSError("spawn unavailable")

        def close(self):
            self.closed = True

    process = BrokenProcess()

    class Context:
        Array = staticmethod(original.Array)
        RawValue = staticmethod(original.RawValue)
        RawArray = staticmethod(original.RawArray)
        Lock = staticmethod(original.Lock)
        Process = staticmethod(lambda **kwargs: process)

    supervisor.context = Context()
    try:
        supervisor.step()
        assert process.closed
        assert all(
            slot.process is None and slot.health is None and slot.failures == 1
            for slot in supervisor.slots
        )
        assert os.environ["OPENBLAS_NUM_THREADS"] == "7"
        assert "OMP_NUM_THREADS" not in os.environ
        assert all(value is None for value in supervisor.inspector.channels.values())
    finally:
        supervisor.close()


def failed_lease_group(config, items, health, previews, measurement):
    import process_groups

    def unavailable(*args):
        raise OSError("temporary filesystem unavailable")

    process_groups.FileLease = unavailable
    process_groups.group_entry(config, items, health, previews, measurement)


def test_transient_group_lease_io_error_retries_instead_of_disabling(
    tmp_path, monkeypatch
):
    import process_groups
    from runtime import Supervisor

    monkeypatch.setattr(process_groups, "group_entry", failed_lease_group)
    supervisor = Supervisor(configuration(tmp_path, ("a", "b")))
    try:

        def failed():
            supervisor.step()
            return any(slot.failures for slot in supervisor.slots)

        wait_for(failed)
        assert not any(slot.disabled for slot in supervisor.slots)
        assert all(
            slot.process is None and slot.retry_at > time.monotonic()
            for slot in supervisor.slots
        )
    finally:
        supervisor.close()
