#!/usr/bin/env python3
"""Build and benchmark the pinned official PocketPy release without vendoring it."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request

VERSION = "2.1.8"
BASE_URL = f"https://github.com/pocketpy/pocketpy/releases/download/v{VERSION}"
ASSETS = {
    "pocketpy.c": "78788bfc789c986cc67b2d1ca9cb5b6e08d4c6661cb433fcc5f99ab69b0afdf6",
    "pocketpy.h": "b27e264e6dd3a7597f75452ee7980879cfcae88b0782d0b4ce0ad9315781f7b1",
}
COLD_SAMPLES = 40
MAX_RSS = re.compile(r"^\s*(\d+)\s+maximum resident set size$", re.MULTILINE)


def download(directory: Path, name: str, expected_sha256: str) -> Path:
    destination = directory / name
    urllib.request.urlretrieve(f"{BASE_URL}/{name}", destination)
    actual = hashlib.sha256(destination.read_bytes()).hexdigest()
    if actual != expected_sha256:
        raise RuntimeError(f"checksum mismatch for {name}: {actual}")
    return destination


def percentile(values: list[float], percentile_value: int) -> float:
    values.sort()
    return values[min(len(values) * percentile_value // 100, len(values) - 1)]


def peak_rss(binary: Path) -> int | None:
    if platform.system() != "Darwin":
        return None
    completed = subprocess.run(
        ["/usr/bin/time", "-l", str(binary), "--suite"],
        check=True,
        capture_output=True,
        text=True,
    )
    match = MAX_RSS.search(completed.stderr)
    if not match:
        raise RuntimeError("could not parse macOS /usr/bin/time -l output")
    return int(match.group(1))


def main() -> int:
    compiler = os.environ.get("CC") or shutil.which("cc") or shutil.which("clang")
    if not compiler:
        raise RuntimeError("a C11 compiler is required (set CC if it is not named cc or clang)")
    source = Path(__file__).with_name("bench.c")

    with tempfile.TemporaryDirectory(prefix="quirl-pocketpy-") as temporary:
        directory = Path(temporary)
        pocketpy_c = download(directory, "pocketpy.c", ASSETS["pocketpy.c"])
        download(directory, "pocketpy.h", ASSETS["pocketpy.h"])
        binary = directory / "pocketpy-bench"
        subprocess.run(
            [
                compiler,
                "-std=c11",
                "-DNDEBUG",
                "-O3",
                f"-I{directory}",
                str(source),
                str(pocketpy_c),
                "-lm",
                "-o",
                str(binary),
            ],
            check=True,
        )

        cold = [
            float(subprocess.check_output([binary, "--cold"], text=True).strip())
            for _ in range(COLD_SAMPLES)
        ]
        suite = json.loads(subprocess.check_output([binary, "--suite"], text=True))
        suite.insert(
            0,
            {
                "runtime": "pocketpy",
                "case": "cold_start",
                "median_microseconds": percentile(cold.copy(), 50),
                "p95_microseconds": percentile(cold.copy(), 95),
                "total_milliseconds": sum(cold) / 1000.0,
            },
        )
        report = {
            "schema_version": 1,
            "runtime": "pocketpy",
            "version": VERSION,
            "samples": {
                "cold_start": COLD_SAMPLES,
                "expression_eval": 400,
                "warm_host_call": 10000,
            },
            "measurements": suite,
            "footprint": {
                "binary_bytes": binary.stat().st_size,
                "maximum_resident_set_bytes": peak_rss(binary),
            },
            "note": "Cold start is measured inside a fresh process; process launch is excluded.",
        }
        json.dump(report, sys.stdout, indent=2)
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
