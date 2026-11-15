"""Git source identities survive checkout newline conversion, never content changes."""
import hashlib
import json

import frozen_proofs
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from source_identity import ROOT, byte_digest, source_digest, source_digests


class SourceIdentityTests(unittest.TestCase):
    def setUp(self):
        temporary = ROOT / "target/tmp"
        temporary.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=temporary)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / ".gitattributes").write_bytes(b"* text=auto eol=lf\nexact/** -text\n")

    def test_checkout_endings_are_equal_but_changed_content_is_not(self):
        path = self.root / "input.json"
        lf = b'{"value":1}\n'
        path.write_bytes(lf)
        expected = hashlib.sha256(lf).hexdigest()
        self.assertEqual(source_digest(path, self.root), expected)
        path.write_bytes(lf.replace(b"\n", b"\r\n"))
        self.assertEqual(source_digest(path, self.root), expected)
        self.assertNotEqual(byte_digest(path), expected)
        path.write_bytes(b'{"value":2}\r\n')
        self.assertNotEqual(source_digest(path, self.root), expected)
        self.assertFalse((self.root / ".git").exists())

    def test_git_attributes_and_binary_detection_are_not_text_heuristics(self):
        (self.root / "exact").mkdir()
        (self.root / "nested").mkdir()
        (self.root / "nested/.gitattributes").write_bytes(b"*.dat -text\n")
        for name, content in (("exact/seed", b"abc\r\n"),
                              ("nested/seed.dat", b"abc\r\n"),
                              ("binary", b"a\0b\r\n"),
                              ("lone-cr.txt", b"a\rb\n")):
            path = self.root / name
            path.write_bytes(content)
            self.assertEqual(source_digest(path, self.root), byte_digest(path))

    def test_batch_receipts_use_same_rule_without_inherited_git_configuration(self):
        first, second = self.root / "a.py", self.root / "b.rs"
        first.write_bytes(b"a\r\n")
        second.write_bytes(b"b\n")
        with patch.dict("os.environ", {"GIT_DIR": "missing-repository", "GIT_CONFIG_COUNT": "1",
                                       "GIT_CONFIG_KEY_0": "core.autocrlf", "GIT_CONFIG_VALUE_0": "false"}):
            actual = source_digests([first, second], self.root)
        self.assertEqual(actual, {first: hashlib.sha256(b"a\n").hexdigest(),
                                  second: hashlib.sha256(b"b\n").hexdigest()})
        with self.assertRaises(ValueError):
            source_digest(ROOT / "Cargo.toml", self.root)

    def test_fixture_generator_is_stable_and_writes_lf(self):
        upstream = self.root / "contracts/upstream"
        upstream.mkdir(parents=True)
        pin = upstream / "adopted.json"
        pin.write_bytes(b"{}\n")
        (upstream / "source.json").write_text(json.dumps({"revision":"test", "files":{
            "adopted.json":{"sha256":byte_digest(pin)}}}), encoding="utf-8")
        fixtures = self.root / "tests/frozen/0.4.0"
        fixtures.mkdir(parents=True)
        fixture = fixtures / "io-branches.json"
        fixture.write_bytes(b'{"fixture":1}\n')
        index = fixtures / "sources.json"
        with patch.object(frozen_proofs, "ROOT", self.root), patch.object(frozen_proofs, "INDEX", index):
            with patch("sys.argv", ["frozen_proofs.py"]):
                self.assertEqual(frozen_proofs.main(), 0)
            expected = index.read_bytes()
            self.assertNotIn(b"\r", expected)
            fixture.write_bytes(b'{"fixture":1}\r\n')
            with patch("sys.argv", ["frozen_proofs.py", "--check"]):
                self.assertEqual(frozen_proofs.main(), 0)
            self.assertEqual(index.read_bytes(), expected)
            fixture.write_bytes(b'{"fixture":2}\r\n')
            with patch("sys.argv", ["frozen_proofs.py", "--check"]), self.assertRaises(SystemExit):
                frozen_proofs.main()
