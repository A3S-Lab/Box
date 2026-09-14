#!/usr/bin/env python3
"""Fail-closed honesty for mutating filesystem request_id surfaces.

Locks Rust MutateInfo / bridge success JSON, language SDK MutateInfo fields,
and language-SDK Unavailable-retry + success tests so MakeDir/Move/Remove
cannot silently regress to void/ok-only returns. Does not flip B2 or invent
Live digests.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

SURFACE_REQUIREMENTS: dict[str, tuple[str, ...]] = {
    "src/sdk/src/sandbox/filesystem.rs": (
        "pub struct MutateInfo",
        "request_id",
        "MutateInfo { request_id }",
    ),
    "src/sdk/src/bridge.rs": (
        'json!({ "ok": true, "request_id": result.request_id })',
    ),
    "sdk/go/models.go": (
        "type MutateInfo struct",
        'RequestID string `json:"request_id"`',
    ),
    "sdk/go/files.go": (
        "filesystem mutate result is missing request_id",
    ),
    "sdk/python/src/a3s_box/models.py": (
        "class MutateInfo",
        "request_id",
    ),
    "sdk/typescript/src/sandbox.ts": (
        "export interface MutateInfo",
        "requestId: string",
        "requiredString(result, 'request_id')",
    ),
}

TEST_REQUIREMENTS: dict[str, tuple[str, ...]] = {
    "src/sdk/src/sandbox/tests.rs": (
        "filesystem_make_dir_unavailable_preserves_request_id_for_retry",
        "filesystem_mutate_success_returns_minted_request_id",
    ),
    "sdk/go/sandbox_test.go": (
        "TestFilesystemMakeDirUnavailablePreservesRequestIDForRetry",
        "TestFilesystemMutateSuccessReturnsRequestID",
    ),
    "sdk/python/tests/test_sdk.py": (
        "test_filesystem_make_dir_unavailable_preserves_request_id_for_retry",
        "test_filesystem_mutate_success_returns_request_id",
    ),
    "sdk/typescript/tests/exports.mjs": (
        "UnavailableOnceMakeDirRuntime",
        "recoveredMakeDir.requestId",
        "mkdir.requestId",
        "moved.requestId",
        "removed.requestId",
    ),
}

FORBIDDEN_ANYWHERE: tuple[str, ...] = (
    "b2_process_session_recovery_closed=true",
    "b2_process_session_recovery_closed = true",
)

CHANGELOG_REQUIREMENTS: tuple[str, ...] = (
    "Does **not** flip B2",
)


def evaluate_tree(root: Path) -> list[str]:
    failures: list[str] = []

    for rel, needles in SURFACE_REQUIREMENTS.items():
        path = root / rel
        if not path.is_file():
            failures.append(f"missing {rel}")
            continue
        text = path.read_text(encoding="utf-8")
        for needle in needles:
            if needle not in text:
                failures.append(f"{rel} missing {needle!r}")

    for rel, needles in TEST_REQUIREMENTS.items():
        path = root / rel
        if not path.is_file():
            failures.append(f"missing {rel}")
            continue
        text = path.read_text(encoding="utf-8")
        for needle in needles:
            if needle not in text:
                failures.append(f"{rel} missing {needle!r}")

    changelog = root / "CHANGELOG.md"
    if not changelog.is_file():
        failures.append("missing CHANGELOG.md")
    else:
        text = changelog.read_text(encoding="utf-8")
        # Tip Unreleased notes for mutate request_id must keep anti-B2 language.
        head = text[:5000]
        if "MutateInfo" in head or "mutating filesystem" in head.lower():
            for needle in CHANGELOG_REQUIREMENTS:
                if needle not in head:
                    failures.append(
                        f"CHANGELOG.md missing {needle!r} near tip mutate notes"
                    )
        for forbidden in FORBIDDEN_ANYWHERE:
            if forbidden in text:
                failures.append(f"CHANGELOG.md must not claim {forbidden!r}")

    return failures


def _write_tree(root: Path, contents: dict[str, str]) -> None:
    for rel, body in contents.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")


def self_test() -> int:
    good_surfaces = {
        rel: "\n".join(needles) + "\n"
        for rel, needles in SURFACE_REQUIREMENTS.items()
    }
    good_tests = {
        rel: "\n".join(needles) + "\n" for rel, needles in TEST_REQUIREMENTS.items()
    }
    good_changelog = (
        "## [Unreleased]\n\n- Mutating filesystem request_id. Does **not** flip B2.\n"
    )

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        _write_tree(root, {**good_surfaces, **good_tests, "CHANGELOG.md": good_changelog})
        if evaluate_tree(root):
            print("self-test: passing tree was rejected", file=sys.stderr)
            print("\n".join(evaluate_tree(root)), file=sys.stderr)
            return 1

        # Drop a language test marker.
        go_test = root / "sdk/go/sandbox_test.go"
        go_test.write_text(
            good_tests["sdk/go/sandbox_test.go"].replace(
                "TestFilesystemMutateSuccessReturnsRequestID",
                "TestFilesystemMutateSuccessOmitsRequestID",
            ),
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("TestFilesystemMutateSuccessReturnsRequestID" in f for f in failures):
            print(
                "self-test: expected missing Go success test to fail",
                file=sys.stderr,
            )
            return 1
        go_test.write_text(good_tests["sdk/go/sandbox_test.go"], encoding="utf-8")

        # Regress bridge success JSON.
        bridge = root / "src/sdk/src/bridge.rs"
        bridge.write_text(
            good_surfaces["src/sdk/src/bridge.rs"].replace(
                'json!({ "ok": true, "request_id": result.request_id })',
                'json!({ "ok": true })',
            ),
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("bridge.rs" in f for f in failures):
            print(
                "self-test: expected bridge ok-only regression to fail",
                file=sys.stderr,
            )
            return 1
        bridge.write_text(good_surfaces["src/sdk/src/bridge.rs"], encoding="utf-8")

        changelog = root / "CHANGELOG.md"
        changelog.write_text(
            good_changelog + "b2_process_session_recovery_closed=true\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("must not claim" in f for f in failures):
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
        print("fs mutate request_id honesty check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("fs mutate request_id honesty check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
