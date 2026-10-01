#!/usr/bin/env python3
"""Fail if build artifacts or archives are tracked in git.

Archives and prebuilt binaries are distributed via GitHub Release assets;
they must never be committed. This guards against accidental `git add` of
local build outputs (for example, stray macOS test executables at the repo
root).

Checks:
1. No tracked file has an archive suffix (.tar, .zip, ...), except the
   temporarily allowlisted vendored libkrun tarballs consumed by build.rs.
2. No tracked file at the repo root is a binary (ELF / Mach-O / PE magic).
   Files in subdirectories are not magic-inspected because vendored trees
   legitimately contain prebuilt artifacts.

Use --self-test to verify the checker against a throwaway git repository.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile

ARCHIVE_SUFFIXES = (
    ".tar",
    ".tar.gz",
    ".tgz",
    ".tar.xz",
    ".txz",
    ".xz",
    ".zip",
    ".7z",
    ".deb",
    ".pkg",
    ".dmg",
    ".msi",
)

# Known, justified exceptions: build-time inputs extracted by
# src/deps/libkrun-sys/build.rs. This allowlist is temporary — it dies with
# the libkrun-sys source-convergence work (roadmap C-1/P3.1), which replaces
# the vendored-tarball mechanism entirely.
ALLOWED_ARCHIVE_PATHS = frozenset(
    {
        "src/deps/libkrun-sys/vendor/libkrun-source.tar",
        "src/deps/libkrun-sys/vendor/krun-windows-x64.tar.xz",
    }
)

# Magic prefixes: Linux ELF, Windows PE, Mach-O (all variants), fat binaries.
BINARY_MAGICS = (
    b"\x7fELF",
    b"MZ",
    b"\xcf\xfa\xed\xfe",
    b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"\xfe\xed\xfa\xce",
    b"\xca\xfe\xba\xbe",
)


def tracked_files(cwd: str) -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", "-z"], check=True, capture_output=True, cwd=cwd
    )
    return [
        entry.decode("utf-8", "surrogateescape")
        for entry in result.stdout.split(b"\0")
        if entry
    ]


def find_violations(root: str) -> list[str]:
    violations: list[str] = []
    for path in tracked_files(root):
        normalized = path.replace("\\", "/")
        lower = normalized.lower()
        if normalized in ALLOWED_ARCHIVE_PATHS:
            continue
        if lower.endswith(ARCHIVE_SUFFIXES):
            violations.append(f"{normalized}: tracked archive (distribute via Release assets)")
            continue
        # Binary-magic check applies only to files at the repo root, where
        # accidental local build outputs historically land. Text files never
        # match the binary magic prefixes.
        if "/" in normalized:
            continue
        try:
            with open(os.path.join(root, normalized), "rb") as handle:
                magic = handle.read(4)
        except OSError:
            continue
        if magic.startswith(BINARY_MAGICS):
            violations.append(f"{normalized}: tracked executable at repo root")
    return violations


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        def git(*args: str) -> None:
            subprocess.run(
                ["git", *args], cwd=tmp, check=True, capture_output=True
            )

        git("init", "-q")
        git("config", "user.email", "self-test@example.invalid")
        git("config", "user.name", "self-test")
        with open(os.path.join(tmp, "README.md"), "w", encoding="utf-8") as handle:
            handle.write("clean\n")
        git("add", "README.md")
        git("commit", "-qm", "clean tree")

        if find_violations(tmp):
            print("self-test: false positive on clean tree")
            return 1

        with open(os.path.join(tmp, "bundle.tar"), "wb") as handle:
            handle.write(b"not really a tarball")
        with open(os.path.join(tmp, "check_spawn_sig"), "wb") as handle:
            handle.write(b"\xcf\xfa\xed\xfe" + b"\0" * 16)
        subprocess.run(["git", "add", "."], cwd=tmp, check=True, capture_output=True)

        violations = find_violations(tmp)
        expected_suffixes = ("bundle.tar", "check_spawn_sig")
        if not all(any(v.startswith(want) for v in violations) for want in expected_suffixes):
            print(f"self-test: missed expected violations, got {violations}")
            return 1
    print("self-test: ok")
    return 0


def main() -> int:
    if "--self-test" in sys.argv[1:]:
        return self_test()
    violations = find_violations(os.getcwd())
    for line in violations:
        print(f"check-no-tracked-artifacts: {line}")
    if violations:
        print(
            "check-no-tracked-artifacts: FAILED — remove tracked artifacts; "
            "distribute archives and prebuilt binaries via Release assets"
        )
        return 1
    print("check-no-tracked-artifacts: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
