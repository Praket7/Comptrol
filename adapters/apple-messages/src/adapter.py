#!/usr/bin/env python3
"""Apple Messages adapter: drafts and sends via the published AppleScript surface.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

There is no HTTP surface and no private IMCore access here: the adapter never
reads ~/Library/Messages/chat.db and never touches IMessage internals. Every
action runs through a FIXED AppleScript template against the published
Messages dictionary (buddies, text chats, send), invoked as subprocess argv
["/usr/bin/osascript", "-e", script] (never shell=True). The model NEVER
supplies script source: payload fields are data only and are quoted/escaped
into closed slots (recipient handle or chat GUID, body, attachment path). Any
payload carrying script-source keys (script, applescript, source, command,
osascript) is refused outright, and body text is embedded as quoted segments
joined with `& linefeed &` so it cannot break out of its string literal.

Binding is exact: either a chat GUID (text chat id) or a participant handle
(phone number or email) resolved to the first matching buddy across services.
Bodies are capped at 2000 characters.

Verification for message.send is a chat message-count increment plus a
last-message match read back through the same scripted surface; anything less
stays honestly unverified. Duplicate sends are prevented per chat with a
client nonce: the nonce is recorded under the state directory after the first
send, and a repeat of the same nonce on the same chat is refused.

message.draft has no server-side API in Messages, so it validates every field
and stages the draft as a local JSON record; the staged file readback is the
verification.

This module imports safely on any platform (subprocess is only invoked per
request); intent calls outside macOS return unsupported.
"""

import hashlib
import json
import os
import re
import secrets
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.apple-messages"
ASSETS_ROOT_ENV = "COMPTROL_MAIL_ASSETS_ROOT"
STATE_DIR_ENV = "COMPTROL_STATE_DIR"
OSASCRIPT = "/usr/bin/osascript"

OSASCRIPT_TIMEOUT_S = 30.0
ATTACHMENT_MAX_BYTES = 10 * 1024 * 1024
MAX_BODY_CHARS = 2000

INTENTS = (
    "message.draft",
    "message.send",
)

_HANDLE_RE = re.compile(r"^[A-Za-z0-9+@._\- ]{1,256}$")
_CHAT_GUID_RE = re.compile(r"^[A-Za-z0-9+;:=._\-]{1,256}$")
_NONCE_RE = re.compile(r"^[A-Za-z0-9_-]{8,64}$")

# Payload keys that would carry script source. Their presence is always a
# refusal: the model supplies typed data fields, never script text.
SCRIPT_KEYS = frozenset({
    "script", "applescript", "apple_script", "source", "command",
    "osascript", "shell", "do_shell_script",
})

AUTOMATION_HINT = ("macOS blocked AppleScript control of Messages. Grant Automation "
                   "permission: System Settings > Privacy & Security > Automation, "
                   "allow the comptrol host process to control Messages, then retry.")


