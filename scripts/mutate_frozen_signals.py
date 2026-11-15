"""Qualify first/second OS signal assertions against temporary CLI mutations."""
import hashlib
import json
import subprocess
import sys

from frozen_rust import ROOT, environment


def exercise(env, directory, label):
    build = subprocess.run(["cargo", "build", "-p", "plenora-rest-cli", "--bin", "plenora-rest", "--locked", "--offline"], cwd=ROOT, env=env, capture_output=True)
    (directory / (label + "-build.log")).write_bytes(build.stdout + build.stderr)
    if build.returncode:
        raise RuntimeError("CLI compilation failed")
    result = subprocess.run([sys.executable, str(ROOT / "scripts/frozen_signals.py")], cwd=ROOT, env=env, capture_output=True, timeout=40)
    log = result.stdout + result.stderr
    (directory / (label + ".log")).write_bytes(log)
    return {"exit":result.returncode, "passed":result.returncode == 0,
            "assertion_failure":result.returncode != 0 and b"AssertionError" in log}


def main():
    path = ROOT / "crates/rest-cli/src/signal.rs"
    original = path.read_bytes()
    output = ROOT / "target/frozen/signal-mutations"
    output.mkdir(parents=True, exist_ok=True)
    env = environment()
    env.pop("SEMPL_CAPTURE_DIR", None)
    env.pop("SEMPL_CHECK_CAPTURE_DIR", None)
    receipts = []
    for name, before, after, count in [
        ("first", b"    token.cancel();", b"    let _ = token;", 1),
        ("second", b"            if first {", b"            if !first { std::process::exit(99); }\n            if first {", 2),
    ]:
        assert original.count(before) == count
        baseline = exercise(env, output, name + "-baseline")
        assert baseline["passed"], baseline
        (output / (name + ".original")).write_bytes(original)
        try:
            path.write_bytes(original.replace(before, after))
            mutated = exercise(env, output, name + "-mutant")
        finally:
            path.write_bytes(original)
        restored = exercise(env, output, name + "-restored")
        assert path.read_bytes() == original
        receipt = {"id":name,"baseline":baseline,"mutant":mutated,"restored":restored,
                   "restored_sha256":hashlib.sha256(original).hexdigest(),
                   "killed":mutated["assertion_failure"] and restored["passed"]}
        (output / (name + ".json")).write_text(json.dumps(receipt,indent=2)+"\n",encoding="utf-8")
        print(f"CLI {name} signal: killed={receipt['killed']}", flush=True)
        receipts.append(receipt)
    return 0 if all(r["killed"] for r in receipts) else 1


if __name__ == "__main__":
    raise SystemExit(main())
