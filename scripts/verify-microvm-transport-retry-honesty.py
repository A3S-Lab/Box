#!/usr/bin/env python3
"""Fail-closed honesty checks for MicroVM guest-channel transport retries.

Asserts the production retry matrix stays wired in source and documented in
README: keyed exec, read-only + keyed mutating filesystem, keyed file upload,
and idempotent file download. Does not claim B2 close or MicroVM cutover.
"""

from __future__ import annotations

import argparse
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# (relative path, required substrings)
SOURCE_REQUIREMENTS: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "src/runtime/src/grpc/exec.rs",
        (
            "should_retry_keyed_guest_exec",
            "should_retry_guest_filesystem",
            "should_retry_guest_file_upload",
            "should_retry_guest_file_download",
            "should_retry_guest_file_transfer",
        ),
    ),
    (
        "src/runtime/src/local_execution/session.rs",
        (
            "should_retry_keyed_guest_exec",
            "should_retry_guest_filesystem",
            "should_retry_guest_file_transfer",
        ),
    ),
    (
        "src/guest/init/src/lib.rs",
        ("mod filesystem_replay", "mod file_replay"),
    ),
)

README_REQUIREMENTS: tuple[str, ...] = (
    "keyed exec",
    "read-only filesystem",
    "keyed mutating filesystem",
    "keyed file uploads",
    "file downloads",
)


def evaluate_tree(root: Path) -> list[str]:
    failures: list[str] = []
    for relative, needles in SOURCE_REQUIREMENTS:
        path = root / relative
        if not path.is_file():
            failures.append(f"missing {relative}")
            continue
        text = path.read_text(encoding="utf-8")
        for needle in needles:
            if needle not in text:
                failures.append(f"{relative} missing {needle!r}")

    readme = root / "README.md"
    if not readme.is_file():
        failures.append("missing README.md")
    else:
        readme_text = readme.read_text(encoding="utf-8")
        # Bound the claim to the MicroVM lifecycle row to avoid matching unrelated text.
        row = ""
        for line in readme_text.splitlines():
            if line.startswith("| MicroVM lifecycle |"):
                row = line.lower()
                break
        if not row:
            failures.append("README.md missing MicroVM lifecycle table row")
        else:
            for needle in README_REQUIREMENTS:
                if needle.lower() not in row:
                    failures.append(
                        f"README MicroVM lifecycle row missing {needle!r}"
                    )
    return failures


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        # Passing fixture mirrors the real matrix.
        (root / "src/runtime/src/grpc").mkdir(parents=True)
        (root / "src/runtime/src/local_execution").mkdir(parents=True)
        (root / "src/guest/init/src").mkdir(parents=True)
        (root / "src/runtime/src/grpc/exec.rs").write_text(
            "\n".join(
                [
                    "should_retry_keyed_guest_exec",
                    "should_retry_guest_filesystem",
                    "should_retry_guest_file_upload",
                    "should_retry_guest_file_download",
                    "should_retry_guest_file_transfer",
                ]
            ),
            encoding="utf-8",
        )
        (root / "src/runtime/src/local_execution/session.rs").write_text(
            "\n".join(
                [
                    "should_retry_keyed_guest_exec",
                    "should_retry_guest_filesystem",
                    "should_retry_guest_file_transfer",
                ]
            ),
            encoding="utf-8",
        )
        (root / "src/guest/init/src/lib.rs").write_text(
            "mod filesystem_replay;\nmod file_replay;\n",
            encoding="utf-8",
        )
        (root / "README.md").write_text(
            "| MicroVM lifecycle | Transport retries cover keyed exec, "
            "read-only filesystem ops, keyed mutating filesystem ops, "
            "keyed file uploads, and file downloads. |\n",
            encoding="utf-8",
        )
        if evaluate_tree(root):
            print("self-test: passing fixture was rejected", file=sys.stderr)
            print("\n".join(evaluate_tree(root)), file=sys.stderr)
            return 1

        # Fail-closed: missing download claim.
        bad_readme = root / "README.md"
        bad_readme.write_text(
            "| MicroVM lifecycle | Transport retries cover keyed exec only. |\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("file downloads" in failure for failure in failures):
            print(
                "self-test: expected missing download claim to fail",
                file=sys.stderr,
            )
            return 1

        # Restore README and remove session transfer wiring.
        bad_readme.write_text(
            "| MicroVM lifecycle | Transport retries cover keyed exec, "
            "read-only filesystem ops, keyed mutating filesystem ops, "
            "keyed file uploads, and file downloads. |\n",
            encoding="utf-8",
        )
        (root / "src/runtime/src/local_execution/session.rs").write_text(
            "should_retry_keyed_guest_exec\nshould_retry_guest_filesystem\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("should_retry_guest_file_transfer" in failure for failure in failures):
            print(
                "self-test: expected missing session file-transfer retry to fail",
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
        print("MicroVM transport-retry honesty check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("MicroVM transport-retry honesty check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
