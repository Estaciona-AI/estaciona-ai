# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""POSIX multi-input headless supervisor. No lab services are required."""

import argparse
import asyncio
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


def _worker_entry(config, item, health):
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

    directory = input_directory(config.state_dir, config.scope, item.input_id)
    try:
        # Lease outlives asyncio.run, including its executor shutdown.
        with FileLease(directory / "worker.lock"):
            asyncio.run(run_input(config, item, stop, report))
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
    def __init__(self, config):
        self.config = config
        # Spawn avoids inheriting CUDA sessions and capture threads across fork.
        self.context = multiprocessing.get_context("spawn")
        self.slots = [_Slot(item) for item in config.inputs]

    def _start(self, slot, now):
        slot.health = self.context.Array("d", [now, float(Status.STARTING), 0.0])
        process = self.context.Process(
            target=_worker_entry,
            args=(self.config, slot.item, slot.health),
            daemon=True,
        )
        try:
            process.start()
        except BaseException:
            process.close()
            slot.health = None
            raise
        slot.process = process
        slot.started = now
        slot.last_status = None
        print(
            f"[VISION] input={slot.item.input_id} state=starting pid={slot.process.pid}",
            flush=True,
        )

    def _stop(self, slot):
        process = slot.process
        if process is None:
            return
        if process.is_alive():
            process.terminate()
            process.join(1)
            if process.is_alive():
                process.kill()
                process.join(2)
            if process.is_alive():
                raise RuntimeError("owned vision worker could not be stopped")
        process.join()
        process.close()
        slot.process = None
        # A killed process can poison an IPC lock. Never reuse its health block.
        slot.health = None

    def _retry(self, slot, now):
        slot.failures += 1
        slot.retry_at = now + min(30.0, 2.0 ** min(slot.failures - 1, 5))
        print(f"[VISION] input={slot.item.input_id} state=backoff", flush=True)

    def step(self):
        now = time.monotonic()
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
            try:
                while not stop.is_set() and not all(
                    slot.disabled for slot in self.slots
                ):
                    self.step()
                    stop.wait(0.1)
            finally:
                self.close()
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
    args = parser.parse_args(argv)
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
            return Supervisor(config).run(stop)
        finally:
            for signum, handler in previous.items():
                signal.signal(signum, handler)
    except ConfigurationError, LeaseConflict:
        print("[VISION] invalid configuration or runtime already owned")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
