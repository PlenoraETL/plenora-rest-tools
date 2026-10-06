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
    from referencing import Registry, Resource
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
UPSTREAM_ROOT = ROOT / "contracts" / "upstream"
ADOPTION_MANIFEST = ROOT / "adoption-manifest.json"
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


# Instances exercised against the published schemas. Metaschema validity and
# digest stability say nothing about whether a schema actually accepts the
# payloads the engine produces and rejects the ones it must refuse, so every
# schema carries a small positive and negative corpus.
#
# `invalid` entries pair an instance with the substring expected in the
# validation message, which keeps a negative fixture from passing for the wrong
# reason.
METRICS = {
    "requests": 1,
    "retries": 0,
    "auth_requests": 0,
    "poll_requests": 0,
    "cache_hits": 0,
    "cache_revalidations": 0,
    "rate_limit_wait_ms": 0,
    "input_records": 0,
    "output_records": 1,
    "bytes_downloaded": 0,
    "bytes_uploaded": 0,
    "elapsed_ms": 3,
}

RECOVERY = {
    "contract": "plenora-rest-async-job-recovery-v1",
    "job_id": "export/1",
    "cancel_requested": False,
}

FILE_OUTPUT = {
    "type": "file",
    "direction": "download",
    "artifact_reference": "artifact://tenant/item",
    "bytes_transferred": 12,
    "checksum": {"algorithm": "sha256", "value": "0" * 64},
}

CAPABILITY_ATTRIBUTES = {
    "contract": "plenora-rest-capability-attributes-v1",
    "http_methods": ["GET", "POST"],
    "authentication": ["none", "bearer"],
    "response_formats": ["json"],
    "resilience": ["retry"],
    "orchestration": ["pagination"],
    "integrity": "sha256",
}

