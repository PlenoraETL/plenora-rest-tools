"""The installed SDK oracle needs no build tree or particular working directory."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


class FrozenEnvironmentTests(unittest.TestCase):
    def test_remote_oracle_in_a_source_copy_without_target_from_another_cwd(self):
        repository = Path(__file__).resolve().parents[2]
        binary = Path(os.environ.get("PLENORA_TEST_CLI", str(repository / "target/debug" /
                      ("plenora-rest.exe" if os.name == "nt" else "plenora-rest")))).resolve()
        self.assertTrue(binary.is_file(), "current CLI fixture is required")
        with tempfile.TemporaryDirectory(prefix="frozen-installed-") as directory:
            root = Path(directory) / "source"
            tests = root / "python/tests"
            tests.mkdir(parents=True)
            shutil.copyfile(repository / "python/tests/test_frozen_private.py", tests / "test_frozen_private.py")
            shutil.copytree(repository / "tests/frozen", root / "tests/frozen")
            outside = Path(directory) / "elsewhere"
            outside.mkdir()
            env = {key: value for key, value in os.environ.items()
                   if key not in {"SEMPL_CAPTURE_DIR", "SEMPL_CHECK_CAPTURE_DIR", "SEMPL_CONTROL_CASE"}}
            env["PLENORA_TEST_CLI"] = str(binary)
            self.assertFalse((root / "target").exists())
            result = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", str(tests), "-v"],
                                    cwd=outside, env=env, capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("Ran 2 tests", result.stderr)
            self.assertNotIn("skipped", result.stderr)
            self.assertFalse((root / "target").exists())
