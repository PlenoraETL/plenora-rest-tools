"""Run installed native SDK proofs with per-test receipts on Python 3.14."""
import json
from pathlib import Path
import sys
import unittest

from frozen_proofs import ROOT, digest, run_suite


def main():
    if sys.version_info[:2] != (3, 14):
        raise SystemExit("SDK qualification requires Python 3.14")
    tests = ROOT / "python/tests"
    from plenora_rest import _native
    sys.path.insert(0, str(tests))
    suite = unittest.TestSuite()
    for pattern in ("test_frozen_private.py", "test_async.py"):
        suite.addTests(unittest.defaultTestLoader.discover(str(tests), pattern=pattern))
    result, log = run_suite(suite)
    result["python"] = sys.version
    result["native_sha256"] = digest(Path(_native.__file__))
    result["files"] = {p.relative_to(ROOT).as_posix():digest(p) for p in sorted((ROOT / "python").rglob("*.py"))}
    directory = ROOT / "target/frozen"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "sdk.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    (directory / "sdk.log").write_text(log, encoding="utf-8")
    print(log)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
