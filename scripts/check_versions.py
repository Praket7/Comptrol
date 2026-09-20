#!/usr/bin/env python3
"""Single-source version consistency gate.

`VERSION` at the repository root is the authoritative version. Every package,
plugin, and workspace manifest must match it exactly. CI fails when any
manifest drifts so published artifacts can never disagree about identity.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]

# (path, field, extractor) triples validated against VERSION.
JSON_MANIFESTS = [
    ("package.json", "version"),
    ("packages/mcp/package.json", "version"),
    ("plugins/comptrol/plugin.json", "version"),
    ("plugins/comptrol/.codex-plugin/plugin.json", "version"),
]


def fail(message: str) -> None:
    print(f"version-consistency: FAIL {message}")
    raise SystemExit(1)


def main() -> None:
    version_file = ROOT / "VERSION"
    if not version_file.exists():
        fail("VERSION file is missing at the repository root")
    expected = version_file.read_text(encoding="utf-8").strip()
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[\w.]+)?", expected):
        fail(f"VERSION does not look like a semver: {expected!r}")

    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    workspace_package = cargo.split("[workspace.package]", 1)[-1].split("[", 1)[0]
    match = re.search(r'^version\s*=\s*"([^"]+)"', workspace_package, re.MULTILINE)
    if not match:
        fail("Cargo.toml [workspace] does not declare a version")
    if match.group(1) != expected:
        fail(f"Cargo.toml workspace version {match.group(1)} != VERSION {expected}")

    for relative, field in JSON_MANIFESTS:
        path = ROOT / relative
        if not path.exists():
            fail(f"required manifest is missing: {relative}")
        try:
            manifest = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as error:
            fail(f"{relative} is not valid JSON: {error}")
        actual = manifest.get(field)
        if actual != expected:
            fail(f"{relative} {field} {actual!r} != VERSION {expected}")

    print(f"version-consistency: OK all manifests at {expected}")


if __name__ == "__main__":
    main()
