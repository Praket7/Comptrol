#!/usr/bin/env python3
"""Verify the V5 runtime advertises every shipped first-party adapter safely."""

import argparse
import json
import subprocess

EXPECTED = {
    "comptrol.apple-mail",
    "comptrol.apple-messages",
    "comptrol.blender",
    "comptrol.canva",
    "comptrol.discord",
    "comptrol.gmail",
    "comptrol.google-workspace",
    "comptrol.libreoffice",
    "comptrol.microsoft-graph-mail",
    "comptrol.obs",
    "comptrol.powerpoint",
    "comptrol.powerpoint-windows",
    "comptrol.resolve",
    "comptrol.vscode",
}


def send(process, request_id, method, params):
    process.stdin.write(
        json.dumps(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
            separators=(",", ":"),
        )
        + "\n"
    )
    process.stdin.flush()
    while True:
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("Comptrol MCP exited before returning adapter catalog")
        response = json.loads(line)
        if response.get("id") == request_id:
            return response


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()

    process = subprocess.Popen(
        [args.binary, "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        response = send(
            process,
            1,
            "tools/call",
            {"name": "inspect", "arguments": {"kind": "adapters"}},
        )
    finally:
        process.stdin.close()
        process.wait(timeout=5)

    if "error" in response:
        raise SystemExit(f"inspect adapters failed: {response}")
    result = response.get("result", {})
    catalog = result.get("structuredContent")
    if not isinstance(catalog, list):
        raise SystemExit(f"adapter catalog is not an array: {response}")

    names = [item.get("name") for item in catalog if isinstance(item, dict)]
    if len(names) != len(set(names)):
        raise SystemExit(f"adapter catalog contains duplicate names: {names}")
    missing = EXPECTED - set(names)
    if missing:
        raise SystemExit(f"runtime adapter catalog missing first-party adapters: {sorted(missing)}")

    by_name = {item["name"]: item for item in catalog if isinstance(item, dict) and item.get("name")}
    for name in sorted(EXPECTED):
        item = by_name[name]
        if not item.get("version"):
            raise SystemExit(f"{name} has no version")
        if not item.get("platforms"):
            raise SystemExit(f"{name} has no platform declaration")
        if not item.get("capabilities"):
            raise SystemExit(f"{name} has no runtime capabilities")
        if not item.get("route"):
            raise SystemExit(f"{name} has no runtime route metadata")
        if item.get("isolation") != "out_of_process_loopback":
            raise SystemExit(f"{name} has unexpected isolation metadata: {item.get('isolation')}")

    print(
        json.dumps(
            {
                "first_party_adapter_count": len(EXPECTED),
                "runtime_catalog_count": len(catalog),
                "verified_first_party_adapters": sorted(EXPECTED),
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
