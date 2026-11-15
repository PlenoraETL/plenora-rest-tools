"""Regression tests for data-free contract gate diagnostics (decision 0021)."""
import shutil
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


class CurrentContractTests(unittest.TestCase):
    def test_current_contracts(self):
        gate.validate()

    def test_current_schema_can_change_without_historical_freeze(self):
        original = gate.load_json
        def read(path):
            value = original(path)
            if path.parent == gate.SCHEMA_ROOT:
                value["description"] = "Current documentation may evolve."
            return value
        with patch.object(gate, "load_json", side_effect=read):
            gate.validate()

    def test_second_local_version_is_rejected(self):
        for attribute, source, pattern in (
            ("SCHEMA_ROOT", gate.SCHEMA_ROOT, "*.schema.json"),
            ("RUST_BINDING", gate.RUST_BINDING.parent, "*.json"),
        ):
            with self.subTest(surface=attribute), tempfile.TemporaryDirectory() as directory:
                dest = Path(directory)
                for path in source.glob(pattern):
                    shutil.copyfile(path, dest / path.name)
                path = next(dest.glob(pattern))
                shutil.copyfile(path, dest / path.name.replace("-v1", "-v2"))
                value = dest if attribute == "SCHEMA_ROOT" else dest / gate.RUST_BINDING.name
                with patch.object(gate, attribute, value), self.assertRaises(SystemExit):
                    gate.validate_current_surface()

    def test_binding_must_match_catalog_and_existing_entrypoints(self):
        original = gate.load_json
        for mutation in ("operation", "entrypoint"):
            def read(path):
                value = original(path)
                if path == gate.RUST_BINDING:
                    if mutation == "operation":
                        value["operations"].pop()
                    else:
                        value["capability_entrypoint"] = "plenora_rest_core::missing_entrypoint"
                return value
            with self.subTest(mutation=mutation), patch.object(gate, "load_json", side_effect=read):
                with self.assertRaises(SystemExit):
                    gate.validate_current_surface()

    def test_all_revision_declarations_must_agree(self):
        original = gate.load_json
        for mutation in ("source", "register", "context", "fixtures"):
            def read(path):
                value = original(path)
                if mutation == "source" and path == gate.UPSTREAM_ROOT / "source.json":
                    value["revision"] = "0" * 40
                if mutation == "fixtures" and path == gate.ROOT / "tests/frozen/0.4.0/sources.json":
                    value["upstream_revision"] = "0" * 40
                if path.name == "adoption-requirements.json":
                    if mutation == "register":
                        value["revision"] = "0" * 40
                    elif mutation == "context":
                        next(entry["upstream_context"] for entry in value["requirements"]
                             if "upstream_context" in entry)["revision"] = "0" * 40
                return value
            with self.subTest(mutation=mutation), patch.object(gate, "load_json", side_effect=read):
                with self.assertRaises(SystemExit):
                    gate.validate_upstream()


if __name__ == "__main__":
    unittest.main()
