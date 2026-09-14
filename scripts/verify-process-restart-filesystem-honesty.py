#!/usr/bin/env python3
"""Fail-closed honesty checks for cross-process filesystem-session fixtures.

Asserts the durable owner fixture keeps filesystem/file recovery coverage and
explicit anti-B2 non-claims. Does not claim real-driver Live FS recovery or
flip ``b2_process_session_recovery_closed``.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

FIXTURE_PATH = (
    "src/runtime/src/local_execution/oci_backend_tests/process_restart.rs"
)
MODEL_PATH = (
    "src/runtime/src/local_execution/oci_backend_tests/process_restart/model.rs"
)

FIXTURE_REQUIREMENTS: tuple[str, ...] = (
    "retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart",
    "must not be cited as closing ROADMAP B2",
    "b2_process_session_recovery_closed",
    "RuntimeOperation::File",
    "RuntimeOperation::Filesystem",
    "linux-native-live-session-qualification",
    "BoxFilesystemOp::Move",
    "BoxFilesystemOp::Remove",
    "fixture-fs-move-before-owner-kill",
    "fixture-fs-remove-before-owner-kill",
)

MODEL_REQUIREMENTS: tuple[str, ...] = (
    "directories",
    "files",
    "file_operations",
    "filesystem_operations",
)

ROADMAP_REQUIREMENTS: tuple[str, ...] = (
    "retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart",
    "mkdir + keyed upload + move/remove survive owner SIGKILL",
    "b2_process_session_recovery_closed` stays false",
    "exit gate remains open",
    "Observation matrix greened",
)

FORBIDDEN_ROADMAP: tuple[str, ...] = (
    "**Closed** by aggregated existing-host",
)

FORBIDDEN_FIXTURE: tuple[str, ...] = (
    "b2_process_session_recovery_closed=true",
    "b2_process_session_recovery_closed = true",
)


def evaluate_tree(root: Path) -> list[str]:
    failures: list[str] = []

    fixture = root / FIXTURE_PATH
    if not fixture.is_file():
        failures.append(f"missing {FIXTURE_PATH}")
    else:
        text = fixture.read_text(encoding="utf-8")
        for needle in FIXTURE_REQUIREMENTS:
            if needle not in text:
                failures.append(f"{FIXTURE_PATH} missing {needle!r}")
        for forbidden in FORBIDDEN_FIXTURE:
            if forbidden in text:
                failures.append(f"{FIXTURE_PATH} must not claim {forbidden!r}")

    model = root / MODEL_PATH
    if not model.is_file():
        failures.append(f"missing {MODEL_PATH}")
    else:
        text = model.read_text(encoding="utf-8")
        for needle in MODEL_REQUIREMENTS:
            if needle not in text:
                failures.append(f"{MODEL_PATH} missing durable field {needle!r}")

    roadmap = root / "ROADMAP.md"
    if not roadmap.is_file():
        failures.append("missing ROADMAP.md")
    else:
        text = roadmap.read_text(encoding="utf-8")
        for needle in ROADMAP_REQUIREMENTS:
            if needle not in text:
                failures.append(f"ROADMAP.md missing {needle!r}")
        for forbidden in FORBIDDEN_ROADMAP:
            if forbidden in text:
                failures.append(
                    f"ROADMAP.md must not use overclaim wording {forbidden!r}"
                )

    return failures


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        fixture_dir = root / "src/runtime/src/local_execution/oci_backend_tests"
        (fixture_dir / "process_restart").mkdir(parents=True)

        good_fixture = "\n".join(FIXTURE_REQUIREMENTS) + "\n"
        (fixture_dir / "process_restart.rs").write_text(good_fixture, encoding="utf-8")
        (fixture_dir / "process_restart" / "model.rs").write_text(
            "\n".join(MODEL_REQUIREMENTS) + "\n",
            encoding="utf-8",
        )
        (root / "ROADMAP.md").write_text(
            "\n".join(ROADMAP_REQUIREMENTS) + "\n",
            encoding="utf-8",
        )
        if evaluate_tree(root):
            print("self-test: passing fixture was rejected", file=sys.stderr)
            print("\n".join(evaluate_tree(root)), file=sys.stderr)
            return 1

        bad_roadmap = root / "ROADMAP.md"
        bad_roadmap.write_text(
            "\n".join(ROADMAP_REQUIREMENTS)
            + "\n**Closed** by aggregated existing-host\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("overclaim wording" in failure for failure in failures):
            print(
                "self-test: expected Closed overclaim wording to fail",
                file=sys.stderr,
            )
            return 1
        bad_roadmap.write_text(
            "\n".join(ROADMAP_REQUIREMENTS) + "\n",
            encoding="utf-8",
        )

        bad = fixture_dir / "process_restart.rs"
        bad.write_text(
            good_fixture.replace(
                "retained_backend_recovers_filesystem_session_after_runtime_owner_process_restart",
                "retained_backend_recovers_after_runtime_owner_process_restart",
            ),
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any(
            "retained_backend_recovers_filesystem_session" in failure
            for failure in failures
        ):
            print(
                "self-test: expected missing filesystem fixture name to fail",
                file=sys.stderr,
            )
            return 1

        bad.write_text(
            good_fixture + "b2_process_session_recovery_closed=true\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("must not claim" in failure for failure in failures):
            print(
                "self-test: expected B2 true claim to fail closed",
                file=sys.stderr,
            )
            return 1

    print("self-test: ok")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Repository root to audit (default: a3s-box checkout)",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run fail-closed fixture checks and exit",
    )
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()

    failures = evaluate_tree(args.repo_root.resolve())
    if failures:
        print(
            "process-restart filesystem honesty check failed:",
            file=sys.stderr,
        )
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("process-restart filesystem honesty check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
