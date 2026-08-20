#!/usr/bin/env python3
"""Validate and assemble deterministic plenora-rest-tools release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
from datetime import datetime, timezone
from pathlib import Path
from typing import NoReturn


ROOT = Path(__file__).resolve().parents[1]
ARTIFACT_SUFFIXES = (".crate", ".whl")
VERSION_PATTERN = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?")


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def extract(pattern: str, path: Path, label: str) -> str:
    match = re.search(pattern, path.read_text(encoding="utf-8"), re.MULTILINE)
    if match is None:
        fail(f"cannot read {label} from {path.relative_to(ROOT)}")
    return match.group(1)


def current_versions() -> dict[str, str]:
    manifest = json.loads((ROOT / "adoption-manifest.json").read_text(encoding="utf-8"))
    release_metadata = json.loads(
        (ROOT / "release-metadata.json").read_text(encoding="utf-8")
    )
    adoption_versions = {str(item["version"]) for item in manifest["artifacts"]}
    if len(adoption_versions) != 1:
        fail("adoption-manifest.json contains inconsistent artifact versions")

    return {
        "Cargo.toml": extract(
            r'^version\s*=\s*"([^"]+)"\s*$',
            ROOT / "Cargo.toml",
            "workspace version",
        ),
        "pyproject.toml": extract(
            r'^version\s*=\s*"([^"]+)"\s*$',
            ROOT / "pyproject.toml",
            "Python version",
        ),
        "contracts/bindings/rust-v1.json": str(
            json.loads(
                (ROOT / "contracts" / "bindings" / "rust-v1.json").read_text(
                    encoding="utf-8"
                )
            )["artifact"]["version"]
        ),
        "adoption-manifest.json": adoption_versions.pop(),
        "release-metadata.json": str(release_metadata["version"]),
    }


def source_date_epoch() -> int:
    metadata = json.loads((ROOT / "release-metadata.json").read_text(encoding="utf-8"))
    epoch = metadata.get("source_date_epoch")
    if not isinstance(epoch, int) or epoch <= 0:
        fail("release-metadata.json source_date_epoch must be a positive integer")
    return epoch


def normalized_version(tag: str) -> str:
    version = tag[1:] if tag.startswith("v") else tag
    if VERSION_PATTERN.fullmatch(version) is None:
        fail(f"release tag must be v<semver>, got {tag!r}")
    return version


def validate_version(expected: str) -> None:
    version = normalized_version(expected)
    versions = current_versions()
    mismatches = {path: actual for path, actual in versions.items() if actual != version}
    if mismatches:
        details = ", ".join(
            f"{path}={actual}" for path, actual in sorted(mismatches.items())
        )
        fail(f"release version {version} is inconsistent: {details}")
    print(version)


def artifact_files(directory: Path) -> list[Path]:
    if not directory.is_dir():
        fail(f"artifact directory does not exist: {directory}")
    files = sorted(
        (
            path
            for path in directory.iterdir()
            if path.is_file() and path.name.endswith(ARTIFACT_SUFFIXES)
        ),
        key=lambda path: path.name,
    )
    suffixes = {
        suffix
        for path in files
        for suffix in ARTIFACT_SUFFIXES
        if path.name.endswith(suffix)
    }
    if suffixes != set(ARTIFACT_SUFFIXES) or len(files) != 2:
        fail(
            f"expected exactly one .crate and one .whl in {directory}, "
            f"found {[path.name for path in files]}"
        )
    return files


def compare(first: Path, second: Path, copy_to: Path | None) -> None:
    first_files = {path.name: path for path in artifact_files(first)}
    second_files = {path.name: path for path in artifact_files(second)}
    if first_files.keys() != second_files.keys():
        fail(
            "reproducibility failure: artifact names differ: "
            f"{sorted(first_files)} != {sorted(second_files)}"
        )

    for name, first_path in first_files.items():
        first_digest = sha256(first_path)
        second_digest = sha256(second_files[name])
        if first_digest != second_digest:
            fail(
                f"reproducibility failure for {name}: "
                f"sha256:{first_digest} != sha256:{second_digest}"
            )
        print(f"sha256:{first_digest}  {name}")

    if copy_to is not None:
        copy_to.mkdir(parents=True, exist_ok=True)
        for name, source in first_files.items():
            shutil.copyfile(source, copy_to / name)


def checksum_files(directory: Path, output: Path) -> None:
    files = sorted(
        (
            path
            for path in directory.iterdir()
            if path.is_file() and path.resolve() != output.resolve()
        ),
        key=lambda path: path.name,
    )
    if not files:
        fail(f"no release files found in {directory}")
    output.write_text(
        "".join(f"{sha256(path)}  {path.name}\n" for path in files),
        encoding="ascii",
        newline="\n",
    )


def normalize_sbom(path: Path, expected_version: str) -> None:
    version = normalized_version(expected_version)
    document = json.loads(path.read_text(encoding="utf-8"))
    if document.get("spdxVersion") != "SPDX-2.3":
        fail(f"expected an SPDX 2.3 document in {path}")
    creation_info = document.get("creationInfo")
    if not isinstance(creation_info, dict):
        fail(f"missing SPDX creationInfo in {path}")

    creation_info["created"] = datetime.fromtimestamp(
        source_date_epoch(), tz=timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")
    document["documentNamespace"] = (
        "https://github.com/PlenoraETL/plenora-rest-tools/"
        f"releases/download/v{version}/plenora-rest-tools-{version}.spdx.json"
    )
    path.write_text(
        json.dumps(document, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )


def single_artifact(directory: Path, suffix: str) -> Path:
    matches = [path for path in artifact_files(directory) if path.name.endswith(suffix)]
    if len(matches) != 1:
        fail(f"expected exactly one {suffix} artifact in {directory}")
    return matches[0]


def check_manifest(directory: Path) -> None:
    manifest = json.loads((ROOT / "adoption-manifest.json").read_text(encoding="utf-8"))
    crate_digest = f"sha256:{sha256(single_artifact(directory, '.crate'))}"
    wheel_digest = f"sha256:{sha256(single_artifact(directory, '.whl'))}"
    expected_by_surface = {
        "rust": crate_digest,
        "runtime": crate_digest,
        "python_sdk": wheel_digest,
    }
    for artifact in manifest["artifacts"]:
        surface = str(artifact["surface"])
        expected = expected_by_surface.get(surface)
        if expected is None:
            fail(f"unknown adoption manifest surface: {surface}")
        actual = str(artifact["digest"])
        if actual != expected:
            fail(
                f"adoption manifest digest mismatch for {surface}: "
                f"expected {expected}, found {actual}"
            )
    print("adoption manifest digests match the release artifacts")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)

    version = commands.add_parser("validate-version")
    version.add_argument("expected", help="expected version or v-prefixed release tag")

    commands.add_parser("current-version")
    commands.add_parser("source-date-epoch")

    compare_command = commands.add_parser("compare")
    compare_command.add_argument("first", type=Path)
    compare_command.add_argument("second", type=Path)
    compare_command.add_argument("--copy-to", type=Path)

    checksums = commands.add_parser("checksums")
    checksums.add_argument("directory", type=Path)
    checksums.add_argument("output", type=Path)

    sbom = commands.add_parser("normalize-sbom")
    sbom.add_argument("path", type=Path)
    sbom.add_argument("version")

    manifest = commands.add_parser("check-manifest")
    manifest.add_argument("directory", type=Path)
    return result


def main() -> None:
    args = parser().parse_args()
    if args.command == "validate-version":
        validate_version(args.expected)
    elif args.command == "current-version":
        versions = current_versions()
        validate_version(next(iter(versions.values())))
    elif args.command == "source-date-epoch":
        print(source_date_epoch())
    elif args.command == "compare":
        compare(args.first, args.second, args.copy_to)
    elif args.command == "checksums":
        checksum_files(args.directory, args.output)
    elif args.command == "normalize-sbom":
        normalize_sbom(args.path, args.version)
    elif args.command == "check-manifest":
        check_manifest(args.directory)


if __name__ == "__main__":
    main()
