"""Enforce the Python minor source, build ABI, CI and wheel metadata."""
import argparse
from email.parser import Parser
from pathlib import Path
import re
import sys
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]


def check(root=ROOT):
    minor = (root / ".python-version").read_text(encoding="utf-8").strip()
    if not re.fullmatch(r"3\.\d+", minor):
        raise ValueError("invalid Python version source")
    project = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))["project"]
    if project["requires-python"] != ">=" + minor:
        raise ValueError("Python requirement differs from source")
    classifiers = re.findall(r"Python :: (3\.\d+)$", "\n".join(project["classifiers"]), re.M)
    if classifiers != [minor]:
        raise ValueError("Python classifiers differ from source")
    cargo = (root / "crates/rest-engine-python/Cargo.toml").read_text(encoding="utf-8")
    if re.findall(r"abi3-py(\d+)", cargo) != [minor.replace(".", "")]:
        raise ValueError("Python ABI differs from source")
    paths = [*sorted((root / ".github/workflows").glob("*.yml")),
             *sorted((root / "scripts").glob("*.ps1")), root / "Dockerfile.release",
             root / "Dockerfile.verify", root / "README.md", *sorted((root / "docs").glob("*.md")),
             root / "scripts/install_python.sh"]
    for path in paths:
        source = path.read_text(encoding="utf-8")
        if any(value != minor for value in re.findall(r"(?<![\d.])3\.\d+", source)) or any(
                value != minor.replace(".", "") for value in re.findall(r"(?:cp|py)(3\d+)\b", source)):
            raise ValueError("obsolete Python reference")
        if path.suffix == ".yml":
            if re.search(r"\bpython-version:", source):
                raise ValueError("CI must read the Python version file")
            if source.count("actions/setup-python@") != source.count("python-version-file: '.python-version'"):
                raise ValueError("CI Python source is missing")
    return minor


def check_wheel(path, minor):
    with zipfile.ZipFile(path) as wheel:
        def single(suffix):
            names = [n for n in wheel.namelist() if n.endswith(suffix)]
            if len(names) != 1:
                raise ValueError("ambiguous wheel metadata")
            return Parser().parsestr(wheel.read(names[0]).decode("utf-8"))
        metadata, tags = single(".dist-info/METADATA"), single(".dist-info/WHEEL")
        if metadata["Requires-Python"] != ">=" + minor:
            raise ValueError("wheel Python requirement differs from source")
        expected = "cp" + minor.replace(".", "") + "-abi3-"
        if not tags.get_all("Tag") or any(not t.startswith(expected) for t in tags.get_all("Tag")):
            raise ValueError("wheel ABI differs from source")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", action="store_true")
    parser.add_argument("--wheel", type=Path)
    args = parser.parse_args()
    try:
        minor = check()
        if args.runtime and tuple(map(int, minor.split("."))) != sys.version_info[:2]:
            raise ValueError("Python runtime differs from source")
        if args.wheel:
            check_wheel(args.wheel, minor)
    except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        raise SystemExit(str(error)) from None
    print("Python policy: OK")
