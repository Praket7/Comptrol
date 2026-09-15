"""Launch an isolated Windows Chrome profile with a loopback CDP endpoint."""

import argparse
import json
import os
import pathlib
import socket
import subprocess
import time
import urllib.request


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def find_chrome():
    candidates = [
        pathlib.Path(os.environ.get("PROGRAMFILES", "C:/Program Files")) / "Google/Chrome/Application/chrome.exe",
        pathlib.Path(os.environ.get("PROGRAMFILES(X86)", "C:/Program Files (x86)")) / "Google/Chrome/Application/chrome.exe",
        pathlib.Path(os.environ.get("LOCALAPPDATA", "")) / "Google/Chrome/Application/chrome.exe",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise FileNotFoundError("Google Chrome was not found in the standard Windows installation paths")


def wait_for_endpoint(endpoint, timeout=15):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"{endpoint}/json/version", timeout=1) as response:
                return json.load(response)
        except Exception:
            time.sleep(0.1)
    raise TimeoutError(f"Chrome did not expose {endpoint}/json/version")


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--url", default="about:blank")
parser.add_argument("--port", type=int, default=0)
parser.add_argument("--profile", type=pathlib.Path)
parser.add_argument("--address", default="127.0.0.1")
args = parser.parse_args()

root = pathlib.Path(__file__).resolve().parents[1]
profile = args.profile or root / "work" / "chrome-cdp-profile"
profile.mkdir(parents=True, exist_ok=True)
port = args.port or free_port()
endpoint = f"http://{args.address}:{port}"
chrome = find_chrome()
process = subprocess.Popen([
    str(chrome),
    f"--remote-debugging-address={args.address}",
    f"--remote-debugging-port={port}",
    f"--user-data-dir={profile}",
    "--no-first-run",
    "--no-default-browser-check",
    args.url,
])
version = wait_for_endpoint(endpoint)
print(json.dumps({
    "pid": process.pid,
    "chrome": str(chrome),
    "profile": str(profile),
    "endpoint": endpoint,
    "browser": version.get("Browser"),
    "webSocketDebuggerUrl": version.get("webSocketDebuggerUrl"),
}, indent=2))
