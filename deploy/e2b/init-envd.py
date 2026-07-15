#!/usr/bin/env python3

import json
import os
import time
import urllib.error
import urllib.request


ENVD_URL = "http://127.0.0.1:49983"
INTERNAL_ENVIRONMENT = {
    "A3S_BOOTSTRAP_MODE",
    "A3S_EXEC_LISTENER_FD",
    "A3S_INIT_LOG_FD",
    "A3S_PTY_LISTENER_FD",
    "HOSTNAME",
    "PWD",
    "SHLVL",
    "_",
}


def wait_for_envd() -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{ENVD_URL}/health", timeout=1) as response:
                if 200 <= response.status < 300:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.05)
    raise TimeoutError("envd did not become healthy within 30 seconds")


def initialize_envd() -> None:
    environment = {
        key: value
        for key, value in os.environ.items()
        if key not in INTERNAL_ENVIRONMENT
    }
    body = json.dumps(
        {
            "defaultUser": "user",
            "defaultWorkdir": "/home/user",
            "envVars": environment,
        },
        separators=(",", ":"),
    ).encode()
    request = urllib.request.Request(
        f"{ENVD_URL}/init",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        if response.status != 204:
            raise RuntimeError(f"envd initialization returned HTTP {response.status}")


if __name__ == "__main__":
    wait_for_envd()
    initialize_envd()
