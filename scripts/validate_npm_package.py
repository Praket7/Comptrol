#!/usr/bin/env python3
"""Validate the npm package contract before publishing a release."""

import json
import pathlib
import shutil
import subprocess
import tempfile


ROOT = pathlib.Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "packages" / "mcp"


def npm_executable():
    for candidate in ("npm.cmd", "npm.exe", "npm"):
        resolved = shutil.which(candidate)
        if resolved:
            return resolved
    raise SystemExit("npm is required to validate the package")


def main():
    npm = npm_executable()
    manifest = json.loads((PACKAGE / "package.json").read_text(encoding="utf-8"))
    if manifest.get("name") != "comptrolling":
        raise SystemExit("unexpected npm package name")
    files = manifest.get("files", [])
    if "native" not in files:
        raise SystemExit("npm package must include the native artifact directory")
    if not (ROOT / "experiments" / "chrome-closed-groups-extension").is_dir():
        raise SystemExit("experimental Chrome extension comparison directory is missing")
        raise SystemExit("experimental Chrome extension comparison directory is missing")
    with tempfile.TemporaryDirectory(prefix="comptrol-npm-pack-") as directory:
        result = subprocess.run([npm, "pack", "--json", "--dry-run"], cwd=PACKAGE, capture_output=True, text=True, check=True)
        packed = json.loads(result.stdout)[0]
        names = {item["path"] for item in packed["files"]}
        if not any(path.startswith("native/") for path in names):
            raise SystemExit("npm dry-run contains no native artifact")
        if any("closed-groups" in path or path.startswith("extensions/") for path in names):
            raise SystemExit("npm dry-run contains experimental Chrome extension files")
    print(json.dumps({"package": manifest["name"], "version": manifest["version"], "validated": True}))


if __name__ == "__main__":
    main()
