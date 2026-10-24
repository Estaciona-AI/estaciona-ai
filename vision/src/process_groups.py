# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Optional shared process boundaries; every input retains independent IO/state."""

import asyncio
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
from dataclasses import dataclass
import os
import signal
import threading

from configuration import ConfigurationError
from storage import FileLease, LeaseConflict, input_directory
from worker import PermanentInputError, Status, run_input


@dataclass
class Group:
    slots: list
    process: object = None


def group_entry(config, items, health, previews, measurement):
    from runtime import _report
    import cv2

    os.umask(0o077)
    with open(os.devnull, "wb") as sink:
        os.dup2(sink.fileno(), 1)
        os.dup2(sink.fileno(), 2)
    cv2.setNumThreads(1)
    stop = threading.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: stop.set())
    failed = False

    async def run_group():
        nonlocal failed
        # Bound Python blocking work. Decoder and ORT native threads are separate.
        asyncio.get_running_loop().set_default_executor(
            ThreadPoolExecutor(
                max_workers=4 * len(items), thread_name_prefix="vision-group"
            )
        )
        local_stops = [threading.Event() for _ in items]

        async def one(index):
            nonlocal failed
            item, block = items[index], health[index]

            def report(*args):
                _report(block, *args)

            metrics, metrics_path, publish = None, None, True
            if measurement is not None:
                from metrics import Metrics

                directory, start, end, publish = measurement
                metrics = Metrics(control=start, end=end)
                metrics_path = directory / f"{item.input_id}.json"
            try:
                await run_input(
                    config,
                    item,
                    local_stops[index],
                    report,
                    preview=previews[index],
                    metrics=metrics,
                    metrics_path=metrics_path,
                    publish=publish,
                    capture_threads=1,
                )
                if not stop.is_set():
                    report(Status.STOPPED, True, 0)
            except PermanentInputError, ConfigurationError, LeaseConflict:
                report(Status.INVALID, False, 0)
                failed = True
                stop.set()
            except Exception:
                report(Status.FAILED, False, 0)
                failed = True
                stop.set()

        tasks = [asyncio.create_task(one(i)) for i in range(len(items))]
        try:
            while not all(task.done() for task in tasks):
                if stop.is_set():
                    for local in local_stops:
                        local.set()
                await asyncio.sleep(0.05)
            await asyncio.gather(*tasks)
        finally:
            for local in local_stops:
                local.set()
            # Do not release leases while native executor work is still running.
            await asyncio.gather(*tasks, return_exceptions=True)

    with ExitStack() as leases:
        for item, block in zip(items, health):
            try:
                directory = input_directory(
                    config.state_dir, config.scope, item.input_id
                )
                leases.enter_context(FileLease(directory / "worker.lock"))
            except LeaseConflict:
                _report(block, Status.INVALID, False, 0)
                raise SystemExit(2) from None
            except OSError:
                _report(block, Status.FAILED, False, 0)
                raise SystemExit(1) from None
        asyncio.run(run_group())
    raise SystemExit(1 if failed else 0)


def start_group(supervisor, group, now):
    from inspection import PreviewChannel

    active = [slot for slot in group.slots if not slot.disabled]
    if not active:
        return
    channels = []
    for slot in active:
        slot.health = supervisor.context.Array("d", [now, float(Status.STARTING), 0.0])
        channel = PreviewChannel(supervisor.context) if supervisor.inspector else None
        channels.append(channel)
        if supervisor.inspector:
            supervisor.inspector.register(slot.item.input_id, channel)
    process = supervisor.context.Process(
        target=group_entry,
        args=(
            supervisor.config,
            tuple(s.item for s in active),
            tuple(s.health for s in active),
            tuple(channels),
            supervisor.measurement,
        ),
        daemon=True,
    )
    # Set before spawn imports NumPy; restore the parent's environment afterwards.
    keys = ("OPENBLAS_NUM_THREADS", "OMP_NUM_THREADS", "MKL_NUM_THREADS")
    previous = {key: os.environ.get(key) for key in keys}
    try:
        for key in keys:
            os.environ[key] = "1"
        process.start()
    except BaseException:
        process.close()
        for slot in active:
            slot.health = None
            if supervisor.inspector:
                supervisor.inspector.register(slot.item.input_id, None)
        raise
    finally:
        for key, value in previous.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    group.process = process
    for slot in active:
        slot.process, slot.started = process, now
        slot.starts += 1
        slot.last_status = None
        print(
            f"[VISION] input={slot.item.input_id} state=starting pid={process.pid}",
            flush=True,
        )


def stop_group(supervisor, slot):
    group = next(
        group for group in supervisor.groups if any(s is slot for s in group.slots)
    )
    if group.process is None:
        return
    code = supervisor._reap(group.process)
    group.process = None
    for member in group.slots:
        if member.process is not None:
            member.last_exit_code = code
        member.process, member.health = None, None
        if supervisor.inspector:
            supervisor.inspector.register(member.item.input_id, None)


def step_groups(supervisor, now):
    for group in supervisor.groups:
        active = [slot for slot in group.slots if not slot.disabled]
        if not active:
            continue
        if group.process is None:
            if now >= max(slot.retry_at for slot in active):
                try:
                    start_group(supervisor, group, now)
                except OSError:
                    for slot in active:
                        supervisor._retry(slot, now)
            continue
        code = group.process.exitcode
        restart = code is not None
        for slot in active:
            snapshot = slot.snapshot()
            if snapshot is None:
                restart = True
                continue
            progress, status, _ = snapshot
            status = Status(int(status))
            if status in (Status.STOPPED, Status.INVALID):
                slot.disabled = True
                slot.last_status = status
                if status == Status.INVALID:
                    restart = True
                continue
            if status != slot.last_status:
                print(
                    f"[VISION] input={slot.item.input_id} state={status.name.lower()}",
                    flush=True,
                )
                slot.last_status = status
            timeout = (
                supervisor.config.startup_timeout
                if status == Status.STARTING
                else supervisor.config.stall_timeout
            )
            if now - progress > timeout:
                restart = True
            if status == Status.ACTIVE and now - slot.started >= 30:
                slot.failures = 0
        if restart or all(slot.disabled for slot in group.slots):
            stop_group(supervisor, group.slots[0])
            for slot in group.slots:
                if not slot.disabled:
                    supervisor._retry(slot, now)
