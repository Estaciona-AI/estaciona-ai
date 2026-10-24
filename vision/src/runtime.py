# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""POSIX multi-input headless supervisor. No lab services are required."""

import argparse
import asyncio
from contextlib import nullcontext
from dataclasses import dataclass
import multiprocessing
import os
import signal
import threading
import time

from configuration import ConfigurationError, load_configuration
from storage import FileLease, LeaseConflict, input_directory
from worker import PermanentInputError, Status, run_input


def _report(health, status, progress, sequence):
    lock = health.get_lock()
    if not lock.acquire(timeout=0.05):
        return
    try:
        if progress:
            health[0] = time.monotonic()
        health[1] = int(status)
        health[2] = sequence
    finally:
        lock.release()


def _worker_entry(config, item, health, preview=None, measurement=None):
    os.umask(0o077)
    # Native decoder/runtime messages may contain an authenticated RTSP URL.
    # Child output is private; the supervisor emits only IDs and status codes.
    with open(os.devnull, "wb") as sink:
        os.dup2(sink.fileno(), 1)
        os.dup2(sink.fileno(), 2)
    stop = threading.Event()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda *_: stop.set())

    def report(*args):
        _report(health, *args)

    metrics, metrics_path, publish = None, None, True
    if measurement is not None:
        from metrics import Metrics

        directory, start, end, publish = measurement
        metrics = Metrics(control=start, end=end)
        metrics_path = directory / f"{item.input_id}.json"
    directory = input_directory(config.state_dir, config.scope, item.input_id)
    try:
        # Lease outlives asyncio.run, including its executor shutdown.
        with FileLease(directory / "worker.lock"):
            asyncio.run(
                run_input(
                    config,
                    item,
                    stop,
                    report,
                    preview=preview,
                    metrics=metrics,
                    metrics_path=metrics_path,
                    publish=publish,
                )
            )
        report(Status.STOPPED, True, 0)
    except PermanentInputError, ConfigurationError, LeaseConflict:
        report(Status.INVALID, False, 0)
        raise SystemExit(2) from None
    except Exception:
        report(Status.FAILED, False, 0)
        raise SystemExit(1) from None


@dataclass
class _Slot:
    item: object
    process: object = None
    health: object = None
    started: float = 0.0
    retry_at: float = 0.0
    failures: int = 0
    starts: int = 0
    last_exit_code: int | None = None
    disabled: bool = False
    last_status: object = None

    def snapshot(self):
        if self.health is None:
            return None
        lock = self.health.get_lock()
        if not lock.acquire(timeout=0.05):
            return None
        try:
            return tuple(self.health[:])
        finally:
            lock.release()


