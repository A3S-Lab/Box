#!/usr/bin/env python3
"""Run pinned official clients against an already-running production service."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

from run_fixtures import (
    FIXTURE_DIR,
    load_artifacts,
    prepare_python,
    prepare_typescript,
)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-url", required=True)
    parser.add_argument("--domain", required=True)
    parser.add_argument("--template", required=True)
    parser.add_argument("--pip-bootstrap-wheel", type=Path)
    parser.add_argument("--artifact-cache", type=Path)
    args = parser.parse_args()

    if not os.environ.get("E2B_API_KEY"):
        raise RuntimeError("E2B_API_KEY is required")

    artifacts = load_artifacts()
    with tempfile.TemporaryDirectory(
        prefix="a3s-e2b-production-official-clients-"
    ) as directory:
        temp = Path(directory)
        python = prepare_python(
            temp,
            artifacts,
            args.pip_bootstrap_wheel,
            args.artifact_cache,
        )
        typescript_client = prepare_typescript(temp, artifacts, args.artifact_cache)
        shutil.copyfile(
            FIXTURE_DIR / "production_typescript_client.mjs", typescript_client
        )

        python_client = FIXTURE_DIR / "production_python_client.py"
        common = [args.api_url, args.domain, args.template]
        subprocess.run(
            [str(python), str(python_client), "sync", *common], check=True
        )
        subprocess.run(
            [str(python), str(python_client), "async", *common], check=True
        )
        subprocess.run(["node", str(typescript_client), *common], check=True)

    print(
        "Official production clients passed: Python sync, Python async, and "
        "TypeScript lifecycle, envd health, Filesystem operations, foreground "
        "and background commands, stdin, PTY resize, warm and filesystem-only "
        "pause/resume, cold-pause rootfs/process/environment/mount semantics, "
        "Volume control/content, bidirectional Sandbox mounts, filesystem "
        "Snapshot capture/list, "
        "source deletion, restore, active-use conflicts and deletion, and Code "
        "Interpreter execution and contexts"
    )


if __name__ == "__main__":
    main()
