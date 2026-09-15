"""Small bounded framed RPC helper shared by the first party adapters."""

import json
import os
import struct
import sys
import time
import uuid

MAX_FRAME = 256 * 1024


def read_frame(stream):
    header = stream.buffer.read(4)
    if not header:
        return None
    if len(header) != 4:
        raise ValueError("truncated adapter frame header")
    length = struct.unpack(">I", header)[0]
    if length > MAX_FRAME:
        raise ValueError("adapter frame exceeds limit")
    payload = stream.buffer.read(length)
    if len(payload) != length:
        raise ValueError("truncated adapter frame")
    return json.loads(payload)


def write_frame(value):
    payload = json.dumps(value, separators=(",", ":")).encode("utf-8")
    if len(payload) > MAX_FRAME:
        raise ValueError("adapter response exceeds limit")
    sys.stdout.buffer.write(struct.pack(">I", len(payload)) + payload)
    sys.stdout.buffer.flush()


def response(request, ok, health, payload=None, error=None):
    return {
        "protocol_version": 1,
        "adapter_instance_id": request.get("adapter_instance_id", ""),
        "request_id": request.get("request_id", ""),
        "ok": ok,
        "health": health,
        "payload": payload if payload is not None else {},
        "error": error,
    }


def serve(handler):
    while True:
        request = read_frame(sys.stdin)
        if request is None:
            return
        try:
            write_frame(handler(request))
        except Exception as exc:  # adapter boundary must return structured failure
            write_frame(response(request, False, "unhealthy", error={"code": "adapter_error", "message": str(exc)}))


def now_ms():
    return int(time.time() * 1000)


def instance_id(prefix):
    return f"{prefix}-{os.getpid()}-{uuid.uuid4().hex[:8]}"