class AdapterError(RuntimeError):
    def __init__(self, code: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code
        self.detail = detail


def _is_darwin() -> bool:
    return sys.platform == "darwin"


def _assets_root() -> Path:
    raw = os.environ.get(ASSETS_ROOT_ENV, "")
    if not raw or not os.path.isabs(raw):
        raise AdapterError("assets_root_missing",
                           ASSETS_ROOT_ENV + " must be an absolute directory path")
    root = Path(raw).resolve()
    if not root.is_dir():
        raise AdapterError("assets_root_missing",
                           ASSETS_ROOT_ENV + " does not name an existing directory")
    return root


def _state_dir() -> Path:
    override = os.environ.get(STATE_DIR_ENV, "")
    if override:
        base = Path(override)
    else:
        home = os.environ.get("HOME") or os.environ.get("USERPROFILE") or "."
        base = Path(home) / ".comptrol"
    return base / "apple-messages"


# ---------------------------------------------------------------- validators


def _reject_script_keys(payload: Dict[str, Any]) -> None:
    bad = SCRIPT_KEYS.intersection(payload.keys())
    if bad:
        raise AdapterError("arbitrary_script_refused",
                           "payload carries script-source keys (%s); only typed "
                           "data fields are accepted" % ", ".join(sorted(bad)))


def _req_str(payload: Dict[str, Any], key: str, max_len: int,
             allow_empty: bool = False) -> str:
    value = payload.get(key)
    if (not isinstance(value, str) or len(value) > max_len
            or (not allow_empty and not value)):
        if allow_empty:
            raise ValueError(key + " must be a string of length 0-%d" % max_len)
        raise ValueError(key + " must be a string of length 1-%d" % max_len)
    if "\x00" in value:
        raise ValueError(key + " must not contain NUL characters")
    return value


def _target(payload: Dict[str, Any]) -> Tuple[str, str]:
    """Return (kind, value): kind is chat or buddy. Exactly one is required."""
    chat_guid = payload.get("chat_guid")
    recipient = payload.get("recipient")
    if chat_guid is not None and recipient is not None:
        raise ValueError("supply exactly one of chat_guid or recipient")
    if chat_guid is not None:
        if not isinstance(chat_guid, str) or not _CHAT_GUID_RE.match(chat_guid):
            raise ValueError("chat_guid must be 1-256 chars of letters, digits "
                             "and +;:=._-")
        return "chat", chat_guid
    if recipient is not None:
        if not isinstance(recipient, str) or not _HANDLE_RE.match(recipient):
            raise ValueError("recipient must be a phone number or email "
                             "(1-256 chars)")
        if "@" not in recipient and not re.search(r"[0-9]", recipient):
            raise ValueError("recipient must look like a phone number or email")
        return "buddy", recipient
    raise ValueError("supply exactly one of chat_guid or recipient")


def _req_body(payload: Dict[str, Any]) -> str:
    body = _req_str(payload, "body", MAX_BODY_CHARS, allow_empty=True)
    return body


def _opt_attachment(payload: Dict[str, Any]) -> Optional[Dict[str, Any]]:
    value = payload.get("attachment")
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ValueError("attachment must be an object with a path")
    raw_path = value.get("path")
    if not isinstance(raw_path, str) or not raw_path:
        raise ValueError("attachment needs a path string")
    root = _assets_root()
    resolved = (root / raw_path).resolve() if not os.path.isabs(raw_path) \
        else Path(raw_path).resolve()
    try:
        resolved.relative_to(root)
    except ValueError:
        raise AdapterError("attachment_outside_scope",
                           "attachment must live under " + ASSETS_ROOT_ENV)
    if not resolved.is_file():
        raise AdapterError("attachment_missing",
                           "attachment file does not exist: " + resolved.name)
    size = resolved.stat().st_size
    if size > ATTACHMENT_MAX_BYTES:
        raise AdapterError("attachment_too_large",
                           "attachment %s is %d bytes; the cap is %d bytes"
                           % (resolved.name, size, ATTACHMENT_MAX_BYTES))
    digest = hashlib.sha256()
    with open(str(resolved), "rb") as handle:
        while True:
            chunk = handle.read(65536)
            if not chunk:
                break
            digest.update(chunk)
    return {"path": str(resolved), "filename": resolved.name,
            "size_bytes": size, "sha256": digest.hexdigest()}


def _nonce(payload: Dict[str, Any]) -> str:
    value = payload.get("client_nonce")
    if value is None:
        return secrets.token_hex(8)
    if not isinstance(value, str) or not _NONCE_RE.match(value):
        raise ValueError("client_nonce must be 8-64 chars of A-Za-z0-9_-")
    return value


def _chat_key(kind: str, value: str) -> str:
    return hashlib.sha256((kind + "\x00" + value).encode("utf-8")).hexdigest()


def _sent_record_path(kind: str, value: str, nonce: str) -> Path:
    return _state_dir() / "sent" / _chat_key(kind, value) / (nonce + ".json")


def _draft_record_path(nonce: str) -> Path:
    return _state_dir() / "drafts" / (nonce + ".json")


# ---------------------------------------------------------------- AppleScript


def _as_quote(value: str) -> str:
    if "\x00" in value:
        raise ValueError("string must not contain NUL characters")
    if "\r" in value or "\n" in value:
        raise ValueError("string must be a single line for this slot")
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _as_text(value: str) -> str:
    if "\x00" in value:
        raise ValueError("body must not contain NUL characters")
    normalized = value.replace("\r\n", "\n").replace("\r", "\n")
    parts = normalized.split("\n")
    quoted = ['"' + p.replace("\\", "\\\\").replace('"', '\\"') + '"'
              for p in parts]
    if len(quoted) == 1:
        return quoted[0]
    return "(" + " & linefeed & ".join(quoted) + ")"


def _as_posix(path: str) -> str:
    return '(POSIX file %s)' % _as_quote(path)


def _resolve_lines(kind: str, value: str) -> List[str]:
    """Fixed receiver-resolution stanza ending with `send` to `targetDest`."""
    if kind == "chat":
        return ["set targetDest to text chat id %s" % _as_quote(value)]
    return [
        "set targetBuddy to missing value",
        "repeat with svc in every service",
        "try",
        "set targetBuddy to (first buddy of svc whose handle is %s)"
        % _as_quote(value),
        "exit repeat",
        "end try",
        "end repeat",
        'if targetBuddy is missing value then error "no such participant: %s"'
        % value.replace("\\", "\\\\").replace('"', '\\"'),
        "set targetDest to targetBuddy",
    ]


def _send_script(kind: str, value: str, body: str,
                 attachment: Optional[Dict[str, Any]]) -> str:
    if not body and attachment is None:
        raise ValueError("body and attachment cannot both be empty")
    lines = ['tell application "Messages"']
    lines.extend(_resolve_lines(kind, value))
    if attachment is not None:
        lines.append("send %s to targetDest" % _as_posix(attachment["path"]))
    if body:
        lines.append("send %s to targetDest" % _as_text(body))
    lines.append('end tell')
    return "\n".join(lines)


def _count_script(kind: str, value: str) -> str:
    """Fixed readback: message count, then the last message text."""
    lines = ['tell application "Messages"']
    lines.extend(_resolve_lines(kind, value))
    lines.append("set msgCount to count of messages of targetDest")
    lines.append("if msgCount is 0 then return \"0\"")
    lines.append("set lastText to text of last message of targetDest")
    lines.append("return (msgCount as string) & linefeed & lastText")
    lines.append("end tell")
    return "\n".join(lines)


def _run_osascript(script: str) -> Tuple[bool, str, str]:
    if not _is_darwin():
        raise AdapterError("unsupported_platform",
                           "Apple Messages control requires macOS")
    if not os.path.exists(OSASCRIPT):
        raise AdapterError("osascript_missing",
                           OSASCRIPT + " is not available")
    try:
        completed = subprocess.run(
            [OSASCRIPT, "-e", script],
            capture_output=True, text=True, timeout=OSASCRIPT_TIMEOUT_S,
            check=False)
    except subprocess.TimeoutExpired:
        raise AdapterError("osascript_timeout",
                           "Messages did not answer within %d seconds"
                           % int(OSASCRIPT_TIMEOUT_S))
    except OSError as exc:
        raise AdapterError("osascript_failed",
                           "could not launch osascript: %s" % exc)
    if completed.returncode != 0:
        blob = (completed.stderr + "\n" + completed.stdout).lower()
        if ("not authorized" in blob or "not authorised" in blob
                or "-1743" in blob or "denied" in blob
                or "operation not permitted" in blob):
            raise AdapterError("automation_permission_required", AUTOMATION_HINT)
        raise AdapterError("applescript_failed",
                           (completed.stderr.strip()
                            or "osascript exited with status %d"
                            % completed.returncode)[:500])
    return True, completed.stdout, completed.stderr


def _parse_count(out: str) -> Tuple[int, str]:
    lines = out.splitlines()
    if not lines:
        raise AdapterError("applescript_failed",
                           "count readback returned no output")
    try:
        count = int(lines[0].strip().split()[-1])
    except (ValueError, IndexError):
        raise AdapterError("applescript_failed",
                           "count readback was not parseable")
    return count, "\n".join(lines[1:])


# ---------------------------------------------------------------- intents


def handle_draft(payload: Dict[str, Any]) -> Dict[str, Any]:
    _reject_script_keys(payload)
    allowed = {"intent", "recipient", "chat_guid", "body", "attachment",
               "client_nonce"}
    unknown = set(payload.keys()) - allowed
    if unknown:
        raise AdapterError("arbitrary_script_refused",
                           "unsupported keys (%s); only typed message fields "
                           "are accepted" % ", ".join(sorted(unknown)))
    kind, value = _target(payload)
    body = _req_body(payload)
    attachment = _opt_attachment(payload)
    if not body and attachment is None:
        raise ValueError("body and attachment cannot both be empty")
    nonce = _nonce(payload)
    record = {
        "adapter": ADAPTER_ID,
        "kind": kind,
        "target": value,
        "body": body,
        "body_sha256": hashlib.sha256(body.encode("utf-8")).hexdigest(),
        "attachment": ({k: attachment[k] for k in ("filename", "size_bytes",
                                                   "sha256")}
                       if attachment is not None else None),
        "client_nonce": nonce,
        "staged_at_ms": int(time.time() * 1000),
        "staged": True,
    }
    path = _draft_record_path(nonce)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(record, separators=(",", ":")),
                    encoding="utf-8")
    # Staged-draft verification is a file readback: the record must exist and
    # round-trip with the same body hash. Messages exposes no remote draft API.
    try:
        check = json.loads(path.read_text(encoding="utf-8"))
        verified = (isinstance(check, dict)
                    and check.get("body_sha256") == record["body_sha256"]
                    and check.get("client_nonce") == nonce)
    except (OSError, ValueError):
        verified = False
    out = dict(record)
    out["draft_path"] = str(path)
    out["verified"] = verified
    out["verification"] = ("messages_draft_staged" if verified
                           else "messages_draft_unverified")
    out["note"] = ("Messages has no remote draft API; sending happens only "
                   "on message.send")
    return out


