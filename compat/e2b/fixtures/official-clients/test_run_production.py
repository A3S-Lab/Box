from __future__ import annotations

import unittest
from pathlib import Path


FIXTURE_DIR = Path(__file__).resolve().parent


class ProductionClientBoundaryTests(unittest.TestCase):
    def test_remote_conformance_clients_remain_official_and_unchanged(self) -> None:
        python_client = (FIXTURE_DIR / "production_python_client.py").read_text(
            encoding="utf-8"
        )
        typescript_client = (
            FIXTURE_DIR / "production_typescript_client.mjs"
        ).read_text(encoding="utf-8")

        self.assertIn("from e2b import", python_client)
        self.assertIn("from e2b_code_interpreter import", python_client)
        self.assertIn("await import('e2b')", typescript_client)
        self.assertIn("await import('@e2b/code-interpreter')", typescript_client)
        self.assertNotIn("A3S_BOX_NATIVE_SDK", python_client)
        self.assertNotIn("A3S_BOX_NATIVE_SDK", typescript_client)
        self.assertNotIn("@a3s-lab/box", typescript_client)

    def test_runner_has_no_native_remote_wrapper_mode(self) -> None:
        runner = (FIXTURE_DIR / "run_production.py").read_text(encoding="utf-8")

        self.assertNotIn("--native-sdks", runner)
        self.assertNotIn("A3S_BOX_ENDPOINT", runner)
        self.assertNotIn("A3S_BOX_API_KEY", runner)


if __name__ == "__main__":
    unittest.main()
