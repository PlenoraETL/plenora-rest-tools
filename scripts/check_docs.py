#!/usr/bin/env python3
"""Controlla che le guide correnti restino risolvibili e semanticamente sane.

Non valida lo stile della prosa. Protegge invece proprietà oggettive: link e
anchor locali, comandi Python esistenti, esempi Python validi e nomi
SDK coerenti con gli export pubblici, senza caricare il binding nativo.
"""

from __future__ import annotations

import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
SKIP_PARTS = {".git", ".agents", ".codex", "target", "assurance-results",
              "node_modules", "__pycache__", ".venv", "venv", "build", "dist",
              ".pytest_cache", "vendor", "campaign-out"}
LINK = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
HEADING = re.compile(r"^#{1,6}\s+(.+?)\s*$", re.MULTILINE)
PYTHON_COMMAND = re.compile(
    r"\bpython(?:3)?\s+(?:\.[\\/])?((?:scripts|python)[\\/][A-Za-z0-9_.\\/-]+\.py)"
)
PYTHON_FENCE = re.compile(r"(?:```|~~~)python[^\S\n]*\n(.*?)(?:```|~~~)", re.DOTALL)


@dataclass(frozen=True)
class Violation:
    path: Path
    reason: str


def markdown_documents(root: Path = ROOT) -> list[Path]:
    return [
        path
        for path in sorted(root.rglob("*.md"))
        if not SKIP_PARTS.intersection(path.relative_to(root).parts)
        # Copie esatte: i link appartengono al checkout upstream, validato
        # dal validatore dei contratti, non alla selezione vendorizzata.
        and not path.is_relative_to(root / "contracts" / "upstream")
    ]


def github_slug(title: str) -> str:
    """Approssimazione deterministica degli anchor Markdown usati nel repo."""

    text = re.sub(r"<[^>]+>", "", title.strip().lower())
    text = re.sub(r"[^\w\s-]", "", text, flags=re.UNICODE)
    return re.sub(r"[\s-]+", "-", text).strip("-")


def anchors(path: Path) -> set[str]:
    counts: dict[str, int] = {}
    found: set[str] = set()
    for heading in HEADING.findall(path.read_text(encoding="utf-8")):
        base = github_slug(heading)
        occurrence = counts.get(base, 0)
        counts[base] = occurrence + 1
        found.add(base if occurrence == 0 else f"{base}-{occurrence}")
    return found


def validate_links(root: Path, documents: list[Path]) -> list[Violation]:
    violations: list[Violation] = []
    for path in documents:
        text = path.read_text(encoding="utf-8")
        for raw in LINK.findall(text):
            target = raw.strip().strip("<>")
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            file_part, separator, anchor = target.partition("#")
            destination = path if not file_part else path.parent / unquote(file_part)
            if not destination.exists():
                violations.append(Violation(path, f"link locale inesistente: {raw}"))
                continue
            if separator and anchor and destination.suffix.lower() == ".md":
                if unquote(anchor).lower() not in anchors(destination):
                    violations.append(Violation(path, f"anchor inesistente: {raw}"))
    return violations


def validate_commands(root: Path, documents: list[Path]) -> list[Violation]:
    violations: list[Violation] = []
    for path in documents:
        text = path.read_text(encoding="utf-8")
        for command_path in PYTHON_COMMAND.findall(text):
            normalized = command_path.replace("\\", "/")
            if not (root / normalized).is_file():
                violations.append(
                    Violation(path, f"comando Python punta a un file assente: {command_path}")
                )
    return violations


def validate_python_examples(documents: list[Path]) -> list[Violation]:
    """Ogni esempio Python deve essere almeno sintatticamente eseguibile."""

    violations: list[Violation] = []
    for path in documents:
        text = path.read_text(encoding="utf-8")
        for position, source in enumerate(PYTHON_FENCE.findall(text), start=1):
            try:
                compile(
                    source,
                    f"{path}#python-{position}",
                    "exec",
                    flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT,
                )
            except SyntaxError as exc:
                violations.append(
                    Violation(
                        path,
                        f"esempio Python {position} non valido: riga {exc.lineno}",
                    )
                )
    return violations


def validate_sdk_example_api(root: Path = ROOT) -> list[Violation]:
    """Confronta i nomi SDK negli esempi con gli export, senza caricare Rust."""
    package = root / "python/plenora_rest"
    tree = ast.parse((package / "__init__.py").read_text(encoding="utf-8"))
    exports = next(
        set(ast.literal_eval(node.value))
        for node in tree.body
        if isinstance(node, ast.Assign)
        and any(isinstance(target, ast.Name) and target.id == "__all__" for target in node.targets)
    )
    sources = [
        (path, source)
        for path in markdown_documents(root)
        if "vendor" not in path.relative_to(root).parts
        for source in PYTHON_FENCE.findall(path.read_text(encoding="utf-8"))
    ]
    sources += [
        (path, path.read_text(encoding="utf-8"))
        for path in sorted((root / "examples").glob("*.py"))
    ]
    violations: list[Violation] = []
    for path, source in sources:
        try:
            example = ast.parse(source)
        except SyntaxError:
            continue  # La validazione sintattica produce il proprio errore.
        aliases = {"plenora_rest"}
        for node in ast.walk(example):
            if isinstance(node, ast.Import):
                aliases.update(
                    item.asname or item.name
                    for item in node.names if item.name == "plenora_rest"
                )
            elif isinstance(node, ast.ImportFrom) and node.module == "plenora_rest":
                for item in node.names:
                    if item.name != "*" and item.name not in exports:
                        violations.append(Violation(path, f"import SDK non pubblico: {item.name}"))
        for node in ast.walk(example):
            if (
                isinstance(node, ast.Attribute)
                and isinstance(node.value, ast.Name)
                and node.value.id in aliases
                and node.attr not in exports
            ):
                violations.append(Violation(path, f"nome SDK non pubblico: {node.attr}"))
    return violations


def scan(root: Path = ROOT) -> tuple[int, list[Violation]]:
    documents = markdown_documents(root)
    violations: list[Violation] = []
    for path in documents:
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            violations.append(Violation(path, f"documento illeggibile: {exc}"))
            continue
        if text.count("```") % 2 or text.count("~~~") % 2:
            violations.append(Violation(path, "code fence non bilanciato"))
    violations += validate_links(root, documents)
    violations += validate_commands(root, documents)
    violations += validate_python_examples(documents)
    violations += validate_sdk_example_api(root)
    return len(documents), violations


def main() -> int:
    checked, violations = scan()
    if violations:
        for violation in violations:
            try:
                relative = violation.path.relative_to(ROOT)
            except ValueError:
                relative = violation.path
            print(f"{relative}: docs: {violation.reason}")
        return 1
    print(f"docs: {checked} documenti controllati, nessuna violazione")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
