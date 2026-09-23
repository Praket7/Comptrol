#!/usr/bin/env python3
"""Verify checksums, SBOM output, and detached release signatures."""

import os
import pathlib
import subprocess
import tempfile


root = pathlib.Path(__file__).resolve().parents[1]
binary = os.environ.get("COMPTROL_BIN", str(root / "target" / "debug" / "comptrol"))
openssl = "openssl"
with tempfile.TemporaryDirectory(prefix="comptrol-release-") as temporary:
    directory = pathlib.Path(temporary)
    key = directory / "signing-key.pem"
    subprocess.run(
        [openssl, "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(key)],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    output = directory / "release"
    subprocess.run(
        [
            "python3",
            str(root / "scripts" / "package_release.py"),
            "--binary",
            binary,
            "--platform",
            "conformance",
            "--version",
            "0.1.0",
            "--output",
            str(output),
            "--signing-key",
            str(key),
        ],
        check=True,
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        ["python3", str(root / "scripts" / "verify_release.py"), "--directory", str(output)],
        check=True,
    )
print("signed release conformance passed")
