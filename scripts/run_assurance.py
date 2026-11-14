"""Run all independent offline gates, collecting failures before returning red."""
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def commands():
    return [
        [sys.executable, "-m", "unittest", "discover", "-s", "scripts", "-p", "test_*.py", "-v"],
        *[[sys.executable, f"scripts/{name}.py"] for name in (
            "check_python_version", "check_assurance", "check_comments", "check_docs", "check_advisory_exemptions", "validate_contracts")],
        [sys.executable, "scripts/release.py", "current-version"],
    ]


def run_all(tasks, run=subprocess.run):
    failed = False
    for command in tasks:
        print("assurance: " + " ".join(command[1:]), flush=True)
        try:
            result = run(command, cwd=ROOT, check=False)
            failed |= result.returncode != 0
        except OSError:
            print("assurance: command could not start", file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(run_all(commands()))
