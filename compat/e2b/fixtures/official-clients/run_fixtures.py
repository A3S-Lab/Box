#!/usr/bin/env python3
"""Download pinned clients and generate or verify lifecycle wire fixtures."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import venv
from pathlib import Path
from typing import Any


FIXTURE_DIR = Path(__file__).resolve().parent
COMPAT_ROOT = FIXTURE_DIR.parent.parent
SOURCE_LOCK = COMPAT_ROOT / "upstream.lock.json"
EXPECTED_REQUESTS = 9


def load_artifacts() -> dict[str, dict[str, Any]]:
    lock = json.loads(SOURCE_LOCK.read_text(encoding="utf-8"))
    return {artifact["id"]: artifact for artifact in lock["artifacts"]}


def download_artifact(artifact: dict[str, Any], destination: Path) -> None:
    with urllib.request.urlopen(artifact["url"], timeout=60) as response:
        payload = response.read()
    actual_sha256 = "sha256:" + hashlib.sha256(payload).hexdigest()
    if actual_sha256 != artifact["sha256"]:
        raise RuntimeError(
            f"artifact {artifact['id']} SHA-256 mismatch: "
            f"expected {artifact['sha256']}, got {actual_sha256}"
        )
    integrity = artifact.get("integrity")
    if integrity:
        actual_integrity = "sha512-" + base64.b64encode(
            hashlib.sha512(payload).digest()
        ).decode()
        if actual_integrity != integrity:
            raise RuntimeError(
                f"artifact {artifact['id']} npm integrity mismatch: "
                f"expected {integrity}, got {actual_integrity}"
            )
    destination.write_bytes(payload)


def prepare_python(temp: Path, artifacts: dict[str, dict[str, Any]]) -> Path:
    environment = temp / "python"
    venv.EnvBuilder(with_pip=True).create(environment)
    python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    wheels = []
    for artifact_id in ["python-e2b-wheel", "python-code-interpreter-wheel"]:
        wheel = temp / Path(artifacts[artifact_id]["url"]).name
        download_artifact(artifacts[artifact_id], wheel)
        wheels.append(str(wheel))
    env = os.environ.copy()
    env["PIP_INDEX_URL"] = "https://pypi.org/simple"
    subprocess.run(
        [str(python), "-m", "pip", "install", "--disable-pip-version-check", *wheels],
        check=True,
        env=env,
    )
    return python


def prepare_typescript(temp: Path, artifacts: dict[str, dict[str, Any]]) -> Path:
    environment = temp / "typescript"
    environment.mkdir()
    tarballs = []
    for artifact_id in [
        "typescript-e2b-tarball",
        "typescript-code-interpreter-tarball",
    ]:
        tarball = temp / Path(artifacts[artifact_id]["url"]).name
        download_artifact(artifacts[artifact_id], tarball)
        tarballs.append(str(tarball))
    subprocess.run(
        [
            "npm",
            "install",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--prefix",
            str(environment),
            *tarballs,
        ],
        check=True,
    )
    client = environment / "typescript_client.mjs"
    shutil.copyfile(FIXTURE_DIR / "typescript_client.mjs", client)
    return client


def run_client(
    mode: str,
    label: str,
    command: list[str],
    temp: Path,
    update: bool,
) -> None:
    capture = temp / f"{label}.jsonl"
    port_file = temp / f"{label}.port"
    server = subprocess.Popen(
        [
            sys.executable,
            str(FIXTURE_DIR / "mock_server.py"),
            "--capture",
            str(capture),
            "--client",
            label,
            "--port-file",
            str(port_file),
        ]
    )
    try:
        deadline = time.monotonic() + 10
        while not port_file.exists():
            if server.poll() is not None:
                raise RuntimeError(f"fixture server exited before {label} started")
            if time.monotonic() >= deadline:
                raise TimeoutError(f"fixture server did not start for {label}")
            time.sleep(0.02)
        api_url = f"http://127.0.0.1:{port_file.read_text(encoding='utf-8')}"
        subprocess.run([*command, api_url], check=True)
    finally:
        server.terminate()
        server.wait(timeout=10)

    lines = capture.read_text(encoding="utf-8").splitlines()
    if len(lines) != EXPECTED_REQUESTS:
        raise RuntimeError(
            f"{label} emitted {len(lines)} requests; expected {EXPECTED_REQUESTS}"
        )
    expected = FIXTURE_DIR / "expected" / f"{label}.jsonl"
    if update:
        expected.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(capture, expected)
    elif not expected.exists() or expected.read_bytes() != capture.read_bytes():
        actual = capture.read_text(encoding="utf-8")
        wanted = expected.read_text(encoding="utf-8") if expected.exists() else "<missing>\n"
        raise RuntimeError(
            f"{mode} fixture drift for {label}\n--- expected\n{wanted}--- actual\n{actual}"
        )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["generate", "verify"], nargs="?", default="verify")
    args = parser.parse_args()
    artifacts = load_artifacts()
    with tempfile.TemporaryDirectory(prefix="a3s-e2b-official-clients-") as directory:
        temp = Path(directory)
        python = prepare_python(temp, artifacts)
        typescript_client = prepare_typescript(temp, artifacts)
        update = args.mode == "generate"
        run_client(
            args.mode,
            "python-sync",
            [str(python), str(FIXTURE_DIR / "python_client.py"), "sync"],
            temp,
            update,
        )
        run_client(
            args.mode,
            "python-async",
            [str(python), str(FIXTURE_DIR / "python_client.py"), "async"],
            temp,
            update,
        )
        run_client(
            args.mode,
            "typescript",
            ["node", str(typescript_client)],
            temp,
            update,
        )


if __name__ == "__main__":
    main()
