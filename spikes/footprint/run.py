#!/usr/bin/env python3
"""Build isolated size-optimized runtime probes and measure their peak RSS."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import time

RUNTIMES = ("lua", "luau", "rhai", "quickjs", "steel", "gluon")
MAX_RSS = re.compile(r"^\s*(\d+)\s+maximum resident set size$", re.MULTILINE)


def build(root: Path, runtime: str) -> tuple[Path, float]:
    target = root / "target" / runtime
    started = time.perf_counter()
    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "--manifest-path",
            str(root / "Cargo.toml"),
            "--no-default-features",
            "--features",
            runtime,
            "--target-dir",
            str(target),
        ],
        check=True,
    )
    return target / "release" / "quirl-footprint-spike", time.perf_counter() - started


def measure(binary: Path, environment: dict[str, str]) -> int:
    completed = subprocess.run(
        ["/usr/bin/time", "-l", str(binary)],
        check=True,
        capture_output=True,
        text=True,
        env=environment,
    )
    match = MAX_RSS.search(completed.stderr)
    if not match:
        raise RuntimeError("could not parse macOS /usr/bin/time -l output")
    return int(match.group(1))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fennel", type=Path)
    arguments = parser.parse_args()
    if platform.system() != "Darwin":
        raise RuntimeError("this footprint probe currently parses macOS /usr/bin/time -l")

    root = Path(__file__).resolve().parent
    environment = os.environ.copy()
    runtimes = list(RUNTIMES)
    if arguments.fennel:
        environment["QUIRL_FENNEL_LUA"] = str(arguments.fennel.resolve())
        runtimes.insert(2, "fennel")

    measurements = []
    for runtime in runtimes:
        binary, build_seconds = build(root, runtime)
        measurements.append(
            {
                "runtime": runtime,
                "binary_bytes": binary.stat().st_size,
                "maximum_resident_set_bytes": measure(binary, environment),
                "clean_feature_build_seconds": build_seconds,
                "external_asset_bytes": (
                    arguments.fennel.stat().st_size if runtime == "fennel" else 0
                ),
            }
        )

    json.dump(
        {
            "schema_version": 1,
            "machine": f"{platform.machine()} {platform.system()} {platform.release()}",
            "build_profile": "release, opt-level=z, thin LTO, one codegen unit, stripped",
            "measurements": measurements,
            "notes": [
                "Peak RSS includes process and system-library overhead.",
                "Each binary initializes one VM and evaluates 20 + 22 once.",
                "Build time is a warm-cache implementation-cost signal, not a user latency metric.",
            ],
        },
        fp=os.sys.stdout,
        indent=2,
    )
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
