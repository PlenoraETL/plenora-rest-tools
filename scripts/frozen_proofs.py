"""Versioned fixture identities and per-test receipts; never infer a pass from discovery."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
INDEX = ROOT / "tests/frozen/0.4.0/sources.json"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_index():
    upstream = ROOT / "contracts/upstream"
    source = json.loads((upstream / "source.json").read_text(encoding="utf-8"))
    entries = {}
    for name, entry in sorted(source["files"].items()):
        actual = digest(upstream / name)
        if actual != entry["sha256"]:
            raise ValueError("adopted fixture differs from its pinned source")
        entries[f"contracts/upstream/{name}"] = actual
    entries["contracts/upstream/source.json"] = digest(upstream / "source.json")
    for path in sorted(INDEX.parent.glob("*.json")):
        if path.name not in {"sources.json", "observed.json"}:
            entries[path.relative_to(ROOT).as_posix()] = digest(path)
    return {"version": "0.4.0", "upstream_revision": source["revision"],
            "kind": "fixture-identities-not-observed-product-results", "files": entries}


def flatten(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from flatten(test)
        else:
            yield test


class ReceiptResult(unittest.TextTestResult):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.receipts = {}

    def startTest(self, test):
        super().startTest(test)
        self.receipts[test.id()] = {"status": "running"}

    def addSuccess(self, test):
        super().addSuccess(test)
        self.receipts[test.id()] = {"status": "current-pass"}

    def addFailure(self, test, err):
        super().addFailure(test, err)
        self.receipts[test.id()] = {"status": "current-fail"}

    def addError(self, test, err):
        super().addError(test, err)
        self.receipts[test.id()] = {"status": "current-error"}

    def addSkip(self, test, reason):
        super().addSkip(test, reason)
        self.receipts[test.id()] = {"status": "skip", "reason": reason}

    def addExpectedFailure(self, test, err):
        super().addExpectedFailure(test, err)
        self.receipts[test.id()] = {"status": "expected-failure"}

    def addUnexpectedSuccess(self, test):
        super().addUnexpectedSuccess(test)
        self.receipts[test.id()] = {"status": "unexpected-success"}

    def addSubTest(self, test, subtest, err):
        super().addSubTest(test, subtest, err)
        if err is not None:
            self.receipts[test.id()] = {"status": "current-fail"}


def run_suite(suite):
    ids = [test.id() for test in flatten(suite)]
    if not ids or len(set(ids)) != len(ids):
        raise ValueError("empty or duplicated discovery")
    stream = io.StringIO()
    result = unittest.TextTestRunner(stream=stream, resultclass=ReceiptResult).run(suite)
    complete = set(result.receipts) == set(ids)
    passed = complete and all(item["status"] == "current-pass" for item in result.receipts.values())
    return {"discovered": ids, "tests": result.receipts, "passed": passed}, stream.getvalue()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check fixture identities without overwriting")
    parser.add_argument("--run-python", action="store_true")
    args = parser.parse_args()
    data = json.dumps(source_index(), indent=2, sort_keys=True) + "\n"
    if args.check:
        if INDEX.read_text(encoding="utf-8") != data:
            raise SystemExit("frozen fixture identities differ")
    else:
        INDEX.write_text(data, encoding="utf-8", newline="\n")
    if args.run_python:
        suite = unittest.defaultTestLoader.discover(str(ROOT / "scripts"), pattern="test_frozen_*.py")
        receipt, log = run_suite(suite)
        receipt["commit"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        receipt["worktree"] = "modified; see file digests"
        paths = [ROOT / "python/plenora_rest/_async_engine.py", ROOT / "scripts/check_packages.py", ROOT / "scripts/release.py"]
        paths += sorted((ROOT / "scripts").glob("*frozen*.py"))
        receipt["files"] = {path.relative_to(ROOT).as_posix(): digest(path) for path in paths}
        receipt["scope"] = "Python assurance and async facade with controlled worker; excludes native SDK and Rust"
        output = ROOT / "target/frozen"
        output.mkdir(parents=True, exist_ok=True)
        (output / "python.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
        (output / "python.log").write_text(log, encoding="utf-8")
        print(log)
        return 0 if receipt["passed"] else 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
