"""The VM collector must retain failures/skips and still run later commands."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

from frozen_vm import Collection
from frozen_rust import environment


class CollectionTests(unittest.TestCase):
    def test_failure_and_skip_do_not_stop_collection_or_become_green(self):
        with tempfile.TemporaryDirectory() as directory:
            collector = Collection(Path(directory),environment())
            for label,code in (("failed",1),("skipped",77),("later",0)):
                collector.run(label,[sys.executable,"-c",f"raise SystemExit({code})"])
            self.assertFalse(collector.save())
            receipt = json.loads((Path(directory)/"receipt.json").read_text())
            self.assertEqual([r["status"] for r in receipt["tests"]],["current-fail","skip","current-pass"])
            self.assertTrue((Path(directory)/"later.log").exists())
