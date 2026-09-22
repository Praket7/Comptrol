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
try:
    import winreg
except ImportError:
    winreg = None


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
        # Windows uses registry for Native Messaging Hosts
        # We still provide files in LOCALAPPDATA as a backup/reference
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
        required=True,
        help="Exact Chrome extension ID authorized to connect to the native host",
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

    manifest_host_path = host_path
    if platform.system() == "Windows":
        launcher_path = os.path.join(os.path.dirname(host_path), "native_host.cmd")
        with open(launcher_path, "w", newline="") as launcher:
            launcher.write("@echo off\r\n")
            launcher.write(f'"{sys.executable}" "{host_path}" %*\r\n')
        manifest_host_path = launcher_path
    else:
        try:
            os.chmod(host_path, os.stat(host_path).st_mode | 0o111)
        except OSError as error:
            print(f"Error: could not make native host executable: {error}", file=sys.stderr)
            sys.exit(1)

    # Build manifest
    manifest = {
        "name": "comptrol_browser_bridge",
        "description": "Comptrol Browser Bridge Native Messaging Host",
        "path": manifest_host_path,
        "type": "stdio",
        "allowed_origins": [],
    }

    manifest["allowed_origins"].append(
        f"chrome-extension://{args.extension_id}/"
    )

    # Install to each browser's directory
    installed = 0
    manifest_files = []
    for manifest_dir in get_manifest_dir():
        os.makedirs(manifest_dir, exist_ok=True)
        manifest_file = os.path.join(
            manifest_dir, "comptrol_browser_bridge.json"
        )
        with open(manifest_file, "w") as f:
            json.dump(manifest, f, indent=2)
        print(f"Installed manifest to: {manifest_file}")
        manifest_files.append(manifest_file)
        installed += 1

    # Windows Registry Installation
    if platform.system() == "Windows" and manifest_files:
        try:
            # Windows expects the registry value to be the path to the manifest JSON file
            # We use the Chrome one as the primary path if available
            primary_manifest = manifest_files[0]
            registry_paths = [
                r"Software\Google\Chrome\NativeMessagingHosts\comptrol_browser_bridge",
                r"Software\Microsoft\Edge\NativeMessagingHosts\comptrol_browser_bridge",
            ]
            for reg_path in registry_paths:
                key = winreg.CreateKey(winreg.HKEY_CURRENT_USER, reg_path)
                winreg.SetValueEx(key, "", 0, winreg.REG_SZ, primary_manifest)
                winreg.CloseKey(key)
            print("Installed Windows registry keys for Browser Bridge")
        except Exception as e:
            print(f"Warning: Failed to install Windows registry keys: {e}", file=sys.stderr)

    if installed == 0:
        print("Warning: No browser native messaging directories found", file=sys.stderr)
        print(f"Manifest content:\n{json.dumps(manifest, indent=2)}")
        sys.exit(1)

    print(f"\nInstalled to {installed} browser(s)")


if __name__ == "__main__":
    main()
