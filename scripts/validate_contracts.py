#!/usr/bin/env python3
"""Validate JSON Schemas and freeze the versioned public contract surface."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import re
from pathlib import Path
from typing import Any, Iterator, NoReturn
from urllib.parse import unquote, urlsplit

try:
    from jsonschema import Draft202012Validator
    from jsonschema.exceptions import SchemaError
except ImportError as error:  # pragma: no cover - exercised by the container gate
    raise SystemExit(
        "jsonschema is required; run the self-contained scripts/verify.ps1 gate"
    ) from error


ROOT = Path(__file__).resolve().parents[1]
SCHEMA_ROOT = ROOT / "contracts" / "schemas"
BASELINE_PATH = ROOT / "contracts" / "compatibility-v1.json"
RUST_SURFACE = ROOT / "crates" / "rest-engine-core" / "src" / "lib.rs"
PYTHON_SURFACE = ROOT / "python" / "plenora_rest" / "__init__.py"
RUST_BINDING = ROOT / "contracts" / "bindings" / "rust-v1.json"
POLICY = (
    "Published v1 schemas and public bindings are immutable. "
    "Breaking changes require a new contract version."
)


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot load {path.relative_to(ROOT)}: {error}")


def canonical_digest(document: Any) -> str:
    encoded = json.dumps(
        document,
        ensure_ascii=True,
        separators=(",", ":"),
        sort_keys=True,
    ).encode("ascii")
    return hashlib.sha256(encoded).hexdigest()


def schema_files() -> list[Path]:
    files = sorted(SCHEMA_ROOT.glob("*.schema.json"), key=lambda path: path.name)
    if not files:
        fail("contracts/schemas does not contain any JSON Schema")
    return files


def references(value: Any) -> Iterator[str]:
    if isinstance(value, dict):
        reference = value.get("$ref")
        if isinstance(reference, str):
            yield reference
        for nested in value.values():
            yield from references(nested)
    elif isinstance(value, list):
        for nested in value:
            yield from references(nested)


def resolve_pointer(document: Any, fragment: str, label: str) -> None:
    if not fragment:
        return
    pointer = unquote(fragment)
    if not pointer.startswith("/"):
        fail(f"{label} uses an unsupported non-pointer JSON Schema anchor: #{fragment}")
    current = document
    for raw_token in pointer[1:].split("/"):
        token = raw_token.replace("~1", "/").replace("~0", "~")
        if isinstance(current, dict) and token in current:
            current = current[token]
        elif isinstance(current, list) and token.isdigit() and int(token) < len(current):
            current = current[int(token)]
        else:
            fail(f"{label} contains an unresolved JSON pointer: #{fragment}")


def validate_references(
    source: Path,
    document: Any,
    documents: dict[Path, Any],
    documents_by_id: dict[str, tuple[Path, Any]],
) -> None:
    for reference in references(document):
        parsed = urlsplit(reference)
        label = f"{source.relative_to(ROOT)} $ref {reference!r}"
        if parsed.scheme or parsed.netloc:
            base = reference.split("#", 1)[0]
            target = documents_by_id.get(base)
            if target is None:
                fail(f"{label} does not resolve to a component-owned schema")
            _, target_document = target
        elif parsed.path:
            target_path = (source.parent / unquote(parsed.path)).resolve()
            try:
                target_path.relative_to(SCHEMA_ROOT.resolve())
            except ValueError:
                fail(f"{label} escapes contracts/schemas")
            target_document = documents.get(target_path)
            if target_document is None:
                fail(f"{label} points to a missing schema")
        else:
            target_document = document
        resolve_pointer(target_document, parsed.fragment, label)


def rust_public_exports() -> list[str]:
    source = RUST_SURFACE.read_text(encoding="utf-8")
    exports: set[str] = set()
    grouped = re.compile(r"pub\s+use\s+[A-Za-z0-9_:]+::\{(.*?)\};", re.DOTALL)
    for match in grouped.finditer(source):
        for item in match.group(1).split(","):
            item = item.strip()
            if not item:
                continue
            exports.add(item.split(" as ")[-1].strip())

    direct = re.compile(
        r"pub\s+use\s+[A-Za-z0-9_:]+::([A-Za-z_][A-Za-z0-9_]*)\s*;"
    )
    exports.update(match.group(1) for match in direct.finditer(source))
    if not exports:
        fail("cannot discover the public Rust exports")
    return sorted(exports)


def python_public_exports() -> list[str]:
    module = ast.parse(PYTHON_SURFACE.read_text(encoding="utf-8"), PYTHON_SURFACE.name)
    for node in module.body:
        if (
            isinstance(node, ast.Assign)
            and any(
                isinstance(target, ast.Name) and target.id == "__all__"
                for target in node.targets
            )
        ):
            value = ast.literal_eval(node.value)
            if not isinstance(value, list) or not all(
                isinstance(item, str) for item in value
            ):
                fail("python/plenora_rest/__init__.py __all__ must be a string list")
            return sorted(value)
    fail("python/plenora_rest/__init__.py does not define __all__")


def rust_binding_signature() -> dict[str, Any]:
    binding = load_json(RUST_BINDING)
    return {
        "schema_version": binding.get("schema_version"),
        "component": binding.get("component"),
        "artifact_name": binding.get("artifact", {}).get("name"),
        "capability_entrypoint": binding.get("capability_entrypoint"),
        "lifecycle_entrypoints": binding.get("lifecycle_entrypoints"),
        "operations": binding.get("operations"),
        "runtime_transport_entrypoint": binding.get("runtime_transport_entrypoint"),
    }


def build_baseline() -> dict[str, Any]:
    schemas: dict[str, dict[str, str]] = {}
    for path in schema_files():
        document = load_json(path)
        schema_id = document.get("$id")
        if not isinstance(schema_id, str) or not schema_id:
            fail(f"{path.relative_to(ROOT)} must define a non-empty $id")
        schemas[path.name] = {
            "id": schema_id,
            "canonical_sha256": canonical_digest(document),
        }
    return {
        "schema_version": 1,
        "policy": POLICY,
        "schemas": schemas,
        "rust_public_exports": rust_public_exports(),
        "python_public_exports": python_public_exports(),
        "rust_binding": rust_binding_signature(),
    }


def validate() -> None:
    paths = schema_files()
    documents = {path.resolve(): load_json(path) for path in paths}
    documents_by_id: dict[str, tuple[Path, Any]] = {}
    for path in paths:
        document = documents[path.resolve()]
        try:
            Draft202012Validator.check_schema(document)
        except SchemaError as error:
            fail(f"{path.relative_to(ROOT)} is not valid Draft 2020-12: {error.message}")
        if document.get("$schema") != "https://json-schema.org/draft/2020-12/schema":
            fail(f"{path.relative_to(ROOT)} must explicitly use JSON Schema Draft 2020-12")
        schema_id = document.get("$id")
        if not isinstance(schema_id, str) or not schema_id:
            fail(f"{path.relative_to(ROOT)} must define a non-empty $id")
        if schema_id in documents_by_id:
            fail(f"duplicate JSON Schema $id: {schema_id}")
        documents_by_id[schema_id] = (path, document)

    for path in paths:
        validate_references(
            path,
            documents[path.resolve()],
            documents,
            documents_by_id,
        )

    expected = load_json(BASELINE_PATH)
    actual = build_baseline()
    if actual != expected:
        expected_schemas = expected.get("schemas", {})
        actual_schemas = actual["schemas"]
        changed = sorted(
            name
            for name in set(expected_schemas) | set(actual_schemas)
            if expected_schemas.get(name) != actual_schemas.get(name)
        )
        surfaces = [
            name
            for name in ("rust_public_exports", "python_public_exports", "rust_binding")
            if expected.get(name) != actual.get(name)
        ]
        details = []
        if changed:
            details.append(f"schemas={changed}")
        if surfaces:
            details.append(f"surfaces={surfaces}")
        fail(
            "published v1 compatibility baseline changed"
            + (f": {', '.join(details)}" if details else "")
            + "; introduce a versioned contract instead of mutating v1"
        )

    print(
        f"validated {len(paths)} Draft 2020-12 schemas, local references, "
        "and immutable v1 public surfaces"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--print-baseline",
        action="store_true",
        help="print the canonical compatibility baseline for explicit review",
    )
    args = parser.parse_args()
    if args.print_baseline:
        print(json.dumps(build_baseline(), indent=2, sort_keys=True))
    else:
        validate()


if __name__ == "__main__":
    main()
