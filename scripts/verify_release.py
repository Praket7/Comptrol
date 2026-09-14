#!/usr/bin/env python3
"""Verify checksums and metadata emitted by package_release.py."""

import argparse
import hashlib
import json
from pathlib import Path


parser = argparse.ArgumentParser()
parser.add_argument("--directory", required=True, type=Path)
args = parser.parse_args()

checksums = sorted(args.directory.glob("*.zip.sha256"))
if not checksums:
    raise SystemExit("no release checksum files found")
for checksum_path in checksums:
    digest, name = checksum_path.read_text(encoding="utf-8").strip().split(maxsplit=1)
    archive = args.directory / name
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if actual != digest:
        raise SystemExit(f"checksum mismatch for {archive}")

sboms = sorted(args.directory.glob("*.sbom.json"))
if len(sboms) != len(checksums):
    raise SystemExit("each release archive needs one SBOM")
for sbom in sboms:
    value = json.loads(sbom.read_text(encoding="utf-8"))
    if not isinstance(value.get("packages"), list):
        raise SystemExit(f"invalid Cargo metadata SBOM {sbom}")
print(f"release verification passed for {len(checksums)} archive(s)")
