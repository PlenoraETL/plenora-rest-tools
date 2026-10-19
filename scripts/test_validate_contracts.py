"""Regression tests for data-free contract gate diagnostics (decision 0021)."""
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import validate_contracts as gate

CANARY = "PRIVATE_DATA_SENTINEL"


class DiagnosticsTests(unittest.TestCase):
    def test_diagnostic_never_renders_input(self):
        with self.assertRaises(SystemExit) as error:
            gate.fail(CANARY)
        self.assertEqual(str(error.exception), "contract validation failed")

    def test_bad_json_and_missing_file_do_not_expose_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / CANARY
            with patch.object(gate, "ROOT", root):
                for contents in (None, '{"' + CANARY + '":'):
                    if contents is not None:
                        path.write_text(contents)
                    with self.assertRaises(SystemExit) as error:
                        gate.load_json(path)
                    self.assertEqual(str(error.exception), "contract validation failed")

    def test_adoption_schema_error_does_not_render_value(self):
        original = gate.load_json
        def read(path):
            value = original(path)
            if path == gate.ADOPTION_MANIFEST:
                value["component"] = CANARY
            return value
        with patch.object(gate, "load_json", side_effect=read):
            with self.assertRaises(SystemExit) as error:
                gate.validate_upstream()
        self.assertEqual(str(error.exception), "contract validation failed")

    def test_requirement_register_cannot_omit_a_rule(self):
        original = gate.load_json
        def read(path):
            value = original(path)
            if path.name == "adoption-requirements.json":
                value["requirements"].pop()
            return value
        with patch.object(gate, "load_json", side_effect=read):
            with self.assertRaises(SystemExit):
                gate.validate_upstream()


if __name__ == "__main__":
    unittest.main()
