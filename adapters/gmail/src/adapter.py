#!/usr/bin/env python3
"""Gmail adapter: drafts, send, search, and read over the Gmail REST API.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

Upstream is the official Gmail REST surface only (users.drafts.create,
users.drafts.send, users.messages.send, users.messages.list,
users.messages.get), using the Python standard library (urllib). There is no
browser automation and no cookie handling anywhere in this adapter.

Auth: the user OAuth bearer token is taken ONLY from the
COMPTROL_GMAIL_ACCESS_TOKEN environment variable at request time. It is never
stored on disk, never logged, and never copied into responses, errors, or
audit payloads. Upstream error text is redacted before it leaves the adapter.
COMPTROL_GMAIL_API_BASE may override the API base so verification can run
against a loopback stub; production defaults to the official endpoint.

Attachments: each entry supplies a local file path that must already exist.
Files are capped at 10 MB each (refused, never truncated), base64-encoded
into the RFC 822 MIME body, and reported back with size_bytes plus a sha256
digest. File bytes never travel through the frame.

Send safety: an HTTP 200 from users.messages.send or users.drafts.send only
proves acceptance. verified=true additionally requires a Sent-folder readback
(users.messages.get on the returned id) whose thread id matches and whose
labelIds include SENT. Anything less returns verified=false with an explicit
gmail_accepted_unverified verification marker.
"""

import base64
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.gmail"
TOKEN_ENV = "COMPTROL_GMAIL_ACCESS_TOKEN"
API_BASE_ENV = "COMPTROL_GMAIL_API_BASE"

DEFAULT_API_BASE = "https://gmail.googleapis.com"

HTTP_TIMEOUT_S = 20.0
MAX_JSON_BYTES = 8 * 1024 * 1024
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
_MSG_ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,256}$")


