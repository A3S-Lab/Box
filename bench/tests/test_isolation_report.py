"""Tests for the isolation benchmark report generator."""

from __future__ import annotations

import csv
import json
import sys
import tempfile
import unittest
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BENCH_DIR))

from isolation_report import percentile, write_summary  # noqa: E402


FIELDS = [
    "host_lane",
    "isolation",
    "mechanism",
    "iteration",
    "status",
    "elapsed_ms",
    "work_units",
    "work_unit",
    "rate_per_second",
    "rate_unit",
    "observed_value",
    "observed_unit",
    "exit_code",
    "note",
]


class IsolationReportTests(unittest.TestCase):
    def test_percentile_uses_nearest_rank(self) -> None:
        self.assertEqual(percentile([10, 20, 30, 40], 50), 20)
        self.assertEqual(percentile([10, 20, 30, 40], 95), 40)
        self.assertEqual(percentile([], 50), 0)

    def test_write_summary_preserves_latency_rate_failure_and_observation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            samples = root / "samples.csv"
            with samples.open("w", newline="", encoding="utf-8") as stream:
                writer = csv.DictWriter(stream, fieldnames=FIELDS)
                writer.writeheader()
                writer.writerows(
                    [
                        self.row(
                            iteration="1",
                            status="pass",
                            elapsed_ms="100",
                            rate_per_second="10",
                        ),
                        self.row(
                            iteration="2",
                            status="pass",
                            elapsed_ms="300",
                            rate_per_second="3.333",
                        ),
                        self.row(
                            iteration="3",
                            status="fail",
                            exit_code="1",
                            note="sample failed",
                        ),
                        self.row(
                            mechanism="idle_memory",
                            iteration="1",
                            status="pass",
                            elapsed_ms="",
                            work_units="",
                            work_unit="",
                            rate_per_second="",
                            rate_unit="",
                            observed_value="1048576",
                            observed_unit="bytes",
                        ),
                    ]
                )

            json_path = root / "summary.json"
            markdown_path = root / "summary.md"
            summaries = write_summary([samples], json_path, markdown_path)

            lifecycle = next(
                item for item in summaries if item["mechanism"] == "cold_lifecycle"
            )
            self.assertEqual(lifecycle["passed"], 2)
            self.assertEqual(lifecycle["failed"], 1)
            self.assertEqual(lifecycle["latency_ms"]["p50"], 100)
            self.assertEqual(lifecycle["latency_ms"]["p95"], 300)
            self.assertEqual(lifecycle["rate_per_second"]["p50"], 3.333)
            self.assertEqual(lifecycle["notes"], ["sample failed"])

            memory = next(
                item for item in summaries if item["mechanism"] == "idle_memory"
            )
            self.assertEqual(memory["observed"]["p50"], 1048576)
            self.assertIn("cold_lifecycle", markdown_path.read_text(encoding="utf-8"))
            self.assertEqual(len(json.loads(json_path.read_text(encoding="utf-8"))), 2)

    @staticmethod
    def row(**overrides: str) -> dict[str, str]:
        row = {
            "host_lane": "test-kvm",
            "isolation": "microvm",
            "mechanism": "cold_lifecycle",
            "iteration": "1",
            "status": "pass",
            "elapsed_ms": "100",
            "work_units": "1",
            "work_unit": "operation",
            "rate_per_second": "10",
            "rate_unit": "operations/s",
            "observed_value": "",
            "observed_unit": "",
            "exit_code": "0",
            "note": "",
        }
        row.update(overrides)
        return row


if __name__ == "__main__":
    unittest.main()
