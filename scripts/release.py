#!/usr/bin/env python3
"""Validate and assemble deterministic plenora-rest-tools release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import NoReturn


ROOT = Path(__file__).resolve().parents[1]
# The reproducible Linux build produces exactly these artifacts: the core
# crate, the manylinux abi3 wheel and the CLI binary. Each kind is matched by
# a predicate and must appear exactly once; any other file in the directory
# is an error rather than something silently left out of the comparison.
CLI_LINUX_ARTIFACT = "plenora-rest-linux-x86_64"
ARTIFACT_KINDS = {
    ".crate": lambda name: name.endswith(".crate"),
    ".whl": lambda name: name.endswith(".whl"),
    CLI_LINUX_ARTIFACT: lambda name: name == CLI_LINUX_ARTIFACT,
}
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


# Images pinned by digest inside the release tooling. Duplicating a digest is
# only safe if something checks that the copies agree; otherwise a partial
# update silently produces a release built by an image the metadata does not
# describe.
#
# The check looks at where each reference actually takes effect — every `FROM`
# instruction, the image argument array that `docker run` receives — rather than
# at digests appearing anywhere in the file, which a stale comment or an unused
# decoy string would satisfy.
IMAGE_REFERENCE = re.compile(r"[A-Za-z0-9][^\s\"']*@sha256:[0-9a-f]{64}")
DOCKERFILE = Path("Dockerfile.release")
RELEASE_SCRIPT = Path("scripts") / "release.ps1"
FROM_INSTRUCTION = re.compile(
    r"^[ \t]*FROM[ \t]+(?:--\S+[ \t]+)*(\S+)(?:[ \t]+AS[ \t]+(\S+))?[ \t]*$",
    re.MULTILINE | re.IGNORECASE,
)
# `$syftArguments = @( ... )` — matched case-insensitively because PowerShell
# variable names are, and captured for every assignment so a second one cannot
# hide behind the first.
SYFT_ASSIGNMENT = re.compile(
    r"\$syftArguments\s*=\s*@\((.*?)^\s*\)",
    re.MULTILINE | re.DOTALL | re.IGNORECASE,
)
# Any later write to the variable or one of its elements would change the image
# actually executed while leaving the checked assignment untouched.
SYFT_MUTATION = re.compile(
    r"\$syftArguments\s*(?:\[[^\]]*\]\s*=|\+=)|\$syftArguments\s*=(?!\s*@\()",
    re.IGNORECASE,
)


def normalized_image(reference: str) -> str:
    """Repository and digest, dropping the optional informational tag.

    `repo:tag@sha256:...` and `repo@sha256:...` denote the same image: the
    digest is what identifies it, the tag is documentation.
    """
    repository, digest = reference.split("@", 1)
    name = repository.rsplit("/", 1)[-1]
    if ":" in name:
        repository = repository[: len(repository) - len(name)] + name.split(":", 1)[0]
    return f"{repository}@{digest}"


def dockerfile_base_images() -> list[str]:
    """Every external image the release Dockerfile builds `FROM`.

    References to an earlier build stage are not images and are skipped; every
    remaining base must be pinned, so an extra unpinned stage cannot slip in
    behind a pinned one. Line continuations are folded first so a `FROM` split
    across lines is still seen.
    """
    content = re.sub(
        r"\\[ \t]*\r?\n[ \t]*", " ", (ROOT / DOCKERFILE).read_text(encoding="utf-8")
    )
    stages: set[str] = set()
    images: list[str] = []
    for match in FROM_INSTRUCTION.finditer(content):
        base, alias = match.group(1), match.group(2)
        if base.lower() not in stages and base.lower() != "scratch":
            if IMAGE_REFERENCE.fullmatch(base) is None:
                fail(
                    f"{DOCKERFILE.as_posix()} builds FROM {base!r}, which is not "
                    "pinned by a full sha256 digest"
                )
            images.append(base)
        if alias:
            stages.add(alias.lower())
    if not images:
        fail(f"{DOCKERFILE.as_posix()} declares no external base image")
    return images


def syft_run_images() -> list[str]:
    """Digest-pinned images inside the argument array passed to `docker run`.

    Restricting the search to that array is what makes the check meaningful: a
    digest sitting in an unused variable elsewhere in the script no longer
    satisfies it, and a later reassignment or element write is refused outright
    rather than silently swapping the image that actually runs.
    """
    content = (ROOT / RELEASE_SCRIPT).read_text(encoding="utf-8")
    assignments = SYFT_ASSIGNMENT.findall(content)
    if not assignments:
        fail(
            f"{RELEASE_SCRIPT.as_posix()} does not define the $syftArguments "
            "array the SBOM builder is invoked with"
        )
    if len(assignments) != 1:
        fail(
            f"{RELEASE_SCRIPT.as_posix()} assigns $syftArguments "
            f"{len(assignments)} times; the image that actually runs is ambiguous"
        )
    if SYFT_MUTATION.search(content):
        fail(
            f"{RELEASE_SCRIPT.as_posix()} mutates $syftArguments after building "
            "it, so the verified image is not necessarily the one executed"
        )
    images = IMAGE_REFERENCE.findall(assignments[0])
    if not images:
        fail(
            f"{RELEASE_SCRIPT.as_posix()} passes no digest-pinned image to "
            "docker run for the SBOM builder"
        )
    return images


BUILDER_USERS = {
    "manylinux": (DOCKERFILE, dockerfile_base_images),
    "sbom": (RELEASE_SCRIPT, syft_run_images),
}


def validate_builders() -> None:
    metadata = json.loads((ROOT / "release-metadata.json").read_text(encoding="utf-8"))
    builders = metadata.get("builders")
    if not isinstance(builders, dict) or not builders:
        fail("release-metadata.json must declare a non-empty builders object")
    if set(builders) != set(BUILDER_USERS):
        fail(
            "release-metadata.json builders must be exactly "
            f"{sorted(BUILDER_USERS)}, found {sorted(builders)}"
        )
    for name, reference in sorted(builders.items()):
        if not isinstance(reference, str) or IMAGE_REFERENCE.fullmatch(reference) is None:
            fail(
                f"builder {name!r} must be an image pinned by a full sha256 "
                f"digest, found {reference!r}"
            )
        relative, discover = BUILDER_USERS[name]
        label = relative.as_posix()
        expected = normalized_image(reference)
        unexpected = sorted(
            {image for image in discover() if normalized_image(image) != expected}
        )
        if unexpected:
            fail(
                f"{label} runs {unexpected} but release-metadata.json declares "
                f"the {name} builder as {reference}"
            )
    # Any other digest-pinned reference in these files is a stale copy that the
    # metadata no longer describes.
    declared = {normalized_image(reference) for reference in builders.values()}
    for relative, _ in BUILDER_USERS.values():
        content = (ROOT / relative).read_text(encoding="utf-8")
        stale = sorted(
            reference
            for reference in set(IMAGE_REFERENCE.findall(content))
            if normalized_image(reference) not in declared
        )
        if stale:
            fail(
                f"{relative.as_posix()} contains digest-pinned images that "
                f"release-metadata.json does not declare: {stale}"
            )
    # stderr: `validate-version` and `current-version` write the resolved
    # version to stdout and callers capture that stream verbatim.
    print(
        f"builder digests are consistent across {len(builders)} images",
        file=sys.stderr,
    )


def normalized_version(tag: str) -> str:
    version = tag[1:] if tag.startswith("v") else tag
    if VERSION_PATTERN.fullmatch(version) is None:
        fail(f"release tag must be v<semver>, got {tag!r}")
    return version


def validate_version(expected: str) -> None:
    validate_builders()
    version = normalized_version(expected)
    versions = current_versions()
    mismatches = {path: actual for path, actual in versions.items() if actual != version}
    if mismatches:
        details = ", ".join(
            f"{path}={actual}" for path, actual in sorted(mismatches.items())
        )
        fail(f"release version {version} is inconsistent: {details}")
    print(version)


def artifact_kind(name: str) -> str | None:
    kinds = [kind for kind, matches in ARTIFACT_KINDS.items() if matches(name)]
    return kinds[0] if len(kinds) == 1 else None


def artifact_files(directory: Path) -> list[Path]:
    if not directory.is_dir():
        fail(f"artifact directory does not exist: {directory}")
    entries = sorted(directory.iterdir(), key=lambda path: path.name)
    files = [path for path in entries if path.is_file()]
    kinds = [artifact_kind(path.name) for path in files]
    expected = sorted(ARTIFACT_KINDS)
    if len(files) != len(entries) or sorted(str(kind) for kind in kinds) != expected:
        fail(
            f"expected exactly one each of {expected} in {directory}, "
            f"found {[path.name for path in entries]}"
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


def single_artifact(directory: Path, kind: str) -> Path:
    matches = [
        path for path in artifact_files(directory) if artifact_kind(path.name) == kind
    ]
    if len(matches) != 1:
        fail(f"expected exactly one {kind} artifact in {directory}")
    return matches[0]


def check_manifest(directory: Path) -> None:
    manifest = json.loads((ROOT / "adoption-manifest.json").read_text(encoding="utf-8"))
    crate_digest = f"sha256:{sha256(single_artifact(directory, '.crate'))}"
    wheel_digest = f"sha256:{sha256(single_artifact(directory, '.whl'))}"
    cli_digest = f"sha256:{sha256(single_artifact(directory, CLI_LINUX_ARTIFACT))}"
    # The cli entry records the reproducible Linux binary; the Windows
    # executable is checksummed and attested but, like the Windows wheel, has
    # no committed digest.
    expected_by_surface = {
        "rust": crate_digest,
        "runtime": crate_digest,
        "python_sdk": wheel_digest,
        "cli": cli_digest,
    }
    surfaces = sorted(str(artifact["surface"]) for artifact in manifest["artifacts"])
    if sorted(set(surfaces)) != sorted(expected_by_surface) or len(surfaces) != len(
        expected_by_surface
    ):
        fail(
            "adoption manifest must declare exactly one artifact per surface "
            f"{sorted(expected_by_surface)}, found {surfaces}"
        )
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
