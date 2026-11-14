"""Mutation proofs: stale inventories and disconnected gates must turn red."""
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

import check_assurance as gate
import run_assurance


class AssuranceTests(unittest.TestCase):
    def setUp(self):
        (gate.ROOT / "target").mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=gate.ROOT / "target")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for directory in ("fuzz", "campaign", "contracts", "crates", ".github", "scripts", "docs", "python"):
            shutil.copytree(gate.ROOT / directory, self.root / directory,
                            ignore=shutil.ignore_patterns("__pycache__"))
        for name in ("Cargo.toml", "pyproject.toml", "Dockerfile.release", "Dockerfile.verify"):
            shutil.copyfile(gate.ROOT / name, self.root / name)

    def mutate(self, name, before, after):
        path = self.root / name
        text = path.read_text(encoding="utf-8")
        self.assertIn(before, text)
        path.write_text(text.replace(before, after), encoding="utf-8")

    def test_current_sources(self):
        self.assertEqual(gate.scan(self.root), [])

    def test_duplicate_inventory_keys_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "duplicate inventory"):
            gate.document('{"probe": 1, "probe": 2}')
        self.mutate(".github/workflows/verify.yml", "jobs:\n", "jobs: {}\njobs:\n")
        with self.assertRaisesRegex(ValueError, "duplicate inventory"):
            gate.workflows(self.root)

    def test_fuzz_matrix_cannot_omit_target(self):
        self.mutate(".github/workflows/fuzz.yml", "          - json_path\n", "")
        with self.assertRaisesRegex(ValueError, "fuzz matrix"):
            gate.check_fuzz(self.root)

    def test_corpus_extra_and_changed_bytes_are_rejected(self):
        extra = self.root / "fuzz/corpus/extra"
        extra.write_bytes(b"extra")
        with self.assertRaisesRegex(ValueError, "seed corpus"):
            gate.check_fuzz(self.root)
        extra.unlink()
        next((self.root / "fuzz/corpus/json_path").iterdir()).write_bytes(b"changed")
        with self.assertRaisesRegex(ValueError, "seed corpus"):
            gate.check_fuzz(self.root)

    def test_new_scenario_requires_all_profile_inventories(self):
        self.mutate("crates/rest-campaign/src/scenarios.rs", '    Ok => "ok",',
                    '    New => "new",\n    Ok => "ok",')
        with self.assertRaisesRegex(ValueError, "mix"):
            gate.check_campaign(self.root)

    def test_campaign_cannot_stop_after_first_failure(self):
        self.mutate(".github/workflows/campaign.yml", "smoke --quick || status=1", "smoke --quick")
        with self.assertRaisesRegex(ValueError, "continue after"):
            gate.check_campaign(self.root)

    def test_stale_runtime_count_is_rejected(self):
        self.mutate("crates/rest-engine-core/tests/runtime_vectors.rs", "exercised, 20,", "exercised, 19,")
        with self.assertRaisesRegex(ValueError, "probe counts"):
            gate.check_runtime(self.root)

    def test_added_probe_is_not_hidden_by_old_count(self):
        path = self.root / "contracts/upstream/source.json"
        pins = json.loads(path.read_text(encoding="utf-8"))
        existing = next(k for k in pins["files"] if k.startswith("runtime-probes-v1/"))
        new = "runtime-probes-v1/new-probe.json"
        pins["files"][new] = pins["files"][existing]
        shutil.copyfile(self.root / "contracts/upstream" / existing, self.root / "contracts/upstream" / new)
        path.write_text(json.dumps(pins), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "probe counts"):
            gate.check_runtime(self.root)

    def test_new_package_source_is_not_hidden(self):
        (self.root / "crates/rest-cli/src/new.rs").write_text("// new source\n")
        with self.assertRaisesRegex(ValueError, "package sources"):
            gate.check_packages(self.root)

    def test_evidence_test_must_exist(self):
        self.mutate("contracts/adoption-requirements.json", "::runtime_v2_discovery_preserves_v1_wire_and_selectors",
                    "::nonexistent_test")
        with self.assertRaisesRegex(ValueError, "evidence test"):
            gate.check_evidence(self.root)

    def test_external_context_cannot_measure_another_revision(self):
        path = self.root / "contracts/adoption-requirements.json"
        data = json.loads(path.read_text(encoding="utf-8"))
        next(e for e in data["requirements"] if "upstream_context" in e)["upstream_context"]["revision"] = "old"
        path.write_text(json.dumps(data), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "adopted revision"):
            gate.check_evidence(self.root)

    def test_windows_cannot_omit_contract_self_tests(self):
        self.mutate(".github/workflows/verify.yml", '-p "test_*.py"', '-p "test_check_*.py"')
        with self.assertRaisesRegex(ValueError, "omits contract tests"):
            gate.check_workflows(self.root)

    def test_pr_filter_and_disconnected_runner_are_rejected(self):
        self.mutate(".github/workflows/verify.yml", "  pull_request:\n", "  pull_request:\n    paths: ['scripts/**']\n")
        with self.assertRaisesRegex(ValueError, "every PR"):
            gate.check_workflows(self.root)
        self.mutate(".github/workflows/verify.yml", "    paths: ['scripts/**']\n", "")
        self.mutate(".github/workflows/verify.yml", "run: python scripts/run_assurance.py", "run: echo disconnected")
        with self.assertRaisesRegex(ValueError, "not connected"):
            gate.check_workflows(self.root)

    def test_release_matrix_drift_and_fail_fast_are_rejected(self):
        self.mutate(".github/workflows/release.yml", "fail-fast: false", "fail-fast: true")
        with self.assertRaisesRegex(ValueError, "collect all"):
            gate.check_workflows(self.root)
        self.mutate(".github/workflows/release.yml", "fail-fast: true", "fail-fast: false")
        self.mutate(".github/workflows/release.yml", ", '3.14'", "")
        with self.assertRaisesRegex(ValueError, "Python support"):
            gate.check_workflows(self.root)

    def test_inventory_collects_new_files_without_a_registry_edit(self):
        path = self.root / "scripts/test_new.py"
        path.write_text("# new self test\n")
        self.assertIn("scripts/test_new.py", gate.inventory(self.root))

    def test_tool_pin_drift_is_rejected(self):
        self.mutate(".github/workflows/audit.yml", "jsonschema==4.26.0", "jsonschema==4.25.1")
        with self.assertRaisesRegex(ValueError, "tool pin"):
            gate.check_tool_pins(self.root)

    def test_self_test_outside_discovery_is_rejected(self):
        (self.root / "scripts/orphan.py").write_text("def test_orphan():\n    pass\n")
        with self.assertRaisesRegex(ValueError, "outside unittest"):
            gate.check_script_discovery(self.root)

    def test_coverage_cannot_omit_shipped_sources(self):
        self.mutate("scripts/coverage_budget.json", '"crates/rest-cli/src/",', '')
        with self.assertRaisesRegex(ValueError, "coverage Rust"):
            gate.check_coverage_sources(self.root)

    def test_scan_collects_all_independent_failures(self):
        def bad_one(root):
            raise ValueError("first")
        def bad_two(root):
            raise ValueError("second")
        with patch.object(gate, "CHECKS", (bad_one, bad_two)):
            self.assertEqual(len(gate.scan(self.root)), 2)


class RunnerTests(unittest.TestCase):
    def test_failure_and_start_error_do_not_skip_later_commands(self):
        seen = []
        def run(command, **kwargs):
            seen.append(command)
            if command == ["missing"]:
                raise OSError("missing")
            return SimpleNamespace(returncode=int(command == ["failed"]))
        commands = [["failed"], ["missing"], ["last"]]
        self.assertEqual(run_assurance.run_all(commands, run), 1)
        self.assertEqual(seen, commands)

    def test_success_returns_zero(self):
        self.assertEqual(run_assurance.run_all([["ok"]], lambda *a, **kw: SimpleNamespace(returncode=0)), 0)


if __name__ == "__main__":
    unittest.main()
