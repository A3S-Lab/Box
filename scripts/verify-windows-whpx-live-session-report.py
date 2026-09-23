#!/usr/bin/env python3
"""Fail-closed honesty checks for a3s.box.windows-whpx-live-session.v1 reports.

Retained-stream proof, retained-filesystem proof (keyed MakeDir / Move / Remove
+ keyed upload IDs, ListDir after reattach), and whpx_microvm_live_claimed are
required together. B2, fixture continuity claims must stay false.
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

SCHEMA = "a3s.box.windows-whpx-live-session.v1"
KEYED_FILE_UPLOAD_BEFORE = "a3s.box.live-session.keyed-file.before-owner-kill"
KEYED_MKDIR_BEFORE = "a3s.box.live-session.keyed-mkdir.before-owner-kill"
KEYED_MOVE_BEFORE = "a3s.box.live-session.keyed-move.before-owner-kill"
KEYED_REMOVE_BEFORE = "a3s.box.live-session.keyed-remove.before-owner-kill"
REQUIRED_TRUE = (
    "retained_stream_handle_proven",
    "whpx_microvm_live_claimed",
    "mkdir_before_kill",
    "move_before_kill",
    "remove_before_kill",
    "list_dir_after_reattach",
    "file_upload_before_kill",
    "file_download_after_reattach",
    "retained_filesystem_proven",
)
FORBIDDEN = (
    "fixture_stream_continuity_claimed",
    "b2_process_session_recovery_closed",
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
    if report.get("whpx_microvm_live_claimed") != report.get(
        "retained_stream_handle_proven"
    ):
        failures.append(
            "whpx_microvm_live_claimed must match retained_stream_handle_proven"
        )
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
    move_id = report.get("move_request_id")
    if move_id != KEYED_MOVE_BEFORE:
        failures.append(f"move_request_id={move_id!r} (expected {KEYED_MOVE_BEFORE!r})")
    remove_id = report.get("remove_request_id")
    if remove_id != KEYED_REMOVE_BEFORE:
        failures.append(
            f"remove_request_id={remove_id!r} (expected {KEYED_REMOVE_BEFORE!r})"
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
        "move_request_id": KEYED_MOVE_BEFORE,
        "remove_request_id": KEYED_REMOVE_BEFORE,
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
        {
            **passing_report(),
            "schema_version": "a3s.box.windows-whpx-live-session.v0",
        },
        {
            **passing_report(),
            "status": "failed",
        },
        {
            **passing_report(),
            "retained_stream_handle_proven": False,
            "whpx_microvm_live_claimed": False,
        },
        {
            **passing_report(),
            "b2_process_session_recovery_closed": True,
        },
        {
            **passing_report(),
            "list_dir_after_reattach": False,
        },
        {
            **passing_report(),
            "move_before_kill": False,
        },
        {
            **passing_report(),
            "remove_before_kill": False,
        },
        {
            **passing_report(),
            "mkdir_request_id": None,
        },
        {
            **passing_report(),
            "move_request_id": None,
        },
        {
            **passing_report(),
            "remove_request_id": None,
        },
        {
            **passing_report(),
            "file_upload_request_id": "unkeyed-or-wrong-id",
        },
        {
            **passing_report(),
            "whpx_microvm_live_claimed": False,
        },
    ]
    for case in bad_cases:
        if not evaluate(case):
            print(f"self-test: expected rejection for {case}", file=sys.stderr)
            return 1
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "report.json"
        path.write_text(json.dumps(passing_report()), encoding="utf-8")
        if main(["verify-windows-whpx-live-session-report.py", str(path)]) != 0:
            print("self-test: passing file was rejected", file=sys.stderr)
            return 1
        missing = Path(tmp) / "missing.json"
        if main(["verify-windows-whpx-live-session-report.py", str(missing)]) == 0:
            print("self-test: missing file was accepted", file=sys.stderr)
            return 1
    print("whpx live-session report verifier self-test passed")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        return self_test()
    if len(argv) != 2:
        print(
            "usage: verify-windows-whpx-live-session-report.py REPORT.json|--self-test",
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
        print("whpx live-session report honesty check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("whpx live-session report honesty check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
