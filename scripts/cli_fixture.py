"""Resolve a real CLI fixture, then pass its absolute path to an SDK test process.

Cargo's compiler-artifact message owns the executable path: no assumptions about
CARGO_TARGET_DIR, llvm-cov's environment, profile directories or target triples.
Downloaded release artifacts use the same file validation and child environment.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def checked_binary(path):
    binary = Path(path).resolve()
    if not binary.is_file():
        raise ValueError(f"CLI fixture is not a file: {binary}")
    if not os.access(binary, os.X_OK):
        raise ValueError(f"CLI fixture is not executable: {binary}")
    return binary


def cargo_binary(output):
    paths = set()
    finished = False
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("reason") == "build-finished":
            finished = message.get("success") is True
        target = message.get("target", {})
        if (message.get("reason") == "compiler-artifact"
                and target.get("name") == "plenora-rest"
                and target.get("kind") == ["bin"]
                and not message.get("profile", {}).get("test", False)
                and message.get("executable")):
            paths.add(message["executable"])
    if not finished or len(paths) != 1:
        raise ValueError("Cargo did not report exactly one successful plenora-rest executable")
    binary = Path(paths.pop())
    if not binary.is_absolute():
        binary = ROOT / binary
    return checked_binary(binary)


def build_binary(offline=False):
    command = ["cargo", "build", "--locked", "-p", "plenora-rest-cli",
               "--bin", "plenora-rest", "--message-format=json-render-diagnostics"]
    if offline:
        command.append("--offline")
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE,
                            text=True, encoding="utf-8")
    if result.returncode:
        raise ValueError(f"CLI fixture build failed (exit {result.returncode})")
    return cargo_binary(result.stdout)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--build", action="store_true")
    source.add_argument("--binary", help="downloaded or previously built CLI artifact")
    parser.add_argument("--offline", action="store_true", help="build from Cargo cache only")
    parser.add_argument("--copy-to", help="stage the resolved binary for artifact upload")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        binary = build_binary(args.offline) if args.build else checked_binary(args.binary)
        if args.copy_to:
            destination = Path(args.copy_to).resolve()
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(binary, destination)
            binary = checked_binary(destination)
        command = args.command
        if command[:1] == ["--"]:
            command = command[1:]
        if not command:
            print(binary)
            return 0
        print(f"CLI fixture: {binary}", file=sys.stderr)
        env = {**os.environ, "PLENORA_TEST_CLI": str(binary)}
        return subprocess.run(command, env=env).returncode
    except (OSError, ValueError) as error:
        print(f"CLI fixture error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
