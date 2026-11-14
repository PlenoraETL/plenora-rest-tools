"""Verify the real Python 3.14/native boundary rejects a reflected request mutant."""
import hashlib
import json
import subprocess

from frozen_rust import ROOT, environment, sdk_program


def main():
    output = ROOT / "target/frozen/sdk-mutation"
    output.mkdir(parents=True, exist_ok=True)
    python = sdk_program("python")
    env = {**environment(), "VIRTUAL_ENV": str(ROOT / "target/sdk-venv"),
           "UV_CACHE_DIR": str(ROOT / "target/uv-cache"), "PYO3_PYTHON": str(python)}
    env.pop("SEMPL_CAPTURE_DIR", None)
    env.pop("SEMPL_CHECK_CAPTURE_DIR", None)
    test = [str(python), "-m", "unittest", "discover", "-s", "python/tests", "-p", "test_frozen_private.py"]

    def run(command, label):
        result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True)
        (output / f"{label}.log").write_bytes(result.stdout + result.stderr)
        return result

    def build(label):
        result = run([str(sdk_program("maturin")), "develop", "--locked", "--offline"], label)
        if result.returncode:
            raise RuntimeError("native build or install failed")

    if run(test, "baseline").returncode:
        raise RuntimeError("native baseline failed")
    path = ROOT / "crates/rest-engine-core/src/engine.rs"
    original = path.read_bytes()
    before = b"serde_json::to_string(&result)"
    after = b'serde_json::to_string(&{ let mut public = serde_json::json!(result); public["diagnostic"] = serde_json::json!(request_json); public })'
    if original.count(before) != 1:
        raise ValueError("ambiguous mutation anchor")
    try:
        path.write_bytes(original.replace(before, after))
        build("mutant-build")
        mutant = run(test, "mutant")
    finally:
        path.write_bytes(original)
        build("restored-build")
    restored = run(test, "restored")
    killed = mutant.returncode == 1 and b"FAILED (failures=" in mutant.stderr and b"errors=" not in mutant.stderr
    receipt = {"id":"T4-native-sdk-reflection", "killed":killed,
               "restored_green":restored.returncode == 0,
               "restored_sha256":hashlib.sha256(path.read_bytes()).hexdigest(),
               "byte_identical":path.read_bytes() == original}
    (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt))
    return 0 if killed and receipt["restored_green"] and receipt["byte_identical"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
