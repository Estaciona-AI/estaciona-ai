# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

from dataclasses import replace
import importlib
import multiprocessing
import time

import pytest

from test_worker import config
from configuration import InputProfile, Mapping


def simulated_worker(config, item, health):
    # Real spawned children: one makes progress, the other hangs after startup.
    with health.get_lock():
        health[0] = time.monotonic()
        health[1] = 2
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if item.input_id == "healthy":
            with health.get_lock():
                health[0] = time.monotonic()
                health[2] += 1
        time.sleep(0.02)


def wait_for(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError("supervisor made no expected progress")


def test_spawned_worker_restart_is_isolated_and_children_are_reaped(
    tmp_path, monkeypatch
):
    module = importlib.import_module("runtime")
    cfg = config(tmp_path)
    base = cfg.inputs[0]
    healthy = replace(base, input_id="healthy")
    stalled = InputProfile(
        "stalled",
        base.source,
        base.detector,
        Mapping(10, 10, {"B": [[0, 0], [9, 0], [9, 9], [0, 9]]}),
    )
    cfg = replace(cfg, inputs=(healthy, stalled), stall_timeout=0.2, startup_timeout=5)
    monkeypatch.setattr(module, "_worker_entry", simulated_worker)
    supervisor = module.Supervisor(cfg)
    pids = {}
    try:

        def started():
            supervisor.step()
            return all(
                slot.process is not None and slot.snapshot()[1] == 2
                for slot in supervisor.slots
            )

        wait_for(started)
        pids = {slot.item.input_id: slot.process.pid for slot in supervisor.slots}

        def restarted():
            supervisor.step()
            return (
                supervisor.slots[1].process is not None
                and supervisor.slots[1].process.pid != pids["stalled"]
            )

        wait_for(restarted)
        assert supervisor.slots[0].process.pid == pids["healthy"]
        assert supervisor.slots[0].snapshot()[2] > 1
    finally:
        supervisor.close()
    assert all(slot.process is None for slot in supervisor.slots)
    assert not any(
        child.pid in pids.values() for child in multiprocessing.active_children()
    )


def test_cli_validation_does_not_start_workers(profile, monkeypatch, capsys):
    module = importlib.import_module("runtime")
    monkeypatch.setattr(
        module.Supervisor,
        "run",
        lambda *args: pytest.fail("validation spawned workers"),
    )
    assert module.main([str(profile[0]), "--validate"]) == 0
    assert "1 input" in capsys.readouterr().out


from test_configuration import profile as profile


def test_failed_spawn_does_not_leave_an_unstarted_process_owned(tmp_path):
    module = importlib.import_module("runtime")
    supervisor = module.Supervisor(config(tmp_path))

    class BrokenProcess:
        closed = False

        def start(self):
            raise OSError("spawn failed")

        def close(self):
            self.closed = True

    process = BrokenProcess()

    class Context:
        Array = staticmethod(supervisor.context.Array)

        @staticmethod
        def Process(**kwargs):
            return process

    supervisor.context = Context()
    supervisor.step()
    assert supervisor.slots[0].failures == 1
    assert supervisor.slots[0].retry_at > time.monotonic()
    assert supervisor.slots[0].process is None
    assert process.closed
    supervisor.close()


def test_inspector_disabled_by_default_and_explicit_port_is_loopback(
    profile, monkeypatch
):
    module = importlib.import_module("runtime")
    monkeypatch.setenv("TEST_EDGE_KEY", "disposable")
    calls = []

    class Supervisor:
        def __init__(self, config, *, inspect_port=None):
            calls.append(inspect_port)

        def run(self, stop):
            return 0

    monkeypatch.setattr(module, "Supervisor", Supervisor)
    # Reuse parsed profile while avoiding source/environment setup in this CLI test.
    cfg = config(profile[0].parent)
    monkeypatch.setattr(module, "load_configuration", lambda _: cfg)
    monkeypatch.setattr(type(cfg.inputs[0].source), "resolve", lambda _: "fixture")
    assert module.main([str(profile[0])]) == 0
    assert calls == [None]
    assert module.main([str(profile[0]), "--inspect", "--inspect-port", "0"]) == 0
    assert calls == [None, 0]


def inspected_worker(config, item, health, preview):
    from runtime import _report
    from worker import Status

    _report(health, Status.ACTIVE, True, 8)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        _report(health, Status.ACTIVE, True, 8)
        if preview.request_until.value > time.monotonic():
            preview.write(
                {"sequence": 8, "epoch": 1, "received_monotonic": time.monotonic()}
            )
        time.sleep(0.02)


def test_inspected_spawn_uses_replaceable_channel_and_reaps_worker(
    tmp_path, monkeypatch
):
    module = importlib.import_module("runtime")
    monkeypatch.setattr(module, "_worker_entry", inspected_worker)
    supervisor = module.Supervisor(config(tmp_path), inspect_port=0)
    try:
        supervisor.step()
        old = supervisor.inspector.channels["cam-A"]
        supervisor.inspector.frame("cam-A")

        def ready():
            supervisor.step()
            supervisor._update_inspector()
            return supervisor.inspector.frame("cam-A")["frame"]

        assert wait_for(ready)["sequence"] == 8
        assert supervisor.inspector.status()["inputs"][0]["status"] == "active"
        supervisor._stop(supervisor.slots[0])
        assert supervisor.inspector.frame("cam-A")["frame"] is None
        assert old.request_until.value == 0
        supervisor.step()
        assert supervisor.inspector.channels["cam-A"] is not old
        assert supervisor.inspector.frame("cam-A")["frame"] is None
    finally:
        supervisor.close()
    assert supervisor.slots[0].process is None
