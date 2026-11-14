"""Targeted Rust mutations: baseline, assertion failure, byte restoration, green recheck."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from frozen_rust import ROOT, environment, executable, rust_passed

CORE = "crates/rest-engine-core/src/"
MUTATIONS = [
    ("T1-control-phase", CORE + "error.rs", "Self::Cancelled => ErrorPhase::Cleanup,", "Self::Cancelled => ErrorPhase::Read,",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_controls_before_send_and_after_headers"])]),
    ("T1-effect", CORE + "error.rs", "error.remote_effect = RemoteEffect::Unknown;", "error.remote_effect = RemoteEffect::None;",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_transition_oracle", "frozen::frozen_serial_concurrent_same_corpus"])]),
    ("T1-phase", CORE + "error.rs", "error.phase = entered;", "error.phase = ErrorPhase::Validate;",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_transition_oracle"])]),
    ("T1-order", CORE + "engine.rs", "for outcome in ordered {", "for outcome in ordered.into_iter().rev() {",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_mixed_records_complete_out_of_order"])]),
    ("T1-batch", CORE + "engine.rs", "request.input.records.chunks(batch.max_size)", "request.input.records.chunks(1)",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_batch_has_its_own_wire_oracle"])]),
    ("T2-arithmetic", CORE + "transport.rs", "let backoff_ms = backoff.next_ms();", "let backoff_ms = backoff.next_ms().saturating_add(1);",
     [("plenora-rest-core", "--lib", None, ["transport::tests::frozen_retry_policy_with_injected_clock_and_rational_oracle"])]),
    ("T2-cap", CORE + "transport.rs", "delay > policy.max_retry_after_ms", "delay >= policy.max_retry_after_ms",
     [("plenora-rest-core", "--lib", None, ["transport::tests::frozen_retry_policy_with_injected_clock_and_rational_oracle"]),
      ("plenora-rest-core", "--test", "black_box", ["frozen::frozen_retry_matrix_buffered_and_download"])]),
    ("T4-core-leak", CORE + "engine.rs", "serde_json::to_string(&result)", 'serde_json::to_string(&serde_json::json!({"status":"failed","metrics":{"requests":0},"diagnostic":request_json}))',
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_private_admission_errors"])]),
    ("T4-runtime-leak", CORE + "runtime.rs", "payload: serde_json::to_value(error).unwrap_or_else(|_| Value::Object(Default::default())),", "payload: request.payload.clone(),",
     [("plenora-rest-core", "--test", "runtime_binding", ["frozen_private_runtime_admission"])]),
    ("T4-cli-leak", "crates/rest-cli/src/commands.rs", "let request = serde_json::from_slice::<ExecutionRequest>(&bytes)", 'eprintln!("{}", String::from_utf8_lossy(&bytes)); let request = serde_json::from_slice::<ExecutionRequest>(&bytes)',
     [("plenora-rest-cli", "--test", "cli", ["frozen_private_admission_stdout_and_stderr"])]),
]


def exercise(targets, env, output, label):
    results = []
    for package, kind, target, tests in targets:
        binary = executable(package, kind, target, env)
        for test in tests:
            result = subprocess.run([binary, test, "--exact", "--nocapture"], env=env, cwd=ROOT,
                                    capture_output=True, text=True, encoding="utf-8", timeout=45)
            log = result.stdout + result.stderr
            (output / f"{label}-{test.replace('::', '-')}.log").write_text(log, encoding="utf-8")
            results.append({"test":test, "passed":rust_passed(result.returncode, log),
                            "assertion_failure":result.returncode != 0 and "assertion" in log and "1 failed" in log})
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--only", help="one mutation ID, otherwise all")
    args = parser.parse_args()
    selected = [m for m in MUTATIONS if args.only is None or m[0] == args.only]
    if not selected:
        parser.error("unknown mutation ID")
    output = ROOT / "target/frozen/rust-mutations"
    output.mkdir(parents=True, exist_ok=True)
    env = environment()
    env.pop("SEMPL_CAPTURE_DIR", None)
    env.pop("SEMPL_CHECK_CAPTURE_DIR", None)
    receipts = []
    for identity, relative, before, after, targets in selected:
        print(f"{identity}: baseline", flush=True)
        path = ROOT / relative
        original = path.read_bytes()
        if original.count(before.encode()) != 1:
            raise ValueError(f"mutation anchor not unique: {identity}")
        baseline = exercise(targets, env, output, identity + "-baseline")
        if not all(test["passed"] for test in baseline):
            raise ValueError(f"baseline failed: {identity}")
        backup = output / f"{identity}.original"
        backup.write_bytes(original)
        try:
            path.write_bytes(original.replace(before.encode(), after.encode()))
            print(f"{identity}: mutant", flush=True)
            mutated = exercise(targets, env, output, identity + "-mutant")
        finally:
            path.write_bytes(original)
        if path.read_bytes() != original:
            raise RuntimeError("restoration failed")
        restored = exercise(targets, env, output, identity + "-restored")
        receipt = {"id":identity,"file":relative,"before":before,"after":after,
                   "baseline":baseline,"mutant":mutated,"restored":restored,
                   "restored_sha256":hashlib.sha256(original).hexdigest(),
                   "killed":all(test["assertion_failure"] for test in mutated) and all(test["passed"] for test in restored)}
        receipts.append(receipt)
        (output / f"{identity}.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
        print(f"{identity}: killed={receipt['killed']}", flush=True)
    return 0 if all(receipt["killed"] for receipt in receipts) else 1


if __name__ == "__main__":
    raise SystemExit(main())
