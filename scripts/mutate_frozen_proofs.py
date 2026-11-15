"""Run short, explicit mutations, restoring exact source bytes even on failure."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MUTATIONS = [
    ("T5-rust-skip", "scripts/frozen_rust.py", 'summaries == [("ok", "1", "0", "0")]', 'True',
     "test_frozen_rust_receipts.RustReceiptTests.test_only_one_executed_success_qualifies"),
    ("T5-rust-continue", "scripts/frozen_rust.py", "for identity in ids:", "for identity in ids[:1]:",
     "test_frozen_rust_receipts.RustReceiptTests.test_discovery_executes_later_tests_after_failure_and_timeout"),
    ("T7-drain", "python/plenora_rest/_async_engine.py", "while not worker.done():", "while False:",
     "test_frozen_async.FrozenWorkerTests.test_repeated_cancel_drains_late_error_and_concurrent_close"),
    ("T7-construction", "python/plenora_rest/_async_engine.py", "            raise value", "            return None",
     "test_frozen_async.FrozenWorkerTests.test_failed_construction_can_be_retried_without_publishing_instance"),
    ("T6-archive-bytes", "scripts/check_packages.py", "check_content(source.read())", "source.read()",
     "test_frozen_packages.FrozenPackageTests.test_archive_mutations_cannot_hide_behind_an_allowed_name"),
    ("T6-release-section", "scripts/release.py", "lines[start + 1 : end]", "lines[start + 1 :]",
     "test_frozen_packages.FrozenPackageTests.test_release_notes_are_exactly_the_dated_version_section"),
    ("T5-continue", "scripts/frozen_proofs.py", "resultclass=ReceiptResult)", "resultclass=ReceiptResult, failfast=True)",
     "test_frozen_receipts.FrozenReceiptTests.test_discovery_and_receipts_preserve_every_outcome"),
    ("T5-skip", "scripts/frozen_proofs.py", 'all(item["status"] == "current-pass" for item in result.receipts.values())', 'result.wasSuccessful()',
     "test_frozen_receipts.FrozenReceiptTests.test_skip_alone_cannot_qualify_and_new_tests_are_discovered"),
    ("T5-empty", "scripts/frozen_proofs.py", "if not ids or len(set(ids)) != len(ids):", "if len(set(ids)) != len(ids):",
     "test_frozen_receipts.FrozenReceiptTests.test_subtest_failure_and_missing_discovery_fail_closed"),
]


def main():
    output = ROOT / "target/frozen"
    output.mkdir(parents=True, exist_ok=True)
    receipts = []
    for name, relative, before, after, test in MUTATIONS:
        path = ROOT / relative
        original = path.read_bytes()
        if original.count(before.encode()) != 1:
            raise RuntimeError(f"mutation anchor is not unique: {name}")
        command = [sys.executable, "-B", "-X", f"pycache_prefix={output / ('cache-' + name)}",
                   "-m", "unittest", test]
        env = {**os.environ, "CARGO_BUILD_JOBS": "2"}
        baseline = subprocess.run(command, cwd=ROOT / "scripts", env=env, capture_output=True, timeout=30)
        if baseline.returncode != 0:
            raise RuntimeError(f"baseline is not green: {name}")
        try:
            path.write_bytes(original.replace(before.encode(), after.encode()))
            mutant = subprocess.run(command, cwd=ROOT / "scripts", env=env, capture_output=True, timeout=30)
        finally:
            path.write_bytes(original)
        if path.read_bytes() != original:
            raise RuntimeError("source restoration failed")
        log = mutant.stdout + mutant.stderr
        (output / f"{name}.log").write_bytes(log)
        killed = mutant.returncode == 1 and b"FAILED (failures=" in log and b"errors=" not in log
        receipts.append({"id": name, "file": relative, "test": test,
                         "baseline": "current-pass", "killed_by_assertion": killed,
                         "restored_sha256": hashlib.sha256(original).hexdigest(),
                         "replacement": {"before": before, "after": after}})
    (output / "mutations.json").write_text(json.dumps(receipts, indent=2) + "\n", encoding="utf-8")
    print(json.dumps([{r["id"]: r["killed_by_assertion"]} for r in receipts]))
    return 0 if all(r["killed_by_assertion"] for r in receipts) else 1


if __name__ == "__main__":
    raise SystemExit(main())
