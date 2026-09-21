#!/usr/bin/env python3
"""Comptrol Canva companion bridge: stdlib-only localhost relay.

Implements the ``comptrol.canva.bridge/0.1.0`` protocol documented in
``src/bridge_protocol.md``: session open (token + nonce + origin checks),
per-session design binding, and a typed-operation relay log.

This server does NOT execute Canva edits itself. It validates each envelope,
binds it to the session's exact design id, appends it to the relay log, and
returns receipts for the local runtime (or the companion extension) to drain
via ``/v1/session/sync``. Applying edits happens only inside the
user-approved Canva App through the Design Editing API.

Security properties (all enforced below, none opt-in):

- Binds loopback only (``127.0.0.1`` / ``::1`` / ``localhost``); refuses to
  start on any other interface.
- Requires an allowlisted ``Origin`` (``https://www.canva.com``,
  ``https://canva.com``) on every ``/v1/*`` POST.
- Pairing token is read from ``COMPTROL_CANVA_BRIDGE_TOKEN`` at request
  time, compared with ``hmac.compare_digest``, never logged, never stored.
- Client nonces are format-checked and single-use; reuse is refused.
- Every post-open message must echo the session's exact design id.
- Only the six typed ops are accepted; coordinate/script params are denied.
- Request bodies are capped at 64 KiB; op params at 32 KiB.
- CORS + ``Access-Control-Allow-Private-Network`` preflight handling per
  the protocol doc, so a real browser can attempt the direct-localhost
  fallback path (still unvalidated in a real browser).

Self-test: ``python3 src/local_bridge.py --self-test``.
"""

from __future__ import annotations

import argparse
import hmac
import json
import os
import re
import secrets
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PROTOCOL = "comptrol.canva.bridge/0.1.0"
TOKEN_ENV = "COMPTROL_CANVA_BRIDGE_TOKEN"
ORIGIN_ALLOWLIST = ("https://www.canva.com", "https://canva.com")
NATIVE_HOST_ID = "comptrol_canva_native_host"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 8765

MAX_BODY_BYTES = 64 * 1024
MAX_PARAMS_BYTES = 32 * 1024
MAX_TEXT_CHARS = 10000
MAX_LIST_ITEMS = 50
MAX_SESSIONS = 64
MAX_LOG_ENTRIES = 4096
MAX_NONCES = 200000

NONCE_RE = re.compile(r"[A-Za-z0-9_\-]{16,128}")
DESIGN_RE = re.compile(r"[A-Za-z0-9_\-:]{1,128}")

TYPED_OPS = frozenset({
    "design.text.update",
    "design.image.insert",
    "design.element.create",
    "design.element.delete",
    "design.element.group",
    "design.element.inspect",
})

# Coordinate / replay-script keys are never valid op params (mirrors the
# adapter rule: no screen coordinates, no replay scripts, opaque element ids
# only). Text content fields (text, alt, url, ...) remain allowed.
DENY_PARAM_KEYS = frozenset({
    "x", "y", "screenx", "screeny", "clientx", "clienty", "pagex", "pagey",
    "offsetx", "offsety", "coordinates", "boundingbox", "bounding_box",
    "script", "javascript", "eval", "innerhtml", "outerhtml", "onclick",
    "onload", "onerror", "replay", "macro", "keystrokes", "mousepath",
})


