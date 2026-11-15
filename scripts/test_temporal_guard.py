"""Compiler-typed mutation probes for the production temporal arithmetic lint."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class TemporalGuard(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not shutil.which("rustup"):
            raise unittest.SkipTest("Rust toolchain unavailable in offline Python-only gate")
        result = subprocess.run(["rustup", "which", "clippy-driver"], cwd=ROOT,
                                capture_output=True, text=True, check=False)
        if result.returncode:
            raise unittest.SkipTest("Clippy unavailable in Python-only gate")
        cls.driver = result.stdout.strip()

    def compile(self, expression, lint="clippy::arithmetic_side_effects"):
        output = ROOT / "target"
        output.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=output, prefix="temporal-guard-") as directory:
            path = Path(directory) / "probe.rs"
            path.write_text("use std::time::{Instant as Clock, SystemTime as Wall, Duration};\n"
                            "pub fn probe(clock: Clock, wall: Wall, delay: Duration) {\n"
                            + expression + "\n}\n", encoding="utf-8")
            result = subprocess.run(
                [self.driver, "--crate-type=lib", "--edition=2024", "--emit=metadata",
                 "--out-dir", directory, "--error-format=json", "-D" + lint,
                 str(path)], cwd=ROOT, env={**os.environ, "CLIPPY_CONF_DIR": str(ROOT)},
                capture_output=True, text=True, check=False)
            diagnostics = [json.loads(line) for line in result.stderr.splitlines() if line.startswith("{")]
            rejected = any((item.get("code") or {}).get("code") == lint
                           for item in diagnostics)
            return result.returncode, rejected, result.stderr

    def test_rejects_typed_operations_independent_of_spelling(self):
        for expression in ["let _ = clock + delay;", "let _ = wall + delay;",
                           "let mut renamed = clock; renamed += delay;",
                           "let alias = wall; let _ = alias - delay;",
                           "macro_rules! add { ($a:expr, $b:expr) => { $a + $b }; } let _ = add!(clock, delay);",
                           "let _ = delay + delay;"]:
            with self.subTest(expression=expression):
                code, rejected, diagnostic = self.compile(expression)
                self.assertNotEqual(code, 0, diagnostic)
                self.assertTrue(rejected, diagnostic)

    def test_refuses_panicking_float_constructors(self):
        for kind in ["f32", "f64"]:
            code, rejected, diagnostic = self.compile(
                f"let _ = Duration::from_secs_{kind}({kind}::NAN);", "clippy::disallowed_methods")
            self.assertNotEqual(code, 0, diagnostic)
            self.assertTrue(rejected, diagnostic)

    def test_accepts_checked_operations_and_primitive_arithmetic(self):
        code, rejected, diagnostic = self.compile(
            "let _ = clock.checked_add(delay); let _ = wall.checked_add(delay);"
            "let _ = delay.checked_add(delay); let n = delay.as_secs(); let _ = n + 1;")
        self.assertEqual(code, 0, diagnostic)
        self.assertFalse(rejected, diagnostic)


if __name__ == "__main__":
    unittest.main()
