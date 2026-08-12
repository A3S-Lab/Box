#!/usr/bin/env python3
"""Summarize A3S Box isolation-mechanism benchmark CSV files."""

from __future__ import annotations

import argparse
import csv
import json
import math
import statistics
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


def percentile(values: list[float], percent: int) -> float:
    """Return a nearest-rank percentile."""
    if not values:
        return 0.0
    ordered = sorted(values)
    index = max(0, math.ceil(len(ordered) * percent / 100) - 1)
    return ordered[index]


def rounded(value: float) -> float:
    return round(value, 3)


def read_rows(paths: Iterable[Path]) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    for path in paths:
        with path.open(newline="", encoding="utf-8") as stream:
            rows.extend(csv.DictReader(stream))
    return rows


def summarize(rows: list[dict[str, str]]) -> list[dict[str, Any]]:
    groups: dict[tuple[str, str, str], list[dict[str, str]]] = defaultdict(list)
    for row in rows:
        groups[(row["host_lane"], row["isolation"], row["mechanism"])].append(row)

    summaries: list[dict[str, Any]] = []
    for (host_lane, isolation, mechanism), group in sorted(groups.items()):
        passed = [row for row in group if row["status"] == "pass"]
        failed = [row for row in group if row["status"] == "fail"]
        skipped = [row for row in group if row["status"] == "skip"]
        elapsed = [
            float(row["elapsed_ms"])
            for row in passed
            if row.get("elapsed_ms", "").strip()
        ]
        rates = [
            float(row["rate_per_second"])
            for row in passed
            if row.get("rate_per_second", "").strip()
        ]
        observed = [
            float(row["observed_value"])
            for row in passed
            if row.get("observed_value", "").strip()
        ]
        summary: dict[str, Any] = {
            "host_lane": host_lane,
            "isolation": isolation,
            "mechanism": mechanism,
            "samples": len(group),
            "passed": len(passed),
            "failed": len(failed),
            "skipped": len(skipped),
            "elapsed_unit": "ms",
            "work_unit": next(
                (row["work_unit"] for row in passed if row.get("work_unit")), ""
            ),
            "rate_unit": next(
                (row["rate_unit"] for row in passed if row.get("rate_unit")), ""
            ),
            "observed_unit": next(
                (
                    row["observed_unit"]
                    for row in passed
                    if row.get("observed_unit")
                ),
                "",
            ),
        }
        if elapsed:
            summary["latency_ms"] = {
                "mean": rounded(statistics.fmean(elapsed)),
                "p50": rounded(percentile(elapsed, 50)),
                "p95": rounded(percentile(elapsed, 95)),
                "min": rounded(min(elapsed)),
                "max": rounded(max(elapsed)),
            }
        if rates:
            summary["rate_per_second"] = {
                "mean": rounded(statistics.fmean(rates)),
                "p50": rounded(percentile(rates, 50)),
                "p05": rounded(percentile(rates, 5)),
            }
        if observed:
            summary["observed"] = {
                "mean": rounded(statistics.fmean(observed)),
                "p50": rounded(percentile(observed, 50)),
                "min": rounded(min(observed)),
                "max": rounded(max(observed)),
            }
        notes = sorted(
            {
                row["note"]
                for row in group
                if row.get("note", "").strip()
                and row["status"] in {"fail", "skip"}
            }
        )
        if notes:
            summary["notes"] = notes
        summaries.append(summary)
    return summaries


def markdown_table(summaries: list[dict[str, Any]]) -> str:
    lines = [
        "| Host lane | Isolation | Mechanism | Pass/total | p50 | p95 | "
        "p50 rate/value |",
        "| --- | --- | --- | ---: | ---: | ---: | ---: |",
    ]
    for item in summaries:
        latency = item.get("latency_ms", {})
        rate = item.get("rate_per_second", {})
        observed = item.get("observed", {})
        p50 = f"{latency['p50']:.3f} ms" if latency else "—"
        p95 = f"{latency['p95']:.3f} ms" if latency else "—"
        if rate:
            rate_or_value = f"{rate['p50']:.3f} {item['rate_unit']}"
        elif observed:
            rate_or_value = f"{observed['p50']:.3f} {item['observed_unit']}"
        else:
            rate_or_value = "—"
        lines.append(
            f"| {item['host_lane']} | {item['isolation']} | "
            f"{item['mechanism']} | {item['passed']}/{item['samples']} | "
            f"{p50} | {p95} | {rate_or_value} |"
        )
    return "\n".join(lines)


def write_summary(
    csv_paths: Iterable[Path], json_path: Path, markdown_path: Path
) -> list[dict[str, Any]]:
    rows = read_rows(csv_paths)
    summaries = summarize(rows)
    json_path.write_text(
        json.dumps(summaries, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    markdown_path.write_text(
        "# A3S Box isolation-mechanism benchmark summary\n\n"
        + markdown_table(summaries)
        + "\n",
        encoding="utf-8",
    )
    return summaries


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("csv", nargs="+", type=Path, help="Input sample CSV files")
    parser.add_argument("--json", type=Path, required=True, help="Summary JSON output")
    parser.add_argument(
        "--markdown", type=Path, required=True, help="Summary Markdown output"
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    write_summary(args.csv, args.json, args.markdown)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
