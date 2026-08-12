"""Tests for isolation benchmark host-resource accounting."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BENCH_DIR))

from isolation_benchmark.core import is_box_shim_argv0  # noqa: E402


class IsolationCoreTests(unittest.TestCase):
    def test_box_shim_detection_uses_argv0_instead_of_process_title(self) -> None:
        self.assertTrue(is_box_shim_argv0("/opt/a3s/bin/a3s-box-shim"))
        self.assertTrue(is_box_shim_argv0("a3s-box-shim"))
        self.assertFalse(is_box_shim_argv0("libkrun VM"))
        self.assertFalse(is_box_shim_argv0("/opt/a3s/bin/a3s-box"))


if __name__ == "__main__":
    unittest.main()
