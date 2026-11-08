#!/usr/bin/env python3
"""Compare everyday shell workloads in Quirl against dash, Bash, and Zsh.

Usage: scripts/bench-shell-workloads.py QUIRL_BINARY [RUNS]

Each workload runs as one non-interactive program: `quirl exec` for Quirl,
which uses the native executor, and `-c` for the reference shells. Every
cell is the median wall time in milliseconds over RUNS fresh processes,
including process startup. A cell reads `fail` when the shell exits
non-zero, and differing outputs are printed below the row. Results depend on
the machine and its load; compare builds on the same machine back to back.
"""
import os
import statistics
import subprocess
import sys
import time

QUIRL = sys.argv[1]
RUNS = int(sys.argv[2]) if len(sys.argv) > 2 else 7

WORKLOADS = {
    "startup": "true",
    "arith-while-10k": "i=0; s=0; while [ $i -lt 10000 ]; do s=$((s + i)); i=$((i + 1)); done; echo $s",
    "for-echo-10k": "for i in $(seq 1 10000); do echo $i; done > /dev/null",
    "func-calls-5k": "f() { x=$1; }; i=0; while [ $i -lt 5000 ]; do f $i; i=$((i + 1)); done; echo $x",
    "case-10k": "i=0; n=0; while [ $i -lt 10000 ]; do case $i in *7*) n=$((n + 1));; esac; i=$((i + 1)); done; echo $n",
    "spawn-200": "i=0; while [ $i -lt 200 ]; do /usr/bin/true; i=$((i + 1)); done",
}

SHELLS = {
    "quirl": lambda src: [QUIRL, "exec", src],
    "dash": lambda src: ["/bin/dash", "-c", src],
    "bash": lambda src: ["/bin/bash", "--norc", "--noprofile", "-c", src],
    "zsh": lambda src: ["/bin/zsh", "-f", "-c", src],
}
SHELLS = {name: argv for name, argv in SHELLS.items() if name == "quirl" or os.path.exists(argv("")[0])}


def measure(argv):
    samples = []
    output = None
    for _ in range(RUNS):
        start = time.perf_counter()
        result = subprocess.run(argv, capture_output=True, text=True, env=os.environ)
        samples.append((time.perf_counter() - start) * 1000)
        if result.returncode != 0:
            return None, result.stderr.strip()[:200]
        output = result.stdout.strip()
    return statistics.median(samples), output


def main():
    names = list(SHELLS)
    print(f"{'workload':<18}" + "".join(f"{name:>12}" for name in names))
    for workload, source in WORKLOADS.items():
        row = []
        outputs = {}
        for name in names:
            median, output = measure(SHELLS[name](source))
            outputs[name] = output
            row.append("fail" if median is None else f"{median:.1f}")
        print(f"{workload:<18}" + "".join(f"{cell:>12}" for cell in row))
        distinct = {value for value in outputs.values()}
        if len(distinct) > 1:
            print(f"   outputs differ: {outputs}")


main()
