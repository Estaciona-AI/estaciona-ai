# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Check first-party declarations and prevent the removed runtime from returning.

This consistency gate is not a legal opinion or a transitive dependency audit.
"""

from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def violations():
    failures = []
    if "Apache License\n" not in (ROOT / "LICENSE").read_text():
        failures.append("LICENSE must contain Apache License 2.0")
    for name in (
        "server/Cargo.toml",
        "vision/pyproject.toml",
        "vision/export/pyproject.toml",
        "gateway/pyproject.toml",
        "shared/pyproject.toml",
    ):
        metadata = tomllib.loads((ROOT / name).read_text())
        package = metadata.get("project", metadata.get("package", {}))
        if package.get("license") != "Apache-2.0":
            failures.append(f"{name}: missing Apache-2.0 metadata")
    paths = (
        subprocess.check_output(
            ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
            cwd=ROOT,
        )
        .decode()
        .split("\0")
    )
    for name in set(paths):
        path = ROOT / name
        if path.suffix not in (".py", ".rs") or not path.is_file():
            continue
        content = path.read_text()
        if "SPDX-License-Identifier: Apache-2.0" not in "\n".join(
            content.splitlines()[:5]
        ):
            failures.append(f"{name}: missing SPDX header")
        if any(
            line.lstrip().startswith(("#", "//"))
            and "GNU Affero General Public License" in line
            for line in content.splitlines()[:20]
        ):
            failures.append(f"{name}: obsolete copyleft declaration")
    lock = tomllib.loads((ROOT / "vision/uv.lock").read_text())
    forbidden = {"ultralytics", "ultralytics-thop", "yolov5", "yolov10"}
    for package in lock.get("package", []):
        if package["name"] in forbidden:
            failures.append(f"vision lock retains forbidden runtime: {package['name']}")
    return failures


if __name__ == "__main__":
    errors = violations()
    for error in errors:
        print(error)
    if errors:
        raise SystemExit(1)
    print("License declarations and vision runtime policy are consistent")
