#!/usr/bin/env python3
"""Regression tests for native release archive verification."""

import hashlib
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
VERIFY = ROOT / "scripts" / "verify_release.py"


def make_archive(directory: Path, name: str, members: dict[str, bytes]) -> None:
    archive = directory / name
    with zipfile.ZipFile(archive, "w") as package:
        for member, data in members.items():
            package.writestr(member, data)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (directory / f"{name}.sha256").write_text(f"{digest}  {name}\n", encoding="utf-8")
    (directory / f"{name.removesuffix('.zip')}.sbom.json").write_text(
        '{"packages": []}\n', encoding="utf-8"
    )


def run(directory: Path, expected_success: bool) -> None:
    result = subprocess.run(
        [sys.executable, str(VERIFY), "--directory", str(directory)],
        capture_output=True,
        text=True,
    )
    if (result.returncode == 0) != expected_success:
        raise AssertionError(result.stderr or result.stdout)


with tempfile.TemporaryDirectory(prefix="comptrol-release-test-") as temporary:
    root = Path(temporary)
    make_archive(root, "valid.zip", {"comptrol/comptrol": b"binary"})
    run(root, True)

    invalid = root / "invalid.zip"
    with zipfile.ZipFile(invalid, "w") as package:
        package.writestr("../escape", b"bad")
        package.writestr("comptrol/comptrol", b"binary")
    digest = hashlib.sha256(invalid.read_bytes()).hexdigest()
    (root / "invalid.zip.sha256").write_text(f"{digest}  invalid.zip\n", encoding="utf-8")
    (root / "invalid.sbom.json").write_text('{"packages": []}\n', encoding="utf-8")
    run(root, False)

print("release verification regression tests passed")
