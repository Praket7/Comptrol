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
import re
import shutil
import secrets
import sys
try:
    import winreg
except ImportError:
    winreg = None


def get_manifest_dir():
    """Get the platform-specific native messaging hosts directory."""
    system = platform.system()
    if system == "Darwin":
        return [os.path.expanduser(
            "~/Library/Application Support/Google/Chrome/NativeMessagingHosts"
        )]
    elif system == "Linux":
        return [os.path.expanduser(
            "~/.config/google-chrome/NativeMessagingHosts"
        )]
    elif system == "Windows":
        # Windows uses a per-user registry entry pointing to the Chrome host manifest.
        local_app_data = os.environ.get("LOCALAPPDATA", "")
        chrome = os.path.join(
            local_app_data, "Google", "Chrome", "NativeMessagingHosts"
        )
        return [chrome] if local_app_data else []
    return []


def is_chrome_extension_id(value):
    return isinstance(value, str) and re.fullmatch(r"[a-p]{32}", value) is not None


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
    if not is_chrome_extension_id(args.extension_id):
        print("Error: --extension-id must be Chrome's exact 32-character extension ID", file=sys.stderr)
        sys.exit(2)

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

    state_dir = os.environ.get(
        "COMPTROL_STATE_DIR",
        os.path.join(os.path.expanduser("~"), ".comptrol"),
    )
    os.makedirs(state_dir, exist_ok=True)
    token_path = os.path.join(state_dir, "browser-bridge.token")
    if not os.path.exists(token_path):
        with open(token_path, "x", encoding="utf-8") as token_file:
            token_file.write(secrets.token_hex(32) + "\n")
        try:
            os.chmod(token_path, 0o600)
        except OSError:
            pass

    config_path = os.path.join(os.path.dirname(host_path), "native_host_config.json")
    with open(config_path, "w", encoding="utf-8") as config_file:
        json.dump(
            {
                "state_dir": os.path.abspath(state_dir),
                "extension_id": args.extension_id,
                "daemon_url": os.environ.get(
                    "COMPTROL_DAEMON_URL",
                    "http://127.0.0.1:7317",
                ),
            },
            config_file,
            indent=2,
        )
        config_file.write("\n")

    # The host reads the state-directory config (or COMPTROL_BRIDGE_CONFIG).
    # Keep that authoritative file aligned with registration; the adjacent
    # file above is retained for older packaged hosts.
    runtime_config_path = os.path.abspath(
        os.environ.get(
            "COMPTROL_BRIDGE_CONFIG", os.path.join(state_dir, "browser-bridge-config.json")
        )
    )
    try:
        with open(runtime_config_path, encoding="utf-8") as config_file:
            runtime_config = json.load(config_file)
        if not isinstance(runtime_config, dict):
            raise ValueError("Browser Bridge configuration must be an object")
    except FileNotFoundError:
        runtime_config = {}
    runtime_config.update({
        "state_dir": os.path.abspath(state_dir),
        "extension_id": args.extension_id,
        "daemon_url": os.environ.get("COMPTROL_DAEMON_URL", "http://127.0.0.1:7317"),
        "native_host_script": host_path,
    })
    runtime_config_parent = os.path.dirname(runtime_config_path)
    if runtime_config_parent:
        os.makedirs(runtime_config_parent, exist_ok=True)
    temporary_config = runtime_config_path + ".tmp"
    with open(temporary_config, "w", encoding="utf-8") as config_file:
        json.dump(runtime_config, config_file, indent=2)
        config_file.write("\n")
    os.replace(temporary_config, runtime_config_path)

    manifest_host_path = host_path
    if platform.system() == "Windows":
        launcher_path = os.path.join(os.path.dirname(host_path), "native_host.bat")
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
            reg_path = r"Software\Google\Chrome\NativeMessagingHosts\comptrol_browser_bridge"
            key = winreg.CreateKey(winreg.HKEY_CURRENT_USER, reg_path)
            winreg.SetValueEx(key, "", 0, winreg.REG_SZ, primary_manifest)
            winreg.CloseKey(key)
            print("Installed the current-user Chrome native messaging registration")
        except Exception as e:
            print(f"Warning: Failed to install Windows registry keys: {e}", file=sys.stderr)

    if installed == 0:
        print("Warning: No browser native messaging directories found", file=sys.stderr)
        print(f"Manifest content:\n{json.dumps(manifest, indent=2)}")
        sys.exit(1)

    print(f"\nInstalled to {installed} browser(s)")


if __name__ == "__main__":
    main()
