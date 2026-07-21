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
    COMPAT_ROOT,
    FIXTURE_DIR,
    load_artifacts,
    prepare_python,
    prepare_typescript,
    require_executable,
)

SDK_ROOT = COMPAT_ROOT.parent.parent / "sdk"
E2B_CONNECTION_ENVIRONMENT = (
    "E2B_API_KEY",
    "E2B_API_URL",
    "E2B_DEBUG",
    "E2B_DOMAIN",
    "E2B_SANDBOX_URL",
    "E2B_VALIDATE_API_KEY",
    "E2B_VOLUME_API_URL",
)


def prepare_native_typescript(temp: Path, client: Path) -> None:
    npm = require_executable("npm")
    source = SDK_ROOT / "typescript"
    if not source.is_dir():
        raise FileNotFoundError(f"TypeScript SDK source not found: {source}")
    environment = client.parent
    build_source = temp / "a3s-typescript-sdk"
    shutil.copytree(
        source,
        build_source,
        ignore=shutil.ignore_patterns("dist", "node_modules"),
    )
    subprocess.run(
        [
            npm,
            "install",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-save",
            "--prefix",
            str(environment),
            "typescript@5.9.3",
        ],
        check=True,
    )
    compiler = require_executable(
        "tsc", str(environment / "node_modules" / ".bin")
    )
    dependencies = build_source / "node_modules"
    copied_dependencies = False
    try:
        dependencies.symlink_to(environment / "node_modules", target_is_directory=True)
    except OSError:
        # Ordinary Windows users cannot create directory symlinks unless
        # Developer Mode or SeCreateSymbolicLinkPrivilege is enabled. Keep the
        # production harness runnable in that fail-closed host posture by
        # copying only this temporary, checksum-pinned dependency tree.
        shutil.copytree(environment / "node_modules", dependencies)
        copied_dependencies = True
    try:
        subprocess.run(
            [str(compiler), "-p", "tsconfig.json"],
            cwd=build_source,
            check=True,
        )
    finally:
        if copied_dependencies:
            shutil.rmtree(dependencies)
        else:
            dependencies.unlink(missing_ok=True)
    packed = subprocess.run(
        [npm, "pack", "--ignore-scripts", "--pack-destination", str(temp)],
        cwd=build_source,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    tarball = temp / packed.splitlines()[-1]
    subprocess.run(
        [
            npm,
            "install",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-save",
            "--prefix",
            str(environment),
            str(tarball),
        ],
        check=True,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-url", required=True)
    parser.add_argument("--domain", required=True)
    parser.add_argument("--template", required=True)
    parser.add_argument("--pip-bootstrap-wheel", type=Path)
    parser.add_argument("--artifact-cache", type=Path)
    parser.add_argument(
        "--native-sdks",
        action="store_true",
        help="repeat the matrix through the repository's A3S SDK packages",
    )
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

        if args.native_sdks:
            python_env = os.environ.copy()
            api_key = python_env["E2B_API_KEY"]
            sandbox_url = python_env.get("E2B_SANDBOX_URL")
            for name in E2B_CONNECTION_ENVIRONMENT:
                python_env.pop(name, None)
            python_env.update(
                {
                    "A3S_BOX_ENDPOINT": args.api_url,
                    "A3S_BOX_DOMAIN": args.domain,
                    "A3S_BOX_API_KEY": api_key,
                }
            )
            if sandbox_url:
                python_env["A3S_BOX_SANDBOX_URL"] = sandbox_url
            python_source = SDK_ROOT / "python" / "src"
            if not python_source.is_dir():
                raise FileNotFoundError(f"Python SDK source not found: {python_source}")
            python_env["PYTHONPATH"] = os.pathsep.join(
                filter(
                    None,
                    [str(python_source), python_env.get("PYTHONPATH")],
                )
            )
            python_env["A3S_BOX_NATIVE_SDK"] = "1"
            subprocess.run(
                [str(python), str(python_client), "sync", *common],
                check=True,
                env=python_env,
            )
            subprocess.run(
                [str(python), str(python_client), "async", *common],
                check=True,
                env=python_env,
            )

            prepare_native_typescript(temp, typescript_client)
            typescript_env = python_env.copy()
            typescript_env["A3S_BOX_NATIVE_SDK"] = "1"
            subprocess.run(
                ["node", str(typescript_client), *common],
                check=True,
                env=typescript_env,
            )

    print(
        "Official production clients passed: Python sync, Python async, and "
        "TypeScript lifecycle, envd health, Filesystem operations, foreground "
        "and background commands, stdin, PTY resize, Volume control/content, "
        "bidirectional Sandbox mounts, filesystem Snapshot capture/list, "
        "source deletion, restore, active-use conflicts and deletion, and Code "
        "Interpreter execution and contexts"
        + (" through both official and A3S SDK packages" if args.native_sdks else "")
    )


if __name__ == "__main__":
    main()
