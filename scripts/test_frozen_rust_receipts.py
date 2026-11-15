"""Rust receipt interpretation refuses empty, skipped, duplicated or failed execution."""
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from frozen_rust import run_target, rust_passed


class RustReceiptTests(unittest.TestCase):
    def test_only_one_executed_success_qualifies(self):
        valid = "test result: ok. 1 passed; 0 failed; 0 ignored;"
        self.assertTrue(rust_passed(0, valid))
        for code, text in [(1, valid), (0, ""), (0, valid + valid),
                           (0, valid.replace("1 passed", "0 passed")),
                           (0, valid.replace("0 ignored", "1 ignored")),
                           (0, valid.replace("0 failed", "1 failed"))]:
            self.assertFalse(rust_passed(code, text))

    def test_discovery_executes_later_tests_after_failure_and_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            listing = subprocess.CompletedProcess([], 0, "a: test\nb: test\nc: test\n", "")
            failed = subprocess.CompletedProcess([], 1, "test result: FAILED. 0 passed; 1 failed; 0 ignored;", "")
            passed = subprocess.CompletedProcess([], 0, "test result: ok. 1 passed; 0 failed; 0 ignored;", "")
            with patch("frozen_rust.subprocess.run", side_effect=[listing, failed, subprocess.TimeoutExpired([], 1), passed]) as run:
                receipts = run_target("binary", {}, Path(directory))
            self.assertEqual(run.call_count, 4)
            self.assertEqual([item["status"] for item in receipts], ["current-fail", "current-timeout", "current-pass"])
