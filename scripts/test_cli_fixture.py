"""CLI discovery is an artifact observation, never a guessed target directory."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import cli_fixture
from check_assurance import workflows

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/cli_fixture.py"


def artifact(path, **changes):
    value = {"reason": "compiler-artifact", "target": {"name": "plenora-rest", "kind": ["bin"]},
             "profile": {"test": False}, "executable": str(path)}
    value.update(changes)
    return value


def output(*messages, success=True):
    return "\n".join(json.dumps(m) for m in [*messages, {"reason": "build-finished", "success": success}])


class CliFixtureTests(unittest.TestCase):
    def test_cargo_reports_the_path_even_without_target_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "cov target" / "aarch64-unknown-linux-gnu" / "custom-profile" / "plenora-rest"
            binary.parent.mkdir(parents=True)
            binary.touch()
            binary.chmod(0o700)
            text = output(artifact(binary), artifact(binary, profile={"test": True}, executable="wrong"),
                          artifact(binary, target={"name": "other", "kind": ["bin"]}))
            env = {k: v for k, v in os.environ.items() if k != "CARGO_TARGET_DIR"}
            env["CARGO_LLVM_COV_TARGET_DIR"] = str(Path(directory) / "not-the-binary-directory")
            with patch.dict(os.environ, env, clear=True), patch("cli_fixture.subprocess.run") as run:
                run.return_value = subprocess.CompletedProcess([], 0, text)
                self.assertEqual(cli_fixture.build_binary(offline=True), binary.resolve())
                self.assertEqual(run.call_args.kwargs["cwd"], cli_fixture.ROOT)
                self.assertIn("--offline", run.call_args.args[0])
                self.assertIn("--message-format=json-render-diagnostics", run.call_args.args[0])

    def test_missing_ambiguous_or_failed_build_is_refused(self):
        for text in [output(), output(artifact(sys.executable), success=False),
                     output(artifact(sys.executable), artifact("another-executable")),
                     output(artifact("missing-cli-fixture")), "not json"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                cli_fixture.cargo_binary(text)
        with patch("cli_fixture.subprocess.run", return_value=subprocess.CompletedProcess([], 1, output(artifact(sys.executable)))):
            with self.assertRaisesRegex(ValueError, "build failed"):
                cli_fixture.build_binary()

    def test_child_receives_absolute_fixture_from_another_cwd(self):
        with tempfile.TemporaryDirectory() as directory:
            env = {**os.environ, "PLENORA_TEST_CLI": "", "CARGO_LLVM_COV_TARGET_DIR": "wrong"}
            child = "import os,pathlib; p=pathlib.Path(os.environ['PLENORA_TEST_CLI']); assert p.is_absolute() and p.is_file(); print(p)"
            result = subprocess.run([sys.executable, str(SCRIPT), "--binary", sys.executable, "--",
                                     sys.executable, "-c", child], cwd=directory, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(Path(result.stdout.strip()), Path(sys.executable).resolve())
            result = subprocess.run([sys.executable, str(SCRIPT), "--binary", sys.executable, "--",
                                     sys.executable, "-c", "raise SystemExit(7)"], cwd=directory)
            self.assertEqual(result.returncode, 7)

    def test_missing_fixture_stops_before_the_suite(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "suite-started"
            child = "from pathlib import Path; Path(" + repr(str(marker)) + ").touch()"
            result = subprocess.run([sys.executable, str(SCRIPT), "--binary", "missing", "--",
                                     sys.executable, "-c", child], cwd=directory, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("CLI fixture is not a file", result.stderr)
            self.assertFalse(marker.exists())

    def test_artifact_staging_uses_the_resolved_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            staged = Path(directory) / "upload folder" / Path(sys.executable).name
            result = subprocess.run([sys.executable, str(SCRIPT), "--binary", sys.executable,
                                     "--copy-to", str(staged)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(Path(result.stdout.strip()), staged.resolve())
            self.assertEqual(staged.read_bytes(), Path(sys.executable).read_bytes())

    def test_all_installed_wheel_jobs_use_the_common_resolver(self):
        documents = workflows(ROOT)  # repository's duplicate-rejecting YAML reader
        runs = {name: "\n".join(step.get("run", "") for job in workflow["jobs"].values()
                               for step in job.get("steps", [])) for name, workflow in documents.items()}
        coverage = runs["coverage.yml"]
        self.assertIn('"$GITHUB_WORKSPACE/scripts/cli_fixture.py" --build --', coverage)
        self.assertNotIn('realpath "$CARGO_TARGET_DIR', coverage)
        self.assertIn("cli_fixture.py --build --copy-to", runs["verify.yml"])
        for name in ("verify.yml", "release.yml"):
            self.assertIn("-CliBinary ", runs[name])
        wheel = (ROOT / "scripts/test_wheel.ps1").read_text(encoding="utf-8")
        self.assertIn("--binary $cliPath -- $venvPython -m unittest", wheel)
        for name in ("Dockerfile.release", "Dockerfile.verify"):
            self.assertIn("scripts/cli_fixture.py", (ROOT / name).read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
