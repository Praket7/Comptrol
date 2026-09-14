#!/usr/bin/env python3
"""Verify scoped pairing creation, acceptance, persistence, and revocation."""

import json
import os
import subprocess
import tempfile


def run(binary, state, arguments, expected=0):
    result = subprocess.run(
        [binary, "pair", *arguments],
        env={**os.environ, "COMPTROL_STATE_DIR": state},
        capture_output=True,
        text=True,
    )
    assert result.returncode == expected, result.stderr
    return json.loads(result.stdout) if result.stdout else None


with tempfile.TemporaryDirectory(prefix="comptrol-pairing-") as state:
    binary = os.environ.get("COMPTROL_BIN", "target/debug/comptrol")
    created = run(binary, state, ["show", "--ttl-ms", "60000", "--scope", "observe", "--scope", "accessibility_read"])
    assert len(created["code"]) == 64
    accepted = run(binary, state, ["accept", created["code"]])
    assert accepted["accepted"] is True
    run(binary, state, ["accept", created["code"]], expected=1)
    listed = run(binary, state, ["list"])
    assert listed["pairings"][0]["scopes"] == ["observe", "accessibility_read"]
    revoked = run(binary, state, ["revoke", created["pairing_id"]])
    assert revoked["revoked"] is True
    run(binary, state, ["accept", created["code"]], expected=1)
    run(binary, state, ["show", "--scope", "unknown"], expected=1)
print("pairing conformance passed")
