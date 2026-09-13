#!/usr/bin/env python3
"""Fail-closed honesty checks for a3s.box.linux-native-live-session.v6 reports.

Retained-stream and retained-filesystem proofs are required, including stable
keyed MakeDir and keyed file-upload request IDs plus ListDir after reattach.
B2, fixture continuity, KVM MicroVM Live, and utility-VM claims must stay
false — this gate does not close those.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

SCHEMA = "a3s.box.linux-native-live-session.v6"
KEYED_FILE_UPLOAD_BEFORE = "a3s.box.live-session.keyed-file.before-owner-kill"
KEYED_MKDIR_BEFORE = "a3s.box.live-session.keyed-mkdir.before-owner-kill"
REQUIRED_TRUE = (
    "retained_stream_handle_proven",
    "mkdir_before_kill",
    "list_dir_after_reattach",
    "file_upload_before_kill",
    "file_download_after_reattach",
    "retained_filesystem_proven",
)
FORBIDDEN = (
    "fixture_stream_continuity_claimed",
    "b2_process_session_recovery_closed",
    "kvm_microvm_live_claimed",
    "utility_vm_claimed",
)


def evaluate(report: dict) -> list[str]:
    failures: list[str] = []
    if report.get("schema_version") != SCHEMA:
        failures.append(f"schema_version={report.get('schema_version')!r}")
    if report.get("status") != "passed":
        failures.append(f"status={report.get('status')!r} error={report.get('error')!r}")
    for required in REQUIRED_TRUE:
        if report.get(required) is not True:
            failures.append(f"{required} is not true")
    upload_id = report.get("file_upload_request_id")
    if upload_id != KEYED_FILE_UPLOAD_BEFORE:
        failures.append(
            f"file_upload_request_id={upload_id!r} "
            f"(expected {KEYED_FILE_UPLOAD_BEFORE!r})"
        )
    mkdir_id = report.get("mkdir_request_id")
    if mkdir_id != KEYED_MKDIR_BEFORE:
        failures.append(
            f"mkdir_request_id={mkdir_id!r} (expected {KEYED_MKDIR_BEFORE!r})"
        )
    for forbidden in FORBIDDEN:
        if report.get(forbidden):
            failures.append(f"{forbidden} must stay false")
    return failures


def passing_report() -> dict:
    report = {
        "schema_version": SCHEMA,
        "status": "passed",
        "file_upload_request_id": KEYED_FILE_UPLOAD_BEFORE,
        "mkdir_request_id": KEYED_MKDIR_BEFORE,
    }
    for required in REQUIRED_TRUE:
        report[required] = True
    for forbidden in FORBIDDEN:
        report[forbidden] = False
    return report


def self_test() -> int:
    if evaluate(passing_report()):
        print("self-test: passing report was rejected", file=sys.stderr)
        return 1
    bad_cases = [
        {"schema_version": "v5", "status": "passed", "retained_stream_handle_proven": True},
        {"schema_version": SCHEMA, "status": "failed", "retained_stream_handle_proven": True},
        {"schema_version": SCHEMA, "status": "passed", "retained_stream_handle_proven": False},
        {
            **passing_report(),
            "b2_process_session_recovery_closed": True,
        },
        {
            **passing_report(),
            "fixture_stream_continuity_claimed": True,
        },
        {
            **passing_report(),
            "list_dir_after_reattach": False,
        },
        {
            **passing_report(),
            "mkdir_before_kill": False,
        },
        {
            **passing_report(),
            "mkdir_request_id": None,
        },
        {
            **passing_report(),
            "file_upload_request_id": None,
        },
        {
            **passing_report(),
            "file_upload_request_id": "unkeyed-or-wrong-id",
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
    print("live-session report verifier self-test passed")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        return self_test()
    if len(argv) != 2:
        print(
            "usage: verify-linux-native-live-session-report.py REPORT.json|--self-test",
            file=sys.stderr,
        )
        return 2
    path = Path(argv[1])
    if not path.is_file():
        print(f"missing report: {path}", file=sys.stderr)
        return 1
    report = json.loads(path.read_text(encoding="utf-8"))
    failures = evaluate(report)
    if failures:
        print("native live-session report honesty check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("native live-session report honesty check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
