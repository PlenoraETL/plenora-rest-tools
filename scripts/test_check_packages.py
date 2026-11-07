"""Regression tests for the Cargo release content guard."""

from pathlib import Path
import tempfile
import unittest

from check_packages import check_files, ROOT


class PackageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (ROOT / "target").mkdir(exist_ok=True)

    def test_rejects_fixture_paths_on_both_platforms(self):
        for name in ("tests/fixtures/tls/ca.pem", "tests\\fixtures\\data.json",
                     "nested/tests/fixtures/data.csv"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "fixture"):
                check_files(ROOT, name)

    def test_rejects_private_keys_outside_fixtures_and_without_pem_suffix(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "target") as temporary:
            directory = Path(temporary)
            for label in ("PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY",
                          "ENCRYPTED PRIVATE KEY", "OPENSSH PRIVATE KEY"):
                (directory / "data.txt").write_text(f"-----BEGIN {label}-----\nsecret")
                with self.subTest(label=label), self.assertRaisesRegex(ValueError, "private key"):
                    check_files(directory, "data.txt")

    def test_accepts_sources_and_cargo_generated_entries(self):
        check_files(ROOT, "Cargo.toml\nCargo.toml.orig\nCargo.lock\n.cargo_vcs_info.json")

    def test_inspects_license_copied_from_workspace(self):
        with tempfile.TemporaryDirectory(dir=ROOT / "target") as temporary:
            directory = Path(temporary)
            crate = directory / "crate"
            crate.mkdir()
            (directory / "LICENSE").write_text("license text")
            check_files(crate, "LICENSE", "../LICENSE")
            (directory / "LICENSE").write_text("-----BEGIN PRIVATE KEY-----")
            with self.assertRaisesRegex(ValueError, "private key"):
                check_files(crate, "LICENSE", "../LICENSE")

    def test_missing_files_fail_closed(self):
        with self.assertRaises(OSError):
            check_files(ROOT, "missing-package-file")

    def test_path_escape_fails_closed(self):
        with self.assertRaises(ValueError):
            check_files(ROOT, "../secret")


if __name__ == "__main__":
    unittest.main()
