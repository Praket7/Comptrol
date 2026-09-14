#!/usr/bin/env python3
"""Package one already built Comptrol binary with a checksum and Cargo metadata."""

import argparse
import hashlib
import json
import pathlib
import subprocess
import zipfile


parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
parser.add_argument("--platform", required=True)
parser.add_argument("--version", required=True)
parser.add_argument("--output", required=True)
args = parser.parse_args()
version = args.version.removeprefix("v")

binary = pathlib.Path(args.binary)
output = pathlib.Path(args.output)
if not binary.is_file():
    raise SystemExit(f"release binary is missing: {binary}")
output.mkdir(parents=True, exist_ok=True)
archive = output / f"comptrol-{version}-{args.platform}.zip"
with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
    package.write(binary, f"comptrol/{binary.name}")

digest = hashlib.sha256(archive.read_bytes()).hexdigest()
(output / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
metadata = subprocess.run(
    ["cargo", "metadata", "--locked", "--format-version", "1"],
    check=True,
    capture_output=True,
    text=True,
)
(output / f"comptrol-{version}-{args.platform}.sbom.json").write_text(
    json.dumps(json.loads(metadata.stdout), indent=2) + "\n",
    encoding="utf-8",
)
print(json.dumps({"archive": str(archive), "sha256": digest, "sbom": True}))
