"""Regression tests for exact package inventories and text-only contents."""

import io
from pathlib import Path
import tarfile
import tempfile
import subprocess
import unittest

from check_packages import check_archive, check_files, expected_files, check_source_content, check_sources, ROOT


class PackageTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "target").mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=ROOT / "target")
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)

    def test_sources_private_formats_and_public_text(self):
        for label in ("PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY",
                      "ENCRYPTED PRIVATE KEY", "OPENSSH PRIVATE KEY", "DSA PRIVATE KEY"):
            with self.subTest(label=label), self.assertRaisesRegex(ValueError, "private key"):
                check_source_content(f"-----BEGIN {label}-----\nsecret".encode())
        for data in (b"0\x06\x02\x01\x00\x02\x01\x01", b"0\x06\x02\x01\x01\x04\x01x",
                     b"0\x08\x02\x01\x000\x00\x04\x01x", b"0\x050\x00\x04\x01x",
                     b"0\x07\x02\x01\x030\x02\x06\x00"):
            with self.assertRaisesRegex(ValueError, "ASN.1"):
                check_source_content(data)
        for data in (b"source text", b"-----BEGIN CERTIFICATE-----\npublic", b"\x00fuzz", b"0 text\n"):
            check_source_content(data)

    def test_git_tracked_sources_outside_packages_and_without_git(self):
        def git(*args):
            subprocess.run(["git", *args], cwd=self.directory, check=True, capture_output=True)
        git("init")
        source = self.directory / "outside.txt"
        source.write_text("public text")
        git("add", "outside.txt")
        check_sources(self.directory)
        for data in (f"-----BEGIN {'PRIVATE KEY'}-----".encode(), b"0\x03\x02\x01\x03"):
            source.write_bytes(data)
            with self.assertRaises(ValueError):
                check_sources(self.directory)
        source.unlink()
        check_sources(self.directory)
        # Tracked build-looking paths are not exempt from the Git scan.
        build = self.directory / "target"
        build.mkdir()
        nested = build / "renamed key.rs"
        nested.write_bytes(b"0\x03\x02\x01\x03")
        git("add", "target/renamed key.rs")
        with self.assertRaisesRegex(ValueError, "ASN.1"):
            check_sources(self.directory)
        archive = self.directory / "archive"
        archive.mkdir()
        (archive / "source").write_text("public source")
        check_sources(archive)
        (archive / "source").write_bytes(b"0\x03\x02\x01\x03")
        with self.assertRaises(ValueError):
            check_sources(archive)

    def test_exact_inventory_with_and_without_git(self):
        entry = {"files": ["Cargo.toml"], "vcs_files": [".cargo_vcs_info.json"]}
        (self.directory / "Cargo.toml").write_text("[package]\n")
        for has_vcs in (False, True):
            expected = expected_files(entry, has_vcs)
            check_files(self.directory, "\n".join(expected), expected)
            wrong = expected_files(entry, not has_vcs)
            with self.assertRaisesRegex(ValueError, "inventory"):
                check_files(self.directory, "\n".join(wrong), expected)

    def test_windows_listing_uses_the_same_exact_inventory(self):
        (self.directory / "src").mkdir()
        (self.directory / "src/lib.rs").write_text("// source\n")
        check_files(self.directory, "src\\lib.rs", ["src/lib.rs"])

    def test_extra_files_fail_anywhere(self):
        for name in ("tests/fixtures/data.json", "src/fixtures/data.json",
                     "examples/fixtures/data.json", "src/new.rs", "identity.key",
                     "identity.p12", "identity.pfx", "identity.der"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "inventory"):
                check_files(self.directory, name, [])

    def test_missing_expected_file_fails(self):
        with self.assertRaisesRegex(ValueError, "inventory"):
            check_files(self.directory, "", ["src/lib.rs"])

    def test_private_pem_rejected_even_if_explicitly_allowed(self):
        for label in ("PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY",
                      "ENCRYPTED PRIVATE KEY", "OPENSSH PRIVATE KEY"):
            (self.directory / "lib.rs").write_text(f"-----BEGIN {label}-----\nsecret")
            with self.subTest(label=label), self.assertRaisesRegex(ValueError, "private key"):
                check_files(self.directory, "lib.rs", ["lib.rs"])

    def test_binary_keys_rejected_even_if_allowed_or_renamed(self):
        # ASN.1 containers: PKCS#1, SEC1, PKCS#8, encrypted PKCS#8, PKCS#12.
        containers = (b"0\x06\x02\x01\x00\x02\x01\x01", b"0\x06\x02\x01\x01\x04\x01x",
                      b"0\x08\x02\x01\x000\x00\x04\x01x", b"0\x050\x00\x04\x01x",
                      b"0\x07\x02\x01\x030\x02\x06\x00", b"\xff\xfe")
        for name in ("key.key", "key.p12", "key.pfx", "key.der", "src.rs"):
            for data in containers:
                (self.directory / name).write_bytes(data)
                with self.subTest(name=name, data=data), self.assertRaisesRegex(ValueError, "binary"):
                    check_files(self.directory, name, [name])

    def test_accepts_text_and_generated_entries(self):
        (self.directory / "Cargo.toml").write_text('[package]\nname = "example"\n')
        names = ["Cargo.toml", "Cargo.toml.orig", "Cargo.lock", ".cargo_vcs_info.json"]
        check_files(self.directory, "\n".join(names), names)

    def test_license_is_inspected(self):
        crate = self.directory / "crate"
        crate.mkdir()
        license_file = self.directory / "LICENSE"
        license_file.write_text("license text")
        check_files(crate, "LICENSE", ["LICENSE"], "../LICENSE")
        license_file.write_text("-----BEGIN " + "PRIVATE KEY-----")
        with self.assertRaisesRegex(ValueError, "private key"):
            check_files(crate, "LICENSE", ["LICENSE"], "../LICENSE")

    def test_missing_source_fails_closed(self):
        with self.assertRaises(OSError):
            check_files(self.directory, "missing", ["missing"])

    def test_invalid_or_duplicate_paths_fail_closed(self):
        for listing in ("../secret", "/secret", "C:/secret", "src\\lib.rs", "a\na", "./a"):
            with self.subTest(listing=listing), self.assertRaises(ValueError):
                check_files(self.directory, listing, listing.splitlines())

    def test_actual_archive_generated_files_are_inspected(self):
        for data in (b"version = 4\n", b"-----BEGIN " + b"PRIVATE KEY-----", b"0\x03\x02\x01\x03"):
            archive = self.directory / "test.crate"
            with tarfile.open(archive, "w:gz") as package:
                entry = tarfile.TarInfo("test-1/Cargo.lock")
                entry.size = len(data)
                package.addfile(entry, io.BytesIO(data))
            if data.startswith(b"version"):
                check_archive(archive, "test-1", ["Cargo.lock"])
                with self.assertRaisesRegex(ValueError, "inventory"):
                    check_archive(archive, "test-1", [])
            else:
                with self.assertRaises(ValueError):
                    check_archive(archive, "test-1", ["Cargo.lock"])


if __name__ == "__main__":
    unittest.main()
