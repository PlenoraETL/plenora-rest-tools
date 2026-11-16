"""Derived versions cannot weaken the independent behavioral oracle."""
from copy import deepcopy
import json
import unittest

from frozen_compare import ROOT, assert_inventory, assert_observation, current_version


class FrozenCompareTests(unittest.TestCase):
    def test_all_historical_cli_envelopes_accept_current_version(self):
        expected = json.loads((ROOT / "tests/frozen/0.4.0/observed.json").read_text(encoding="utf-8"))["cases"]
        original = deepcopy(expected)
        actual = deepcopy(expected)
        for identity, value in actual.items():
            envelope = ("document" if identity.startswith(("private-cli-", "cli-signals-"))
                        else "cli" if identity.startswith("remote-surfaces-") else None)
            if envelope:
                value[envelope]["component_version"] = current_version()
        current = deepcopy(actual)
        assert_inventory(actual, expected)
        self.assertEqual(expected, original)
        self.assertEqual(actual, current)

    def test_mutations_fail_at_the_right_check(self):
        for identity, envelope in (("private-cli-probe", "document"),
                                   ("cli-signals-primary", "document"),
                                   ("remote-surfaces-probe", "cli")):
            actual = {envelope: {"component_version": current_version(), "status": "error",
                                 "result": {"component_version": "application-data"}}, "exit": 2}
            expected = deepcopy(actual)
            expected[envelope]["component_version"] = "historical-version"
            assert_observation(identity, actual, expected)
            nested = deepcopy(expected)
            nested[envelope]["result"]["component_version"] = "changed"
            with self.assertRaisesRegex(AssertionError, "observed baseline"):
                assert_observation(identity, actual, nested)
            for field in ("status", "result"):
                changed = deepcopy(expected)
                changed[envelope][field] = "changed"
                with self.assertRaisesRegex(AssertionError, "observed baseline"):
                    assert_observation(identity, actual, changed)
            wrong = deepcopy(actual)
            wrong[envelope]["component_version"] = "wrong-version"
            with self.assertRaisesRegex(AssertionError, "current component_version"):
                assert_observation(identity, wrong, wrong)
            missing = deepcopy(actual)
            del missing[envelope]["component_version"]
            with self.assertRaises(KeyError):
                assert_observation(identity, missing, expected)

    def test_inventory_and_non_cli_version_are_not_ignored(self):
        with self.assertRaisesRegex(AssertionError, "inventory"):
            assert_inventory({}, {"case": {}})
        with self.assertRaisesRegex(AssertionError, "observed baseline"):
            assert_observation("core", {"component_version": "a"}, {"component_version": "b"})
