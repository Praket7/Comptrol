#!/usr/bin/env python3
"""Canva adapter: Connect API reads/exports plus Apps SDK Design Editing bridge.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

Upstream is the official Canva Connect REST surface only
(https://api.canva.com/rest/v1: designs, design pages, exports), using the
Python standard library (urllib). There is no browser automation anywhere in
this adapter, and full-screen coordinate replay is never performed for any
intent: element-level inspection and every edit resolve through the companion
Canva App built on the Apps SDK Design Editing model.

Auth: the user OAuth bearer token is taken ONLY from the
COMPTROL_CANVA_ACCESS_TOKEN environment variable at request time. It is never
stored on disk, never logged, and never copied into responses, errors, or
audit payloads. Upstream error text is redacted before it leaves the adapter.

Reads (design.list, design.read, design.page.list) are served live from the
Connect API. design.element.inspect and every edit intent (design.text.update,
design.image.insert, design.element.create, design.element.delete,
design.element.group) have no Connect API equivalent, so the adapter binds the
exact design id first (GET /designs/{designId}), validates the typed
operation, and returns design_editing_app_required with bridge guidance for
the companion App (currently in preview). The typed operation is echoed back
so the bridge can apply exactly what was validated, without coordinates.

Exports create an asynchronous job (POST /exports), poll GET /exports/{jobId}
with a bounded timeout, then stream the resulting bytes straight to files
under COMPTROL_CANVA_EXPORT_DIR, returning only size_bytes plus sha256
digests. File bytes never travel through the frame. When the export directory
is unset the adapter returns job metadata only and persists nothing.
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.canva"
TOKEN_ENV = "COMPTROL_CANVA_ACCESS_TOKEN"
EXPORT_DIR_ENV = "COMPTROL_CANVA_EXPORT_DIR"
BASE_ENV = "COMPTROL_CANVA_BASE"

DEFAULT_BASE = "https://api.canva.com/rest/v1"

HTTP_TIMEOUT_S = 20.0
MAX_JSON_BYTES = 8 * 1024 * 1024
EXPORT_MAX_BYTES = 128 * 1024 * 1024
MAX_EXPORT_FILES = 100
READ_CHUNK = 65536
EXPORT_POLL_TIMEOUT_S = 120.0
EXPORT_POLL_INTERVAL_S = 2.0

MAX_TEXT_CHARS = 10000
MAX_ALT_CHARS = 500
MAX_URL_CHARS = 2048
MAX_QUERY_CHARS = 255
MAX_LIST_LIMIT = 100
MAX_PAGE_LIMIT = 200
MAX_EXPORT_PAGES = 100
MAX_GROUP_ELEMENTS = 50
MAX_CREATE_PARAMS_BYTES = 8192
MAX_BATCH_OPS = 32

INTENTS = (
    "design.list",
    "design.read",
    "design.page.list",
    "design.element.inspect",
    "design.text.update",
    "design.image.insert",
    "design.element.create",
    "design.element.delete",
    "design.element.group",
    "design.batch_edit",
    "design.export",
)

EXPORT_FORMATS = frozenset({
    "pdf",
    "jpg",
    "png",
    "gif",
    "pptx",
    "mp4",
    "csv",
    "html_bundle",
    "html_standalone",
})

EXPORT_EXT = {
    "pdf": ".pdf",
    "jpg": ".jpg",
    "png": ".png",
    "gif": ".gif",
    "pptx": ".pptx",
    "mp4": ".mp4",
    "csv": ".csv",
    "html_bundle": ".zip",
    "html_standalone": ".html",
}

ELEMENT_TYPES = frozenset({
    "text",
    "image",
    "shape",
    "chart",
    "table",
    "video",
    "embed",
})

OWNERSHIPS = frozenset({"any", "owned", "shared"})

APP_BRIDGE_ORIGINS = ("https://www.canva.com",)

BRIDGE_GUIDANCE = (
    "No Canva Connect API endpoint expresses this operation. Apply it through "
    "the companion Canva App built on the Apps SDK Design Editing model: open "
    "the design edit_url returned by design.read, run the typed operation "
    "echoed in this payload inside the App iframe (postMessage origin-checked "
    "against the allowed origins), reread the design to confirm the new "
    "revision, and report the readback. The adapter and the bridge never "
    "perform full-screen coordinate replay and never accept raw screen "
    "coordinates; element references are opaque ids bound to the exact "
    "design_id above."
)

_ID_CHARS = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
)


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


def _base() -> str:
    return os.environ.get(BASE_ENV, DEFAULT_BASE).rstrip("/") or DEFAULT_BASE


def _from_http_error(exc: "urllib.error.HTTPError", token: str) -> UpstreamError:
    try:
        raw = exc.read(4096)
    except Exception:
        raw = b""
    snippet = _redact(raw.decode("utf-8", errors="replace"), token)
    message = snippet
    try:
        parsed = json.loads(snippet)
        if isinstance(parsed, dict):
            if isinstance(parsed.get("message"), str) and parsed["message"]:
                message = parsed["message"][:500]
            elif isinstance(parsed.get("error"), dict) and isinstance(
                parsed["error"].get("message"), str
            ):
                message = parsed["error"]["message"][:500]
    except ValueError:
        pass
    status = exc.code
    if status in (401, 403):
        return UpstreamError(
            "auth_failed",
            "Canva API rejected the credential (HTTP %d): %s" % (status, message[:300]),
        )
    if status == 404:
        return UpstreamError("not_found", "Canva resource not found (HTTP 404)")
    if status == 429:
        return UpstreamError("rate_limited", "Canva API rate limit hit; retry with backoff")
    return UpstreamError("upstream_error", "Canva API HTTP %d: %s" % (status, message[:300]))


def _http_json(method: str, url: str, token: str, body: "dict | None" = None) -> dict:
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
        raise UpstreamError(
            "upstream_unreachable",
            "Canva API unreachable: %s" % _redact(str(exc.reason), token)[:200],
        )
    if len(raw) > MAX_JSON_BYTES:
        raise UpstreamError("response_too_large", "upstream JSON exceeded the bounded read cap")
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise UpstreamError("upstream_error", "upstream returned non-JSON content")
    if not isinstance(parsed, dict):
        raise UpstreamError("upstream_error", "upstream returned an unexpected JSON shape")
    return parsed


# ---------------------------------------------------------------- validators


def _req_design_id(payload: dict) -> str:
    value = payload.get("design_id")
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 128
        or any(c not in _ID_CHARS for c in value)
    ):
        raise ValueError("design_id must be the exact Canva design id (1-128 chars, A-Za-z0-9-_)")
    return value


def _req_element_id(payload: dict, key: str = "element_id") -> str:
    value = payload.get(key)
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 256
        or any(c in "/?#\r\n\t " for c in value)
    ):
        raise ValueError(key + " must be an opaque element id (1-256 chars, no URL parts)")
    return value


def _opt_page_id(payload: dict) -> "str | None":
    value = payload.get("page_id")
    if value is None:
        return None
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 256
        or any(c in "/?#\r\n\t " for c in value)
    ):
        raise ValueError("page_id must be an opaque page id (1-256 chars, no URL parts)")
    return value


def _req_str(payload: dict, key: str, max_len: int) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or not value or len(value) > max_len:
        raise ValueError(key + " must be a non-empty string of at most %d chars" % max_len)
    return value


def _opt_str(payload: dict, key: str, max_len: int) -> "str | None":
    value = payload.get(key)
    if value is None:
        return None
    if not isinstance(value, str) or not value or len(value) > max_len:
        raise ValueError(key + " must be a non-empty string of at most %d chars" % max_len)
    return value


def _opt_revision(payload: dict) -> "str | None":
    value = payload.get("expected_revision")
    if value is None:
        return None
    if not isinstance(value, str) or not value or len(value) > 512:
        raise ValueError("expected_revision must be a non-empty string up to 512 chars")
    return value


def _req_https_url(payload: dict, key: str) -> str:
    value = _req_str(payload, key, MAX_URL_CHARS)
    try:
        parts = urllib.parse.urlsplit(value)
    except ValueError:
        raise ValueError(key + " must be a valid https URL")
    if parts.scheme != "https" or not parts.netloc:
        raise ValueError(key + " must be an https URL")
    return value


def _req_pages(payload: dict) -> "list | None":
    value = payload.get("pages")
    if value is None:
        return None
    if (
        not isinstance(value, list)
        or not 1 <= len(value) <= MAX_EXPORT_PAGES
        or any(isinstance(v, bool) or not isinstance(v, int) or not 1 <= v <= 10000 for v in value)
    ):
        raise ValueError("pages must be a list of 1-%d one-based page numbers" % MAX_EXPORT_PAGES)
    return list(value)


def _req_asset_id(payload: dict) -> str:
    value = payload.get("asset_id")
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 256
        or any(c not in _ID_CHARS for c in value)
    ):
        raise ValueError("asset_id must be a Canva asset id (1-256 chars, A-Za-z0-9-_)")
    return value


def _req_element_ids(payload: dict) -> list:
    value = payload.get("element_ids")
    if (
        not isinstance(value, list)
        or not 2 <= len(value) <= MAX_GROUP_ELEMENTS
        or any(
            not isinstance(e, str) or not 1 <= len(e) <= 256 or any(c in "/?#\r\n\t " for c in e)
            for e in value
        )
        or len(set(value)) != len(value)
    ):
        raise ValueError(
            "element_ids must be a list of 2-%d unique opaque element ids" % MAX_GROUP_ELEMENTS
        )
    return list(value)


def _req_element_type(payload: dict) -> str:
    value = payload.get("element_type")
    if value not in ELEMENT_TYPES:
        raise ValueError("element_type must be one of: " + ", ".join(sorted(ELEMENT_TYPES)))
    return value


def _req_create_params(payload: dict) -> dict:
    value = payload.get("params", {})
    if not isinstance(value, dict):
        raise ValueError("params must be an object")
    if len(json.dumps(value, default=str)) > MAX_CREATE_PARAMS_BYTES:
        raise ValueError("params exceed the %d-byte bound" % MAX_CREATE_PARAMS_BYTES)
    return value


# ---------------------------------------------------------------- Connect API


def _api_get_design(design_id: str, token: str) -> dict:
    url = _base() + "/designs/" + urllib.parse.quote(design_id, safe="")
    body = _http_json("GET", url, token)
    design = body.get("design")
    if not isinstance(design, dict):
        raise UpstreamError("upstream_error", "designs.get returned an unexpected shape")
    return design


def _api_list_designs(
    token: str, query: str, ownership: str, limit: int, continuation: "str | None"
) -> dict:
    params = {"ownership": ownership, "limit": str(limit)}
    if query:
        params["query"] = query
    if continuation:
        params["continuation"] = continuation
    url = _base() + "/designs?" + urllib.parse.urlencode(params)
    return _http_json("GET", url, token)


def _api_list_pages(design_id: str, token: str, limit: int, offset: "int | None") -> dict:
    params = {"limit": str(limit)}
    if offset is not None:
        params["offset"] = str(offset)
    url = (
        _base()
        + "/designs/"
        + urllib.parse.quote(design_id, safe="")
        + "/pages?"
        + urllib.parse.urlencode(params)
    )
    return _http_json("GET", url, token)


def _api_create_export(design_id: str, token: str, format_type: str, pages: "list | None") -> str:
    fmt: dict = {"type": format_type}
    if pages is not None:
        fmt["pages"] = pages
    body = _http_json(
        "POST", _base() + "/exports", token, {"design_id": design_id, "format": fmt}
    )
    job = body.get("job")
    if not isinstance(job, dict) or not isinstance(job.get("id"), str) or not job["id"]:
        raise UpstreamError("upstream_error", "exports.create returned no job id")
    return job["id"]


def _api_get_export(export_id: str, token: str) -> dict:
    url = _base() + "/exports/" + urllib.parse.quote(export_id, safe="")
    body = _http_json("GET", url, token)
    job = body.get("job")
    if not isinstance(job, dict):
        raise UpstreamError("upstream_error", "exports.get returned an unexpected shape")
    return job


def _design_revision(design: dict) -> str:
    for key in ("updated_at", "modified_at"):
        value = design.get(key)
        if isinstance(value, str) and value:
            return value[:256]
    digest = hashlib.sha256(
        json.dumps(design, sort_keys=True, default=str).encode("utf-8")
    ).hexdigest()[:16]
    return "design-hash-" + digest


def _refuse_if_stale(expected: "str | None", current: str) -> None:
    if expected is not None and expected != current:
        raise UpstreamError(
            "stale_revision",
            "expected_revision does not match the current design revision; reread and retry",
        )


def _summarize_design(design: dict) -> dict:
    owner = design.get("owner") if isinstance(design.get("owner"), dict) else {}
    urls = design.get("urls") if isinstance(design.get("urls"), dict) else {}
    thumbnail = design.get("thumbnail") if isinstance(design.get("thumbnail"), dict) else {}
    title = design.get("title", "")
    return {
        "id": design.get("id", ""),
        "title": title if isinstance(title, str) else "",
        "owner_user_id": owner.get("user_id", "") if isinstance(owner.get("user_id"), str) else "",
        "owner_team_id": owner.get("team_id", "") if isinstance(owner.get("team_id"), str) else "",
        "edit_url": urls.get("edit_url", "") if isinstance(urls.get("edit_url"), str) else "",
        "view_url": urls.get("view_url", "") if isinstance(urls.get("view_url"), str) else "",
        "thumbnail_url": thumbnail.get("url", "")
        if isinstance(thumbnail.get("url"), str)
        else "",
        "page_count": design.get("page_count") if isinstance(design.get("page_count"), int) else None,
        "revision": _design_revision(design),
    }


def _bind_exact(design_id: str, token: str) -> dict:
    """Bind one exact design id; never search, never fuzzy-match."""
    return _api_get_design(design_id, token)


def _need_app(request: dict, operation: str, echo: dict) -> dict:
    """Bridge gap: no Connect API equivalent, so resolve via the App bridge."""
    payload = {
        "adapter": ADAPTER_ID,
        "operation": operation,
        "bridge": "canva-app-design-editing",
        "bridge_status": "preview",
        "allowed_origins": list(APP_BRIDGE_ORIGINS),
        "guidance": BRIDGE_GUIDANCE,
        "forbidden": [
            "full-screen coordinate replay",
            "raw screen coordinates",
            "token in iframe URL or postMessage body",
        ],
    }
    payload.update(echo)
    return response(
        request,
        False,
        "available",
        payload,
        error={
            "code": "design_editing_app_required",
            "message": operation
            + " has no Connect API equivalent and requires the companion Canva App bridge (preview)",
        },
    )


# ---------------------------------------------------------------- reads


def handle_design_list(payload: dict, token: str) -> dict:
    query = payload.get("query", "")
    if not isinstance(query, str) or len(query) > MAX_QUERY_CHARS:
        raise ValueError("query must be a string of at most %d chars" % MAX_QUERY_CHARS)
    ownership = payload.get("ownership", "any")
    if ownership not in OWNERSHIPS:
        raise ValueError("ownership must be one of: " + ", ".join(sorted(OWNERSHIPS)))
    limit = payload.get("limit", 25)
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= MAX_LIST_LIMIT:
        raise ValueError("limit must be an integer in [1, %d]" % MAX_LIST_LIMIT)
    continuation = payload.get("continuation")
    if continuation is not None and (
        not isinstance(continuation, str) or not continuation or len(continuation) > 2048
    ):
        raise ValueError("continuation must be a non-empty opaque string up to 2048 chars")
    body = _api_list_designs(token, query, ownership, limit, continuation)
    items = body.get("items", [])
    if not isinstance(items, list):
        raise UpstreamError("upstream_error", "designs.list returned an unexpected shape")
    summaries = []
    for item in items[:MAX_LIST_LIMIT]:
        if isinstance(item, dict):
            summaries.append(_summarize_design(item))
    cont = body.get("continuation", "")
    return {
        "designs": summaries,
        "count": len(summaries),
        "continuation": cont if isinstance(cont, str) else "",
        "verified": True,
        "verification": "canva_designs_readback",
    }


def handle_design_read(payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    design = _bind_exact(design_id, token)
    if design.get("id", design_id) != design_id:
        raise UpstreamError("upstream_error", "design id binding mismatch")
    summary = _summarize_design(design)
    summary.update({"verified": True, "verification": "canva_design_readback"})
    return summary


def handle_page_list(payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    limit = payload.get("limit", 50)
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= MAX_PAGE_LIMIT:
        raise ValueError("limit must be an integer in [1, %d]" % MAX_PAGE_LIMIT)
    offset = payload.get("offset")
    if offset is not None and (
        isinstance(offset, bool) or not isinstance(offset, int) or not 0 <= offset <= 100000
    ):
        raise ValueError("offset must be an integer in [0, 100000]")
    body = _api_list_pages(design_id, token, limit, offset)
    items = body.get("items", [])
    if not isinstance(items, list):
        raise UpstreamError("upstream_error", "design pages returned an unexpected shape")
    pages = []
    for item in items[:MAX_PAGE_LIMIT]:
        if not isinstance(item, dict):
            continue
        thumbnail = item.get("thumbnail") if isinstance(item.get("thumbnail"), dict) else {}
        title = item.get("title", "")
        pages.append(
            {
                "page_id": item.get("id", "") if isinstance(item.get("id"), str) else "",
                "title": title if isinstance(title, str) else "",
                "thumbnail_url": thumbnail.get("url", "")
                if isinstance(thumbnail.get("url"), str)
                else "",
            }
        )
    cont = body.get("continuation", "")
    return {
        "design_id": design_id,
        "pages": pages,
        "count": len(pages),
        "offset": offset if offset is not None else 0,
        "limit": limit,
        "continuation": cont if isinstance(cont, str) else "",
        "verified": True,
        "verification": "canva_pages_readback",
    }


# ---------------------------------------------------------------- element/edit bridge operations
#
# Each intent below has a dedicated handler branch in handler(). The Connect
# API cannot express element inspection or element edits, so every branch
# binds the exact design id, validates the typed operation (including the
# optional expected_revision guard), and returns design_editing_app_required
# with the validated operation echoed for the companion App bridge. None of
# them performs, or ever requests, full-screen coordinate replay.


def handle_element_inspect(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    element_id = _req_element_id(payload)
    page_id = _opt_page_id(payload)
    design = _bind_exact(design_id, token)
    return _need_app(request, "design.element.inspect", {
        "design_id": design_id,
        "element_id": element_id,
        "page_id": page_id,
        "revision": _design_revision(design),
    })


def handle_text_update(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    element_id = _req_element_id(payload)
    text = _req_str(payload, "text", MAX_TEXT_CHARS)
    page_id = _opt_page_id(payload)
    design = _bind_exact(design_id, token)
    _refuse_if_stale(_opt_revision(payload), _design_revision(design))
    return _need_app(request, "design.text.update", {
        "design_id": design_id,
        "element_id": element_id,
        "page_id": page_id,
        "text": text,
        "text_chars": len(text),
        "revision_before": _design_revision(design),
    })


def handle_image_insert(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    image_url = None
    asset_id = None
    if "image_url" in payload:
        image_url = _req_https_url(payload, "image_url")
    if "asset_id" in payload:
        asset_id = _req_asset_id(payload)
    if image_url is None and asset_id is None:
        raise ValueError("either image_url (https) or asset_id is required")
    page_id = _opt_page_id(payload)
    alt_text = _opt_str(payload, "alt_text", MAX_ALT_CHARS)
    design = _bind_exact(design_id, token)
    _refuse_if_stale(_opt_revision(payload), _design_revision(design))
    return _need_app(request, "design.image.insert", {
        "design_id": design_id,
        "page_id": page_id,
        "image_url": image_url,
        "asset_id": asset_id,
        "alt_text": alt_text,
        "revision_before": _design_revision(design),
    })


def handle_element_create(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    element_type = _req_element_type(payload)
    params = _req_create_params(payload)
    page_id = _opt_page_id(payload)
    design = _bind_exact(design_id, token)
    _refuse_if_stale(_opt_revision(payload), _design_revision(design))
    return _need_app(request, "design.element.create", {
        "design_id": design_id,
        "page_id": page_id,
        "element_type": element_type,
        "params": params,
        "revision_before": _design_revision(design),
    })


def handle_element_delete(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    element_id = _req_element_id(payload)
    page_id = _opt_page_id(payload)
    design = _bind_exact(design_id, token)
    _refuse_if_stale(_opt_revision(payload), _design_revision(design))
    return _need_app(request, "design.element.delete", {
        "design_id": design_id,
        "element_id": element_id,
        "page_id": page_id,
        "revision_before": _design_revision(design),
    })


def handle_element_group(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    element_ids = _req_element_ids(payload)
    page_id = _opt_page_id(payload)
    design = _bind_exact(design_id, token)
    _refuse_if_stale(_opt_revision(payload), _design_revision(design))
    return _need_app(request, "design.element.group", {
        "design_id": design_id,
        "element_ids": element_ids,
        "element_count": len(element_ids),
        "page_id": page_id,
        "revision_before": _design_revision(design),
    })


def handle_batch_edit(request: dict, payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    ops = payload.get("ops")
    if not isinstance(ops, list) or not 1 <= len(ops) <= MAX_BATCH_OPS:
        raise ValueError("ops must contain between 1 and %d operations" % MAX_BATCH_OPS)
    design = _bind_exact(design_id, token)
    revision = _design_revision(design)
    _refuse_if_stale(_opt_revision(payload), revision)
    validated = []
    for index, raw in enumerate(ops):
        if not isinstance(raw, dict):
            raise ValueError("batch operation %d must be an object" % index)
        op = raw.get("op")
        item = {"op": op}
        if op == "text.update":
            item.update({
                "element_id": _req_element_id(raw),
                "page_id": _opt_page_id(raw),
                "text": _req_str(raw, "text", MAX_TEXT_CHARS),
            })
        elif op == "image.insert":
            image_url = _req_https_url(raw, "image_url") if "image_url" in raw else None
            asset_id = _req_asset_id(raw) if "asset_id" in raw else None
            if image_url is None and asset_id is None:
                raise ValueError("image.insert requires image_url or asset_id")
            item.update({
                "page_id": _opt_page_id(raw),
                "image_url": image_url,
                "asset_id": asset_id,
                "alt_text": _opt_str(raw, "alt_text", MAX_ALT_CHARS),
            })
        elif op == "element.create":
            item.update({
                "page_id": _opt_page_id(raw),
                "element_type": _req_element_type(raw),
                "params": _req_create_params(raw),
            })
        elif op == "element.delete":
            item.update({
                "element_id": _req_element_id(raw),
                "page_id": _opt_page_id(raw),
            })
        elif op == "element.group":
            item.update({
                "element_ids": _req_element_ids(raw),
                "page_id": _opt_page_id(raw),
            })
        else:
            raise ValueError("unsupported batch operation: %s" % op)
        validated.append(item)
    return _need_app(request, "design.batch_edit", {
        "design_id": design_id,
        "revision_before": revision,
        "ops": validated,
        "op_count": len(validated),
    })


# ---------------------------------------------------------------- export


def _download_to_file(url: str, dest_path: str) -> "tuple[int, str]":
    try:
        parts = urllib.parse.urlsplit(url)
    except ValueError:
        raise UpstreamError("upstream_error", "export returned an invalid download URL")
    if parts.scheme != "https" or not parts.netloc:
        raise UpstreamError("upstream_error", "export returned a non-https download URL")
    req = urllib.request.Request(url, headers={"Accept": "*/*"}, method="GET")
    digest = hashlib.sha256()
    total = 0
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT_S) as resp:
            with open(dest_path, "wb") as handle:
                while True:
                    chunk = resp.read(READ_CHUNK)
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > EXPORT_MAX_BYTES:
                        raise UpstreamError(
                            "export_too_large",
                            "export exceeded the %d-byte cap" % EXPORT_MAX_BYTES,
                        )
                    digest.update(chunk)
                    handle.write(chunk)
    except urllib.error.HTTPError as exc:
        raise UpstreamError("upstream_error", "export download HTTP %d" % exc.code)
    except urllib.error.URLError as exc:
        raise UpstreamError(
            "upstream_unreachable", "export download unreachable: %s" % str(exc.reason)[:200]
        )
    return total, digest.hexdigest()


def _poll_export_urls(job_id: str, token: str) -> list:
    deadline = time.monotonic() + EXPORT_POLL_TIMEOUT_S
    while True:
        job = _api_get_export(job_id, token)
        status = job.get("status")
        if status == "success":
            urls = job.get("urls", [])
            if not isinstance(urls, list) or not urls or len(urls) > MAX_EXPORT_FILES:
                raise UpstreamError("upstream_error", "export job returned invalid download URLs")
            for url in urls:
                if not isinstance(url, str) or not url or len(url) > 8192:
                    raise UpstreamError("upstream_error", "export job returned invalid download URLs")
            return urls
        if status == "failed":
            err = job.get("error")
            detail = ""
            if isinstance(err, dict) and isinstance(err.get("message"), str):
                detail = err["message"][:300]
            raise UpstreamError("export_failed", "Canva export job failed: %s" % detail)
        if status != "in_progress":
            raise UpstreamError("upstream_error", "export job returned unknown status")
        if time.monotonic() >= deadline:
            raise UpstreamError(
                "export_timeout",
                "export job did not complete within %ds" % int(EXPORT_POLL_TIMEOUT_S),
            )
        time.sleep(EXPORT_POLL_INTERVAL_S)


def handle_export(payload: dict, token: str) -> dict:
    design_id = _req_design_id(payload)
    format_type = payload.get("format")
    if format_type not in EXPORT_FORMATS:
        raise ValueError("format must be one of: " + ", ".join(sorted(EXPORT_FORMATS)))
    pages = _req_pages(payload)
    design = _bind_exact(design_id, token)
    revision_before = _design_revision(design)
    job_id = _api_create_export(design_id, token, format_type, pages)
    urls = _poll_export_urls(job_id, token)
    export_dir = os.environ.get(EXPORT_DIR_ENV, "")
    if export_dir:
        if not os.path.isabs(export_dir):
            raise ValueError(EXPORT_DIR_ENV + " must be an absolute path")
        os.makedirs(export_dir, exist_ok=True)
        ext = EXPORT_EXT[format_type]
        artifacts = []
        total_bytes = 0
        try:
            for index, url in enumerate(urls):
                dest_path = os.path.join(
                    export_dir, "%s_%s_%04d%s" % (design_id, format_type, index + 1, ext)
                )
                size, digest = _download_to_file(url, dest_path)
                total_bytes += size
                artifacts.append(
                    {
                        "index": index + 1,
                        "path": dest_path,
                        "size_bytes": size,
                        "sha256": digest,
                    }
                )
        except UpstreamError:
            for artifact in artifacts:
                try:
                    if os.path.exists(artifact["path"]) and os.path.getsize(
                        artifact["path"]
                    ) == 0:
                        os.remove(artifact["path"])
                except OSError:
                    pass
            raise
        return {
            "design_id": design_id,
            "format": format_type,
            "job_id": job_id,
            "revision_before": revision_before,
            "artifacts": artifacts,
            "artifact_count": len(artifacts),
            "total_bytes": total_bytes,
            "persisted": True,
            "verified": True,
            "verification": "canva_export_artifact",
        }
    return {
        "design_id": design_id,
        "format": format_type,
        "job_id": job_id,
        "revision_before": revision_before,
        "urls": urls,
        "url_count": len(urls),
        "url_expiry_note": "download URLs expire after 24 hours",
        "persisted": False,
        "verified": False,
        "verification": "canva_export_metadata_only",
        "reason": EXPORT_DIR_ENV + " is not set, so the artifact was not persisted",
    }


# ---------------------------------------------------------------- dispatch


_DISPATCH_READS = {
    "design.list": handle_design_list,
    "design.read": handle_design_read,
    "design.page.list": handle_page_list,
    "design.export": handle_export,
}

_DISPATCH_BRIDGE = {
    "design.element.inspect": handle_element_inspect,
    "design.text.update": handle_text_update,
    "design.image.insert": handle_image_insert,
    "design.element.create": handle_element_create,
    "design.element.delete": handle_element_delete,
    "design.element.group": handle_element_group,
    "design.batch_edit": handle_batch_edit,
}


def handler(request: dict) -> dict:
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": ADAPTER_ID, "protocol": "canva-connect-https"})
    if method == "capabilities":
        return response(
            request,
            True,
            "available",
            {"backend": "canva-connect-v1+apps-sdk-design-editing", "intents": list(INTENTS)},
        )
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return response(request, False, "degraded", error={"code": "invalid_payload", "message": "payload must be an object"})
    intent = payload.get("intent")
    bridge_func = _DISPATCH_BRIDGE.get(intent)
    read_func = _DISPATCH_READS.get(intent)
    if bridge_func is None and read_func is None:
        return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
    token = ""
    try:
        token = _token()
        if bridge_func is not None:
            return bridge_func(request, payload, token)
        return response(request, True, "available", read_func(payload, token))
    except UpstreamError as exc:
        return response(
            request, False, "degraded", error={"code": exc.code, "message": _redact(exc.detail, token)}
        )
    except (ValueError, TypeError, KeyError) as exc:
        return response(
            request, False, "degraded", error={"code": "invalid_payload", "message": _redact(str(exc), token)}
        )
    except Exception as exc:
        return response(
            request, False, "degraded", error={"code": "adapter_error", "message": _redact(str(exc), token)}
        )


if __name__ == "__main__":
    serve(handler)
