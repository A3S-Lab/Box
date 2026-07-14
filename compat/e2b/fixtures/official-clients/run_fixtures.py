#!/usr/bin/env python3
"""Download pinned clients and generate or verify lifecycle wire fixtures."""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
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
DOWNLOAD_ATTEMPTS = 3
DOWNLOAD_TIMEOUT_SECONDS = 120


def load_artifacts() -> dict[str, dict[str, Any]]:
    lock = json.loads(SOURCE_LOCK.read_text(encoding="utf-8"))
    return {artifact["id"]: artifact for artifact in lock["artifacts"]}


def download_artifact(
    artifact: dict[str, Any],
    destination: Path,
    artifact_cache: Path | None,
) -> None:
    cache_path = None
    if artifact_cache:
        cache_path = artifact_cache.resolve() / Path(artifact["url"]).name
    if cache_path and cache_path.is_file():
        payload = cache_path.read_bytes()
    else:
        payload = download_url(artifact["url"])
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
    if cache_path and not cache_path.exists():
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        cache_path.write_bytes(payload)
    destination.write_bytes(payload)


def download_url(url: str) -> bytes:
    last_error: Exception | None = None
    for attempt in range(1, DOWNLOAD_ATTEMPTS + 1):
        try:
            with urllib.request.urlopen(
                url, timeout=DOWNLOAD_TIMEOUT_SECONDS
            ) as response:
                return response.read()
        except (OSError, http.client.HTTPException) as error:
            last_error = error
            if attempt < DOWNLOAD_ATTEMPTS:
                time.sleep(attempt)
    raise RuntimeError(
        f"download failed after {DOWNLOAD_ATTEMPTS} attempts: {url}"
    ) from last_error


def prepare_python(
    temp: Path,
    artifacts: dict[str, dict[str, Any]],
    pip_bootstrap_wheel: Path | None,
    artifact_cache: Path | None,
) -> Path:
    environment = temp / "python"
    python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
    wheels = []
    for artifact_id in ["python-e2b-wheel", "python-code-interpreter-wheel"]:
        wheel = temp / Path(artifacts[artifact_id]["url"]).name
        download_artifact(artifacts[artifact_id], wheel, artifact_cache)
        wheels.append(str(wheel))

    env = os.environ.copy()
    env.setdefault("PIP_INDEX_URL", "https://pypi.org/simple")
    env.setdefault("PIP_DEFAULT_TIMEOUT", "60")
    env.setdefault("PIP_RETRIES", "5")
    uv = shutil.which("uv")
    if uv:
        subprocess.run(
            [uv, "venv", "--python", sys.executable, str(environment)],
            check=True,
            env=env,
        )
        subprocess.run(
            [uv, "pip", "install", "--python", str(python), *wheels],
            check=True,
            env=env,
        )
    elif pip_bootstrap_wheel:
        bootstrap = pip_bootstrap_wheel.resolve()
        if not bootstrap.is_file():
            raise FileNotFoundError(f"pip bootstrap wheel not found: {bootstrap}")
        venv.EnvBuilder(with_pip=False).create(environment)
        bootstrap_env = env.copy()
        bootstrap_env["PYTHONPATH"] = str(bootstrap)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                *wheels,
            ],
            check=True,
            env=bootstrap_env,
        )
    else:
        venv.EnvBuilder(with_pip=True).create(environment)
        subprocess.run(
            [
                str(python),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                *wheels,
            ],
            check=True,
            env=env,
        )
    return python


def prepare_typescript(
    temp: Path,
    artifacts: dict[str, dict[str, Any]],
    artifact_cache: Path | None,
) -> Path:
    environment = temp / "typescript"
    environment.mkdir()
    tarballs = []
    for artifact_id in [
        "typescript-e2b-tarball",
        "typescript-code-interpreter-tarball",
    ]:
        tarball = temp / Path(artifacts[artifact_id]["url"]).name
        download_artifact(artifacts[artifact_id], tarball, artifact_cache)
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
    parser.add_argument(
        "--pip-bootstrap-wheel",
        type=Path,
        help="use this pip wheel when uv and ensurepip are unavailable",
    )
    parser.add_argument(
        "--artifact-cache",
        type=Path,
        help="reuse verified SDK artifacts from this directory",
    )
    args = parser.parse_args()
    artifacts = load_artifacts()
    with tempfile.TemporaryDirectory(prefix="a3s-e2b-official-clients-") as directory:
        temp = Path(directory)
        python = prepare_python(
            temp,
            artifacts,
            args.pip_bootstrap_wheel,
            args.artifact_cache,
        )
        typescript_client = prepare_typescript(temp, artifacts, args.artifact_cache)
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
