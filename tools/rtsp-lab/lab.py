# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Local CLI for explicit RTSP scenarios and their disposable media services."""

import argparse
import json
from pathlib import Path
import sys


def main(argv=None):
    sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "vision/src"))
    from scenarios import generate
    from services import LabError, LabSession
    from storage import LeaseConflict

    parser = argparse.ArgumentParser(
        description="Synthetic RTSP laboratory; never configures production"
    )
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("generate")
    for name in ("output", "video", "model", "spots"):
        create.add_argument("--" + name, required=True, type=Path)
    create.add_argument("--inputs", type=int, default=20)
    create.add_argument("--total", type=int, default=400)
    create.add_argument("--device", choices=("cpu", "cuda"), default="cuda")
    create.add_argument("--gateway-url", default="ws://127.0.0.1:18001/ws/edge")
    for name in ("start", "stop", "status", "pause-publisher", "restart-publisher"):
        commands.add_parser(name).add_argument("directory", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "generate":
            output = generate(
                args.output,
                video=args.video,
                model=args.model,
                spots=args.spots,
                inputs=args.inputs,
                total=args.total,
                device=args.device,
                gateway_url=args.gateway_url,
            )
            print(
                f"Synthetic scenario: {output}; {args.inputs} inputs / {args.total} logical spots"
            )
        else:
            session = LabSession(args.directory)
            if args.command == "start":
                print(json.dumps(session.start()))
                print(f"RTSP environment file: {session.directory / 'stream.env'}")
            elif args.command == "stop":
                session.stop()
                print("Owned lab services stopped; scenario and outboxes preserved")
            elif args.command == "status":
                print(json.dumps(session.status()))
            elif args.command == "pause-publisher":
                session.pause_publisher()
            else:
                session.restart_publisher()
        return 0
    except (LabError, LeaseConflict, OSError, ValueError):
        print(
            "Lab operation failed; check inputs, ownership and Docker logs",
            file=sys.stderr,
        )
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
