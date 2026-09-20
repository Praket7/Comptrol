#!/usr/bin/env python3
"""Apple Mail adapter: drafts, send, search, and read via fixed AppleScript.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

There is no HTTP surface here. Every Mail action runs through a FIXED
AppleScript template invoked as subprocess argv ["/usr/bin/osascript", "-e",
script] (never shell=True). The model NEVER supplies script source: payload
fields are data only and are quoted/escaped into closed slots (account name,
recipient, subject, body, attachment path). Any payload carrying script-source
keys (script, applescript, source, command, osascript) is refused outright,
and body text is embedded as quoted segments joined with `& linefeed &` so it
cannot break out of its string literal.

Attachments must resolve under COMPTROL_MAIL_ASSETS_ROOT (absolute directory),
must already exist, are capped at 10 MB each, and are reported with size_bytes
plus a sha256 digest.

Verification: mail.send is followed by a sent-mailbox readback counting
messages with the same subject; a match reports verified=true with
apple_mail_sent_readback, anything else reports verified=false with
apple_mail_delivery_only_unverified. osascript delivery alone is never proof.

This module imports safely on any platform (subprocess is only invoked per
request); intent calls outside macOS return unsupported.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.apple-mail"
ASSETS_ROOT_ENV = "COMPTROL_MAIL_ASSETS_ROOT"
OSASCRIPT = "/usr/bin/osascript"

OSASCRIPT_TIMEOUT_S = 30.0
ATTACHMENT_MAX_BYTES = 10 * 1024 * 1024
MAX_ATTACHMENTS = 10
MAX_RECIPIENTS = 50
MAX_SUBJECT_CHARS = 500
MAX_BODY_CHARS = 500000
MAX_RESULTS = 25
READ_EXCERPT_CHARS = 4000
HEADER_EXCERPT_CHARS = 500

INTENTS = (
    "mail.draft",
    "mail.send",
    "mail.search",
    "mail.read",
)

_EMAIL_RE = re.compile(r"^[^@\s\x00-\x1f]{1,64}@[^@\s\x00-\x1f]{1,253}$")
_ACCOUNT_RE = re.compile(r"^[A-Za-z0-9 ._\-@]{1,64}$")
_MAILBOX_RE = re.compile(r"^[A-Za-z0-9 ._\-/]{1,64}$")

# Payload keys that would carry script source. Their presence is always a
# refusal: the model supplies typed data fields, never script text.
SCRIPT_KEYS = frozenset({
    "script", "applescript", "apple_script", "source", "command",
    "osascript", "shell", "do_shell_script",
})

AUTOMATION_HINT = ("macOS blocked AppleScript control of Mail. Grant Automation "
                   "permission: System Settings > Privacy & Security > Automation, "
                   "allow the comptrol host process to control Mail, then retry.")


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


def _req_int(payload: Dict[str, Any], key: str, minimum: int,
             maximum: int, default: Optional[int] = None) -> int:
    if key not in payload:
        if default is None:
            raise ValueError(key + " is required")
        return default
    value = payload.get(key)
    if (isinstance(value, bool) or not isinstance(value, int)
            or not minimum <= value <= maximum):
        raise ValueError(key + " must be an integer in [%d, %d]"
                         % (minimum, maximum))
    return value


def _check_email(value: Any, key: str) -> str:
    if not isinstance(value, str) or not _EMAIL_RE.match(value) \
            or ".." in value:
        raise ValueError(key + " must be a valid email address")
    return value


def _req_addresses(payload: Dict[str, Any], key: str,
                   required: bool) -> List[str]:
    value = payload.get(key)
    if value is None:
        if required:
            raise ValueError(key + " is required")
        return []
    if not isinstance(value, list) or (required and not value) \
            or len(value) > MAX_RECIPIENTS:
        raise ValueError(key + " must be a list of 1-%d email addresses"
                         % MAX_RECIPIENTS)
    return [_check_email(v, key) for v in value]


def _opt_account(payload: Dict[str, Any]) -> Optional[str]:
    value = payload.get("account")
    if value is None:
        return None
    if not isinstance(value, str) or not _ACCOUNT_RE.match(value):
        raise ValueError("account must be 1-64 chars of letters, digits, "
                         "space, . _ - @")
    return value


def _req_attachments(payload: Dict[str, Any]) -> List[Dict[str, Any]]:
    value = payload.get("attachments", [])
    if not isinstance(value, list) or len(value) > MAX_ATTACHMENTS:
        raise ValueError("attachments must be a list of at most %d entries"
                         % MAX_ATTACHMENTS)
    root = _assets_root()
    out = []
    for entry in value:
        if not isinstance(entry, dict):
            raise ValueError("each attachment must be an object")
        raw_path = entry.get("path")
        if not isinstance(raw_path, str) or not raw_path:
            raise ValueError("each attachment needs a path string")
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
        out.append({"path": str(resolved), "filename": resolved.name,
                    "size_bytes": size, "sha256": digest.hexdigest()})
    return out


# ---------------------------------------------------------------- AppleScript


def _as_quote(value: str) -> str:
    """Quote one single-line AppleScript string literal."""
    if "\x00" in value:
        raise ValueError("string must not contain NUL characters")
    if "\r" in value or "\n" in value:
        raise ValueError("string must be a single line for this slot")
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _as_text(value: str) -> str:
    """Embed multi-line body text as quoted segments joined with linefeed."""
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


def _recipient_lines(addrs: List[str], kind: str) -> List[str]:
    lines = []
    for addr in addrs:
        lines.append(
            "make new %s recipient at end of %s recipients with properties "
            "{address:%s}" % (kind, kind, _as_quote(addr)))
    return lines


def _compose_script(account: Optional[str], to_addrs: List[str],
                    cc: List[str], bcc: List[str], subject: str, body: str,
                    attachments: List[Dict[str, Any]],
                    action: str) -> str:
    """Assemble the fixed outgoing-message template. `action` is save|send."""
    lines = ['tell application "Mail"']
    lines.append("set newMsg to make new outgoing message with properties "
                 "{subject:%s, content:%s, visible:false}"
                 % (_as_quote(subject), _as_text(body)))
    lines.append("tell newMsg")
    lines.extend(_recipient_lines(to_addrs, "to"))
    lines.extend(_recipient_lines(cc, "cc"))
    lines.extend(_recipient_lines(bcc, "bcc"))
    for att in attachments:
        lines.append("make new attachment with properties {file name:%s} "
                     "at after last paragraph" % _as_posix(att["path"]))
    lines.append("end tell")
    if account is not None:
        lines.append("set sender of newMsg to %s" % _as_quote(account))
    if action == "send":
        lines.append("send newMsg")
    else:
        lines.append("save newMsg")
    lines.append("return (subject of newMsg)")
    lines.append('end tell')
    return "\n".join(lines)


def _drafts_count_script(subject: str) -> str:
    return "\n".join([
        'tell application "Mail"',
        "return (count of (every message of drafts mailbox whose subject is %s))"
        % _as_quote(subject),
        "end tell",
    ])


def _sent_count_script(subject: str) -> str:
    return "\n".join([
        'tell application "Mail"',
        "return (count of (every message of sent mailbox whose subject is %s))"
        % _as_quote(subject),
        "end tell",
    ])


def _search_script(mailbox: str, field: str, needle: str,
                   max_results: int) -> str:
    if field == "subject":
        matcher = "whose subject contains %s" % _as_quote(needle)
    else:
        matcher = "whose sender contains %s" % _as_quote(needle)
    return "\n".join([
        'tell application "Mail"',
        "set foundMsgs to (every message of mailbox %s %s)"
        % (_as_quote(mailbox), matcher),
        "set out to \"\"",
        "set n to 0",
        "repeat with m in foundMsgs",
        "set n to n + 1",
        "if n > %d then exit repeat" % max_results,
        "set out to out & (subject of m) & linefeed & (sender of m) "
        "& linefeed & \"---MSG---\" & linefeed",
        "end repeat",
        "return out",
        "end tell",
    ])


def _read_script(mailbox: str, subject: str) -> str:
    return "\n".join([
        'tell application "Mail"',
        "set m to first message of mailbox %s whose subject is %s"
        % (_as_quote(mailbox), _as_quote(subject)),
        "return (subject of m) & linefeed & (sender of m) & linefeed "
        "& (content of m)",
        "end tell",
    ])


def _run_osascript(script: str) -> Tuple[bool, str, str]:
    """Run one fixed template. Returns (ok, stdout, stderr)."""
    if not _is_darwin():
        raise AdapterError("unsupported_platform",
                           "Apple Mail control requires macOS")
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
                           "Mail did not answer within %d seconds"
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


# ---------------------------------------------------------------- intents


def _common_compose(payload: Dict[str, Any]
                    ) -> Tuple[Optional[str], List[str], List[str], List[str],
                               str, str, List[Dict[str, Any]]]:
    _reject_script_keys(payload)
    allowed = {"intent", "account", "to", "cc", "bcc", "subject", "body",
               "attachments"}
    unknown = set(payload.keys()) - allowed
    if unknown:
        raise AdapterError("arbitrary_script_refused",
                           "unsupported keys (%s); only typed mail fields "
                           "are accepted" % ", ".join(sorted(unknown)))
    account = _opt_account(payload)
    to_addrs = _req_addresses(payload, "to", True)
    cc = _req_addresses(payload, "cc", False)
    bcc = _req_addresses(payload, "bcc", False)
    subject = _req_str(payload, "subject", MAX_SUBJECT_CHARS)
    if "\r" in subject or "\n" in subject:
        raise ValueError("subject must be a single line")
    body = _req_str(payload, "body", MAX_BODY_CHARS, allow_empty=True)
    attachments = _req_attachments(payload)
    return account, to_addrs, cc, bcc, subject, body, attachments


def handle_draft(payload: Dict[str, Any]) -> Dict[str, Any]:
    account, to_addrs, cc, bcc, subject, body, attachments = \
        _common_compose(payload)
    script = _compose_script(account, to_addrs, cc, bcc, subject, body,
                             attachments, "save")
    _run_osascript(script)
    # Draft verification is a drafts-mailbox readback: a message with the same
    # subject must be observable. osascript success alone is not proof.
    _, out, _ = _run_osascript(_drafts_count_script(subject))
    try:
        count = int(out.strip().split()[-1])
    except (ValueError, IndexError):
        count = 0
    verified = count >= 1
    return {
        "to": to_addrs,
        "cc": cc,
        "bcc": bcc,
        "subject": subject,
        "account": account or "",
        "attachments": [{k: a[k] for k in ("filename", "size_bytes", "sha256")}
                        for a in attachments],
        "drafts_with_subject": count,
        "verified": verified,
        "verification": "apple_mail_drafts_readback" if verified
        else "apple_mail_delivery_only_unverified",
    }


def handle_send(payload: Dict[str, Any]) -> Dict[str, Any]:
    account, to_addrs, cc, bcc, subject, body, attachments = \
        _common_compose(payload)
    script = _compose_script(account, to_addrs, cc, bcc, subject, body,
                             attachments, "send")
    _run_osascript(script)
    # Send verification is a sent-mailbox readback where Mail exposes it.
    # Delivery without a readback match stays honestly unverified.
    verified = False
    sent_count = -1
    try:
        _, out, _ = _run_osascript(_sent_count_script(subject))
        sent_count = int(out.strip().split()[-1])
        verified = sent_count >= 1
    except AdapterError as exc:
        if exc.code == "automation_permission_required":
            raise
        sent_count = -1
        verified = False
    except (ValueError, IndexError):
        sent_count = -1
        verified = False
    body_out: Dict[str, Any] = {
        "to": to_addrs,
        "subject": subject,
        "account": account or "",
        "sent_with_subject": sent_count,
        "verified": verified,
        "verification": "apple_mail_sent_readback" if verified
        else "apple_mail_delivery_only_unverified",
    }
    if not verified:
        body_out["reason"] = ("the message was handed to Mail but no matching "
                              "sent-mailbox entry was observed; the sent mailbox "
                              "name varies by provider and may not be scriptable")
    return body_out


def handle_search(payload: Dict[str, Any]) -> Dict[str, Any]:
    _reject_script_keys(payload)
    allowed = {"intent", "mailbox", "field", "query", "max_results"}
    unknown = set(payload.keys()) - allowed
    if unknown:
        raise AdapterError("arbitrary_script_refused",
                           "unsupported keys (%s); only typed search fields "
                           "are accepted" % ", ".join(sorted(unknown)))
    mailbox = payload.get("mailbox", "INBOX")
    if not isinstance(mailbox, str) or not _MAILBOX_RE.match(mailbox):
        raise ValueError("mailbox must be 1-64 chars of letters, digits, "
                         "space, . _ - /")
    field = payload.get("field", "subject")
    if field not in ("subject", "sender"):
        raise ValueError("field must be subject or sender")
    query = _req_str(payload, "query", 500)
    max_results = _req_int(payload, "max_results", 1, MAX_RESULTS, 10)
    _, out, _ = _run_osascript(_search_script(mailbox, field, query,
                                              max_results))
    messages = []
    for chunk in out.split("---MSG---"):
        lines = chunk.strip().splitlines()
        if len(lines) >= 2:
            messages.append({"subject": lines[0][:HEADER_EXCERPT_CHARS],
                             "sender": lines[1][:HEADER_EXCERPT_CHARS]})
        if len(messages) >= max_results:
            break
    return {
        "mailbox": mailbox,
        "field": field,
        "query": query,
        "messages": messages,
        "verified": True,
        "verification": "apple_mail_search_readback",
    }


def handle_read(payload: Dict[str, Any]) -> Dict[str, Any]:
    _reject_script_keys(payload)
    allowed = {"intent", "mailbox", "subject"}
    unknown = set(payload.keys()) - allowed
    if unknown:
        raise AdapterError("arbitrary_script_refused",
                           "unsupported keys (%s); only typed read fields "
                           "are accepted" % ", ".join(sorted(unknown)))
    mailbox = payload.get("mailbox", "INBOX")
    if not isinstance(mailbox, str) or not _MAILBOX_RE.match(mailbox):
        raise ValueError("mailbox must be 1-64 chars of letters, digits, "
                         "space, . _ - /")
    subject = _req_str(payload, "subject", MAX_SUBJECT_CHARS)
    if "\r" in subject or "\n" in subject:
        raise ValueError("subject must be a single line")
    try:
        _, out, _ = _run_osascript(_read_script(mailbox, subject))
    except AdapterError as exc:
        if exc.code == "applescript_failed":
            raise AdapterError("not_found",
                               "no message with that subject in " + mailbox)
        raise
    lines = out.splitlines()
    found_subject = lines[0] if lines else ""
    sender = lines[1] if len(lines) > 1 else ""
    content = "\n".join(lines[2:]) if len(lines) > 2 else ""
    return {
        "mailbox": mailbox,
        "subject": found_subject[:HEADER_EXCERPT_CHARS],
        "sender": sender[:HEADER_EXCERPT_CHARS],
        "body_excerpt": content[:READ_EXCERPT_CHARS],
        "body_truncated": len(content) > READ_EXCERPT_CHARS,
        "verified": True,
        "verification": "apple_mail_message_readback",
    }


# ---------------------------------------------------------------- dispatch


_DISPATCH = {
    "mail.draft": handle_draft,
    "mail.send": handle_send,
    "mail.search": handle_search,
    "mail.read": handle_read,
}


def handler(request: Dict[str, Any]) -> Dict[str, Any]:
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available",
                        {"adapter": ADAPTER_ID,
                         "protocol": "apple-mail-fixed-applescript",
                         "platform": sys.platform,
                         "supported": _is_darwin()})
    if method == "capabilities":
        return response(request, True, "available",
                        {"backend": "mail_applescript_closed_templates",
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
                               "message": "Apple Mail control requires macOS"})
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
