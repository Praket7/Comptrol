#!/usr/bin/env python3
"""Verify checksums and metadata emitted by package_release.py."""

import argparse
import hashlib
import json
import pathlib
from pathlib import Path
import shutil
import subprocess
import zipfile


parser = argparse.ArgumentParser()
parser.add_argument("--directory", required=True, type=Path)
parser.add_argument("--public-key", type=Path)
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
    with zipfile.ZipFile(archive) as package:
        members = package.namelist()
        if any(path.startswith("/") or ".." in pathlib.PurePosixPath(path).parts for path in members):
            raise SystemExit(f"unsafe archive path in {archive}")
        binaries = [path for path in members if path in ("comptrol/comptrol", "comptrol/comptrol.exe")]
        if len(binaries) != 1:
            raise SystemExit(f"release archive must contain exactly one Comptrol binary: {archive}")
        if package.getinfo(binaries[0]).file_size == 0:
            raise SystemExit(f"empty release binary in {archive}")
        for adapter in ("blender", "davinci-resolve", "canva", "powerpoint"):
            manifest = f"comptrol/adapters/{adapter}/adapter.toml"
            source = f"comptrol/adapters/{adapter}/src/adapter.py"
            if manifest not in members or source not in members:
                raise SystemExit(f"creative adapter bundle missing from {archive}: {adapter}")

sboms = sorted(args.directory.glob("*.sbom.json"))
if len(sboms) != len(checksums):
    raise SystemExit("each release archive needs one SBOM")
for sbom in sboms:
    value = json.loads(sbom.read_text(encoding="utf-8"))
    if not isinstance(value.get("packages"), list):
        raise SystemExit(f"invalid Cargo metadata SBOM {sbom}")
public_key = args.public_key or (args.directory / "release-public-key.pem")
signatures = sorted(args.directory.glob("*.zip.sig"))
if signatures or args.public_key:
    if not public_key.is_file():
        raise SystemExit("a public key is required to verify release signatures")
    openssl = shutil.which("openssl")
    if not openssl:
        raise SystemExit("openssl is required to verify release signatures")
    expected = {archive.with_name(f"{archive.name}.sig") for archive in (args.directory / name for name in [path.read_text(encoding="utf-8").strip().split(maxsplit=1)[1] for path in checksums])}
    if set(signatures) != expected:
        raise SystemExit("each release archive needs one detached signature")
    for signature in signatures:
        archive = signature.with_suffix("")
        subprocess.run(
            [openssl, "dgst", "-sha256", "-verify", str(public_key), "-signature", str(signature), str(archive)],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
print(f"release verification passed for {len(checksums)} archive(s)")
