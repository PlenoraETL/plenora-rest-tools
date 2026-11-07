"""Reject test fixtures and PEM private keys in workspace Cargo packages."""

import json
from pathlib import Path, PurePosixPath
import re
import subprocess


ROOT = Path(__file__).resolve().parents[1]
PRIVATE_KEY = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----")


def check_files(directory, listing, license_file=None):
    """Inspect exactly the files Cargo lists; never print their contents."""
    for name in listing.splitlines():
        path = PurePosixPath(name.replace("\\", "/"))
        if path.is_absolute() or ".." in path.parts:
            raise ValueError("invalid package path")
        if any(a == "tests" and b == "fixtures"
               for a, b in zip(path.parts, path.parts[1:])):
            raise ValueError("test fixture in Cargo package")
        # Cargo synthesizes these files; the original manifest is inspected.
        if name in {"Cargo.lock", ".cargo_vcs_info.json"}:
            continue
        source = directory / ("Cargo.toml" if name == "Cargo.toml.orig" else path)
        if license_file is not None and name == Path(license_file).name:
            source = directory / license_file
        if PRIVATE_KEY.search(source.read_bytes()):
            raise ValueError("PEM private key in Cargo package")


def cargo(*args):
    return subprocess.run(
        ["cargo", *args], cwd=ROOT, check=True, capture_output=True, text=True,
        encoding="utf-8",
    ).stdout


def main():
    metadata = json.loads(cargo("metadata", "--no-deps", "--locked", "--format-version", "1"))
    members = set(metadata["workspace_members"])
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        listing = cargo("package", "--list", "--locked", "--allow-dirty",
                        "--package", package["name"])
        check_files(Path(package["manifest_path"]).parent, listing, package.get("license_file"))
        print(f"{package['name']}: package contents OK")


if __name__ == "__main__":
    main()
