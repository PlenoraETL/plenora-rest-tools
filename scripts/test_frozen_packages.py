"""T6 checks actual archive members and release-note semantics."""
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import check_packages
import release


class FrozenPackageTests(unittest.TestCase):
    def setUp(self):
        root = Path(__file__).resolve().parents[1] / "target"
        root.mkdir(exist_ok=True)
        temporary = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def archive(self, entries):
        path = self.root / "probe.crate"
        with tarfile.open(path, "w:gz") as archive:
            for name, kind, body in entries:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.size = len(body)
                member.linkname = "outside"
                archive.addfile(member, io.BytesIO(body))
        return path

    def test_archive_mutations_cannot_hide_behind_an_allowed_name(self):
        good = ("probe-1/Cargo.toml", tarfile.REGTYPE, b"[package]\n")
        check_packages.check_archive(self.archive([good]), "probe-1", ["Cargo.toml"])
        mutations = [
            [good, good],
            [("probe-1/../Cargo.toml", tarfile.REGTYPE, b"text")],
            [("other/Cargo.toml", tarfile.REGTYPE, b"text")],
            [(good[0], tarfile.SYMTYPE, b"")],
            [(good[0], tarfile.LNKTYPE, b"")],
            [(good[0], tarfile.DIRTYPE, b"")],
            [(good[0], tarfile.REGTYPE, b"\x00secret")],
            [],
        ]
        for index, entries in enumerate(mutations):
            with self.subTest(mutation=index), self.assertRaises(ValueError):
                check_packages.check_archive(self.archive(entries), "probe-1", ["Cargo.toml"])

    def test_release_notes_are_exactly_the_dated_version_section(self):
        output = self.root / "notes.md"
        with patch.object(release, "ROOT", self.root):
            (self.root / "CHANGELOG.md").write_text(
                "# Changes\n\n## 0.4.0 (2026-11-14)\n\nKept.\n\n## 0.3.0 (2026-10-01)\n\nExcluded.\n", encoding="utf-8")
            release.release_notes("0.4.0", output)
            self.assertEqual(output.read_text(encoding="utf-8").strip(), "Kept.")
            for body in ("## 0.4.0 (2026-02-30)\ntext", "## 0.4.0 (unreleased)\ntext",
                         "## 0.4.0 (2026-11-14)\n", "## 0.3.0 (2026-11-14)\ntext",
                         "## 0.4.0 (2026-11-14)\none\n## 0.4.0 (2026-11-14)\ntwo"):
                (self.root / "CHANGELOG.md").write_text(body, encoding="utf-8")
                with self.subTest(body=body), self.assertRaises(SystemExit):
                    release.release_notes("0.4.0", output)
