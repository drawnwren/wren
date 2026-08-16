#!/usr/bin/env python3
"""Require Criterion's provider GPU medians to beat matching CPU medians."""

from __future__ import annotations

import argparse
import json
import math
from pathlib import Path


WORKLOADS = ("4-mib", "8-mib", "32-mib")


def median_nanos(root: Path, backend: str, workload: str) -> float:
    path = root / backend / workload / "new" / "estimates.json"
    if not path.is_file():
        raise SystemExit(f"missing Criterion result: {path}")
    report = json.loads(path.read_text(encoding="utf-8"))
    value = float(report["median"]["point_estimate"])
    if not math.isfinite(value) or value <= 0:
        raise SystemExit(f"invalid median in {path}: {value}")
    return value


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("criterion_root", type=Path)
    parser.add_argument("--minimum-speedup", type=float, default=1.5)
    arguments = parser.parse_args()
    if not math.isfinite(arguments.minimum_speedup) or arguments.minimum_speedup <= 1:
        raise SystemExit("--minimum-speedup must be finite and greater than 1")

    failed = []
    for workload in WORKLOADS:
        cpu = median_nanos(arguments.criterion_root, "cpu", workload)
        gpu = median_nanos(arguments.criterion_root, "gpu", workload)
        speedup = cpu / gpu
        print(
            f"{workload}: CPU {cpu / 1_000_000:.3f} ms, "
            f"GPU {gpu / 1_000_000:.3f} ms, {speedup:.2f}x"
        )
        if speedup < arguments.minimum_speedup:
            failed.append((workload, speedup))

    if failed:
        details = ", ".join(f"{name}={speedup:.2f}x" for name, speedup in failed)
        raise SystemExit(
            f"provider GPU speedup below {arguments.minimum_speedup:.2f}x: {details}"
        )


if __name__ == "__main__":
    main()
