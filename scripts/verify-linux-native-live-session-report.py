#!/usr/bin/env python3
"""Fail-closed honesty checks for a3s.box.linux-native-live-session.v4 reports.

Retained-stream and retained-filesystem proofs are required. B2, fixture
continuity, KVM MicroVM Live, and utility-VM claims must stay false — this gate
does not close those.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

SCHEMA = "a3s.box.linux-native-live-session.v4"
REQUIRED_TRUE = (
    "retained_stream_handle_proven",
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
    for forbidden in FORBIDDEN:
        if report.get(forbidden):
            failures.append(f"{forbidden} must stay false")
    return failures


def passing_report() -> dict:
    report = {
        "schema_version": SCHEMA,
        "status": "passed",
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
        {"schema_version": "v3", "status": "passed", "retained_stream_handle_proven": True},
        {"schema_version": SCHEMA, "status": "failed", "retained_stream_handle_proven": True},
        {"schema_version": SCHEMA, "status": "passed", "retained_stream_handle_proven": False},
        {
            "schema_version": SCHEMA,
            "status": "passed",
            "retained_stream_handle_proven": True,
            "file_upload_before_kill": True,
            "file_download_after_reattach": True,
            "retained_filesystem_proven": True,
            "b2_process_session_recovery_closed": True,
        },
        {
            "schema_version": SCHEMA,
            "status": "passed",
            "retained_stream_handle_proven": True,
            "file_upload_before_kill": True,
            "file_download_after_reattach": True,
            "retained_filesystem_proven": True,
            "fixture_stream_continuity_claimed": True,
        },
        {
            "schema_version": SCHEMA,
            "status": "passed",
            "retained_stream_handle_proven": True,
            "file_upload_before_kill": True,
            "file_download_after_reattach": False,
            "retained_filesystem_proven": True,
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
        print(f"missing live-session report: {path}", file=sys.stderr)
        return 1
    report = json.loads(path.read_text(encoding="utf-8"))
    failures = evaluate(report)
    if failures:
        print("live-session report failed honesty checks:", file=sys.stderr)
        for item in failures:
            print(f"  {item}", file=sys.stderr)
        return 1
    print(
        "live-session v4 retained-stream+filesystem proven; "
        "B2/fixture/KVM/utility-VM claims remain false"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