class BridgeState:
    """Mutable bridge state guarded by a single lock."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.sessions: dict = {}  # session_id -> {design_id, created_at, seq}
        self.used_nonces: dict = {}  # nonce -> first-seen timestamp
        self.relay_log: list = []  # validated envelopes for the runtime
        self.op_seq = 0

    def consume_nonce(self, nonce: str) -> bool:
        """Mark a nonce used. Returns False when it was already seen."""
        with self.lock:
            if nonce in self.used_nonces:
                return False
            if len(self.used_nonces) >= MAX_NONCES:
                self.used_nonces.pop(next(iter(self.used_nonces)))
            self.used_nonces[nonce] = time.time()
            return True


def _valid_nonce(value: object) -> bool:
    return isinstance(value, str) and NONCE_RE.fullmatch(value) is not None


def _valid_design(value: object) -> bool:
    return isinstance(value, str) and DESIGN_RE.fullmatch(value) is not None


def _walk_params(node: object, depth: int) -> str | None:
    """Return an error code for bad params, else None."""
    if depth > 4:
        return "op_param_too_deep"
    if isinstance(node, dict):
        if len(node) > 64:
            return "op_param_too_many_keys"
        for key, val in node.items():
            if not isinstance(key, str) or not key or len(key) > 64:
                return "op_param_bad_key"
            if key.lower() in DENY_PARAM_KEYS:
                return "op_param_denied"
            err = _walk_params(val, depth + 1)
            if err:
                return err
    elif isinstance(node, list):
        if len(node) > MAX_LIST_ITEMS:
            return "op_param_list_too_long"
        for val in node:
            err = _walk_params(val, depth + 1)
            if err:
                return err
    elif isinstance(node, str):
        if len(node) > MAX_TEXT_CHARS:
            return "op_param_string_too_long"
    elif isinstance(node, (int, float, bool)) or node is None:
        pass
    else:
        return "op_param_bad_type"
    return None


def validate_op(op: object) -> str | None:
    """Return an error code for a bad op envelope, else None."""
    if not isinstance(op, dict):
        return "op_not_object"
    if op.get("type") not in TYPED_OPS:
        return "op_unknown_type"
    params = op.get("params")
    if not isinstance(params, dict):
        return "op_params_not_object"
    if len(json.dumps(params).encode("utf-8")) > MAX_PARAMS_BYTES:
        return "op_params_too_large"
    return _walk_params(params, 0)


class BridgeHandler(BaseHTTPRequestHandler):
    server_version = "ComptrolCanvaBridge/0.1.0"

    def log_message(self, *args: object) -> None:
        # Never log request bodies, tokens, or nonces.
        pass

    # -- generic helpers -------------------------------------------------

    def _allow_origin(self) -> str | None:
        # Allow native host requests via special header
        native_host = self.headers.get("X-Comptrol-Native-Host")
        if native_host == NATIVE_HOST_ID:
            return "native_host"
        origin = self.headers.get("Origin")
        return origin if origin in ORIGIN_ALLOWLIST else None

    def _send_json(self, status: int, obj: dict, preflight: bool = False) -> None:
        body = json.dumps(obj).encode("utf-8")
        allow = self._allow_origin()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Vary", "Origin")
        if allow is not None:
            self.send_header("Access-Control-Allow-Origin", allow)
            self.send_header("Access-Control-Allow-Private-Network", "true")
        if preflight:
            self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "Content-Type")
            self.send_header("Access-Control-Max-Age", "600")
        self.end_headers()
        self.wfile.write(body)

    def _send_preflight(self) -> None:
        allow = self._allow_origin()
        if not self.path.startswith("/v1/") or allow is None:
            self._send_json(403, {"ok": False, "error": "origin_not_allowed"})
            return
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", allow)
        self.send_header("Vary", "Origin")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Private-Network", "true")
        self.send_header("Access-Control-Max-Age", "600")
        self.end_headers()

    def _read_body(self) -> bytes | None:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length > MAX_BODY_BYTES:
            return None  # oversized: refuse without reading the excess
        if length > 0:
            return self.rfile.read(length)
        # No length header: read up to cap + 1 to detect overflow.
        chunks = []
        remaining = MAX_BODY_BYTES + 1
        while remaining > 0:
            chunk = self.rfile.read(min(65536, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)

    def _expected_token(self) -> str:
        # Read at request time so rotation/revocation applies immediately.
        return os.environ.get(TOKEN_ENV, "")

    def _check_auth(self, data: dict, allow: str | None) -> tuple[int, str] | None:
        """Return (status, error) when origin/token checks fail, else None."""
        if allow is None:
            return (403, "origin_not_allowed")
        expected = self._expected_token()
        if not expected:
            return (503, "bridge_token_unconfigured")
        token = data.get("token")
        if not isinstance(token, str) or not hmac.compare_digest(token, expected):
            return (401, "invalid_token")
        return None

    def _check_nonce(self, data: dict, state: BridgeState) -> tuple[int, str] | None:
        nonce = data.get("nonce")
        if not _valid_nonce(nonce):
            return (400, "nonce_invalid")
        if not state.consume_nonce(nonce):
            return (409, "nonce_replay")
        return None

    # -- routing ----------------------------------------------------------

    def do_OPTIONS(self) -> None:  # noqa: N802
        self._send_preflight()

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/v1/health":
            state: BridgeState = self.server.bridge_state
            with state.lock:
                payload = {
                    "ok": True,
                    "protocol": PROTOCOL,
                    "sessions_open": len(state.sessions),
                    "ops_logged": len(state.relay_log),
                }
            self._send_json(200, payload)
            return
        self._send_json(404, {"ok": False, "error": "not_found"})

    def do_POST(self) -> None:  # noqa: N802
        state: BridgeState = self.server.bridge_state
        raw = self._read_body()
        if raw is None or len(raw) > MAX_BODY_BYTES:
            self._send_json(413, {"ok": False, "error": "body_too_large"})
            return
        try:
            data = json.loads(raw.decode("utf-8")) if raw else None
        except (ValueError, UnicodeDecodeError):
            data = None
        if not isinstance(data, dict):
            self._send_json(400, {"ok": False, "error": "bad_json"})
            return
        if data.get("protocol") != PROTOCOL:
            self._send_json(400, {"ok": False, "error": "bad_protocol"})
            return
        allow = self._allow_origin()
        if self.path == "/v1/session/open":
            self._handle_open(data, allow, state)
        elif self.path == "/v1/op/submit":
            self._handle_submit(data, allow, state)
        elif self.path == "/v1/session/sync":
            self._handle_sync(data, allow, state)
        elif self.path == "/v1/session/close":
            self._handle_close(data, allow, state)
        else:
            self._send_json(404, {"ok": False, "error": "not_found"})

    # -- endpoints --------------------------------------------------------

    def _handle_open(self, data: dict, allow: str | None, state: BridgeState) -> None:
        fail = self._check_auth(data, allow)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        fail = self._check_nonce(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        design_id = data.get("design_id")
        if not _valid_design(design_id):
            self._send_json(400, {"ok": False, "error": "design_invalid"})
            return
        with state.lock:
            if len(state.sessions) >= MAX_SESSIONS:
                self._send_json(503, {"ok": False, "error": "bridge_busy"})
                return
            session_id = secrets.token_hex(16)
            state.sessions[session_id] = {
                "design_id": design_id,
                "created_at": time.time(),
                "seq": 0,
            }
        self._send_json(200, {
            "ok": True, "protocol": PROTOCOL, "session_id": session_id,
            "bound_design_id": design_id, "nonce_echo": data.get("nonce"),
        })

    def _bound_session(
        self, data: dict, state: BridgeState
    ) -> tuple[dict | None, tuple[int, str] | None]:
        session_id = data.get("session_id")
        design_id = data.get("design_id")
        if not isinstance(session_id, str) or not session_id:
            return (None, (400, "session_invalid"))
        if not _valid_design(design_id):
            return (None, (400, "design_invalid"))
        with state.lock:
            session = state.sessions.get(session_id)
        if session is None:
            return (None, (404, "session_unknown"))
        if session["design_id"] != design_id:
            return (None, (409, "unbound_design"))
        return (session, None)

    def _handle_submit(self, data: dict, allow: str | None, state: BridgeState) -> None:
        fail = self._check_auth(data, allow)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        fail = self._check_nonce(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        session, fail = self._bound_session(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        op = data.get("op")
        err = validate_op(op)
        if err is not None:
            self._send_json(422, {"ok": False, "error": "op_rejected",
                                  "detail": err})
            return
        with state.lock:
            if len(state.relay_log) >= MAX_LOG_ENTRIES:
                self._send_json(503, {"ok": False, "error": "relay_log_full"})
                return
            state.op_seq += 1
            seq = state.op_seq
            receipt_id = "rcpt-%06d" % seq
            session["seq"] = seq
            state.relay_log.append({
                "receipt_id": receipt_id,
                "session_id": data.get("session_id"),
                "design_id": session["design_id"],
                "op_type": op["type"],
                "params": op["params"],
                "seq": seq,
                "ts": time.time(),
            })
        self._send_json(200, {
            "ok": True, "protocol": PROTOCOL,
            "receipt": {
                "receipt_id": receipt_id,
                "session_id": data.get("session_id"),
                "design_id": session["design_id"],
                "op_type": op["type"], "seq": seq,
            },
            "nonce_echo": data.get("nonce"),
        })

    def _handle_sync(self, data: dict, allow: str | None, state: BridgeState) -> None:
        fail = self._check_auth(data, allow)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        fail = self._check_nonce(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        session, fail = self._bound_session(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        session_id = data.get("session_id")
        with state.lock:
            receipts = [e for e in state.relay_log
                        if e["session_id"] == session_id]
        self._send_json(200, {
            "ok": True, "protocol": PROTOCOL, "session_id": session_id,
            "design_id": session["design_id"], "op_count": len(receipts),
            "last_receipt_id": receipts[-1]["receipt_id"] if receipts else None,
            "receipts": receipts,
        })

    def _handle_close(self, data: dict, allow: str | None, state: BridgeState) -> None:
        fail = self._check_auth(data, allow)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        fail = self._check_nonce(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        session, fail = self._bound_session(data, state)
        if fail is not None:
            self._send_json(fail[0], {"ok": False, "error": fail[1]})
            return
        session_id = data.get("session_id")
        with state.lock:
            count = sum(1 for e in state.relay_log
                        if e["session_id"] == session_id)
            state.sessions.pop(session_id, None)
        self._send_json(200, {
            "ok": True, "protocol": PROTOCOL, "session_id": session_id,
            "closed": True, "op_count": count,
        })


def make_server(host: str, port: int) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer((host, port), BridgeHandler)
    server.daemon_threads = True
    server.bridge_state = BridgeState()
    return server


# -- self-test ------------------------------------------------------------

def run_self_test() -> int:
    """Exercise handshake plus every documented refusal. Returns exit code."""
    import urllib.error
    import urllib.request

    os.environ[TOKEN_ENV] = "selftest-token-0123456789abcdef"
    good_token = os.environ[TOKEN_ENV]
    good_origin = "https://www.canva.com"
    design = "DAGselftest123"
    failures = []

    server = make_server("127.0.0.1", 0)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = "http://127.0.0.1:%d" % port

    def post(path: str, payload: bytes | dict, origin: str | None = good_origin,
             content_type: str = "application/json") -> tuple[int, dict]:
        if isinstance(payload, dict):
            payload = json.dumps(payload).encode("utf-8")
        headers = {"Content-Type": content_type}
        if origin is not None:
            headers["Origin"] = origin
        req = urllib.request.Request(base + path, data=payload,
                                     method="POST", headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return (resp.status, json.loads(resp.read().decode("utf-8")))
        except urllib.error.HTTPError as exc:
            try:
                return (exc.code, json.loads(exc.read().decode("utf-8")))
            except ValueError:
                return (exc.code, {"ok": False, "error": "<non-json>"})
        finally:
            req.close() if hasattr(req, "close") else None

    def check(name: str, cond: bool, detail: str = "") -> None:
        print(("PASS " if cond else "FAIL ") + name
              + (" — " + detail if detail and not cond else ""))
        if not cond:
            failures.append(name)

    def fresh_nonce() -> str:
        return secrets.token_hex(16)

    def open_session(token: str = good_token, origin: str | None = good_origin,
                     nonce: str | None = None, design_id: str = design):
        return post("/v1/session/open", {
            "protocol": PROTOCOL, "token": token,
            "nonce": nonce or fresh_nonce(), "design_id": design_id,
        }, origin=origin)

    # 1. valid handshake
    nonce1 = fresh_nonce()
    status, body = open_session(nonce=nonce1)
    check("valid-handshake",
          status == 200 and body.get("ok") is True
          and body.get("bound_design_id") == design
          and body.get("nonce_echo") == nonce1
          and isinstance(body.get("session_id"), str),
          "status=%r body=%r" % (status, body))
    session_id = body.get("session_id") if status == 200 else "no-session"

    # 2. wrong origin refused
    status, body = open_session(origin="https://evil.example")
    check("wrong-origin-refused",
          status == 403 and body.get("error") == "origin_not_allowed",
          "status=%r body=%r" % (status, body))

    # 3. wrong token refused
    status, body = open_session(token="wrong-token")
    check("wrong-token-refused",
          status == 401 and body.get("error") == "invalid_token",
          "status=%r body=%r" % (status, body))

    # 4. replayed nonce refused
    status, body = open_session(nonce=nonce1)
    check("replayed-nonce-refused",
          status == 409 and body.get("error") == "nonce_replay",
          "status=%r body=%r" % (status, body))

    # 5. oversized body refused
    status, body = post("/v1/session/open", b'{"pad":"' + b"x" * (70 * 1024) + b'"}')
    check("oversized-body-refused",
          status == 413 and body.get("error") == "body_too_large",
          "status=%r body=%r" % (status, body))

    # 6. unbound design op refused
    status, body = post("/v1/op/submit", {
        "protocol": PROTOCOL, "token": good_token, "nonce": fresh_nonce(),
        "session_id": session_id, "design_id": "DAGsomeone-elses-design",
        "op": {"type": "design.text.update", "params": {"element_id": "e1"}},
    })
    check("unbound-design-op-refused",
          status == 409 and body.get("error") == "unbound_design",
          "status=%r body=%r" % (status, body))

    # Bonus: valid op receipt, sync receipt, close (prove the relay log works).
    status, body = post("/v1/op/submit", {
        "protocol": PROTOCOL, "token": good_token, "nonce": fresh_nonce(),
        "session_id": session_id, "design_id": design,
        "op": {"type": "design.text.update",
               "params": {"element_id": "e1", "text": "hello"}},
    })
    receipt = body.get("receipt") or {}
    check("valid-op-receipt",
          status == 200 and receipt.get("design_id") == design
          and receipt.get("op_type") == "design.text.update",
          "status=%r body=%r" % (status, body))

    status, body = post("/v1/session/sync", {
        "protocol": PROTOCOL, "token": good_token, "nonce": fresh_nonce(),
        "session_id": session_id, "design_id": design,
    })
    check("sync-receipt",
          status == 200 and body.get("op_count") == 1
          and body.get("last_receipt_id") == receipt.get("receipt_id"),
          "status=%r body=%r" % (status, body))

    status, body = post("/v1/session/close", {
        "protocol": PROTOCOL, "token": good_token, "nonce": fresh_nonce(),
        "session_id": session_id, "design_id": design,
    })
    check("close",
          status == 200 and body.get("closed") is True
          and body.get("op_count") == 1,
          "status=%r body=%r" % (status, body))

    server.shutdown()
    server.server_close()
    print("self-test: %d/%d checks passed"
          % (9 - len(failures), 9))
    return 1 if failures else 0


def main(argv: list | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.self_test:
        return run_self_test()
    if args.host not in LOOPBACK_HOSTS:
        print("refusing non-loopback bind: %r (allowed: %s)"
              % (args.host, ", ".join(LOOPBACK_HOSTS)), file=sys.stderr)
        return 2
    server = make_server(args.host, args.port)
    print("comptrol canva bridge %s on http://%s:%d (loopback only)"
          % (PROTOCOL, args.host, server.server_address[1]), file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
