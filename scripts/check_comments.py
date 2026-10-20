#!/usr/bin/env python3
"""Controlla le regole oggettive dei commenti nel repository.

Il controllo non misura quanto un commento sia elegante. Presidia soltanto le
regole che possono essere applicate senza interpretazione: niente debito
anonimo e niente cronaca del processo di sviluppo. Motivazioni, invarianti,
limiti e compatibilita correnti restano invece contenuto utile.
"""

from __future__ import annotations

import ast
import io
import re
import tokenize
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Iterator

ROOT = Path(__file__).resolve().parents[1]

SKIP_PARTS = {
    ".agents",
    ".codex",
    ".git",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".venv",
    "__pycache__",
    "campaign-out",
    "htmlcov",
    "node_modules",
    "assurance-results",
    "benchmarks",
    "build",
    "contracts",
    "dist",
    "docs",
    "target",
    "vendor",
    "venv",
}

PYTHON_SUFFIXES = {".py", ".pyi"}
C_LIKE_SUFFIXES = {".rs"}
HASH_SUFFIXES = {".ps1", ".sh", ".toml", ".yaml", ".yml"}
DASH_SUFFIXES: set[str] = set()

DEBT_MARKER = re.compile(r"\b(?:TODO|FIXME|HACK|XXX)\b")
HISTORY_MARKERS: tuple[re.Pattern[str], ...] = tuple(
    re.compile(pattern, re.IGNORECASE)
    for pattern in (
        r"\bprima (?:stesura|versione|implementazione|esecuzione|campagna)\b",
        r"\b(?:versione|forma|stesura|commento|contratto) precedente\b",
        r"\bvecchio contratto\b",
        r"\bprecedente al fix\b",
        r"\bsweep ha\b",
        r"\bqui c[’']era\b",
        r"\bera rimast",
        r"\bprima era\b",
        r"\berano inline\b",
        r"\bda allora\b",
        r"\bentrata per ultima\b",
        r"\bnello stesso commit\b",
        r"\ball[’']epoca\b(?!\s+Unix\b)",
        r"\bfino a poco fa\b",
        r"\bera stat[oa] (?:aggiunt[oa]|rimoss[oa])\b",
        r"\bprima mancava\b",
        r"\bpoi mancava\b",
        r"\bprima duplicava\b",
        r"\bprima diceva\b",
        r"\b(?:commento|contratto|documentazione|messaggio|riga di aiuto) diceva\b",
        r"\bcosa dicevano\b",
        r"\baveva (?:concluso|provato)\b",
        r"\bfino alla separazione\b",
        r"\bnon e piu uno scaffold\b",
        r"\bdiceva il contrario\b",
        r"\bquesta guardia diceva\b",
        r"\bquando e stata scritta\b",
        r"\bcio che e cambiato\b",
        r"\broadmap\b",
        r"\bmilestone\b",
        r"\bf\d+(?:[.-]\d+)+(?:[a-z])?\b",
        r"\bf\d+[a-z]\b",
        r"\bp\d+\.\d+\b",
        r"(?:^|=+\s*)[ab]\d+(?:[a-z+.-]\w*)?\s*:",
        r"\bopz\s+\d+\b",
        r"\bfase\s+a\d+\b",
        r"\badapter temporaneo\b",
        r"\bplaceholder for future\b",
        r"\bpost-review\b",
        r"\bpre-fix\b",
        r"\bfix review\b",
        r"\btranche\b",
        r"\bADR[- ]+\d+\b",
        r"\bprima di questo fix\b",
    )
)


@dataclass(frozen=True)
class Comment:
    line: int
    text: str


@dataclass(frozen=True)
class Violation:
    path: Path
    line: int
    rule: str
    excerpt: str


def _python_comments(source: str) -> Iterator[Comment]:
    """Estrae commenti e docstring senza confonderli con stringhe ordinarie."""

    try:
        tokens = tokenize.generate_tokens(io.StringIO(source).readline)
        for token in tokens:
            if token.type == tokenize.COMMENT:
                yield Comment(token.start[0], token.string.removeprefix("#").strip())
    except (IndentationError, tokenize.TokenError):
        return

    try:
        tree = ast.parse(source)
    except SyntaxError:
        return
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            continue
        if not node.body:
            continue
        first = node.body[0]
        if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant):
            if isinstance(first.value.value, str):
                yield Comment(first.lineno, first.value.value)


