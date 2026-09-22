#!/usr/bin/env python3
"""Validate the npm package contract for source CI or release publishing."""

import argparse
import json
import pathlib
import shutil
import subprocess


ROOT = pathlib.Path(__file__).resolve().parents[1]
PACKAGE = ROOT / "packages" / "mcp"
REQUIRED_NATIVE = {
    "native/darwin-arm64/comptrol",
    "native/darwin-x64/comptrol",
    "native/linux-arm64/comptrol",
    "native/linux-x64/comptrol",
    "native/win32-x64/comptrol.exe",
}
REQUIRED_BRIDGE = {
    "browser-bridge/manifest.json",
    "browser-bridge/native_host.py",
    "browser-bridge/install.py",
    "browser-bridge/src/service_worker.js",
}


def npm_executable():
    for candidate in ("npm.cmd", "npm.exe", "npm"):
        resolved = shutil.which(candidate)
        if resolved:
            return resolved
    raise SystemExit("npm is required to validate the package")


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--allow-missing-native",
        action="store_true",
        help=(
            "Allow a source-tree package with no staged native binaries. "
            "If any native binary is present, the complete release matrix is still required."
        ),
    )
    return parser.parse_args()


def main():
    args = parse_args()
    npm = npm_executable()
    manifest = json.loads((PACKAGE / "package.json").read_text(encoding="utf-8"))
    if manifest.get("name") != "comptrolling":
        raise SystemExit("unexpected npm package name")
    files = manifest.get("files", [])
    if "native" not in files:
        raise SystemExit("npm package must include the native artifact directory")
    if "browser-bridge" not in files:
        raise SystemExit("npm package must include Browser Bridge assets")
    if "adapters" not in files:
        raise SystemExit("npm package must include the isolated first-party adapters")

    result = subprocess.run(
        [npm, "pack", "--json", "--dry-run"],
        cwd=PACKAGE,
        capture_output=True,
        text=True,
        check=True,
    )
    packed = json.loads(result.stdout)[0]
    names = {item["path"] for item in packed["files"]}

    present_native = {path for path in names if path.startswith("native/")}
    missing_native = REQUIRED_NATIVE - names
    if args.allow_missing_native:
        if present_native and missing_native:
            raise SystemExit(
                "npm dry-run contains a partial native release matrix; missing: "
                f"{sorted(missing_native)}"
            )
    elif missing_native:
        raise SystemExit(
            "npm dry-run is missing required native release artifacts: "
            f"{sorted(missing_native)}"
        )

    missing_bridge = REQUIRED_BRIDGE - names
    if missing_bridge:
        raise SystemExit(
            f"npm dry-run is missing Browser Bridge assets: {sorted(missing_bridge)}"
        )
    required_adapters = {
        "adapters/blender/adapter.toml",
        "adapters/blender/src/adapter.py",
        "adapters/davinci-resolve/adapter.toml",
        "adapters/davinci-resolve/src/adapter.py",
        "adapters/canva/adapter.toml",
        "adapters/canva/src/adapter.py",
        "adapters/powerpoint/adapter.toml",
        "adapters/powerpoint/src/adapter.py",
    }
    missing_adapters = required_adapters - names
    if missing_adapters:
        raise SystemExit(f"npm dry-run is missing creative adapters: {sorted(missing_adapters)}")
    if "bin/comptrol-browser-setup.js" not in names:
        raise SystemExit("npm dry-run is missing the Browser Bridge setup command")
    if any("closed-groups" in path or path.startswith("extensions/") for path in names):
        raise SystemExit("npm dry-run contains experimental or repository extension paths")

    print(
        json.dumps(
            {
                "package": manifest["name"],
                "version": manifest["version"],
                "validated": True,
                "native_mode": "source" if args.allow_missing_native else "release",
                "native_artifacts": len(present_native),
            }
        )
    )


if __name__ == "__main__":
    main()
