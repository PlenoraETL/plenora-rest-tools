"""Execute discovered Rust proofs and capture/check observable 0.4.0 results."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from frozen_proofs import ROOT
from source_identity import source_digests

TARGETS = [("plenora-rest-core", "--lib", None),
           ("plenora-rest-core", "--test", "black_box"),
           ("plenora-rest-core", "--test", "runtime_binding"),
           ("plenora-rest-cli", "--test", "cli"),
           ("plenora-rest-core", "--test", "runtime_vectors")]
BASELINE = ROOT / "tests/frozen/0.4.0/observed.json"


def environment():
    temporary = ROOT / "target/tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    return {**os.environ, "CARGO_BUILD_JOBS": "2", "TEMP": str(temporary), "TMP": str(temporary)}


def sdk_program(name):
    directory = ROOT / "target/sdk-venv" / ("Scripts" if os.name == "nt" else "bin")
    return directory / (name + ".exe" if os.name == "nt" else name)


def executable(package, kind, target, env):
    command = ["cargo", "test", "-p", package, kind]
    if target:
        command.append(target)
    command += ["--locked", "--offline", "--no-run", "--message-format=json"]
    built = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8")
    print(built.stderr, end="", flush=True)
    if built.returncode:
        raise RuntimeError("target compilation failed")
    artifacts = [json.loads(line) for line in built.stdout.splitlines() if line.startswith("{")]
    paths = [item["executable"] for item in artifacts if item.get("reason") == "compiler-artifact"
             and item.get("executable") and item["profile"]["test"]]
    if len(paths) != 1:
        raise ValueError("ambiguous test executable")
    return paths[0]


def run_target(binary, env, directory, selection="frozen_", retained=()):
    listed = subprocess.run([binary, selection, "--list"], env=env, capture_output=True, text=True, check=True)
    ids = [line.removesuffix(": test") for line in listed.stdout.splitlines() if line.endswith(": test")]
    if not ids or len(ids) != len(set(ids)):
        raise ValueError("empty or duplicate Rust discovery")
    for identity in retained:
        probe = subprocess.run([binary, identity, "--exact", "--list"], env=env, capture_output=True, text=True, check=True)
        if probe.stdout.splitlines().count(identity + ": test") != 1 or identity in ids:
            raise ValueError("retained test absent or duplicated")
        ids.append(identity)
    receipts = []
    for identity in ids:
        command = [binary, identity, "--exact", "--nocapture"]
        try:
            result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8", timeout=45)
        except subprocess.TimeoutExpired:
            receipts.append({"id":identity,"status":"current-timeout"})
            continue
        log = result.stdout + result.stderr
        name = identity.replace("::", "-")
        (directory / f"{name}.log").write_text(log, encoding="utf-8", newline="\n")
        passed = rust_passed(result.returncode, log)
        receipts.append({"id": identity, "status": "current-pass" if passed else "current-fail", "exit":result.returncode})
        print(f"{identity}: {receipts[-1]['status']}", flush=True)
        if not passed:
            print(log, flush=True)
    return receipts


def rust_passed(code, log):
    summaries = re.findall(r"test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;", log)
    return code == 0 and summaries == [("ok", "1", "0", "0")]


def rust_source_paths(root=ROOT):
    """Production sources include private modules nested below src/."""
    return sorted((root / "crates").glob("*/src/**/*.rs"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--record", action="store_true", help="explicitly replace observed baseline after successful execution")
    parser.add_argument("--check", action="store_true", help="compare against observed baseline (default)")
    args = parser.parse_args()
    if args.record and args.check:
        parser.error("choose record or check")
    directory = ROOT / "target/frozen/rust"
    directory.mkdir(parents=True, exist_ok=True)
    env = environment()
    env.pop("SEMPL_CAPTURE_DIR", None)
    env.pop("SEMPL_CHECK_CAPTURE_DIR", None)
    env.pop("SEMPL_CONTROL_CASE", None)
    results = []
    with tempfile.TemporaryDirectory(dir=ROOT / "target", prefix="frozen-observed-") as capture:
        if args.record:
            env["SEMPL_CAPTURE_DIR"] = capture
        else:
            env["SEMPL_CHECK_CAPTURE_DIR"] = capture
        for package, kind, target in TARGETS:
            try:
                binary = executable(package, kind, target, env)
                retained = ()
                if target == "black_box":
                    retained = ("gzip_body_timeout_is_classified_after_headers",
                                "gzip_body_timeout_reaches_reqwest_decode_after_send_succeeds",
                                "a_retry_after_beyond_the_cap_is_not_shortened_into_an_early_retry",
                                "a_retry_cut_short_by_the_deadline_is_not_counted")
                results.extend(run_target(binary, env, directory,
                                          selection="" if target == "runtime_vectors" else "frozen_", retained=retained))
            except (RuntimeError, ValueError, subprocess.SubprocessError) as error:
                results.append({"id": f"{package}/{target or 'lib'}", "status": "blocked", "reason": str(error)})
        try:
            signals = subprocess.run([sys.executable, str(ROOT / "scripts/frozen_signals.py")], cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8", timeout=40)
            print(signals.stdout + signals.stderr, flush=True)
            results.append({"id":"cli-os-signals", "status":"current-pass" if signals.returncode == 0 else "current-fail"})
        except (OSError, subprocess.SubprocessError) as error:
            results.append({"id":"cli-os-signals", "status":"blocked", "reason":str(error)})
        try:
            sdk = subprocess.run([str(sdk_program("python")), str(ROOT / "scripts/frozen_sdk.py")], cwd=ROOT, env=env, capture_output=True, text=True, encoding="utf-8", timeout=60)
            print(sdk.stdout + sdk.stderr, flush=True)
            (directory / "sdk-process.log").write_text(sdk.stdout + sdk.stderr, encoding="utf-8", newline="\n")
            results.append({"id":"native-sdk", "status":"current-pass" if sdk.returncode == 0 else "current-fail"})
        except (OSError, subprocess.SubprocessError) as error:
            results.append({"id":"native-sdk", "status":"blocked", "reason":str(error)})
        passed = bool(results) and all(item["status"] == "current-pass" for item in results)
        cases = {p.stem: json.loads(p.read_text(encoding="utf-8")) for p in sorted(Path(capture).glob("*.json"))}
        if not args.record:
            expected = json.loads(BASELINE.read_text(encoding="utf-8"))["cases"]
            if cases != expected:
                results.append({"id":"complete-snapshot-inventory","status":"current-fail",
                                "missing":sorted(set(expected)-set(cases)),"extra":sorted(set(cases)-set(expected))})
                passed = False
        paths = sorted((ROOT / "crates").rglob("*.rs")) + sorted((ROOT / "tests/frozen/0.4.0").glob("*.json"))
        paths += [ROOT / "scripts/frozen_signals.py", ROOT / "python/tests/test_frozen_private.py"]
        receipt = {"commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                   "tests": results, "passed": passed, "files": {p.relative_to(ROOT).as_posix(): value for p, value in source_digests(paths).items()},
                   "mode": "record" if args.record else "check", "source_identity":"sha256-git-clean"}
        (directory / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8", newline="\n")
        if args.record and passed:
            if not cases:
                raise ValueError("no observations captured")
            BASELINE.write_text(json.dumps({"version":"0.4.0", "source_commit":receipt["commit"],
                "source_identity":"sha256-git-clean",
                "source_files":{p.relative_to(ROOT).as_posix(): value for p, value in source_digests(rust_source_paths()).items()},
                "normalization":["execution metrics.elapsed_ms at explicit core/CLI/SDK/runtime payload paths", "runtime response plenora.message.id", "remote fixture loopback origin (asserted before replacement)"],
                "contract_checked_cases":sorted(key for key,value in cases.items() if key.startswith("control-artifacts-") and "rule" in value),
                "artifact_observations":"Raw files/bytes/results are retained in target/frozen/control-artifacts/<os>; contract_checked_cases freeze invariants, not scheduler outcomes.",
                "cases":cases}, indent=2, sort_keys=True) + "\n", encoding="utf-8", newline="\n")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
