#!/usr/bin/env python3
"""Microsoft Graph mail adapter: drafts, send, search, and read over Graph.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

Upstream is the official Microsoft Graph v1.0 mail surface only
(/me/messages, /me/sendMail, /me/mailFolders/sentitems/messages), using the
Python standard library (urllib). There is no browser automation and no cookie
handling anywhere in this adapter.

Auth: the user OAuth bearer token is taken ONLY from the
COMPTROL_GRAPH_ACCESS_TOKEN environment variable at request time. It is never
stored on disk, never logged, and never copied into responses, errors, or
audit payloads. Upstream error text is redacted before it leaves the adapter.
COMPTROL_GRAPH_API_BASE may override the API base so verification can run
against a loopback stub; production defaults to the official endpoint.

Attachments: each entry supplies a local file path that must already exist.
Files are capped at 10 MB each (refused, never truncated), base64-encoded
into fileAttachment payloads, and reported back with size_bytes plus a sha256
digest. File bytes never travel through the frame.

Send safety: POST /me/sendMail and POST /me/messages/{id}/send answer 202,
which only means accepted-for-processing. verified=true additionally requires
a Sent Items readback (/me/mailFolders/sentitems/messages) matching the sent
subject plus recipients. Anything less returns verified=false with an explicit
graph_accepted_unverified verification marker.
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

ADAPTER_ID = "comptrol.microsoft-graph-mail"
TOKEN_ENV = "COMPTROL_GRAPH_ACCESS_TOKEN"
API_BASE_ENV = "COMPTROL_GRAPH_API_BASE"

DEFAULT_API_BASE = "https://graph.microsoft.com/v1.0"

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
_MSG_ID_RE = re.compile(r"^[A-Za-z0-9_.=+/-]{1,512}$")


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
                             "Graph API rejected the credential (HTTP %d): %s"
                             % (status, message[:300]))
    if status == 404:
        return UpstreamError("not_found", "Graph resource not found (HTTP 404)")
    if status == 400:
        return UpstreamError("invalid_request",
                             "Graph API rejected the request (HTTP 400): %s"
                             % message[:300])
    if status == 429:
        return UpstreamError("rate_limited",
                             "Graph API rate limit hit (HTTP 429); retry later")
    return UpstreamError("upstream_error",
                         "Graph API HTTP %d: %s" % (status, message[:300]))


def _http(method: str, url: str, token: str,
          body: Optional[Dict[str, Any]] = None,
          extra_headers: Optional[Dict[str, str]] = None
          ) -> Tuple[int, Optional[Dict[str, Any]]]:
    """Raw request returning (status, parsed JSON or None for empty bodies)."""
    data = None
    headers = {"Accept": "application/json"}
    if extra_headers:
        headers.update(extra_headers)
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json; charset=utf-8"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
            status = resp.status
            raw = resp.read(MAX_JSON_BYTES + 1)
    except urllib.error.HTTPError as exc:
        raise _from_http_error(exc, token)
    except urllib.error.URLError as exc:
        raise UpstreamError("upstream_unreachable",
                            "Graph API unreachable: %s"
                            % _redact(str(exc.reason), token)[:200])
    if len(raw) > MAX_JSON_BYTES:
        raise UpstreamError("response_too_large",
                            "upstream JSON exceeded the bounded read cap")
    text = raw.decode("utf-8").strip()
    if not text:
        return status, None
    try:
        parsed = json.loads(text)
    except ValueError:
        raise UpstreamError("upstream_error",
                            "upstream returned non-JSON content")
    if not isinstance(parsed, dict):
        raise UpstreamError("upstream_error",
                            "upstream returned an unexpected JSON shape")
    return status, parsed


def _http_json(method: str, url: str, token: str,
               body: Optional[Dict[str, Any]] = None,
               extra_headers: Optional[Dict[str, str]] = None
               ) -> Dict[str, Any]:
    _, parsed = _http(method, url, token, body, extra_headers)
    if parsed is None:
        raise UpstreamError("upstream_error",
                            "upstream returned an empty body where JSON was required")
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


# ---------------------------------------------------------------- Graph model


def _recipients(addrs: List[str]) -> List[Dict[str, Any]]:
    return [{"emailAddress": {"address": a}} for a in addrs]


def _attachment_payloads(attachments: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    payloads = []
    for att in attachments:
        with open(att["path"], "rb") as handle:
            blob = handle.read()
        payloads.append({
            "@odata.type": "#microsoft.graph.fileAttachment",
            "name": att["filename"],
            "contentType": att["mime_type"],
            "contentBytes": base64.b64encode(blob).decode("ascii"),
        })
    return payloads


def _compose_message(payload: Dict[str, Any]) -> Tuple[Dict[str, Any],
                                                       List[Dict[str, Any]],
                                                       List[str], str]:
    to_addrs = _req_addresses(payload, "to", True)
    cc = _req_addresses(payload, "cc", False)
    bcc = _req_addresses(payload, "bcc", False)
    subject = _req_str(payload, "subject", MAX_SUBJECT_CHARS)
    if "\r" in subject or "\n" in subject:
        raise ValueError("subject must be a single line")
    body = _req_str(payload, "body", MAX_BODY_CHARS, allow_empty=True)
    body_type = payload.get("body_type", "Text")
    if body_type not in ("Text", "HTML"):
        raise ValueError("body_type must be Text or HTML")
    attachments = _req_attachments(payload)
    message: Dict[str, Any] = {
        "subject": subject,
        "body": {"contentType": body_type, "content": body},
        "toRecipients": _recipients(to_addrs),
    }
    if cc:
        message["ccRecipients"] = _recipients(cc)
    if bcc:
        message["bccRecipients"] = _recipients(bcc)
    if attachments:
        message["attachments"] = _attachment_payloads(attachments)
    return message, attachments, to_addrs, subject


def _addr_list(recips: Any) -> List[str]:
    out = []
    if isinstance(recips, list):
        for entry in recips:
            if isinstance(entry, dict):
                addr = entry.get("emailAddress", {})
                if isinstance(addr, dict) and isinstance(addr.get("address"), str):
                    out.append(addr["address"].lower())
    return out


def _sent_items_match(subject: str, to_addrs: List[str], token: str,
                      ) -> Tuple[bool, Dict[str, Any]]:
    """Read back Sent Items newest-first and match subject plus recipients."""
    params = {
        "$top": "10",
        "$orderby": "sentDateTime desc",
        "$select": "id,subject,toRecipients,internetMessageId,sentDateTime",
    }
    url = (_api_base() + "/me/mailFolders/sentitems/messages?"
           + urllib.parse.urlencode(params))
    try:
        listed = _http_json("GET", url, token)
    except UpstreamError:
        return False, {}
    values = listed.get("value", [])
    if not isinstance(values, list):
        return False, {}
    wanted = sorted(a.lower() for a in to_addrs)
    for item in values:
        if not isinstance(item, dict):
            continue
        if not isinstance(item.get("subject"), str) \
                or item.get("subject") != subject:
            continue
        if sorted(_addr_list(item.get("toRecipients"))) != wanted:
            continue
        return True, {
            "id": item.get("id", "") if isinstance(item.get("id"), str) else "",
            "internet_message_id": item.get("internetMessageId", "")
            if isinstance(item.get("internetMessageId"), str) else "",
            "sent_date_time": item.get("sentDateTime", "") if isinstance(
                item.get("sentDateTime"), str) else "",
        }
    return False, {}


def handle_draft(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    message, attachments, to_addrs, subject = _compose_message(payload)
    # Attachments ride inline on message creation; Graph returns the draft id.
    created = _http_json("POST", _api_base() + "/me/messages", token, message)
    draft_id = created.get("id")
    if not isinstance(draft_id, str) or not draft_id:
        raise UpstreamError("upstream_error",
                            "message creation returned no id")
    # Draft verification is a GET readback: the message must exist and still
    # be a draft with the composed subject. A bare create response is not proof.
    verify_url = (_api_base() + "/me/messages/"
                  + urllib.parse.quote(draft_id, safe="")
                  + "?$select=" + urllib.parse.quote("id,isDraft,subject", safe=""))
    verified = False
    try:
        check = _http_json("GET", verify_url, token)
        verified = (check.get("id") == draft_id
                    and check.get("isDraft") is True
                    and check.get("subject") == subject)
    except UpstreamError:
        verified = False
    return {
        "draft_id": draft_id,
        "to": to_addrs,
        "subject": subject,
        "attachments": [{k: a[k] for k in ("filename", "mime_type",
                                           "size_bytes", "sha256")}
                        for a in attachments],
        "verified": verified,
        "verification": "graph_draft_readback" if verified
        else "graph_accepted_unverified",
    }


def handle_send(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    draft_id = payload.get("draft_id")
    to_addrs: List[str] = []
    subject = ""
    status = 0
    if draft_id is not None:
        if not isinstance(draft_id, str) or not _MSG_ID_RE.match(draft_id):
            raise ValueError("draft_id must be a Graph message id")
        # Bind the draft first so the Sent Items match uses exact fields.
        bound = _http_json(
            "GET", _api_base() + "/me/messages/"
            + urllib.parse.quote(draft_id, safe="")
            + "?$select=" + urllib.parse.quote(
                "id,subject,toRecipients", safe=""), token)
        if isinstance(bound.get("subject"), str):
            subject = bound["subject"]
        to_addrs = _addr_list(bound.get("toRecipients"))
        status, _ = _http("POST", _api_base() + "/me/messages/"
                          + urllib.parse.quote(draft_id, safe="") + "/send",
                          token)
    else:
        message, _, to_addrs, subject = _compose_message(payload)
        status, _ = _http("POST", _api_base() + "/me/sendMail", token,
                          {"message": message, "saveToSentItems": True})
    accepted = status in (200, 201, 202, 204)
    if not accepted:
        raise UpstreamError("upstream_error",
                            "Graph send returned HTTP %d" % status)
    # 202 means accepted-for-processing only. verified=true requires the Sent
    # Items readback to match subject plus recipients.
    verified, match = _sent_items_match(subject, to_addrs, token)
    body_out: Dict[str, Any] = {
        "accepted_for_processing": True,
        "http_status": status,
        "subject": subject,
        "to": to_addrs,
        "sent_items_match": match,
        "verified": verified,
        "verification": "graph_sent_items_readback" if verified
        else "graph_accepted_unverified",
    }
    if draft_id is not None:
        body_out["draft_id"] = draft_id
    if not verified:
        body_out["reason"] = ("send was accepted for processing but no matching "
                              "message was found in Sent Items; retry mail.read "
                              "or mail.search before treating it as delivered")
    return body_out


def handle_search(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    query = payload.get("q", "")
    if not isinstance(query, str) or len(query) > 1000:
        raise ValueError("q must be a string of length 0-1000")
    if "\x00" in query:
        raise ValueError("q must not contain NUL characters")
    max_results = _req_int(payload, "max_results", 1, MAX_RESULTS, 10)
    params = {
        "$top": str(max_results),
        "$orderby": "receivedDateTime desc",
        "$select": "id,subject,from,receivedDateTime,hasAttachments,internetMessageId",
    }
    headers: Dict[str, str] = {}
    if query:
        params["$search"] = '"%s"' % query.replace('"', " ")
        headers["ConsistencyLevel"] = "eventual"
    url = (_api_base() + "/me/messages?" + urllib.parse.urlencode(params))
    listed = _http_json("GET", url, token, extra_headers=headers or None)
    values = listed.get("value", [])
    if not isinstance(values, list):
        values = []
    messages = []
    for item in values[:max_results]:
        if not isinstance(item, dict):
            continue
        sender = item.get("from", {})
        addr = ""
        if isinstance(sender, dict):
            inner = sender.get("emailAddress", {})
            if isinstance(inner, dict) and isinstance(inner.get("address"), str):
                addr = inner["address"][:HEADER_EXCERPT_CHARS]
        messages.append({
            "id": item.get("id", "") if isinstance(item.get("id"), str) else "",
            "subject": item.get("subject", "")[:HEADER_EXCERPT_CHARS] if isinstance(
                item.get("subject"), str) else "",
            "from": addr,
            "received_date_time": item.get("receivedDateTime", "")
            if isinstance(item.get("receivedDateTime"), str) else "",
            "has_attachments": item.get("hasAttachments") is True,
            "internet_message_id": item.get("internetMessageId", "")
            if isinstance(item.get("internetMessageId"), str) else "",
        })
    return {
        "q": query,
        "messages": messages,
        "verified": True,
        "verification": "graph_search_readback",
    }


def handle_read(payload: Dict[str, Any], token: str) -> Dict[str, Any]:
    message_id = payload.get("message_id")
    if not isinstance(message_id, str) or not _MSG_ID_RE.match(message_id):
        raise ValueError("message_id must be a Graph message id")
    url = (_api_base() + "/me/messages/"
           + urllib.parse.quote(message_id, safe="") + "?$select="
           + urllib.parse.quote("id,subject,body,from,toRecipients,ccRecipients,"
                                "receivedDateTime,sentDateTime,hasAttachments,"
                                "internetMessageId", safe=""))
    detail = _http_json("GET", url, token)
    sender = detail.get("from", {})
    addr = ""
    if isinstance(sender, dict):
        inner = sender.get("emailAddress", {})
        if isinstance(inner, dict) and isinstance(inner.get("address"), str):
            addr = inner["address"][:HEADER_EXCERPT_CHARS]
    body = detail.get("body", {}) if isinstance(detail.get("body"), dict) else {}
    content = body.get("content", "") if isinstance(body.get("content"), str) else ""
    attachments: List[Dict[str, Any]] = []
    if detail.get("hasAttachments") is True:
        att_url = (_api_base() + "/me/messages/"
                   + urllib.parse.quote(message_id, safe="")
                   + "/attachments?$select="
                   + urllib.parse.quote("id,name,contentType,size", safe=""))
        try:
            att_list = _http_json("GET", att_url, token)
            for entry in att_list.get("value", [])[:MAX_ATTACHMENTS]:
                if not isinstance(entry, dict):
                    continue
                attachments.append({
                    "id": entry.get("id", "") if isinstance(
                        entry.get("id"), str) else "",
                    "filename": entry.get("name", "")[:255] if isinstance(
                        entry.get("name"), str) else "",
                    "mime_type": entry.get("contentType", "") if isinstance(
                        entry.get("contentType"), str) else "",
                    "size_bytes": entry.get("size", 0) if isinstance(
                        entry.get("size"), int) else 0,
                })
        except UpstreamError:
            attachments = []
    return {
        "id": detail.get("id", ""),
        "subject": detail.get("subject", "")[:HEADER_EXCERPT_CHARS] if isinstance(
            detail.get("subject"), str) else "",
        "from": addr,
        "to": _addr_list(detail.get("toRecipients")),
        "cc": _addr_list(detail.get("ccRecipients")),
        "body_type": body.get("contentType", "") if isinstance(
            body.get("contentType"), str) else "",
        "body_excerpt": content[:READ_EXCERPT_CHARS],
        "body_truncated": len(content) > READ_EXCERPT_CHARS,
        "received_date_time": detail.get("receivedDateTime", "") if isinstance(
            detail.get("receivedDateTime"), str) else "",
        "sent_date_time": detail.get("sentDateTime", "") if isinstance(
            detail.get("sentDateTime"), str) else "",
        "internet_message_id": detail.get("internetMessageId", "") if isinstance(
            detail.get("internetMessageId"), str) else "",
        "attachments": attachments,
        "verified": True,
        "verification": "graph_message_readback",
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
                        {"adapter": ADAPTER_ID, "protocol": "graph-mail-https"})
    if method == "capabilities":
        return response(request, True, "available",
                        {"backend": "graph-v1.0-mail",
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
