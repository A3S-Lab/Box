#!/usr/bin/env python3
"""Fail-closed honesty checks for a3s.box.linux-sandbox-setuid-launcher-proof.v1.

Proves the operator setuid launcher gate: root-owned mode 4755, non-root proof
identity, and explicit refusal to treat CI setpriv as setuid evidence. B2 and
MicroVM cutover claims must stay false.
"""

from __future__ import annotations

import json
import re
import sys
import tempfile
from pathlib import Path

SCHEMA = "a3s.box.linux-sandbox-setuid-launcher-proof.v1"
REQUIRED_TRUE = (
    "setpriv_wrapper_unset",
    "ci_setpriv_not_claimed_as_setuid",
)
FORBIDDEN = (
    "b2_process_session_recovery_closed",
    "microvm_cutover_claimed",
)
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


def evaluate(report: dict) -> list[str]:
    failures: list[str] = []
    if report.get("schema_version") != SCHEMA:
        failures.append(f"schema_version={report.get('schema_version')!r}")
    if report.get("status") != "passed":
        failures.append(f"status={report.get('status')!r} error={report.get('error')!r}")
    for required in REQUIRED_TRUE:
        if report.get(required) is not True:
            failures.append(f"{required} is not true")
    for forbidden in FORBIDDEN:
        if report.get(forbidden):
            failures.append(f"{forbidden} must stay false")

    mode = report.get("launcher_mode")
    if mode != "4755":
        failures.append(f"launcher_mode={mode!r} must be '4755'")
    if report.get("launcher_uid") != 0:
        failures.append(f"launcher_uid={report.get('launcher_uid')!r} must be 0")
    proof_uid = report.get("proof_uid")
    if not isinstance(proof_uid, int) or proof_uid <= 0:
        failures.append(f"proof_uid={proof_uid!r} must be a positive non-root uid")
    digest = report.get("launcher_sha256")
    if not isinstance(digest, str) or not SHA256_RE.fullmatch(digest):
        failures.append("launcher_sha256 must be a 64-char lowercase hex digest")
    path = report.get("launcher_path")
    if not isinstance(path, str) or not path.startswith("/") or "/../" in path:
        failures.append(f"launcher_path={path!r} must be an absolute normalized path")
    return failures


def passing_report() -> dict:
    return {
        "schema_version": SCHEMA,
        "status": "passed",
        "launcher_path": "/usr/local/libexec/a3s-box-sandbox-oci-launcher",
        "launcher_mode": "4755",
        "launcher_uid": 0,
        "launcher_gid": 0,
        "launcher_sha256": "0" * 64,
        "proof_uid": 1000,
        "setpriv_wrapper_unset": True,
        "ci_setpriv_not_claimed_as_setuid": True,
        "b2_process_session_recovery_closed": False,
        "microvm_cutover_claimed": False,
    }


def self_test() -> int:
    if evaluate(passing_report()):
        print("self-test: passing report was rejected", file=sys.stderr)
        return 1
    bad_cases = [
        {"schema_version": "v0", "status": "passed"},
        {"schema_version": SCHEMA, "status": "failed"},
        {
            **passing_report(),
            "setpriv_wrapper_unset": False,
        },
        {
            **passing_report(),
            "ci_setpriv_not_claimed_as_setuid": False,
        },
        {
            **passing_report(),
            "b2_process_session_recovery_closed": True,
        },
        {
            **passing_report(),
            "microvm_cutover_claimed": True,
        },
        {
            **passing_report(),
            "launcher_mode": "0755",
        },
        {
            **passing_report(),
            "launcher_uid": 1000,
        },
        {
            **passing_report(),
            "proof_uid": 0,
        },
        {
            **passing_report(),
            "launcher_sha256": "deadbeef",
        },
        {
            **passing_report(),
            "launcher_path": "relative/path",
        },
    ]
    for case in bad_cases:
        if not evaluate(case):
            print(f"self-test: expected rejection for {case}", file=sys.stderr)
            return 1
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "report.json"
        path.write_text(json.dumps(passing_report()), encoding="utf-8")
        if main(["verify", str(path)]) != 0:
            print("self-test: passing file was rejected", file=sys.stderr)
            return 1
        missing = Path(tmp) / "missing.json"
        if main(["verify", str(missing)]) == 0:
            print("self-test: missing file was accepted", file=sys.stderr)
            return 1
    print("setuid-launcher proof verifier self-test passed")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        return self_test()
    if len(argv) != 2:
        print(
            "usage: verify-linux-sandbox-setuid-launcher-proof.py REPORT.json|--self-test",
            file=sys.stderr,
        )
        return 2
    path = Path(argv[1])
    if not path.is_file():
        print(f"missing setuid-launcher proof report: {path}", file=sys.stderr)
        return 1
    report = json.loads(path.read_text(encoding="utf-8"))
    failures = evaluate(report)
    if failures:
        print("setuid-launcher proof failed honesty checks:", file=sys.stderr)
        for item in failures:
            print(f"  {item}", file=sys.stderr)
        return 1
    print(
        "operator setuid launcher proven; "
        "CI setpriv / B2 / MicroVM cutover claims remain false"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
