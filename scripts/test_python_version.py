"""Mutation proofs for the single Python version source and artifact ABI."""
from pathlib import Path
import shutil
import tempfile
import unittest
import zipfile
import check_python_version as gate


class PythonPolicyTests(unittest.TestCase):
    def setUp(self):
        (gate.ROOT / "target").mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=gate.ROOT / "target")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in (".python-version", "pyproject.toml", "README.md", "Dockerfile.release", "Dockerfile.verify"):
            shutil.copyfile(gate.ROOT / name, self.root / name)
        for name in ("scripts", ".github", "docs", "crates/rest-engine-python"):
            shutil.copytree(gate.ROOT / name, self.root / name)

    def test_guard_is_connected_to_assurance(self):
        import run_assurance
        self.assertTrue(any(command[1:] == ["scripts/check_python_version.py"]
                            for command in run_assurance.commands()))

    def test_current(self):
        self.assertEqual(gate.check(self.root), "3.14")

    def test_mutations(self):
        for name, old, new in (
            ("pyproject.toml", ">=3.14", ">=3.13"),
            ("crates/rest-engine-python/Cargo.toml", "abi3-py314", "abi3-py313"),
            (".github/workflows/verify.yml", "python-version-file: '.python-version'", "python-version: '3.14'"),
            ("scripts/test_wheel.ps1", "cp314", "cp313"),
            ("Dockerfile.release", "cp314", "cp313"),
            ("README.md", "CPython 3.14", "CPython 3.13"),
        ):
            path = self.root / name
            source = path.read_text(encoding="utf-8")
            self.assertIn(old, source)
            path.write_text(source.replace(old, new), encoding="utf-8")
            with self.subTest(name=name), self.assertRaises(ValueError):
                gate.check(self.root)
            path.write_text(source, encoding="utf-8")

    def test_wheel_metadata(self):
        path = self.root / "sample.whl"
        for requirement, tag in ((">=3.14", "cp314-abi3-win_amd64"),
                                 (">=3.13", "cp314-abi3-win_amd64"),
                                 (">=3.14", "cp313-abi3-win_amd64")):
            with zipfile.ZipFile(path, "w") as wheel:
                wheel.writestr("p.dist-info/METADATA", "Requires-Python: " + requirement + "\n")
                wheel.writestr("p.dist-info/WHEEL", "Tag: " + tag + "\n")
            if requirement == ">=3.14" and tag.startswith("cp314-"):
                gate.check_wheel(path, "3.14")
            else:
                with self.assertRaises(ValueError):
                    gate.check_wheel(path, "3.14")
