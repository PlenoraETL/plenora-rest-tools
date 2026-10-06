#!/usr/bin/env python3
"""Self-test del gate di coverage."""

from __future__ import annotations

import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

import check_coverage


def report(*, covered: int = 34, count: int = 100, percent: float = 34.0) -> dict:
    totals = {
        metric: {"covered": covered, "count": count, "percent": percent}
        for metric in check_coverage.METRICS
    }
    return {
        "type": "llvm.coverage.json.export",
        "version": "3.1.0",
        "data": [{"files": [{"filename": "/repo/src/lib.rs"}], "totals": totals}],
    }


def budget(*, minimum: float = 34.0) -> dict:
    return {
        "schema_version": 1,
        "surfaces": {
            "unit": {
                "report_format": "llvm",
                "minimum_percent": {
                    metric: minimum for metric in check_coverage.METRICS
                },
                "sources": ["src/"],
            }
        },
    }


class CoverageGateTests(unittest.TestCase):
    def write(self, directory: Path, name: str, value: dict) -> Path:
        path = directory / name
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def run_check(self, report_value: dict, budget_value: dict) -> bool:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(
                io.StringIO()
            ):
                return check_coverage.check(
                    self.write(root, "summary.json", report_value),
                    self.write(root, "budget.json", budget_value),
                    "unit",
                    Path("/repo"),
                )

    def test_equal_to_budget_passes(self) -> None:
        self.assertTrue(self.run_check(report(), budget()))

    def test_a_metric_below_budget_fails(self) -> None:
        value = report()
        value["data"][0]["totals"]["lines"] = {
            "covered": 33,
            "count": 100,
            "percent": 33.0,
        }
        self.assertFalse(self.run_check(value, budget()))

    def test_missing_surface_and_metric_are_rejected(self) -> None:
        with self.assertRaises(check_coverage.CoverageError):
            check_coverage.read_budget(budget(), "missing")
        incomplete = budget()
        del incomplete["surfaces"]["unit"]["minimum_percent"]["regions"]
        with self.assertRaises(check_coverage.CoverageError):
            check_coverage.read_budget(incomplete, "unit")

    def test_report_must_have_one_complete_data_block(self) -> None:
        for value in (
            {"type": "llvm.coverage.json.export", "data": []},
            {"type": "wrong", "data": report()["data"]},
        ):
            with self.subTest(value=value), self.assertRaises(
                check_coverage.CoverageError
            ):
                check_coverage.read_totals(value)

    def test_python_lines_and_branches_are_checked(self) -> None:
        python_report = {
            "meta": {"format": 3, "branch_coverage": True},
            "files": {"package/module.py": {}},
            "totals": {
                "covered_lines": 73,
                "num_statements": 100,
                "covered_branches": 41,
                "num_branches": 50,
            },
        }
        python_budget = {
            "schema_version": 1,
            "surfaces": {
                "unit": {
                    "report_format": "coverage.py",
                    "minimum_percent": {"lines": 73.0, "branches": 82.0},
                    "sources": ["package/"],
                }
            },
        }
        self.assertTrue(self.run_check(python_report, python_budget))
        escaping = json.loads(json.dumps(python_report))
        escaping["files"] = {"site/package/../other/module.py": {}}
        with self.assertRaises(check_coverage.CoverageError):
            self.run_check(escaping, python_budget)
        python_budget["surfaces"]["unit"]["minimum_percent"]["branches"] = 82.1
        self.assertFalse(self.run_check(python_report, python_budget))

    def test_python_report_is_fail_closed(self) -> None:
        base = {
            "meta": {"format": 3, "branch_coverage": True},
            "files": {"package/module.py": {}},
            "totals": {
                "covered_lines": 1,
                "num_statements": 2,
                "covered_branches": 1,
                "num_branches": 2,
            },
        }
        invalid = []
        for key, value in (
            ("format", 2),
            ("branch_coverage", False),
        ):
            candidate = json.loads(json.dumps(base))
            candidate["meta"][key] = value
            invalid.append(candidate)
        no_files = json.loads(json.dumps(base))
        no_files["files"] = {}
        invalid.append(no_files)
        no_branches = json.loads(json.dumps(base))
        no_branches["totals"]["num_branches"] = 0
        invalid.append(no_branches)
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(
                check_coverage.CoverageError
            ):
                check_coverage.read_python_totals(value)

    def test_files_must_belong_to_the_declared_sources(self) -> None:
        for files in (
            [],
            [{"filename": "/repo/vendor/geo/src/lib.rs"}],
            [{"filename": "/repo/src/lib.rs"}, {"filename": "/repo/vendor/x.rs"}],
            [{"filename": "/altro/src/lib.rs"}],
            [{"filename": "/repository/src/lib.rs"}],
            [{"filename": "/repo/src/../vendor/geo/lib.rs"}],
            [{"filename": "/repo/src/./lib.rs"}],
            [{"filename": "\\repo\\src\\..\\vendor\\lib.rs"}],
            [{"filename": 3}],
        ):
            value = report()
            value["data"][0]["files"] = files
            with self.subTest(files=files), self.assertRaises(
                check_coverage.CoverageError
            ):
                self.run_check(value, budget())

    def test_every_declared_source_needs_a_file(self) -> None:
        two = budget()
        two["surfaces"]["unit"]["sources"] = ["src/", "other/"]
        with self.assertRaises(check_coverage.CoverageError):
            self.run_check(report(), two)
        windows = report()
        windows["data"][0]["files"] = [{"filename": "\\repo\\src\\lib.rs"}]
        self.assertTrue(self.run_check(windows, budget()))

    def test_sources_are_required_in_the_budget(self) -> None:
        for sources in (None, [], ["src"], ["/"], ["src/", "src/"]):
            value = budget()
            if sources is None:
                del value["surfaces"]["unit"]["sources"]
            else:
                value["surfaces"]["unit"]["sources"] = sources
            with self.subTest(sources=sources), self.assertRaises(
                check_coverage.CoverageError
            ):
                check_coverage.read_budget(value, "unit")

    def test_inconsistent_or_non_finite_numbers_are_rejected(self) -> None:
        inconsistent = report(percent=99.0)
        non_finite = report(percent=float("nan"))
        boolean = report()
        boolean["data"][0]["totals"]["lines"]["covered"] = True
        for value in (inconsistent, non_finite, boolean):
            with self.subTest(value=value), self.assertRaises(
                check_coverage.CoverageError
            ):
                check_coverage.read_totals(value)


if __name__ == "__main__":
    unittest.main()
