#!/usr/bin/env python3
"""
Comptrol Browser Bridge - Installation Script

Installs the native messaging host manifest and extension for Chrome.
Run this script after building Comptrol to register the Browser Bridge.

Usage:
    python3 install.py [--host-path PATH] [--extension-id ID]
"""

import argparse
import json
import os
import platform
import shutil
import sys


def get_manifest_dir():
    """Get the platform-specific native messaging hosts directory."""
    system = platform.system()
    if system == "Darwin":
        chrome = os.path.expanduser(
            "~/Library/Application Support/Google/Chrome/NativeMessagingHosts"
        )
        edge = os.path.expanduser(
            "~/Library/Application Support/Microsoft Edge/NativeMessagingHosts"
        )
        return [chrome, edge]
    elif system == "Linux":
        chrome = os.path.expanduser(
            "~/.config/google-chrome/NativeMessagingHosts"
        )
        edge = os.path.expanduser(
            "~/.config/microsoft-edge/NativeMessagingHosts"
        )
        return [chrome, edge]
    elif system == "Windows":
        # Windows uses registry, but we can also write the manifest
        local_app_data = os.environ.get("LOCALAPPDATA", "")
        chrome = os.path.join(
            local_app_data, "Google", "Chrome", "NativeMessagingHosts"
        )
        edge = os.path.join(
            local_app_data, "Microsoft", "Edge", "NativeMessagingHosts"
        )
        return [chrome, edge]
    return []


def main():
    parser = argparse.ArgumentParser(
        description="Install Comptrol Browser Bridge native messaging host"
    )
    parser.add_argument(
        "--host-path",
        help="Path to native_host.py (default: auto-detect)",
    )
    parser.add_argument(
        "--extension-id",
        help="Chrome extension ID (default: not set, use for development)",
    )
    args = parser.parse_args()

    # Determine host path
    if args.host_path:
        host_path = os.path.abspath(args.host_path)
    else:
        host_path = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "native_host.py"
        )

    if not os.path.exists(host_path):
        print(f"Error: native_host.py not found at {host_path}", file=sys.stderr)
        sys.exit(1)

    # Build manifest
    manifest = {
        "name": "comptrol_browser_bridge",
        "description": "Comptrol Browser Bridge Native Messaging Host",
        "path": host_path,
        "type": "stdio",
        "allowed_origins": [],
    }

    if args.extension_id:
        manifest["allowed_origins"].append(
            f"chrome-extension://{args.extension_id}/"
        )

    # Install to each browser's directory
    installed = 0
    for manifest_dir in get_manifest_dir():
        os.makedirs(manifest_dir, exist_ok=True)
        manifest_file = os.path.join(
            manifest_dir, "comptrol_browser_bridge.json"
        )
        with open(manifest_file, "w") as f:
            json.dump(manifest, f, indent=2)
        print(f"Installed manifest to: {manifest_file}")
        installed += 1

    if installed == 0:
        print("Warning: No browser native messaging directories found", file=sys.stderr)
        print(f"Manifest content:\n{json.dumps(manifest, indent=2)}")
        sys.exit(1)

    print(f"\nInstalled to {installed} browser(s)")
    if not args.extension_id:
        print(
            "\nNote: No extension ID specified. "
            "Add --extension-id to allow the extension to connect."
        )


if __name__ == "__main__":
    main()