class UpstreamError(RuntimeError):
    """Upstream or credential failure with a safe, token-free detail string."""

    def __init__(self, code: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code
        self.detail = detail


def _redact(text: str, token: str) -> str:
    if token and token in text:
        return text.replace(token, "[redacted]")
    return text


def _token() -> str:
    token = os.environ.get(TOKEN_ENV, "")
    if not isinstance(token, str) or not token.strip():
        raise UpstreamError("auth_missing", TOKEN_ENV + " is not set")
    return token


def _api_base() -> str:
    return (os.environ.get(API_BASE_ENV, DEFAULT_API_BASE).rstrip("/")
            or DEFAULT_API_BASE)


def _from_http_error(exc: urllib.error.HTTPError, token: str) -> UpstreamError:
    try:
        raw = exc.read(4096)
    except Exception:
        raw = b""
    snippet = _redact(raw.decode("utf-8", errors="replace"), token)
    message = snippet
    try:
        parsed = json.loads(snippet)
        err = parsed.get("error") if isinstance(parsed, dict) else None
        if isinstance(err, dict) and isinstance(err.get("message"), str) and err["message"]:
            message = err["message"][:500]
    except ValueError:
        pass
    status = exc.code
    if status in (401, 403):
        return UpstreamError("auth_failed",
                             "Gmail API rejected the credential (HTTP %d): %s"
                             % (status, message[:300]))
    if status == 404:
        return UpstreamError("not_found", "Gmail resource not found (HTTP 404)")
    if status == 400:
        return UpstreamError("invalid_request",
                             "Gmail API rejected the request (HTTP 400): %s"
                             % message[:300])
    if status == 429:
        return UpstreamError("rate_limited",
                             "Gmail API rate limit hit (HTTP 429); retry later")
    return UpstreamError("upstream_error",
                         "Gmail API HTTP %d: %s" % (status, message[:300]))


def _http_json(method: str, url: str, token: str,
               body: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    data = None
    headers = {"Accept": "application/json"}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json; charset=utf-8"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
            raw = resp.read(MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise _from_http_error(exc, token)
    except urllib.error.URLError as exc:
        raise UpstreamError("upstream_unreachable",
                            "Gmail API unreachable: %s"
                            % _redact(str(exc.reason), token)[:200])
    if len(raw) > MAX_JSON_BYTES:
        raise UpstreamError("response_too_large",
                            "upstream JSON exceeded the bounded read cap")
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise UpstreamError("upstream_error",
                            "upstream returned non-JSON content")
    if not isinstance(parsed, dict):
        raise UpstreamError("upstream_error",
                            "upstream returned an unexpected JSON shape")
    return parsed


# ---------------------------------------------------------------- validators


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


def _check_email(value: str, key: str) -> str:
    if not _EMAIL_RE.match(value) or ".." in value:
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
    return [_check_email(v, key) if isinstance(v, str)
            else (_ for _ in ()).throw(ValueError(key + " entries must be strings"))
            for v in value]


def _req_attachments(payload: Dict[str, Any]) -> List[Dict[str, Any]]:
    value = payload.get("attachments", [])
    if not isinstance(value, list) or len(value) > MAX_ATTACHMENTS:
        raise ValueError("attachments must be a list of at most %d entries"
                         % MAX_ATTACHMENTS)
    out = []
    for entry in value:
        if not isinstance(entry, dict):
            raise ValueError("each attachment must be an object")
        raw_path = entry.get("path")
        if not isinstance(raw_path, str) or not raw_path:
            raise ValueError("each attachment needs a path string")
        path = Path(raw_path)
        if not path.is_file():
            raise UpstreamError("attachment_missing",
                                "attachment file does not exist: " + path.name)
        size = path.stat().st_size
        if size > ATTACHMENT_MAX_BYTES:
            raise UpstreamError("attachment_too_large",
                                "attachment %s is %d bytes; the cap is %d bytes"
                                % (path.name, size, ATTACHMENT_MAX_BYTES))
        mime_type = entry.get("mime_type", "application/octet-stream")
        if not isinstance(mime_type, str) or not mime_type \
                or len(mime_type) > 127 or "/" not in mime_type:
            raise ValueError("attachment mime_type must look like type/subtype")
        filename = entry.get("filename") or path.name
        if not isinstance(filename, str) or not filename \
                or len(filename) > 255 or "/" in filename \
                or "\\" in filename or "\x00" in filename:
            raise ValueError("attachment filename must be a bare file name")
        digest = hashlib.sha256()
        with open(str(path), "rb") as handle:
            while True:
                chunk = handle.read(65536)
                if not chunk:
                    break
                digest.update(chunk)
        out.append({"path": str(path), "filename": filename,
                    "mime_type": mime_type, "size_bytes": size,
                    "sha256": digest.hexdigest()})
    return out


# ---------------------------------------------------------------- MIME


def _rfc2047(text: str) -> str:
    try:
        text.encode("ascii")
        return text.replace("\r", "").replace("\n", "")
    except UnicodeEncodeError:
        encoded = base64.b64encode(text.encode("utf-8")).decode("ascii")
        return "=?utf-8?b?%s?=" % encoded


def _build_raw(to_addrs: List[str], cc: List[str], bcc: List[str],
               subject: str, body: str,
               attachments: List[Dict[str, Any]]) -> bytes:
    boundary = "comptrol-%s" % hashlib.sha256(
        os.urandom(16)).hexdigest()[:24]
    lines = ["To: " + ", ".join(to_addrs),
             "Subject: " + _rfc2047(subject),
             "MIME-Version: 1.0"]
    if cc:
        lines.append("Cc: " + ", ".join(cc))
    if bcc:
        lines.append("Bcc: " + ", ".join(bcc))
    if not attachments:
        lines += ["Content-Type: text/plain; charset=utf-8",
                  "Content-Transfer-Encoding: 8bit", "",
                  body.replace("\r\n", "\n").replace("\r", "\n")]
        return "\r\n".join(lines).encode("utf-8")
    lines.append('Content-Type: multipart/mixed; boundary="%s"' % boundary)
    lines.append("")
    lines.append("--" + boundary)
    lines += ["Content-Type: text/plain; charset=utf-8",
              "Content-Transfer-Encoding: 8bit", "",
              body.replace("\r\n", "\n").replace("\r", "\n")]
    for att in attachments:
        with open(att["path"], "rb") as handle:
            blob = handle.read()
        encoded = base64.b64encode(blob).decode("ascii")
        lines.append("--" + boundary)
        lines.append("Content-Type: %s; name=%s"
                     % (att["mime_type"], _rfc2047(att["filename"])))
        lines.append("Content-Disposition: attachment; filename=%s"
                     % _rfc2047(att["filename"]))
        lines.append("Content-Transfer-Encoding: base64")
        lines.append("")
        for i in range(0, len(encoded), 76):
            lines.append(encoded[i:i + 76])
    lines.append("--" + boundary + "--")
    lines.append("")
    return "\r\n".join(lines).encode("utf-8")


def _b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii")


def _parse_addresses(payload: Dict[str, Any]
                     ) -> Tuple[List[str], List[str], List[str]]:
    to_addrs = _req_addresses(payload, "to", True)
    cc = _req_addresses(payload, "cc", False)
    bcc = _req_addresses(payload, "bcc", False)
    return to_addrs, cc, bcc


def _common_compose(payload: Dict[str, Any]
                    ) -> Tuple[List[str], List[str], List[str], str, str,
                                 List[Dict[str, Any]]]:
    to_addrs, cc, bcc = _parse_addresses(payload)
    subject = _req_str(payload, "subject", MAX_SUBJECT_CHARS)
    if "\r" in subject or "\n" in subject:
        raise ValueError("subject must be a single line")
    body = _req_str(payload, "body", MAX_BODY_CHARS, allow_empty=True)
    attachments = _req_attachments(payload)
    return to_addrs, cc, bcc, subject, body, attachments


# ---------------------------------------------------------------- Gmail API


def _msg_url(suffix: str) -> str:
    return _api_base() + "/gmail/v1/users/me" + suffix


def _sent_readback(message_id: str, expected_thread: Optional[str],
                   token: str) -> Tuple[bool, Dict[str, Any]]:
    """Re-read the message and require SENT membership plus thread match."""
    url = (_msg_url("/messages/" + urllib.parse.quote(message_id, safe=""))
           + "?format=metadata&metadataHeaders="
           + urllib.parse.quote("Subject", safe=""))
    try:
        full = _http_json("GET", url, token)
    except UpstreamError:
        return False, {}
    label_ids = full.get("labelIds", [])
    thread_id = full.get("threadId", "")
    sent = isinstance(label_ids, list) and "SENT" in label_ids
    thread_ok = expected_thread is None or thread_id == expected_thread
    return bool(sent and thread_ok and isinstance(message_id, str)
                and full.get("id") == message_id), {
                    "label_ids": label_ids if isinstance(label_ids, list) else [],
                    "thread_id": thread_id if isinstance(thread_id, str) else "",
                }


def handle_draft(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    to_addrs, cc, bcc, subject, body, attachments = _common_compose(payload)
    raw = _build_raw(to_addrs, cc, bcc, subject, body, attachments)
    created = _http_json("POST", _msg_url("/drafts"), token,
                         {"message": {"raw": _b64url(raw)}})
    draft_id = created.get("id")
    if not isinstance(draft_id, str) or not draft_id:
        raise UpstreamError("upstream_error",
                            "drafts.create returned no draft id")
    message = created.get("message", {}) if isinstance(
        created.get("message"), dict) else {}
    # Draft verification is a drafts.get readback: the draft must exist and
    # reference the created message id. A bare create response is not proof.
    verify_url = (_msg_url("/drafts/" + urllib.parse.quote(draft_id, safe=""))
                  + "?format=metadata")
    verified = False
    readback_id = ""
    try:
        check = _http_json("GET", verify_url, token)
        check_msg = check.get("message", {}) if isinstance(
            check.get("message"), dict) else {}
        readback_id = check_msg.get("id", "") if isinstance(
            check_msg.get("id"), str) else ""
        verified = (check.get("id") == draft_id
                    and (not message.get("id")
                         or readback_id == message.get("id")))
    except UpstreamError:
        verified = False
    return {
        "draft_id": draft_id,
        "message_id": message.get("id", "") if isinstance(
            message.get("id"), str) else "",
        "thread_id": message.get("threadId", "") if isinstance(
            message.get("threadId"), str) else "",
        "to": to_addrs,
        "subject": subject,
        "attachments": [{k: a[k] for k in ("filename", "mime_type",
                                           "size_bytes", "sha256")}
                        for a in attachments],
        "verified": verified,
        "verification": "gmail_draft_readback" if verified
        else "gmail_accepted_unverified",
    }


def handle_send(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    draft_id = payload.get("draft_id")
    expected_thread: Optional[str] = None
    if draft_id is not None:
        if not isinstance(draft_id, str) or not _MSG_ID_RE.match(draft_id):
            raise ValueError("draft_id must be a Gmail resource id")
        sent = _http_json("POST", _msg_url("/drafts/send"), token,
                          {"id": draft_id})
    else:
        to_addrs, cc, bcc, subject, body, attachments = _common_compose(payload)
        raw = _build_raw(to_addrs, cc, bcc, subject, body, attachments)
        sent = _http_json("POST", _msg_url("/messages/send"), token,
                          {"raw": _b64url(raw)})
    message_id = sent.get("id")
    if not isinstance(message_id, str) or not message_id:
        raise UpstreamError("upstream_error",
                            "send returned no message id")
    thread_id = sent.get("threadId", "")
    if isinstance(thread_id, str) and thread_id:
        expected_thread = thread_id
    verified, readback = _sent_readback(message_id, expected_thread, token)
    body_out: Dict[str, Any] = {
        "message_id": message_id,
        "thread_id": readback.get("thread_id", expected_thread or ""),
        "label_ids": readback.get("label_ids", []),
        "verified": verified,
        "verification": "gmail_sent_readback" if verified
        else "gmail_accepted_unverified",
    }
    if draft_id is not None:
        body_out["draft_id"] = draft_id
    if not verified:
        body_out["reason"] = ("send was accepted but the Sent-folder readback "
                              "did not confirm SENT membership; retry mail.read")
    return body_out


def _metadata_headers(message: Dict[str, Any]) -> Dict[str, str]:
    out = {"subject": "", "from": "", "date": ""}
    payload = message.get("payload", {})
    headers = payload.get("headers", []) if isinstance(payload, dict) else []
    if not isinstance(headers, list):
        return out
    for header in headers:
        if not isinstance(header, dict):
            continue
        name = str(header.get("name", "")).lower()
        if name in out:
            value = header.get("value", "")
            if isinstance(value, str):
                out[name] = value[:HEADER_EXCERPT_CHARS]
    return out


def handle_search(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    query = payload.get("q", "")
    if not isinstance(query, str) or len(query) > 1000:
        raise ValueError("q must be a string of length 0-1000")
    if "\x00" in query:
        raise ValueError("q must not contain NUL characters")
    max_results = _req_int(payload, "max_results", 1, MAX_RESULTS, 10)
    params = {"maxResults": str(max_results)}
    if query:
        params["q"] = query
    page_token = payload.get("page_token")
    if page_token is not None:
        if not isinstance(page_token, str) or not page_token \
                or len(page_token) > 1024:
            raise ValueError("page_token must be a non-empty string up to 1024 chars")
        params["pageToken"] = page_token
    listed = _http_json("GET", _msg_url("/messages?") + urllib.parse.urlencode(params),
                        token)
    entries = listed.get("messages", [])
    if not isinstance(entries, list):
        entries = []
    messages = []
    for entry in entries[:max_results]:
        if not isinstance(entry, dict) or not isinstance(entry.get("id"), str):
            continue
        mid = entry["id"]
        detail = _http_json(
            "GET", _msg_url("/messages/" + urllib.parse.quote(mid, safe=""))
            + "?format=metadata&metadataHeaders="
            + urllib.parse.quote("Subject,From,Date", safe=""), token)
        headers = _metadata_headers(detail)
        messages.append({
            "id": mid,
            "thread_id": detail.get("threadId", "") if isinstance(
                detail.get("threadId"), str) else "",
            "subject": headers["subject"],
            "from": headers["from"],
            "date": headers["date"],
            "snippet": detail.get("snippet", "")[:HEADER_EXCERPT_CHARS] if isinstance(
                detail.get("snippet"), str) else "",
        })
    return {
        "q": query,
        "messages": messages,
        "result_size_estimate": listed.get("resultSizeEstimate", len(messages)),
        "next_page_token": listed.get("nextPageToken", "") if isinstance(
            listed.get("nextPageToken"), str) else "",
        "verified": True,
        "verification": "gmail_search_readback",
    }


def _walk_parts(part: Any, texts: List[str], files: List[Dict[str, Any]],
                budget: List[int]) -> None:
    if budget[0] <= 0 or not isinstance(part, dict):
        return
    mime = part.get("mimeType", "")
    body = part.get("body", {}) if isinstance(part.get("body"), dict) else {}
    data = body.get("data", "") if isinstance(body, dict) else ""
    if isinstance(data, str) and data and mime.startswith("text/"):
        try:
            padded = data + "=" * (-len(data) % 4)
            text = base64.urlsafe_b64decode(padded).decode("utf-8", errors="replace")
        except (ValueError, UnicodeError):
            text = ""
        if text:
            chunk = text[:budget[0]]
            texts.append(chunk)
            budget[0] -= len(chunk)
    filename = part.get("filename", "")
    if isinstance(filename, str) and filename and isinstance(body, dict):
        files.append({
            "filename": filename[:255],
            "mime_type": mime if isinstance(mime, str) else "",
            "size_bytes": body.get("size", 0) if isinstance(
                body.get("size"), int) else 0,
            "attachment_id": body.get("attachmentId", "") if isinstance(
                body.get("attachmentId"), str) else "",
        })
    sub = part.get("parts", [])
    if isinstance(sub, list):
        for child in sub:
            _walk_parts(child, texts, files, budget)
            if budget[0] <= 0:
                return


def handle_read(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    message_id = payload.get("message_id")
    if not isinstance(message_id, str) or not _MSG_ID_RE.match(message_id):
        raise ValueError("message_id must be a Gmail message id")
    detail = _http_json(
        "GET", _msg_url("/messages/" + urllib.parse.quote(message_id, safe=""))
        + "?format=full", token)
    headers = _metadata_headers(detail)
    texts: List[str] = []
    files: List[Dict[str, Any]] = []
    _walk_parts(detail.get("payload", {}), texts, files,
                [READ_EXCERPT_CHARS])
    label_ids = detail.get("labelIds", [])
    return {
        "id": detail.get("id", ""),
        "thread_id": detail.get("threadId", "") if isinstance(
            detail.get("threadId"), str) else "",
        "label_ids": label_ids if isinstance(label_ids, list) else [],
        "subject": headers["subject"],
        "from": headers["from"],
        "date": headers["date"],
        "snippet": detail.get("snippet", "")[:HEADER_EXCERPT_CHARS] if isinstance(
            detail.get("snippet"), str) else "",
        "body_excerpt": "".join(texts)[:READ_EXCERPT_CHARS],
        "body_truncated": sum(len(t) for t in texts) >= READ_EXCERPT_CHARS,
        "attachments": files[:MAX_ATTACHMENTS],
        "verified": True,
        "verification": "gmail_message_readback",
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
                        {"adapter": ADAPTER_ID, "protocol": "gmail-https"})
    if method == "capabilities":
        return response(request, True, "available",
                        {"backend": "gmail-v1",
                         "intents": list(INTENTS)})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return response(request, False, "degraded",
                        error={"code": "invalid_payload",
                               "message": "payload must be an object"})
    intent = payload.get("intent")
    func = _DISPATCH.get(intent)  # type: ignore[arg-type]
    if func is None:
        return response(request, False, "unsupported",
                        error={"code": "unsupported_intent",
                               "message": str(intent)})
    token = ""
    try:
        token = _token()
        return response(request, True, "available", func(payload, token))
    except UpstreamError as exc:
        return response(request, False, "degraded",
                        error={"code": exc.code,
                               "message": _redact(exc.detail, token)})
    except (ValueError, TypeError, KeyError) as exc:
        return response(request, False, "degraded",
                        error={"code": "invalid_payload",
                               "message": _redact(str(exc), token)})
    except Exception as exc:
        return response(request, False, "degraded",
                        error={"code": "adapter_error",
                               "message": _redact(str(exc), token)})


if __name__ == "__main__":
    serve(handler)
