#!/usr/bin/env python3
"""Verifica fail-closed dei budget di coverage Rust e Python.

Il report e il budget sono input non fidati: una chiave assente, un numero non
finito o una percentuale incoerente non devono trasformarsi in un verde. Il
gate misura separatamente prodotto Rust, binding nativo e SDK Python, per
evitare che una superficie grande nasconda la regressione dell'altra.

Ogni superficie dichiara nel budget i sorgenti che misura (`sources`): un
report senza file, con un file fuori da quei sorgenti o senza file di uno di
essi non sostiene un verdetto, perché misurerebbe un'altra cosa. Per i report
llvm i sorgenti sono cartelle relative alla radice del repository (`--root`) e
un file fuori dalla radice è fuori; per coverage.py, che misura il pacchetto
installato, sono la cartella del pacchetto, come segmento del percorso.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_BUDGET = ROOT / "scripts" / "coverage_budget.json"
LLVM_METRICS = ("functions", "lines", "regions")
PYTHON_METRICS = ("lines", "branches")
REPORT_METRICS = {
    "llvm": LLVM_METRICS,
    "coverage.py": PYTHON_METRICS,
}
# Alias mantenuto per i consumatori del checker esistente.
METRICS = LLVM_METRICS


class CoverageError(ValueError):
    """Il report o il budget non possono sostenere un verdetto."""


def _object(value: Any, where: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise CoverageError(f"{where}: atteso un oggetto JSON")
    return value


def _number(value: Any, where: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise CoverageError(f"{where}: atteso un numero")
    number = float(value)
    if not math.isfinite(number):
        raise CoverageError(f"{where}: il numero deve essere finito")
    return number


def load_json(path: Path, kind: str) -> dict[str, Any]:
    try:
        with path.open(encoding="utf-8") as handle:
            return _object(json.load(handle), kind)
    except (OSError, json.JSONDecodeError) as error:
        raise CoverageError(f"{kind}: impossibile leggere {path}: {error}") from error


def llvm_files(report: dict[str, Any]) -> list[str]:
    data = report.get("data")
    if not isinstance(data, list) or len(data) != 1:
        raise CoverageError("report: atteso esattamente un blocco data")
    files = _object(data[0], "report.data[0]").get("files")
    if not isinstance(files, list) or not files:
        raise CoverageError("report: nessun file misurato")
    names = []
    for index, item in enumerate(files):
        name = _object(item, f"report.files[{index}]").get("filename")
        if not isinstance(name, str) or not name:
            raise CoverageError(f"report.files[{index}].filename: atteso un percorso")
        names.append(name)
    return names


def python_files(report: dict[str, Any]) -> list[str]:
    files = report.get("files")
    if not isinstance(files, dict) or not files:
        raise CoverageError("report: nessun file Python misurato")
    return list(files)


def _slash(path: str) -> str:
    return path.replace("\\", "/")


def check_sources(files: list[str], sources: list[str], root: str | None) -> None:
    """Ogni file sta in un sorgente dichiarato, ogni sorgente ha almeno un file.

    Con `root` il percorso deve stare sotto la radice e cominciare, relativo a
    lei, con il sorgente; senza, il sorgente è un segmento del percorso.
    """
    prefix = _slash(root).rstrip("/") + "/" if root is not None else None
    matched: set[str] = set()
    for name in files:
        normalized = _slash(name)
        # Un segmento `.` o `..` potrebbe uscire dal sorgente che il prefisso
        # sembra indicare: un percorso cosi' non sostiene un verdetto.
        if any(segment in {".", ".."} for segment in normalized.split("/")):
            raise CoverageError("report: un file misurato ha un segmento . o ..")
        if prefix is not None:
            if not normalized.startswith(prefix):
                raise CoverageError("report: un file misurato è fuori dalla radice")
            relative = normalized[len(prefix):]
            owners = [source for source in sources if relative.startswith(source)]
        else:
            segmented = "/" + normalized.lstrip("/")
            owners = [source for source in sources if "/" + source in segmented]
        if not owners:
            raise CoverageError(
                "report: un file misurato è fuori dai sorgenti della superficie"
            )
        matched.update(owners)
    missing = [source for source in sources if source not in matched]
    if missing:
        raise CoverageError(f"report: nessun file misurato sotto {', '.join(missing)}")


def read_totals(report: dict[str, Any]) -> dict[str, tuple[int, int, float]]:
    if report.get("type") != "llvm.coverage.json.export":
        raise CoverageError("report: tipo llvm-cov assente o sconosciuto")
    data = report.get("data")
    if not isinstance(data, list) or len(data) != 1:
        raise CoverageError("report: atteso esattamente un blocco data")
    totals = _object(_object(data[0], "report.data[0]").get("totals"), "report.totals")

    result: dict[str, tuple[int, int, float]] = {}
    for metric in LLVM_METRICS:
        item = _object(totals.get(metric), f"report.totals.{metric}")
        count_number = _number(item.get("count"), f"report.{metric}.count")
        covered_number = _number(item.get("covered"), f"report.{metric}.covered")
        percent = _number(item.get("percent"), f"report.{metric}.percent")
        if not count_number.is_integer() or count_number <= 0:
            raise CoverageError(f"report.{metric}.count: atteso un intero positivo")
        if not covered_number.is_integer() or not 0 <= covered_number <= count_number:
            raise CoverageError(f"report.{metric}.covered: conteggio non valido")
        if not 0 <= percent <= 100:
            raise CoverageError(f"report.{metric}.percent: fuori dall'intervallo 0..100")
        expected = 100.0 * covered_number / count_number
        if not math.isclose(percent, expected, rel_tol=0.0, abs_tol=1e-6):
            raise CoverageError(f"report.{metric}.percent: incoerente con i conteggi")
        result[metric] = (int(covered_number), int(count_number), percent)
    return result


def read_python_totals(report: dict[str, Any]) -> dict[str, tuple[int, int, float]]:
    meta = _object(report.get("meta"), "report.meta")
    if meta.get("format") != 3:
        raise CoverageError("report: formato coverage.py assente o sconosciuto")
    if meta.get("branch_coverage") is not True:
        raise CoverageError("report: branch coverage Python non abilitata")
    python_files(report)
    totals = _object(report.get("totals"), "report.totals")
    fields = {
        "lines": ("covered_lines", "num_statements"),
        "branches": ("covered_branches", "num_branches"),
    }
    result: dict[str, tuple[int, int, float]] = {}
    for metric, (covered_key, count_key) in fields.items():
        count_number = _number(totals.get(count_key), f"report.{metric}.{count_key}")
        covered_number = _number(
            totals.get(covered_key), f"report.{metric}.{covered_key}"
        )
        if not count_number.is_integer() or count_number <= 0:
            raise CoverageError(f"report.{metric}.{count_key}: atteso un intero positivo")
        if not covered_number.is_integer() or not 0 <= covered_number <= count_number:
            raise CoverageError(f"report.{metric}.{covered_key}: conteggio non valido")
        result[metric] = (
            int(covered_number),
            int(count_number),
            100.0 * covered_number / count_number,
        )
    return result


def read_budget(
    budget: dict[str, Any], surface: str
) -> tuple[str, dict[str, float], list[str]]:
    if budget.get("schema_version") != 1:
        raise CoverageError("budget: schema_version deve essere 1")
    surfaces = _object(budget.get("surfaces"), "budget.surfaces")
    selected = _object(surfaces.get(surface), f"budget.surfaces.{surface}")
    report_format = selected.get("report_format")
    if not isinstance(report_format, str) or report_format not in REPORT_METRICS:
        raise CoverageError(f"budget.{surface}: report_format assente o sconosciuto")
    metrics = REPORT_METRICS[report_format]
    minimum = _object(selected.get("minimum_percent"), f"budget.{surface}.minimum_percent")
    if set(minimum) != set(metrics):
        raise CoverageError(
            f"budget.{surface}: servono esattamente {', '.join(metrics)}"
        )
    result: dict[str, float] = {}
    for metric in metrics:
        value = _number(minimum[metric], f"budget.{surface}.{metric}")
        if not 0 <= value <= 100:
            raise CoverageError(f"budget.{surface}.{metric}: fuori da 0..100")
        result[metric] = value
    sources = selected.get("sources")
    if (
        not isinstance(sources, list)
        or not sources
        or not all(
            isinstance(source, str) and len(source) > 1 and source.endswith("/")
            for source in sources
        )
        or len(set(sources)) != len(sources)
    ):
        raise CoverageError(
            f"budget.{surface}.sources: attesa una lista di cartelle che finiscono con /"
        )
    return report_format, result, sources


def check(summary: Path, budget_path: Path, surface: str, root: Path = ROOT) -> bool:
    report_format, minimum, sources = read_budget(
        load_json(budget_path, "budget"), surface
    )
    report = load_json(summary, "report")
    if report_format == "llvm":
        totals = read_totals(report)
        check_sources(llvm_files(report), sources, str(root))
    else:
        totals = read_python_totals(report)
        check_sources(python_files(report), sources, None)
    failures: list[str] = []
    print(f"coverage: {surface}")
    for metric in REPORT_METRICS[report_format]:
        covered, count, actual = totals[metric]
        threshold = minimum[metric]
        status = "OK" if actual >= threshold else "FAIL"
        print(
            f"  {status:4} {metric:9} {actual:6.2f}% "
            f"({covered}/{count}), minimo {threshold:.2f}%"
        )
        if actual < threshold:
            failures.append(metric)
    if failures:
        print(f"coverage sotto budget: {', '.join(failures)}", file=sys.stderr)
        return False
    return True


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--summary", required=True, type=Path)
    parser.add_argument("--surface", required=True)
    parser.add_argument("--budget", type=Path, default=DEFAULT_BUDGET)
    parser.add_argument("--root", type=Path, default=ROOT)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        return 0 if check(args.summary, args.budget, args.surface, args.root) else 1
    except CoverageError as error:
        print(f"coverage non verificabile: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
