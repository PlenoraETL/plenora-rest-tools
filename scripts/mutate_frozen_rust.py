"""Targeted Rust mutations: baseline, assertion failure, byte restoration, green recheck."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from frozen_rust import ROOT, environment, executable, rust_passed

CORE = "crates/rest-engine-core/src/"
INJECTED = "transport::tests::injected_transport::"
CONTROL = [("plenora-rest-core", "--lib", None, [INJECTED + "frozen_controls_visit_every_declared_branch"])]
MUTATIONS = [
    ("branches-artifact-bytes", CORE + "transport.rs", ".write_all(&chunk)", '.write_all(b"x")', CONTROL),
    ("branches-cleanup", CORE + "transport.rs", "let _ = std::fs::remove_file(&self.temporary);", "let _ = &self.temporary;", CONTROL),
    ("branches-cancel", CORE + "error.rs", 'C::Cancelled,\n                P::Cleanup,', 'C::Cancelled,\n                P::Read,', CONTROL),
    ("branches-deadline", CORE + "error.rs", 'Self::Timeout => (\n                "TIMEOUT",', 'Self::Timeout => (\n                "WRONG_TIMEOUT",', CONTROL),
    ("branches-io", CORE + "transport.rs", "EngineError::DownloadWrite(crate::error::io_detail(&error))", "EngineError::FileIo(crate::error::io_detail(&error))",
     [("plenora-rest-core", "--lib", None, [INJECTED + "frozen_io_faults_preserve_publication_and_redaction"])]),
    ("branches-wait", CORE + "transport.rs", "sleep(delay).await;\n                        continue;", "sleep(Duration::ZERO).await;\n                        continue;",
     [("plenora-rest-core", "--lib", None, [INJECTED + "frozen_internal_transport_waits_use_injected_clock"])]),
    ("branches-headers", CORE + "engine.rs", "!is_sensitive_header_name(name)\n                &&", "(!is_sensitive_header_name(name) || name == &\"authorization\")\n                &&",
     [("plenora-rest-core", "--test", "black_box", ["frozen::remote_private::frozen_remote_body_and_sensitive_header_matrix"]),
      ("plenora-rest-core", "--test", "runtime_binding", ["frozen_private_remote_runtime"])]),
    ("branches-inventory", "crates/rest-engine-core/tests/support/injection.rs", "    Prepare,\n    OAuth,", "    Prepare,\n    UndeclaredInCorpus,\n    OAuth,",
     [("plenora-rest-core", "--lib", None, [INJECTED + "frozen_io_branch_table_is_exhaustive_for_declared_ports"])]),
    ("T1-control-phase", CORE + "error.rs", 'C::Cancelled,\n                P::Cleanup,', 'C::Cancelled,\n                P::Read,',
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_controls_before_send_and_after_headers"])]),
    ("T1-effect", CORE + "error.rs", "error.remote_effect = RemoteEffect::Unknown;", "error.remote_effect = RemoteEffect::None;",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_transition_oracle", "frozen::frozen_serial_concurrent_same_corpus"])]),
    ("T1-phase", CORE + "error.rs", "error.phase = entered;", "error.phase = ErrorPhase::Validate;",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_transition_oracle"])]),
    ("T1-order", CORE + "engine/records.rs", "for outcome in ordered {", "for outcome in ordered.into_iter().rev() {",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_mixed_records_complete_out_of_order"])]),
    ("T1-batch", CORE + "engine/records.rs", "plan.records.chunks(size.get())", "plan.records.chunks(1)",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_batch_has_its_own_wire_oracle"])]),
    ("T1-record-merge", CORE + "engine/records.rs", "enriched.extend(additions);", "let _ = additions;",
     [("plenora-rest-core", "--test", "black_box", ["frozen::frozen_mixed_records_complete_out_of_order", "frozen::frozen_batch_has_its_own_wire_oracle"])]),
    ("T1-control-race", CORE + "control.rs",
     "_ = self.cancellation.cancelled() => ControlOutcome::Cancelled,\n            _ = elapsed => ControlOutcome::Deadline,",
     "_ = elapsed => ControlOutcome::Deadline,\n            _ = self.cancellation.cancelled() => ControlOutcome::Cancelled,",
     [("plenora-rest-core", "--lib", None, ["control::tests::admission_and_running_control_have_explicit_precedence"])]),
    ("T1-control-admission", CORE + "control.rs", "            Err(EngineError::DeadlineExpired)\n        } else {", "            Err(EngineError::Timeout)\n        } else {",
     [("plenora-rest-core", "--test", "black_box", ["an_expired_deadline_is_refused_before_anything_runs"])]),
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
    parser.add_argument("--prefix", help="only mutation IDs with this prefix")
    args = parser.parse_args()
    selected = [m for m in MUTATIONS if (args.only is None or m[0] == args.only) and (args.prefix is None or m[0].startswith(args.prefix))]
    if not selected:
        parser.error("unknown mutation ID")
    output = ROOT / "target/frozen/rust-mutations"
    output.mkdir(parents=True, exist_ok=True)
    env = environment()
    env.pop("SEMPL_CAPTURE_DIR", None)
    env.pop("SEMPL_CHECK_CAPTURE_DIR", None)
    env.pop("SEMPL_CONTROL_CASE", None)
    receipts = []
    for identity, relative, before, after, targets in selected:
        print(f"{identity}: baseline", flush=True)
        path = ROOT / relative
        original = path.read_bytes()
        before = before.replace("\n", "\r\n") if b"\r\n" in original else before
        after = after.replace("\n", "\r\n") if b"\r\n" in original else after
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
