#!/usr/bin/env python3
"""Refuse advisory exemptions that outlived the justification for them.

Every entry in `deny.toml` `[advisories].ignore` is accepted only because no
fixed release of the affected crate builds on the published MSRV. Once the MSRV
moves past that bound the justification is gone, so the exemption must not
survive silently: this check fails the Audit workflow until it is removed
together with a lockfile update.

`deny.toml` is parsed as TOML rather than scanned with a regular expression, so
an entry written as a bare string, or as a table with the fields in a different
order, is still seen.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path
from typing import Any, NoReturn


ROOT = Path(__file__).resolve().parents[1]

# Accepted advisories, each with the Rust version from which a fixed release
# exists. An exemption is only justified while the published MSRV is below its
# bound, so the mapping is explicit per advisory rather than a single global
# rule: an ID that is not listed here has never been reviewed and must not be
# silenced by adding it to deny.toml.
ACCEPTED: dict[str, tuple[int, int]] = {
    # `time` fixes the RFC 2822 stack exhaustion from 0.3.47, which declares
    # rust-version 1.88.0.
    "RUSTSEC-2026-0009": (1, 88),
}


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def declared_msrv() -> tuple[int, int]:
    manifest = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^rust-version\s*=\s*"([0-9]+)\.([0-9]+)', manifest, re.MULTILINE)
    if match is None:
        fail("Cargo.toml does not declare rust-version")
    return int(match.group(1)), int(match.group(2))


def exemption_id(entry: Any) -> str:
    if isinstance(entry, str):
        return entry
    if isinstance(entry, dict):
        identifier = entry.get("id")
        if isinstance(identifier, str):
            return identifier
    fail(f"deny.toml has an advisory exemption in an unrecognised form: {entry!r}")


def main() -> None:
    try:
        policy = tomllib.loads((ROOT / "deny.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot read deny.toml: {error}")

    ignored = policy.get("advisories", {}).get("ignore", [])
    if not isinstance(ignored, list):
        fail("deny.toml [advisories].ignore must be a list")
    exemptions = [exemption_id(entry) for entry in ignored]

    duplicates = sorted({name for name in exemptions if exemptions.count(name) > 1})
    if duplicates:
        fail(f"deny.toml lists the same advisory more than once: {duplicates}")

    unreviewed = sorted(set(exemptions) - set(ACCEPTED))
    if unreviewed:
        fail(
            f"deny.toml silences advisories that were never reviewed: {unreviewed}. "
            "Add them to ACCEPTED in this script, with the MSRV from which a "
            "fixed release exists, only as an explicit decision"
        )

    msrv = declared_msrv()
    expired = sorted(name for name in exemptions if msrv >= ACCEPTED[name])
    if expired:
        fail(
            f"MSRV is now {msrv[0]}.{msrv[1]}; the advisory exemptions {expired} "
            "were justified only by an older MSRV and must be removed together "
            "with a lockfile update"
        )
    print(f"MSRV {msrv[0]}.{msrv[1]}, {len(exemptions)} accepted advisories")


if __name__ == "__main__":
    main()
