"""Controprove per link, comandi ed esempi della documentazione corrente."""

from pathlib import Path
import tempfile
import unittest

import check_docs as gate


class DocsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.doc = self.write("README.md", "")
        self.write("python/plenora_rest/__init__.py", '__all__ = ["Engine"]\n')

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def test_links_and_unicode_anchors(self):
        self.write("docs/guide.md", "# Compatibilit\u00e0\n# Compatibilit\u00e0\n")
        self.write("README.md",
                   "[guide](docs/guide.md#compatibilit%C3%A0)\n"
                   "[duplicate](docs/guide.md#compatibilit%C3%A0-1)\n"
                   "[external](https://example.com/missing)\n")
        self.assertEqual(gate.validate_links(self.root, [self.doc]), [])
        self.write("README.md", "[bad](missing.md)\n[bad](docs/guide.md#absent)")
        self.assertEqual(len(gate.validate_links(self.root, [self.doc])), 2)

    def test_commands(self):
        self.write("scripts/real.py", "")
        self.write("python/example.py", "")
        self.write("README.md", "python ./scripts/real.py\npython3 python/example.py")
        self.assertEqual(gate.validate_commands(self.root, [self.doc]), [])
        self.write("README.md", "python .\\scripts\\missing.py")
        self.assertEqual(len(gate.validate_commands(self.root, [self.doc])), 1)

    def test_python_fence_styles_and_public_exports(self):
        for fence in ("```", "~~~"):
            with self.subTest(fence=fence):
                self.write("README.md", f"{fence}python\nfrom plenora_rest import Engine\n{fence}")
                self.assertEqual(gate.scan(self.root)[1], [])
                self.write("README.md", f"{fence}python\nfrom plenora_rest import Missing\n{fence}")
                self.assertEqual(len(gate.validate_sdk_example_api(self.root)), 1)
                self.write("README.md", f"{fence}python\nimport plenora_rest as sdk\nsdk.Missing()\n{fence}")
                self.assertEqual(len(gate.validate_sdk_example_api(self.root)), 1)
                self.write("README.md", f"{fence}python\nif:\n{fence}")
                self.assertEqual(len(gate.validate_python_examples([self.doc])), 1)

    def test_unbalanced_fence(self):
        self.write("README.md", "~~~python\nvalue = 1\n")
        self.assertTrue(gate.scan(self.root)[1])

    def test_scope(self):
        included = ("docs/guide.md", "python/README.md", "contracts/README.md")
        for name in included + ("contracts/upstream/spec.md", ".venv/README.md"):
            self.write(name, "# Guide")
        self.assertEqual(
            {p.relative_to(self.root).as_posix() for p in gate.markdown_documents(self.root)},
            {"README.md", *included},
        )

    def test_repository(self):
        checked, violations = gate.scan()
        self.assertGreater(checked, 10)
        self.assertEqual(violations, [])


if __name__ == "__main__":
    unittest.main()
