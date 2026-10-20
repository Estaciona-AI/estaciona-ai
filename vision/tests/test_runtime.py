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
    with pytest.raises(OSError, match="spawn failed"):
        supervisor.step()
    assert supervisor.slots[0].process is None
    assert process.closed
    supervisor.close()
