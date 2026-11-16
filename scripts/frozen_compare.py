"""Compare behavior, checking derived CLI versions separately from history."""
from copy import deepcopy
import json
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def current_version():
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = workspace["workspace"]["package"]["version"]
    metadata = json.loads((ROOT / "release-metadata.json").read_text(encoding="utf-8"))
    if version != metadata["version"]:
        raise AssertionError("workspace/release version mismatch")
    return version


def assert_observation(identity, actual, expected):
    actual, expected = deepcopy(actual), deepcopy(expected)
    envelope = ("document" if identity.startswith(("private-cli-", "cli-signals-"))
                else "cli" if identity.startswith("remote-surfaces-") else None)
    if envelope:
        if actual[envelope]["component_version"] != current_version():
            raise AssertionError("current component_version mismatch")
        for value in (actual, expected):
            if not isinstance(value[envelope]["component_version"], str):
                raise AssertionError("component_version must be a string")
            value[envelope]["component_version"] = "<workspace-version>"
    if actual != expected:
        raise AssertionError(f"observed baseline: {identity}")


def assert_inventory(actual, expected):
    if actual.keys() != expected.keys():
        raise AssertionError("observed case inventory mismatch")
    for identity in expected:
        assert_observation(identity, actual[identity], expected[identity])
