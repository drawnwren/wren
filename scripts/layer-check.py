#!/usr/bin/env python3
"""Fail when workspace dependencies violate the bootstrap layer DAG."""

from __future__ import annotations

import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]

ALLOWED_INTERNAL = {
    "wren-types": set(),
    "wren-scheduling": set(),
    "wren-shmem": {"wren-types"},
    "wren-client-state": {"wren-types", "wren-view", "wren-shmem"},
    "wren-text": {"wren-types"},
    "wren-position": {"wren-types", "wren-text"},
    "wren-grammar": {"wren-types"},
    "wren-config": {"wren-types", "wren-grammar"},
    "wren-provider": {"wren-types"},
    "wren-derived": {"wren-types"},
    "wren-workflow": {"wren-types"},
    "wren-remote": {"wren-types", "wren-proto"},
    "wren-engine": {"wren-types", "wren-text", "wren-position", "wren-grammar"},
    "wren-command": {"wren-types"},
    "wren-view": {"wren-types", "wren-engine"},
    "wren-term": {"wren-view"},
    "wren-presenter": {"wren-view", "wren-term", "wren-scheduling"},
    "wren-proto": {"wren-types"},
    "wren-session": {"wren-types", "wren-text"},
    "wren-tui": {
        "wren-types",
        "wren-command",
        "wren-client-state",
        "wren-config",
        "wren-text",
        "wren-position",
        "wren-grammar",
        "wren-engine",
        "wren-view",
        "wren-workflow",
        "wren-term",
        "wren-presenter",
        "wren-provider",
        "wren-proto",
        "wren-session",
        "wren-scheduling",
    },
    "wren-sessiond": {
        "wren-session",
        "wren-proto",
        "wren-remote",
        "wren-shmem",
        "wren-types",
    },
}

DETERMINISTIC_CORE = {
    "wren-types",
    "wren-text",
    "wren-position",
    "wren-grammar",
    "wren-engine",
    "wren-view",
}
ASYNC_DEPENDENCIES = {"tokio", "async-std", "smol"}
OS_DEPENDENCIES = {
    "libc",
    "nix",
    "rustix",
    "termios",
    "filedescriptor",
    "windows",
    "windows-sys",
}


def main() -> int:
    failures: list[str] = []
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    metadata = json.loads(result.stdout)
    packages = [
        package
        for package in metadata["packages"]
        if pathlib.Path(package["manifest_path"]).is_relative_to(ROOT)
    ]
    for descriptor in packages:
        package = descriptor["name"]
        dependencies = {dependency["name"] for dependency in descriptor["dependencies"]}
        internal = {name for name in dependencies if name.startswith("wren-")}
        if package in ALLOWED_INTERNAL:
            forbidden = internal - ALLOWED_INTERNAL[package]
            if forbidden:
                failures.append(
                    f"{package}: forbidden internal dependencies: {', '.join(sorted(forbidden))}"
                )
        if package in DETERMINISTIC_CORE:
            async_found = dependencies & ASYNC_DEPENDENCIES
            os_found = dependencies & OS_DEPENDENCIES
            if async_found:
                failures.append(
                    f"{package}: async runtimes forbidden: {', '.join(sorted(async_found))}"
                )
            if os_found:
                failures.append(
                    f"{package}: direct OS dependencies forbidden: {', '.join(sorted(os_found))}"
                )

    if failures:
        print("layer check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print(f"layer check passed ({len(packages)} manifests)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
