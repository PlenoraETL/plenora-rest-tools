"""Enforce exact, text-only Cargo package inventories and reject private PEM."""

import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile


ROOT = Path(__file__).resolve().parents[1]
REGISTRY = ROOT / "scripts/package_files.json"
PRIVATE_KEY = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----")


def inventory(names):
    """Accept canonical relative file names only, without duplicates."""
    result = set()
    for name in names:
        path = PurePosixPath(name)
        if (not name or "\\" in name or ":" in name or path.is_absolute()
                or ".." in path.parts or str(path) != name or name in result):
            raise ValueError("invalid package inventory")
        result.add(name)
    return result


def expected_files(entry, has_vcs):
    """Cargo emits VCS metadata only in a checkout, not the Docker context."""
    files = entry["files"] + (entry["vcs_files"] if has_vcs else [])
    inventory(files)
    return files


def check_content(data):
    """Packages contain text only: binary key containers fail closed as well."""
    if PRIVATE_KEY.search(data):
        raise ValueError("private key in Cargo package")
    try:
        data.decode("utf-8")
    except UnicodeDecodeError as error:
        raise ValueError("binary content in Cargo package") from error
    # DER/PKCS#12, including encrypted keys, contain ASN.1 control octets.
    # No binary format is permitted, even when its name is a source filename.
    if any(byte < 32 and byte not in (9, 10, 13) or byte == 127 for byte in data):
        raise ValueError("binary content in Cargo package")


def check_source_content(data):
    """Reject private PEM and binary ASN.1 containers, regardless of filename.

    Binary fuzz inputs are allowed; DER SEQUENCE containers are not. This
    deliberately also rejects public DER certificates: use public PEM instead.
    """
    if PRIVATE_KEY.search(data):
        raise ValueError("private key in source tree")
    if data.startswith(b"0") and any(byte < 32 and byte not in (9, 10, 13) for byte in data):
        raise ValueError("binary ASN.1 container in source tree")


def check_sources(root):
    """Inspect tracked working-tree files; unstaged deletions are absent sources.

    Without Git (Docker/source archive), inspect the source tree itself. Build
    output is excluded only in that mode, never from the tracked-file check.
    """
    if (root / ".git").exists():
        result = subprocess.run(["git", "ls-files", "-z", "--cached"], cwd=root,
                                check=True, capture_output=True)
        names = inventory(name.decode("utf-8") for name in result.stdout.split(b"\0") if name)
        deleted = subprocess.run(["git", "ls-files", "-z", "--deleted"], cwd=root,
                                 check=True, capture_output=True)
        names -= {name.decode("utf-8") for name in deleted.stdout.split(b"\0") if name}
        paths = [root / name for name in sorted(names)]
    else:
        excluded = {"target", "dist", ".venv", "__pycache__", ".git"}
        paths = sorted(p for p in root.rglob("*") if p.is_file()
                       and not excluded.intersection(p.relative_to(root).parts))
    for path in paths:
        if path.is_symlink():
            raise ValueError("symlink in source tree")
        check_source_content(path.read_bytes())
    print(f"source tree: contents OK ({len(paths)} files)")


def check_files(directory, listing, expected, license_file=None):
    """Compare both directions, then inspect source files without leaking data."""
    names = inventory(name.replace("\\", "/") for name in listing.splitlines())
    if names != inventory(expected):
        raise ValueError("Cargo package inventory mismatch")
    for name in sorted(names):
        # Their actual generated contents are checked in the archive below.
        if name in {"Cargo.lock", ".cargo_vcs_info.json"}:
            continue
        source = directory / ("Cargo.toml" if name == "Cargo.toml.orig" else name)
        if license_file is not None and name == Path(license_file).name:
            source = directory / license_file
        check_content(source.read_bytes())


def check_archive(archive, prefix, expected):
    """Verify the actual generated manifest, lockfile and all packaged bytes."""
    with tarfile.open(archive, "r:gz") as package:
        members = package.getmembers()
        names = []
        for member in members:
            if not member.isfile() or not member.name.startswith(prefix + "/"):
                raise ValueError("invalid Cargo archive entry")
            names.append(member.name[len(prefix) + 1:])
        if inventory(names) != inventory(expected):
            raise ValueError("Cargo archive inventory mismatch")
        for member in members:
            with package.extractfile(member) as source:
                check_content(source.read())


def cargo(*args):
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True,
        encoding="utf-8",
    ).stdout


def main():
    check_sources(ROOT)
    registry = json.loads(REGISTRY.read_text(encoding="utf-8"))
    metadata = json.loads(cargo("metadata", "--no-deps", "--locked", "--format-version", "1"))
    members = set(metadata["workspace_members"])
    packages = [p for p in metadata["packages"] if p["id"] in members]
    if set(registry) != {p["name"] for p in packages}:
        raise ValueError("workspace package registry mismatch")
    for package in packages:
        name = package["name"]
        expected = expected_files(registry[name], (ROOT / ".git").exists())
        listing = cargo("package", "--list", "--locked", "--allow-dirty", "--package", name)
        check_files(Path(package["manifest_path"]).parent, listing,
                    expected, package.get("license_file"))
        # Only the core ships as a .crate; adapters ship as binary/wheel.
        if name == "plenora-rest-core":
            cargo("package", "--no-verify", "--locked", "--allow-dirty", "--package", name)
            prefix = f"{name}-{package['version']}"
            check_archive(Path(metadata["target_directory"]) / "package" / f"{prefix}.crate",
                          prefix, expected)
        print(f"{name}: package contents OK ({len(expected)} files)")


if __name__ == "__main__":
    main()