FIXTURES: dict[str, dict[str, list[Any]]] = {
    "plenora-rest-async-job-recovery-v1.schema.json": {
        "valid": [RECOVERY, {**RECOVERY, "cancel_accepted": True}],
        "invalid": [
            (
                {"contract": "other", "job_id": "a", "cancel_requested": False},
                "was expected",
            ),
            ({"contract": RECOVERY["contract"], "job_id": "a"}, "cancel_requested"),
        ],
    },
    "plenora-rest-capability-attributes-v1.schema.json": {
        "valid": [
            CAPABILITY_ATTRIBUTES,
            {
                **CAPABILITY_ATTRIBUTES,
                "direction": "upload",
                "transfer": ["streaming", "runtime_artifact_reference"],
            },
        ],
        "invalid": [
            ({**CAPABILITY_ATTRIBUTES, "http_methods": []}, "non-empty"),
            # `direction` and `transfer` are only meaningful together.
            ({**CAPABILITY_ATTRIBUTES, "direction": "upload"}, "transfer"),
        ],
    },
    "plenora-rest-execution-request-v1.schema.json": {
        "valid": [
            {
                "schema_version": 1,
                "operation": "test",
                "connection": {"url": "https://api.example.com/items"},
            },
            {
                "schema_version": 1,
                "operation": "enrich",
                "connection": {
                    "url": "https://api.example.com/items/{id}",
                    "method": "GET",
                    "headers": {"Accept": "application/json"},
                    "parameters": [
                        {
                            "name": "id",
                            "mode": "mapped",
                            "source": "identifier",
                            "location": "path",
                            "required": True,
                        }
                    ],
                    "success_statuses": [200, 204],
                    "requests_per_second": 5,
                },
                "input": {"params": {}, "records": [{"identifier": "a"}]},
                "options": {
                    "capture_response_metadata": True,
                    "response_headers": ["etag"],
                    "enrichment_concurrency": 4,
                    "idempotency_key": "abc-123",
                },
            },
        ],
        "invalid": [
            (
                {"operation": "test", "connection": {"url": "https://a.test/"}},
                "schema_version",
            ),
            (
                {
                    "schema_version": 1,
                    "operation": "delete",
                    "connection": {"url": "https://a.test/"},
                },
                "delete",
            ),
            ({"schema_version": 1, "operation": "test", "connection": {}}, "url"),
            (
                {
                    "schema_version": 1,
                    "operation": "test",
                    "connection": {"url": "https://a.test/", "unexpected": True},
                },
                "unexpected",
            ),
            (
                {
                    "schema_version": 1,
                    "operation": "test",
                    "connection": {"url": "https://a.test/"},
                    "options": {"enrichment_concurrency": 0},
                },
                "minimum",
            ),
        ],
    },
    "plenora-rest-execution-result-v1.schema.json": {
        "valid": [
            {
                "schema_version": 1,
                "status": "success",
                "output": {"type": "json", "value": {"ok": True}},
                "metrics": METRICS,
                "responses": [
                    {
                        "status": 200,
                        "final_url": "https://api.example.com",
                        "attempts": 1,
                        "headers": {"etag": "v1"},
                    }
                ],
                "errors": [],
                "recoveries": [RECOVERY],
            }
        ],
        "invalid": [
            # A cache hit used to report `attempts: 0`, which the contract forbids.
            (
                {
                    "schema_version": 1,
                    "status": "success",
                    "output": {"type": "none"},
                    "metrics": METRICS,
                    "responses": [
                        {
                            "status": 200,
                            "final_url": "https://api.example.com",
                            "attempts": 0,
                            "headers": {},
                        }
                    ],
                    "errors": [],
                },
                "minimum",
            ),
            # More than 128 recovery handles must never reach a public result.
            (
                {
                    "schema_version": 1,
                    "status": "failed",
                    "output": {"type": "none"},
                    "metrics": METRICS,
                    "responses": [],
                    "errors": [],
                    "recoveries": [
                        {**RECOVERY, "job_id": f"job-{index}"} for index in range(129)
                    ],
                },
                "128",
            ),
            (
                {
                    "schema_version": 1,
                    "status": "success",
                    "output": {"type": "none"},
                    "metrics": METRICS,
                    "responses": [],
                    "errors": [{"category": "io"}],
                },
                "phase",
            ),
        ],
    },
    "plenora-rest-file-transfer-input-v1.schema.json": {
        "valid": [
            {
                "schema_version": 1,
                "operation": "download",
                "connection": {"url": "https://api.example.com/artifact"},
                "input": {
                    "file": {"artifact_sink": {"reference": "artifact://tenant/item"}}
                },
            },
            {
                "schema_version": 1,
                "operation": "upload",
                "connection": {
                    "url": "https://api.example.com/artifact",
                    "method": "PUT",
                },
                "input": {"file": {"path": "payload.bin", "expected_sha256": "0" * 64}},
            },
        ],
        "invalid": [
            (
                {
                    "schema_version": 1,
                    "operation": "test",
                    "connection": {"url": "https://a.test/"},
                    "input": {"file": {"path": "payload.bin"}},
                },
                "test",
            ),
            # A download must name exactly one destination.
            (
                {
                    "schema_version": 1,
                    "operation": "download",
                    "connection": {"url": "https://a.test/"},
                    "input": {
                        "file": {
                            "path": "payload.bin",
                            "artifact_sink": {"reference": "artifact://tenant/item"},
                        }
                    },
                },
                "is not valid",
            ),
            (
                {
                    "schema_version": 1,
                    "operation": "upload",
                    "connection": {"url": "https://a.test/"},
                    "input": {
                        "file": {"path": "payload.bin", "expected_sha256": "nope"}
                    },
                },
                "does not match",
            ),
        ],
    },
    "plenora-rest-file-transfer-result-v1.schema.json": {
        "valid": [
            {
                "schema_version": 1,
                "status": "success",
                "output": FILE_OUTPUT,
                "metrics": METRICS,
                "responses": [],
                "errors": [],
            },
            {
                "schema_version": 1,
                "status": "failed",
                "output": {"type": "none"},
                "metrics": METRICS,
                "responses": [],
                "errors": [],
                "recoveries": [RECOVERY],
            },
        ],
        "invalid": [
            (
                {
                    "schema_version": 1,
                    "status": "success",
                    "output": {**FILE_OUTPUT, "direction": "sideways"},
                    "metrics": METRICS,
                    "responses": [],
                    "errors": [],
                },
                "sideways",
            ),
            # A successful transfer must carry a file output, not `none`.
            (
                {
                    "schema_version": 1,
                    "status": "success",
                    "output": {"type": "none"},
                    "metrics": METRICS,
                    "responses": [],
                    "errors": [],
                },
                "required",
            ),
        ],
    },
}


def schema_registry(documents: dict[Path, Any]) -> Registry:
    """Registry of the component-owned schemas, addressable by absolute `$id`."""
    resources = []
    for document in documents.values():
        identifier = document.get("$id")
        if isinstance(identifier, str) and identifier:
            resources.append((identifier, Resource.from_contents(document)))
    return Registry().with_resources(resources)


def validate_fixtures(documents: dict[Path, Any]) -> int:
    """Runs every fixture through its schema; returns how many were checked."""
    available = {path.name for path in schema_files()}
    covered = set(FIXTURES)
    if covered != available:
        missing = sorted(available - covered)
        unknown = sorted(covered - available)
        fail(
            "every published schema needs a fixture corpus"
            + (f"; missing: {missing}" if missing else "")
            + (f"; unknown: {unknown}" if unknown else "")
        )

    registry = schema_registry(documents)
    checked = 0
    for name, corpus in sorted(FIXTURES.items()):
        document = documents[(SCHEMA_ROOT / name).resolve()]
        validator = Draft202012Validator(document, registry=registry)

        for instance in corpus["valid"]:
            errors = sorted(validator.iter_errors(instance), key=str)
            if errors:
                fail(
                    f"{name} rejects a payload the engine produces at "
                    f"{errors[0].json_path}: {errors[0].message}"
                )
            checked += 1

        for instance, expected in corpus["invalid"]:
            messages = [error.message for error in validator.iter_errors(instance)]
            if not messages:
                fail(f"{name} accepts a payload the engine must refuse: {instance!r}")
            if not any(expected in message for message in messages):
                fail(
                    f"{name} rejected a negative fixture for the wrong reason; "
                    f"expected {expected!r} in {messages}"
                )
            checked += 1
    return checked