def handle_send(payload: Dict[str, Any]) -> Dict[str, Any]:
    _reject_script_keys(payload)
    allowed = {"intent", "recipient", "chat_guid", "body", "attachment",
               "client_nonce"}
    unknown = set(payload.keys()) - allowed
    if unknown:
        raise AdapterError("arbitrary_script_refused",
                           "unsupported keys (%s); only typed message fields "
                           "are accepted" % ", ".join(sorted(unknown)))
    kind, value = _target(payload)
    body = _req_body(payload)
    attachment = _opt_attachment(payload)
    if not body and attachment is None:
        raise ValueError("body and attachment cannot both be empty")
    nonce = _nonce(payload)
    record_path = _sent_record_path(kind, value, nonce)
    if record_path.is_file():
        try:
            prior = json.loads(record_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            prior = {}
        raise AdapterError("duplicate_send_refused",
                           "client_nonce %s was already sent on this chat "
                           "(first sent at %s)" % (nonce, prior.get("sent_at_ms", "?")))
    # Bind the chat first: pre-send count and last-message text.
    count_before = -1
    try:
        _, out, _ = _run_osascript(_count_script(kind, value))
        count_before, _ = _parse_count(out)
    except AdapterError as exc:
        if exc.code in ("automation_permission_required",
                        "unsupported_platform", "osascript_missing"):
            raise
        count_before = -1
    _run_osascript(_send_script(kind, value, body, attachment))
    expected_delta = (1 if body else 0) + (1 if attachment is not None else 0)
    # Post-send readback: count increment plus last-message match.
    verified = False
    count_after = -1
    last_text = ""
    try:
        _, out, _ = _run_osascript(_count_script(kind, value))
        count_after, last_text = _parse_count(out)
        count_ok = (count_before >= 0
                    and count_after == count_before + expected_delta)
        text_ok = (not body) or (last_text.strip() == body.strip())
        verified = bool(count_ok and text_ok)
    except AdapterError as exc:
        if exc.code == "automation_permission_required":
            raise
        verified = False
    record = {
        "adapter": ADAPTER_ID,
        "kind": kind,
        "target": value,
        "body_sha256": hashlib.sha256(body.encode("utf-8")).hexdigest(),
        "attachment": ({k: attachment[k] for k in ("filename", "size_bytes",
                                                   "sha256")}
                       if attachment is not None else None),
        "client_nonce": nonce,
        "sent_at_ms": int(time.time() * 1000),
        "count_before": count_before,
        "count_after": count_after,
        "verified": verified,
    }
    record_path.parent.mkdir(parents=True, exist_ok=True)
    record_path.write_text(json.dumps(record, separators=(",", ":")),
                           encoding="utf-8")
    body_out: Dict[str, Any] = {
        "target_kind": kind,
        "client_nonce": nonce,
        "count_before": count_before,
        "count_after": count_after,
        "last_message_match": (last_text.strip() == body.strip()) if body else None,
        "verified": verified,
        "verification": ("messages_chat_readback" if verified
                         else "messages_delivery_only_unverified"),
    }
    if not verified:
        body_out["reason"] = ("the message was handed to Messages but the "
                              "count/last-message readback did not confirm it; "
                              "do not treat delivery as completion")
    return body_out


# ---------------------------------------------------------------- dispatch


_DISPATCH = {
    "message.draft": handle_draft,
    "message.send": handle_send,
}


def handler(request: Dict[str, Any]) -> Dict[str, Any]:
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available",
                        {"adapter": ADAPTER_ID,
                         "protocol": "apple-messages-fixed-applescript",
                         "platform": sys.platform,
                         "supported": _is_darwin()})
    if method == "capabilities":
        return response(request, True, "available",
                        {"backend": "messages_applescript_closed_templates",
                         "intents": list(INTENTS),
                         "supported": _is_darwin()})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return response(request, False, "degraded",
                        error={"code": "invalid_payload",
                               "message": "payload must be an object"})
    if not _is_darwin():
        return response(request, False, "unsupported",
                        error={"code": "unsupported_platform",
                               "message": "Apple Messages control requires macOS"})
    intent = payload.get("intent")
    func = _DISPATCH.get(intent)  # type: ignore[arg-type]
    if func is None:
        return response(request, False, "unsupported",
                        error={"code": "unsupported_intent",
                               "message": str(intent)})
    try:
        return response(request, True, "available", func(payload))
    except AdapterError as exc:
        health = "unsupported" if exc.code in ("unsupported_platform",) else "degraded"
        return response(request, False, health,
                        error={"code": exc.code, "message": exc.detail})
    except (ValueError, TypeError, KeyError) as exc:
        return response(request, False, "degraded",
                        error={"code": "invalid_payload", "message": str(exc)})
    except Exception as exc:
        return response(request, False, "degraded",
                        error={"code": "adapter_error", "message": str(exc)})


if __name__ == "__main__":
    serve(handler)
