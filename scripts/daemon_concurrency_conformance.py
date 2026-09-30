#!/usr/bin/env python3
"""P3.2 / P3.3 live conformance: resident daemon serves clients concurrently.

Gate 2 checks, verified live against `comptrol daemon`:
  1. Two clients hold the named pipe/socket AT THE SAME TIME and both are
     served (before P3.2 the second client blocked until the first closed).
  2. Health reports `resident: true` and a live `clients` count.
  3. Killing one client leaves the daemon serving the other (residency).

Run: python scripts/daemon_concurrency_conformance.py [pipe_or_socket_path]
"""

import json
import os
import struct
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)


def frame(obj):
    payload = json.dumps(obj).encode("utf-8")
    return struct.pack(">I", len(payload)) + payload


def read_frame(stream):
    header = stream.read(4)
    if len(header) < 4:
        return None
    (length,) = struct.unpack(">I", header)
    return json.loads(stream.read(length))


def connect(endpoint):
    if os.name == "nt":
        return open(endpoint, "r+b", buffering=0)
    import socket

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(endpoint)
    return sock.makefile("rwb", buffering=0)


def health(stream, ident):
    stream.write(frame({"version": 1, "id": ident, "method": "health"}))
    return read_frame(stream)


def main():
    if os.name == "nt":
        endpoint = sys.argv[1] if len(sys.argv) > 1 else r"\\.\pipe\comptrol"
    else:
        endpoint = (
            sys.argv[1]
            if len(sys.argv) > 1
            else os.path.join(
                os.environ.get("COMPTROL_STATE_DIR", os.path.expanduser("~/.comptrol")),
                "comptrol.sock",
            )
        )

    failures = []

    # Client 1 holds its connection open for the whole run.
    client1 = connect(endpoint)
    reply1 = health(client1, "h1")
    if not (reply1 and reply1.get("result", {}).get("ready")):
        failures.append("client 1 was not served")
        print("FAIL", failures)
        return 1
    if reply1["result"].get("resident") is not True:
        failures.append("health does not report resident: true")

    # Client 2 must be served WHILE client 1 is still connected.
    client2 = connect(endpoint)
    started = time.time()
    reply2 = health(client2, "h2")
    elapsed_ms = (time.time() - started) * 1000
    if not (reply2 and reply2.get("result", {}).get("ready")):
        failures.append("client 2 was not served while client 1 was connected")
    elif elapsed_ms > 2000:
        failures.append(f"client 2 waited {elapsed_ms:.0f}ms (serialized, not concurrent)")
    else:
        print(f"PASS concurrent: client 2 served in {elapsed_ms:.0f}ms with client 1 open")

    count = reply2.get("result", {}).get("clients") if reply2 else None
    if count is not None and count >= 2:
        print(f"PASS clients count: {count}")
    else:
        failures.append(f"clients count not >= 2 while two held connections (saw {count})")

    # Residency: close client 2 (simulates a killed client), client 1 still works.
    client2.close()
    time.sleep(0.2)
    reply3 = health(client1, "h3")
    if reply3 and reply3.get("result", {}).get("ready"):
        print("PASS residency: daemon still serving after peer client vanished")
    else:
        failures.append("daemon stopped serving after a client disconnected")

    client1.close()

    if failures:
        print("FAIL", "; ".join(failures))
        return 1
    print("PASS all daemon concurrency conformance checks")
    return 0


if __name__ == "__main__":
    sys.exit(main())
