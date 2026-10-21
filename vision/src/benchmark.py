# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Explicit local vision benchmark CLI; no automatic model downloads."""

import argparse
import math
from pathlib import Path
import signal
import threading

from benchmark_capacity import capacity_trial, can_advance
from benchmark_comparison import compare
from benchmark_profiles import load_profiles, results_directory
from configuration import ConfigurationError, load_configuration
from metrics import atomic_json


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Explicit local vision benchmarks; no automatic model downloads"
    )
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("compare", "capacity"):
        command = commands.add_parser(name)
        command.add_argument("configuration", type=Path)
        command.add_argument(
            "--output",
            type=Path,
            required=True,
            help="New private results directory, outside Git",
        )
    comparison = commands.choices["compare"]
    comparison.add_argument("--profiles", type=Path, required=True)
    comparison.add_argument("--video", type=Path, required=True)
    comparison.add_argument("--input-id", required=True)
    comparison.add_argument(
        "--mode", choices=("sequential", "simultaneous"), default="sequential"
    )
    comparison.add_argument("--frames", type=int, default=30)
    comparison.add_argument("--warmup-frames", type=int, default=3)
    comparison.add_argument("--stride", type=int, default=12)
    capacity = commands.choices["capacity"]
    capacity.add_argument("--counts", default="1,5,10,20")
    capacity.add_argument("--seconds", type=float, default=60)
    capacity.add_argument("--warmup", type=float, default=10)
    capacity.add_argument("--delivery", choices=("shadow", "gateway"), default="shadow")
    args = parser.parse_args(argv)
    stop = threading.Event()
    previous = {
        signum: signal.getsignal(signum) for signum in (signal.SIGINT, signal.SIGTERM)
    }
    try:
        for signum in previous:
            signal.signal(signum, lambda *_: stop.set())
        cfg = load_configuration(args.configuration)
        if args.command == "compare":
            if (
                not 1 <= args.frames <= 1000
                or not 0 <= args.warmup_frames <= 100
                or not 1 <= args.stride <= 1000
            ):
                raise ValueError("invalid comparison limits")
            report = compare(
                cfg,
                load_profiles(args.profiles),
                args.video,
                args.output,
                input_id=args.input_id,
                mode=args.mode,
                frames=args.frames,
                warmup=args.warmup_frames,
                stride=args.stride,
                stop=stop,
            )
            return 0 if report["completed"] else 1
        counts = [int(value) for value in args.counts.split(",")]
        if (
            not counts
            or counts != sorted(set(counts))
            or len(counts) > 8
            or min(counts) < 1
            or max(counts) > len(cfg.inputs)
            or (max(counts) > 20 and 20 not in counts)
            or not math.isfinite(args.seconds)
            or not 1 <= args.seconds <= 28800
            or not math.isfinite(args.warmup)
            or not 0 <= args.warmup <= 600
        ):
            raise ValueError("invalid capacity limits")
        args.output = results_directory(args.output)
        runs = []
        blocked = None
        previous_report = None
        for count in counts:
            if count > 20 and not can_advance(previous_report, count):
                blocked = {"inputs": count, "reason": "insufficient_measured_margin"}
                break
            report = capacity_trial(
                cfg,
                args.output / f"{count:02d}-inputs",
                count=count,
                seconds=args.seconds,
                warmup=args.warmup,
                delivery=args.delivery,
                stop=stop,
            )
            previous_report = report
            runs.append(
                {
                    "inputs": count,
                    "completed": report["completed"],
                    "vision_target_met": report["vision_target_met"],
                }
            )
            if not report["completed"] or stop.is_set():
                break
        atomic_json(
            args.output / "ramp.json",
            {
                "schema_version": 1,
                "runs": runs,
                "blocked": blocked,
                "full_pc_validated": False,
            },
        )
        return (
            0
            if len(runs) == len(counts) and all(run["completed"] for run in runs)
            else 1
        )
    except ConfigurationError, ValueError, OSError, RuntimeError:
        print(
            "[BENCHMARK] failed; inspect bounded diagnostic artifacts; no secret values logged"
        )
        return 2
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)


if __name__ == "__main__":
    raise SystemExit(main())
