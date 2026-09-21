#!/usr/bin/env python3
"""Exercise the mutual-TLS HTTP transport with and without a client certificate."""

import http.client
import json
import os
import shutil
import socket
import ssl
import subprocess
import tempfile
import time
from pathlib import Path


BIN = os.environ.get("COMPTROL_BIN", "target/release/comptrol")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def run(command, cwd):
    subprocess.run(command, cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def request(port, context):
    connection = http.client.HTTPSConnection("127.0.0.1", port, context=context, timeout=5)
    connection.request(
        "POST",
        "/mcp",
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2026-07-28"}}),
        {"Content-Type": "application/json", "Accept": "application/json", "MCP-Protocol-Version": "2026-07-28"},
    )
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, json.loads(payload)


with tempfile.TemporaryDirectory(prefix="comptrol-mtls-") as directory:
    root = Path(directory)
    if shutil.which("openssl") is None:
        raise SystemExit("openssl is required for mTLS conformance")
    run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.crt", "-subj", "/CN=Comptrol Test CA", "-days", "1", "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"], root)
    run(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key", "-out", "server.csr", "-subj", "/CN=127.0.0.1"], root)
    (root / "server.ext").write_text("subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n", encoding="utf-8")
    run(["openssl", "x509", "-req", "-in", "server.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial", "-out", "server.crt", "-days", "1", "-extfile", "server.ext"], root)
    run(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "client.key", "-out", "client.csr", "-subj", "/CN=Comptrol Test Client"], root)
    (root / "client.ext").write_text("extendedKeyUsage=clientAuth\n", encoding="utf-8")
    run(["openssl", "x509", "-req", "-in", "client.csr", "-CA", "ca.crt", "-CAkey", "ca.key", "-CAcreateserial", "-out", "client.crt", "-days", "1", "-extfile", "client.ext"], root)
    port = free_port()
    environment = {**os.environ, "COMPTROL_STATE_DIR": str(root / "state"), "COMPTROL_MTLS_BIND": "127.0.0.1", "COMPTROL_MTLS_CERT": str(root / "server.crt"), "COMPTROL_MTLS_KEY": str(root / "server.key"), "COMPTROL_MTLS_CLIENT_CA": str(root / "ca.crt"), "COMPTROL_MTLS_AUTO_PAIR": "1"}
    process = subprocess.Popen([BIN, "serve-mtls", str(port)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=environment)
    try:
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    break
            except OSError:
                time.sleep(0.05)
        else:
            raise RuntimeError("mTLS server did not start")
        context = ssl.create_default_context(cafile=str(root / "ca.crt"))
        context.load_cert_chain(str(root / "client.crt"), str(root / "client.key"))
        status, payload = request(port, context)
        assert status == 200 and payload["result"]["protocolVersion"] == "2026-07-28"
        unauthenticated = ssl.create_default_context(cafile=str(root / "ca.crt"))
        try:
            request(port, unauthenticated)
        except (ssl.SSLError, ConnectionError, OSError):
            pass
        else:
            raise AssertionError("mTLS server accepted a client without a certificate")
        print("mTLS interoperability conformance passed")
    finally:
        process.terminate()
        process.wait(timeout=5)