def _c_like_comments(source: str) -> Iterator[Comment]:
    """Estrae commenti lineari e a blocco, ignorando stringhe e raw string Rust."""

    index = 0
    line = 1
    length = len(source)
    while index < length:
        raw = re.match(r"(?:br|rb|r)(?P<hashes>#{0,255})\"", source[index:])
        if raw:
            delimiter = '"' + raw.group("hashes")
            start = index + raw.end()
            end = source.find(delimiter, start)
            if end < 0:
                return
            segment = source[index : end + len(delimiter)]
            line += segment.count("\n")
            index = end + len(delimiter)
            continue
        if source[index] == '"':
            index += 1
            while index < length:
                if source[index] == "\\":
                    index += 2
                    continue
                if source[index] == '"':
                    index += 1
                    break
                if source[index] == "\n":
                    line += 1
                index += 1
            continue
        if source.startswith("//", index):
            start_line = line
            end = source.find("\n", index + 2)
            if end < 0:
                end = length
            yield Comment(start_line, source[index + 2 : end].strip())
            index = end
            continue
        if source.startswith("/*", index):
            start_line = line
            start = index + 2
            index = start
            depth = 1
            while index < length and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    if source[index] == "\n":
                        line += 1
                    index += 1
            yield Comment(start_line, source[start : max(start, index - 2)].strip())
            continue
        if source[index] == "\n":
            line += 1
        index += 1


def _line_comments(source: str, marker: str) -> Iterator[Comment]:
    """Estrae commenti da formati nei quali il marcatore vale fino a EOL."""

    for line_number, line in enumerate(source.splitlines(), 1):
        quote: str | None = None
        escaped = False
        index = 0
        while index <= len(line) - len(marker):
            char = line[index]
            if escaped:
                escaped = False
                index += 1
                continue
            if char == "\\" and quote:
                escaped = True
                index += 1
                continue
            if char in {'"', "'"}:
                quote = None if quote == char else char if quote is None else quote
                index += 1
                continue
            if quote is None and line.startswith(marker, index):
                yield Comment(line_number, line[index + len(marker) :].strip())
                break
            index += 1


def comments_for(path: Path, source: str) -> Iterable[Comment]:
    """Seleziona l'estrattore in base al formato del file."""

    suffix = path.suffix.lower()
    if suffix in PYTHON_SUFFIXES:
        return _python_comments(source)
    if suffix in C_LIKE_SUFFIXES:
        return _c_like_comments(source)
    if suffix in HASH_SUFFIXES or path.name.startswith("Dockerfile"):
        return _line_comments(source, "#")
    if suffix in DASH_SUFFIXES:
        return _line_comments(source, "--")
    return ()


def source_files(root: Path = ROOT) -> Iterator[Path]:
    """Visita i formati commentabili, inclusi i file non ancora tracciati."""

    for path in root.rglob("*"):
        if not path.is_file() or any(part in SKIP_PARTS for part in path.relative_to(root).parts):
            continue
        suffix = path.suffix.lower()
        if (
            suffix in PYTHON_SUFFIXES
            or suffix in C_LIKE_SUFFIXES
            or suffix in HASH_SUFFIXES
            or suffix in DASH_SUFFIXES
            or path.name.startswith("Dockerfile")
        ):
            yield path


def check_file(path: Path, root: Path = ROOT) -> list[Violation]:
    """Restituisce tutte le violazioni trovate in un file."""

    source = path.read_text(encoding="utf-8")
    violations: list[Violation] = []
    for comment in comments_for(path, source):
        compact = " ".join(comment.text.split())
        if match := DEBT_MARKER.search(compact):
            violations.append(
                Violation(path.relative_to(root), comment.line, "debito anonimo", match.group(0))
            )
        for pattern in HISTORY_MARKERS:
            if match := pattern.search(compact):
                violations.append(
                    Violation(path.relative_to(root), comment.line, "cronaca obsoleta", match.group(0))
                )
    return violations


def check_repository(root: Path = ROOT) -> tuple[int, list[Violation]]:
    """Controlla il repository e restituisce numero di file ed errori."""

    paths = sorted(source_files(root))
    violations = [violation for path in paths for violation in check_file(path, root)]
    return len(paths), violations


def main() -> int:
    checked, violations = check_repository()
    for violation in violations:
        print(f"{violation.path}:{violation.line}: {violation.rule}: {violation.excerpt}")
    if violations:
        print(f"commenti: {len(violations)} violazioni in {checked} file controllati")
        return 1
    print(f"commenti: {checked} file controllati, nessuna violazione")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