def validate_upstream() -> int:
    """Pins of the files copied from plenora-contracts, and what they check.

    `contracts/upstream/source.json` names the adopted revision and the SHA-256
    of every copied file. The copy must match its pin byte for byte, must come
    from the revision the adoption manifest declares, and nothing may sit in
    the directory without a pin. The runtime vectors are then validated
    against the copied runtime-vector schema (error payloads also against the
    common error schema), and the adoption manifest against the copied
    manifest v4 schema plus the cross-reference rules of ADOPTION.md.
    Returns the number of documents validated.
    """
    source = load_json(UPSTREAM_ROOT / "source.json")
    manifest = load_json(ADOPTION_MANIFEST)
    adopted = manifest.get("contracts_source", {})
    if source.get("revision") != adopted.get("revision"):
        fail(
            "contracts/upstream/source.json and adoption-manifest.json adopt "
            "different plenora-contracts revisions"
        )
    if source.get("repository") != adopted.get("repository"):
        fail("contracts/upstream/source.json names a different contracts repository")
    pins = source.get("files")
    if not isinstance(pins, dict) or not pins:
        fail("contracts/upstream/source.json must pin at least one file")
    present = sorted(
        path.relative_to(UPSTREAM_ROOT).as_posix()
        for path in UPSTREAM_ROOT.rglob("*")
        if path.is_file() and path.name != "source.json"
    )
    if present != sorted(pins):
        fail(
            "contracts/upstream differs from its pin list: "
            f"unpinned={sorted(set(present) - set(pins))}, "
            f"missing={sorted(set(pins) - set(present))}"
        )
    for name, pin in sorted(pins.items()):
        digest = hashlib.sha256((UPSTREAM_ROOT / name).read_bytes()).hexdigest()
        if not isinstance(pin, dict) or digest != pin.get("sha256"):
            fail(f"contracts/upstream/{name} differs from its pinned upstream copy")

    names = (
        "runtime-vector-v1.schema.json",
        "error-v1.schema.json",
        "adoption-manifest-v4.schema.json",
    )
    schemas = {name: load_json(UPSTREAM_ROOT / "schemas" / name) for name in names}
    for name, schema in schemas.items():
        try:
            Draft202012Validator.check_schema(schema)
        except SchemaError as error:
            fail(
                f"contracts/upstream/schemas/{name} is not valid Draft 2020-12: "
                f"{error.message}"
            )
    registry = schema_registry(
        {
            (UPSTREAM_ROOT / "schemas" / name).resolve(): schema
            for name, schema in schemas.items()
        }
    )

    def check(schema_name: str, instance: Any, label: str) -> None:
        validator = Draft202012Validator(schemas[schema_name], registry=registry)
        errors = sorted(validator.iter_errors(instance), key=str)
        if errors:
            fail(
                f"{label} does not satisfy {schema_name} at "
                f"{errors[0].json_path}: {errors[0].message}"
            )

    checked = 0
    for name in sorted(name for name in pins if name.startswith("runtime-v1/")):
        vector = load_json(UPSTREAM_ROOT / name)
        check("runtime-vector-v1.schema.json", vector, f"contracts/upstream/{name}")
        if vector.get("kind") == "error":
            check(
                "error-v1.schema.json",
                vector.get("payload"),
                f"contracts/upstream/{name} payload",
            )
        checked += 1

    check("adoption-manifest-v4.schema.json", manifest, "adoption-manifest.json")
    for error in adoption_cross_reference_errors(manifest):
        fail(f"adoption-manifest.json: {error}")
    return checked + 1


def adoption_cross_reference_errors(document: dict[str, Any]) -> list[str]:
    """ADOPTION.md rules beyond the manifest v4 schema.

    The same rules as `adoption_errors` in tools/conformance_checks.py of the
    adopted revision: one description per artifact name, one status per
    contract, and a deviation that names an artifact names a declared one on
    the same surface.
    """
    errors = []
    artifacts: dict[str, Any] = {}
    for artifact in document["artifacts"]:
        identity = {key: value for key, value in artifact.items() if key != "verification"}
        previous = artifacts.get(artifact["name"])
        if previous is not None and identity != {
            key: value for key, value in previous.items() if key != "verification"
        }:
            errors.append("ambiguous artifact name in adoption manifest")
        artifacts[artifact["name"]] = artifact
    statuses: dict[str, str] = {}
    for contract in document["contracts"]:
        previous_status = statuses.get(contract["id"])
        if previous_status is not None and previous_status != contract["status"]:
            errors.append("duplicate contract identity with conflicting adoption status")
        statuses[contract["id"]] = contract["status"]
    for deviation in document["deviations"]:
        name = deviation.get("artifact")
        if name is None:
            continue
        named = artifacts.get(name)
        if named is None:
            errors.append("deviation refers to an undeclared artifact")
        elif "surface" in deviation and deviation["surface"] != named["surface"]:
            errors.append("deviation surface differs from the named artifact")
    return errors


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

    checked = validate_fixtures(documents)
    upstream = validate_upstream()

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
        f"{checked} schema fixtures, {upstream} pinned upstream documents, "
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
