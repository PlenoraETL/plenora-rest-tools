"""Controprove del gate commenti, adattate dalla suite di famiglia IO."""

from pathlib import Path
import tempfile
import unittest

import check_comments as gate


class CommentTests(unittest.TestCase):
    def violations(self, source, name="sample.rs"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / name
            path.write_text(source, encoding="utf-8")
            return gate.check_file(path, root)

    def test_debt_markers(self):
        for marker in ("TODO", "FIXME", "HACK", "XXX"):
            for name, prefix in (("a.rs", "//"), ("a.py", "#"), ("a.yml", "#")):
                with self.subTest(marker=marker, name=name):
                    found = self.violations(f"{prefix} {marker}: repair\n", name)
                    self.assertEqual([v.rule for v in found], ["debito anonimo"])

    def test_words_and_string_values_are_not_debt(self):
        for source in (
            "// METODO_CHIUSO PREFIXME_NO XXXL\n",
            'let value = "TODO // FIXME";\n',
            'let value = r###"TODO /* FIXME */"###;\n',
        ):
            self.assertEqual(self.violations(source), [])
        self.assertEqual(self.violations('value = "TODO # FIXME"\n', "a.py"), [])
        self.assertEqual(self.violations('value: "TODO # FIXME"\n', "a.yml"), [])

    def test_history_and_current_rationale(self):
        for phrase in ("tranche 2", "writer pre-fix", "prima mancava", "roadmap"):
            self.assertTrue(self.violations("// " + phrase))
        for phrase in (
            "Il retry rispetta il limite del server.",
            "Un istante precedente all'epoca Unix restituisce None.",
            "Criteri di accettazione della campagna.",
        ):
            self.assertEqual(self.violations("// " + phrase), [])
        self.assertTrue(self.violations("// all'epoca il controllo mancava"))

    def test_docstrings_and_nested_blocks(self):
        self.assertTrue(self.violations('"""TODO: repair."""\n', "a.py"))
        self.assertTrue(self.violations("/* outer /* inner */ FIXME */"))
        found = self.violations("\n// TODO\n")
        self.assertEqual(found[0].line, 2)

    def test_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            included = ("python/pkg/a.py", "python/pkg/a.pyi", "crates/core/a.rs",
                        "scripts/a.ps1", ".github/workflows/a.yml", "Dockerfile.verify")
            excluded = ("target/a.rs", ".venv/a.py", "vendor/a.rs",
                        "contracts/upstream/a.py", "docs/a.md", "__pycache__/a.py")
            for name in included + excluded:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("", encoding="utf-8")
            self.assertEqual(
                {p.relative_to(root).as_posix() for p in gate.source_files(root)},
                set(included),
            )

    def test_repository(self):
        checked, violations = gate.check_repository()
        self.assertGreater(checked, 50)
        self.assertEqual(violations, [])


if __name__ == "__main__":
    unittest.main()