class Supervisor:
    def __init__(self, config, *, inspect_port=None, measurement=None, observer=None):
        self.config = config
        self.measurement = measurement
        self.observer = observer
        # Spawn avoids inheriting CUDA sessions and capture threads across fork.
        self.context = multiprocessing.get_context("spawn")
        self.slots = [_Slot(item) for item in config.inputs]
        self.groups = []
        if config.inputs_per_process > 1:
            from process_groups import Group

            size = config.inputs_per_process
            self.groups = [
                Group(self.slots[i : i + size]) for i in range(0, len(self.slots), size)
            ]
        self.inspect_port = inspect_port
        self.inspector = None
        if inspect_port is not None:
            from inspection import PreviewBroker

            self.inspector = PreviewBroker(
                (item.input_id for item in config.inputs),
                config.max_frame_age,
                profiles={
                    item.input_id: {
                        "model": item.detector.path.name,
                        "device": item.detector.device,
                        "target_fps": item.fps,
                    }
                    for item in config.inputs
                },
            )

    def _start(self, slot, now):
        slot.health = self.context.Array("d", [now, float(Status.STARTING), 0.0])
        args = (self.config, slot.item, slot.health)
        if self.inspector is not None:
            from inspection import PreviewChannel

            channel = PreviewChannel(self.context)
            self.inspector.register(slot.item.input_id, channel)
            args = (*args, channel)
        if self.measurement is not None:
            if len(args) == 3:
                args = (*args, None)
            args = (*args, self.measurement)
        process = self.context.Process(
            target=_worker_entry,
            args=args,
            daemon=True,
        )
        try:
            process.start()
        except BaseException:
            process.close()
            slot.health = None
            if self.inspector is not None:
                self.inspector.register(slot.item.input_id, None)
            raise
        slot.process = process
        slot.started = now
        slot.starts += 1
        slot.last_status = None
        print(
            f"[VISION] input={slot.item.input_id} state=starting pid={slot.process.pid}",
            flush=True,
        )

    @staticmethod
    def _reap(process):
        if process.is_alive():
            process.terminate()
            process.join(1)
            if process.is_alive():
                process.kill()
                process.join(2)
            if process.is_alive():
                raise RuntimeError("owned vision worker could not be stopped")
        process.join()
        code = process.exitcode
        process.close()
        return code

    def _stop(self, slot):
        if self.groups:
            from process_groups import stop_group

            return stop_group(self, slot)
        process = slot.process
        if process is None:
            return
        slot.last_exit_code = self._reap(process)
        slot.process = None
        # A killed process can poison an IPC lock. Never reuse its health block.
        slot.health = None
        if self.inspector is not None:
            self.inspector.register(slot.item.input_id, None)

    def _retry(self, slot, now):
        slot.failures += 1
        slot.retry_at = now + min(30.0, 2.0 ** min(slot.failures - 1, 5))
        print(f"[VISION] input={slot.item.input_id} state=backoff", flush=True)

    def step(self):
        now = time.monotonic()
        if self.groups:
            from process_groups import step_groups

            return step_groups(self, now)
        for slot in self.slots:
            if slot.disabled:
                continue
            if slot.process is None:
                if now >= slot.retry_at:
                    try:
                        self._start(slot, now)
                    except OSError:
                        self._retry(slot, now)
                continue
            snapshot = slot.snapshot()
            code = slot.process.exitcode
            if code is not None:
                status = (
                    int(snapshot[1]) if snapshot is not None else int(Status.FAILED)
                )
                self._stop(slot)
                if code == 2 or (code == 0 and status == Status.STOPPED):
                    slot.disabled = True
                    slot.last_status = Status.INVALID if code == 2 else Status.STOPPED
                    print(
                        f"[VISION] input={slot.item.input_id} state={'invalid' if code == 2 else 'stopped'}",
                        flush=True,
                    )
                else:
                    self._retry(slot, now)
                continue
            if snapshot is not None:
                progress, status, _ = snapshot
                if status != slot.last_status:
                    print(
                        f"[VISION] input={slot.item.input_id} state={Status(int(status)).name.lower()}",
                        flush=True,
                    )
                    slot.last_status = status
                timeout = (
                    self.config.startup_timeout
                    if status == Status.STARTING
                    else self.config.stall_timeout
                )
                stalled = now - progress > timeout
                if status == Status.ACTIVE and now - slot.started >= 30:
                    slot.failures = 0
            else:
                stalled = True
            if stalled:
                self._stop(slot)
                self._retry(slot, now)

    def _update_inspector(self):
        if self.inspector is None:
            return
        for slot in self.slots:
            status = (
                Status(int(slot.last_status)).name.lower()
                if slot.last_status is not None
                else "starting"
            )
            if slot.process is None and not slot.disabled and slot.failures:
                status = "backoff"
            self.inspector.update(slot.item.input_id, status)

    def close(self):
        errors = []
        for slot in self.slots:
            try:
                self._stop(slot)
            except Exception as error:
                errors.append(error)
        if errors:
            raise RuntimeError("failed to reap all owned vision workers") from errors[0]

    def run(self, stop):
        lease = (
            input_directory(
                self.config.state_dir, self.config.scope, "supervisor"
            ).parent
            / "runtime.lock"
        )
        with FileLease(lease):
            service = nullcontext()
            if self.inspector is not None:
                from inspector_http import InspectorServer

                service = InspectorServer(self.inspector, port=self.inspect_port)
            try:
                with service as inspector:
                    if inspector is not None:
                        print(f"[VISION] inspector={inspector.origin}", flush=True)
                    return self._run_loop(stop)
            finally:
                self.close()

    def _run_loop(self, stop):
        while not stop.is_set() and not all(slot.disabled for slot in self.slots):
            self.step()
            self._update_inspector()
            if self.observer is not None:
                self.observer(self)
            stop.wait(0.1)
        return (
            1 if any(slot.last_status == Status.INVALID for slot in self.slots) else 0
        )


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Run installation-scoped vision inputs headlessly"
    )
    parser.add_argument("configuration")
    parser.add_argument(
        "--validate",
        action="store_true",
        help="Validate local files without loading models or starting workers",
    )
    parser.add_argument(
        "--inspect",
        action="store_true",
        help="Enable the read-only inspector on 127.0.0.1 only",
    )
    parser.add_argument(
        "--inspect-port",
        type=int,
        default=None,
        help="Local inspector port (default: 8765; 0 chooses a free port)",
    )
    args = parser.parse_args(argv)
    if args.inspect_port is not None and not args.inspect:
        parser.error("--inspect-port requires --inspect")
    if args.inspect_port is not None and not 0 <= args.inspect_port <= 65535:
        parser.error("invalid inspector port")
    try:
        config = load_configuration(args.configuration)
        if args.validate:
            print(
                f"Validated {len(config.inputs)} input(s); environment={config.environment}"
            )
            return 0
        # Validate secret references before starting any worker; never print values.
        credential = os.environ.get(config.credential_env)
        if (
            not credential
            or len(credential) > 4096
            or any(ord(c) < 32 for c in credential)
        ):
            raise ConfigurationError("invalid credential environment")
        for item in config.inputs:
            item.source.resolve()
        stop = threading.Event()
        previous = {
            signum: signal.getsignal(signum)
            for signum in (signal.SIGINT, signal.SIGTERM)
        }
        try:
            for signum in previous:
                signal.signal(signum, lambda *_: stop.set())
            return Supervisor(
                config,
                inspect_port=(
                    args.inspect_port if args.inspect_port is not None else 8765
                )
                if args.inspect
                else None,
            ).run(stop)
        finally:
            for signum, handler in previous.items():
                signal.signal(signum, handler)
    except OSError:
        print("[VISION] local runtime resource unavailable")
        return 2
    except ConfigurationError, LeaseConflict:
        print("[VISION] invalid configuration or runtime already owned")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
