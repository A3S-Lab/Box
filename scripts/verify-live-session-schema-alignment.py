#!/usr/bin/env python3
"""Fail-closed alignment of Live-session tip schemas across harness + verifiers.

Keeps Native/KVM example SCHEMA_VERSION strings identical to their report
verifiers, and requires ROADMAP to record that tip-schema greening is still
pending. Does not flip B2 or claim existing-host digests for tip schemas.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

NATIVE_EXAMPLE = (
    "src/runtime/examples/linux_native_live_session_qualification.rs"
)
KVM_EXAMPLE = "src/runtime/examples/linux_kvm_live_session_qualification.rs"
NATIVE_VERIFIER = "scripts/verify-linux-native-live-session-report.py"
KVM_VERIFIER = "scripts/verify-linux-kvm-live-session-report.py"

CONST_RE = re.compile(
    r'const\s+(SCHEMA_VERSION|KEYED_FILE_UPLOAD_BEFORE|KEYED_MKDIR_BEFORE)\s*:\s*&str\s*=\s*"([^"]+)"'
)
PY_SCHEMA_RE = re.compile(r'^SCHEMA\s*=\s*"([^"]+)"\s*$', re.M)
PY_CONST_RE = re.compile(
    r'^(KEYED_FILE_UPLOAD_BEFORE|KEYED_MKDIR_BEFORE)\s*=\s*"([^"]+)"\s*$',
    re.M,
)


def rust_consts(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    return {name: value for name, value in CONST_RE.findall(text)}


def py_consts(path: Path) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    out: dict[str, str] = {}
    match = PY_SCHEMA_RE.search(text)
    if match:
        out["SCHEMA"] = match.group(1)
    for name, value in PY_CONST_RE.findall(text):
        out[name] = value
    return out


def evaluate_tree(root: Path) -> list[str]:
    failures: list[str] = []
    pairs = (
        ("native", NATIVE_EXAMPLE, NATIVE_VERIFIER, "v6"),
        ("kvm", KVM_EXAMPLE, KVM_VERIFIER, "v4"),
    )
    for label, example_rel, verifier_rel, tip_tag in pairs:
        example = root / example_rel
        verifier = root / verifier_rel
        if not example.is_file():
            failures.append(f"missing {example_rel}")
            continue
        if not verifier.is_file():
            failures.append(f"missing {verifier_rel}")
            continue
        rust = rust_consts(example)
        py = py_consts(verifier)
        for key in (
            "SCHEMA_VERSION",
            "KEYED_FILE_UPLOAD_BEFORE",
            "KEYED_MKDIR_BEFORE",
        ):
            if key not in rust:
                failures.append(f"{example_rel} missing const {key}")
        if "SCHEMA" not in py:
            failures.append(f"{verifier_rel} missing SCHEMA")
        for key in ("KEYED_FILE_UPLOAD_BEFORE", "KEYED_MKDIR_BEFORE"):
            if key not in py:
                failures.append(f"{verifier_rel} missing {key}")
        if "SCHEMA_VERSION" in rust and "SCHEMA" in py:
            if rust["SCHEMA_VERSION"] != py["SCHEMA"]:
                failures.append(
                    f"{label} schema drift: example={rust['SCHEMA_VERSION']!r} "
                    f"verifier={py['SCHEMA']!r}"
                )
        for key in ("KEYED_FILE_UPLOAD_BEFORE", "KEYED_MKDIR_BEFORE"):
            if key in rust and key in py and rust[key] != py[key]:
                failures.append(
                    f"{label} {key} drift: example={rust[key]!r} verifier={py[key]!r}"
                )

        roadmap = root / "ROADMAP.md"
        if not roadmap.is_file():
            failures.append("missing ROADMAP.md")
        else:
            text = roadmap.read_text(encoding="utf-8")
            schema = rust.get("SCHEMA_VERSION", "")
            if schema and schema not in text:
                failures.append(f"ROADMAP.md missing tip schema {schema!r}")
            pending = f"greening of a {tip_tag} digest remains pending"
            if pending not in text:
                failures.append(f"ROADMAP.md missing {pending!r}")

    readme = root / "README.md"
    if not readme.is_file():
        failures.append("missing README.md")
    else:
        text = readme.read_text(encoding="utf-8")
        for forbidden in (
            "lifecycle + Native Live v4)",
            "and Native Live v4 retained stream",
            "the Native Live v4 observation gate",
            "and Native Live v4 use the production",
        ):
            if forbidden in text:
                failures.append(
                    f"README.md overclaims tip Live as {forbidden!r}; "
                    "require tip harness v6 with v4-scoped greened digests"
                )
        for required in (
            "tip harness v6",
            "greened digests remain v4-scoped",
        ):
            if required not in text:
                failures.append(f"README.md missing honesty phrase {required!r}")

    return failures


def self_test() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "src/runtime/examples").mkdir(parents=True)
        (root / "scripts").mkdir(parents=True)

        (root / NATIVE_EXAMPLE).write_text(
            'const SCHEMA_VERSION: &str = "a3s.box.linux-native-live-session.v6";\n'
            'const KEYED_FILE_UPLOAD_BEFORE: &str = "a3s.box.live-session.keyed-file.before-owner-kill";\n'
            'const KEYED_MKDIR_BEFORE: &str = "a3s.box.live-session.keyed-mkdir.before-owner-kill";\n',
            encoding="utf-8",
        )
        (root / KVM_EXAMPLE).write_text(
            'const SCHEMA_VERSION: &str = "a3s.box.linux-kvm-live-session.v4";\n'
            'const KEYED_FILE_UPLOAD_BEFORE: &str = "a3s.box.live-session.keyed-file.before-owner-kill";\n'
            'const KEYED_MKDIR_BEFORE: &str = "a3s.box.live-session.keyed-mkdir.before-owner-kill";\n',
            encoding="utf-8",
        )
        (root / NATIVE_VERIFIER).write_text(
            'SCHEMA = "a3s.box.linux-native-live-session.v6"\n'
            'KEYED_FILE_UPLOAD_BEFORE = "a3s.box.live-session.keyed-file.before-owner-kill"\n'
            'KEYED_MKDIR_BEFORE = "a3s.box.live-session.keyed-mkdir.before-owner-kill"\n',
            encoding="utf-8",
        )
        (root / KVM_VERIFIER).write_text(
            'SCHEMA = "a3s.box.linux-kvm-live-session.v4"\n'
            'KEYED_FILE_UPLOAD_BEFORE = "a3s.box.live-session.keyed-file.before-owner-kill"\n'
            'KEYED_MKDIR_BEFORE = "a3s.box.live-session.keyed-mkdir.before-owner-kill"\n',
            encoding="utf-8",
        )
        (root / "ROADMAP.md").write_text(
            "a3s.box.linux-native-live-session.v6\n"
            "greening of a v6 digest remains pending\n"
            "a3s.box.linux-kvm-live-session.v4\n"
            "greening of a v4 digest remains pending\n",
            encoding="utf-8",
        )
        (root / "README.md").write_text(
            "tip harness v6; published greened digests remain v4-scoped\n",
            encoding="utf-8",
        )
        if evaluate_tree(root):
            print("self-test: passing fixture was rejected", file=sys.stderr)
            print("\n".join(evaluate_tree(root)), file=sys.stderr)
            return 1

        bad_readme = root / "README.md"
        bad_readme.write_text(
            "lifecycle + Native Live v4) proves the route\n",
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("overclaims tip Live" in failure for failure in failures):
            print("self-test: expected README Live v4 overclaim to fail", file=sys.stderr)
            return 1
        bad_readme.write_text(
            "tip harness v6; published greened digests remain v4-scoped\n",
            encoding="utf-8",
        )

        bad = root / NATIVE_VERIFIER
        bad.write_text(
            'SCHEMA = "a3s.box.linux-native-live-session.v5"\n'
            'KEYED_FILE_UPLOAD_BEFORE = "a3s.box.live-session.keyed-file.before-owner-kill"\n'
            'KEYED_MKDIR_BEFORE = "a3s.box.live-session.keyed-mkdir.before-owner-kill"\n',
            encoding="utf-8",
        )
        failures = evaluate_tree(root)
        if not any("schema drift" in failure for failure in failures):
            print("self-test: expected schema drift to fail", file=sys.stderr)
            return 1

    print("self-test: ok")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    failures = evaluate_tree(args.repo_root.resolve())
    if failures:
        print("live-session schema alignment check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("live-session schema alignment check passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
