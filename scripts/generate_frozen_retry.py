"""Independent rational oracle for the retry matrix (no engine imports)."""
import argparse
from fractions import Fraction
import json
from pathlib import Path

PATH = Path(__file__).resolve().parents[1] / "tests/frozen/0.4.0/retry.json"


def matrix():
    cases = []
    for label, base, numerator, denominator, cap in [
        ("dyadic", 7, 3, 2, 100), ("constant", 11, 1, 1, 100),
        ("zero", 0, 2, 1, 100), ("initial-cap", 300, 2, 1, 100),
        ("wide-integer", 9007199254740993, 2, 1, 18446744073709551615),
    ]:
        for respect in (False, True):
            headers = [None, 500, None, 1001, None, 0, 1000, None]
            current = Fraction(min(base, cap))
            expected = []
            for header in headers:
                if respect and header is not None and header > 1000:
                    expected.append(None)
                    continue
                expected.append(header if respect and header is not None else current.numerator // current.denominator)
                current = min(Fraction(cap), current * Fraction(numerator, denominator))
            cases.append({"id": f"{label}-respect-{respect}", "base_ms": base,
                          "factor": numerator / denominator, "cap_ms": cap,
                          "respect": respect, "retry_cap_ms": 1000,
                          "headers_ms": headers, "waits_ms": expected})
    return {"oracle": "Fraction recurrence; factors are exact dyadic rationals; no 128-bit truncation within these eight steps",
            "cases": cases,
            "network": [
                {"id":"recover", "method":"GET", "statuses":[503,200], "header":"0", "respect":True, "max_attempts":3, "success":True},
                {"id":"over-cap", "method":"GET", "statuses":[503], "header":"1", "respect":True, "max_attempts":3, "success":False},
                {"id":"ignore-header", "method":"GET", "statuses":[503,200], "header":"1", "respect":False, "max_attempts":3, "success":True},
                {"id":"post-refused", "method":"POST", "statuses":[503], "header":"0", "respect":True, "max_attempts":3, "success":False},
                {"id":"attempt-budget", "method":"GET", "statuses":[503,503], "header":"0", "respect":True, "max_attempts":2, "success":False},
                {"id":"status-refused", "method":"GET", "statuses":[400], "header":"0", "respect":True, "max_attempts":3, "success":False}
            ],
            "http_date": {"now_unix_seconds": 1700000000,
                          "header": "Tue, 14 Nov 2023 22:13:27 GMT", "wait_ms": 7000}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    expected = json.dumps(matrix(), indent=2) + "\n"
    if args.check:
        if PATH.read_text(encoding="utf-8") != expected:
            raise SystemExit("retry oracle differs")
    else:
        PATH.write_text(expected, encoding="utf-8", newline="\n")


if __name__ == "__main__":
    main()
