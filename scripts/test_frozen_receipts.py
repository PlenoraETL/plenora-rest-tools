"""T5 discovery and receipts tested through execution, including negative outcomes."""
import unittest

from frozen_proofs import run_suite


class FrozenReceiptTests(unittest.TestCase):
    def test_discovery_and_receipts_preserve_every_outcome(self):
        visited = []

        class Probes(unittest.TestCase):
            def test_a_failure(self):
                visited.append("failure")
                self.fail("injected")

            def test_b_skip(self):
                self.skipTest("unavailable platform")

            def test_c_later(self):
                visited.append("later")

        result, _ = run_suite(unittest.defaultTestLoader.loadTestsFromTestCase(Probes))
        self.assertFalse(result["passed"])
        self.assertEqual(visited, ["failure", "later"])
        self.assertEqual(set(result["tests"]), set(result["discovered"]))
        self.assertEqual([r["status"] for r in result["tests"].values()],
                         ["current-fail", "skip", "current-pass"])

    def test_skip_alone_cannot_qualify_and_new_tests_are_discovered(self):
        class Probes(unittest.TestCase):
            def test_skip(self):
                self.skipTest("not executed")

        result, _ = run_suite(unittest.defaultTestLoader.loadTestsFromTestCase(Probes))
        self.assertFalse(result["passed"])
        Probes.test_added = lambda self: None
        result, _ = run_suite(unittest.defaultTestLoader.loadTestsFromTestCase(Probes))
        self.assertEqual(len(result["discovered"]), 2)
        self.assertTrue(any(name.endswith("test_added") for name in result["tests"]))

    def test_subtest_failure_and_missing_discovery_fail_closed(self):
        class Probes(unittest.TestCase):
            def test_subtests(self):
                with self.subTest(case="failure"):
                    self.fail("injected")

        result, _ = run_suite(unittest.defaultTestLoader.loadTestsFromTestCase(Probes))
        self.assertFalse(result["passed"])
        with self.assertRaises(ValueError):
            run_suite(unittest.TestSuite())
