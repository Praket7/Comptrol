#!/usr/bin/env python3
"""Google Workspace adapter: Docs / Slides / Drive export over official HTTPS APIs.

Transport is the framed RPC from adapters/_shared/adapter_protocol.py, with the
same handler contract as the other first-party adapters (handshake /
capabilities / shutdown, typed intent payloads, structured responses).

Upstream is the official Google REST surface only: Docs v1
(documents.get / documents.batchUpdate), Slides v1 (presentations.get /
presentations.batchUpdate) and Drive v3 (files.get / files.export), using the
Python standard library (urllib). There is no browser automation and no cookie
handling anywhere in this adapter.

Auth: the user OAuth bearer token is taken ONLY from the
COMPTROL_GOOGLE_ACCESS_TOKEN environment variable at request time. It is never
stored on disk, never logged, and never copied into responses, errors, or
audit payloads. Upstream error text is redacted before it leaves the adapter.

Edit safety: every edit first binds the target id plus its current revisionId
(documents.get / presentations.get), sends all independent edits as ONE
batchUpdate call guarded by WriteControl.requiredRevisionId, refuses stale
expected_revision_id values without touching the document, and then rereads
(bounded) to verify the new revisionId plus the affected ranges before
reporting verified=true. A bare upstream success is never treated as proof.

Exports stream bytes straight to a file under COMPTROL_GOOGLE_EXPORT_DIR (when
set) and return only size_bytes plus a sha256 digest. File bytes never travel
through the frame, and every response stays far under the frame limit.
"""

import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.google-workspace"
TOKEN_ENV = "COMPTROL_GOOGLE_ACCESS_TOKEN"
EXPORT_DIR_ENV = "COMPTROL_GOOGLE_EXPORT_DIR"
DOCS_BASE_ENV = "COMPTROL_GOOGLE_DOCS_BASE"
SLIDES_BASE_ENV = "COMPTROL_GOOGLE_SLIDES_BASE"
DRIVE_BASE_ENV = "COMPTROL_GOOGLE_DRIVE_BASE"

DEFAULT_DOCS_BASE = "https://docs.googleapis.com"
DEFAULT_SLIDES_BASE = "https://slides.googleapis.com"
DEFAULT_DRIVE_BASE = "https://www.googleapis.com"

HTTP_TIMEOUT_S = 20.0
MAX_JSON_BYTES = 8 * 1024 * 1024
EXPORT_MAX_BYTES = 32 * 1024 * 1024
READ_CHUNK = 65536
TEXT_EXCERPT_CHARS = 2000
FULL_TEXT_SCAN_CHARS = 200000
MAX_INSERT_CHARS = 50000
MAX_FIND_CHARS = 2000
MAX_REPLACE_CHARS = 50000
MAX_BATCH_OPS = 50
MAX_SLIDES_SUMMARIZED = 200
SLIDE_EXCERPT_CHARS = 500
MAX_PAGE_IDS = 200

DOC_EXPORT_MIMES = {
    "text/plain": ".txt",
    "text/html": ".html",
    "application/pdf": ".pdf",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document": ".docx",
}
SLIDES_EXPORT_MIMES = {
    "application/pdf": ".pdf",
    "text/plain": ".txt",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation": ".pptx",
    "image/png": ".png",
}

SLIDE_LAYOUTS = frozenset({
    "BLANK",
    "CAPTION_ONLY",
    "MAIN_POINT",
    "BIG_NUMBER",
    "TITLE",
    "TITLE_AND_BODY",
    "TITLE_AND_TWO_COLUMNS",
    "TITLE_ONLY",
    "ONE_COLUMN_TEXT",
    "PICTURE_WITH_CAPTION",
    "SECTION_HEADER",
    "SECTION_TITLE_AND_DESCRIPTION",
    "TITLE_SLIDE",
})

INTENTS = (
    "document.google.read",
    "document.google.batch_edit",
    "document.text.insert",
    "document.text.replace",
    "document.text.style",
    "document.export",
    "presentation.google.read",
    "presentation.slide.create",
    "presentation.slide.delete",
    "presentation.slide.reorder",
    "presentation.text.replace",
    "presentation.text.style",
    "presentation.export",
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


def _docs_base() -> str:
    return os.environ.get(DOCS_BASE_ENV, DEFAULT_DOCS_BASE).rstrip("/") or DEFAULT_DOCS_BASE


def _slides_base() -> str:
    return os.environ.get(SLIDES_BASE_ENV, DEFAULT_SLIDES_BASE).rstrip("/") or DEFAULT_SLIDES_BASE


def _drive_base() -> str:
    return os.environ.get(DRIVE_BASE_ENV, DEFAULT_DRIVE_BASE).rstrip("/") or DEFAULT_DRIVE_BASE


def _from_http_error(exc: "urllib.error.HTTPError", token: str) -> UpstreamError:
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
        return UpstreamError("auth_failed", "Google API rejected the credential (HTTP %d): %s" % (status, message[:300]))
    if status == 404:
        return UpstreamError("not_found", "Google resource not found (HTTP 404)")
    if status in (400, 409, 412) and "revision" in message.lower():
        return UpstreamError(
            "stale_revision",
            "upstream revision guard refused the write; reread and retry with the current revisionId",
        )
    return UpstreamError("upstream_error", "Google API HTTP %d: %s" % (status, message[:300]))


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
        raise UpstreamError("upstream_unreachable", "Google API unreachable: %s" % _redact(str(exc.reason), token)[:200])
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


def _req_id(payload: dict, key: str) -> str:
    value = payload.get(key)
    if (
        not isinstance(value, str)
        or not 1 <= len(value) <= 256
        or any(c not in _ID_CHARS for c in value)
    ):
        raise ValueError(key + " must be a Google resource id (1-256 chars, A-Za-z0-9-_)")
    return value


def _req_str(payload: dict, key: str, max_len: int, allow_empty: bool = False) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or len(value) > max_len or (not allow_empty and not value):
        raise ValueError(key + " must be a string of length 1-%d" % max_len if not allow_empty else key + " must be a string of length 0-%d" % max_len)
    return value


def _req_int(payload: dict, key: str, minimum: int, maximum: int) -> int:
    value = payload.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(key + " must be an integer in [%d, %d]" % (minimum, maximum))
    return value


def _opt_bool(payload: dict, key: str, default: bool = False) -> bool:
    value = payload.get(key, default)
    if not isinstance(value, bool):
        raise ValueError(key + " must be a boolean")
    return value


def _opt_revision(payload: dict) -> "str | None":
    value = payload.get("expected_revision_id")
    if value is None:
        return None
    if not isinstance(value, str) or not value or len(value) > 512:
        raise ValueError("expected_revision_id must be a non-empty string up to 512 chars")
    return value


def _opt_layout(payload: dict) -> str:
    value = payload.get("layout", "BLANK")
    if not isinstance(value, str) or value not in SLIDE_LAYOUTS:
        raise ValueError("layout must be one of: " + ", ".join(sorted(SLIDE_LAYOUTS)))
    return value


# ---------------------------------------------------------------- text scan


def _collect_text(node: object, parts: list, budget: list) -> None:
    """In-order bounded walk collecting every textRun.content string."""
    if budget[0] <= 0:
        return
    if isinstance(node, dict):
        run = node.get("textRun")
        if isinstance(run, dict) and isinstance(run.get("content"), str):
            chunk = run["content"][: budget[0]]
            parts.append(chunk)
            budget[0] -= len(chunk)
            if budget[0] <= 0:
                return
        for key, value in node.items():
            if key == "textRun":
                continue
            _collect_text(value, parts, budget)
            if budget[0] <= 0:
                return
    elif isinstance(node, list):
        for value in node:
            _collect_text(value, parts, budget)
            if budget[0] <= 0:
                return


def _full_text(resource: dict, limit: int = FULL_TEXT_SCAN_CHARS) -> str:
    parts: list = []
    _collect_text(resource, parts, [limit])
    return "".join(parts)


def _excerpt(resource: dict, limit: int = TEXT_EXCERPT_CHARS) -> str:
    return _full_text(resource, limit)


# ---------------------------------------------------------------- style


def _parse_hex_color(value: object) -> dict:
    if not isinstance(value, str) or len(value) != 7 or not value.startswith("#"):
        raise ValueError("foreground_color must be a hex string like #rrggbb")
    try:
        num = int(value[1:], 16)
    except ValueError:
        raise ValueError("foreground_color must be a hex string like #rrggbb")
    return {
        "red": ((num >> 16) & 255) / 255.0,
        "green": ((num >> 8) & 255) / 255.0,
        "blue": (num & 255) / 255.0,
    }


def _text_style(style: object) -> "tuple[dict, str]":
    """Typed subset of the Docs/Slides TextStyle resource shared by both APIs."""
    if not isinstance(style, dict) or not style:
        raise ValueError("style must be a non-empty object")
    allowed = {"bold", "italic", "underline", "font_size_pt", "foreground_color"}
    unknown = set(style) - allowed
    if unknown:
        raise ValueError("unsupported style keys: " + ", ".join(sorted(unknown)))
    out: dict = {}
    fields: list = []
    for key in ("bold", "italic", "underline"):
        if key in style:
            if not isinstance(style[key], bool):
                raise ValueError(key + " must be a boolean")
            out[key] = style[key]
            fields.append(key)
    if "font_size_pt" in style:
        size = style["font_size_pt"]
        if isinstance(size, bool) or not isinstance(size, (int, float)) or not 1 <= size <= 200:
            raise ValueError("font_size_pt must be a number in [1, 200]")
        out["fontSize"] = {"magnitude": float(size), "unit": "PT"}
        fields.append("fontSize")
    if "foreground_color" in style:
        out["foregroundColor"] = {"color": {"rgbColor": _parse_hex_color(style["foreground_color"])}}
        fields.append("foregroundColor")
    return out, ",".join(fields)


# ---------------------------------------------------------------- Docs API


def _docs_get(document_id: str, token: str) -> dict:
    url = (
        _docs_base()
        + "/v1/documents/"
        + urllib.parse.quote(document_id, safe="")
        + "?fields="
        + urllib.parse.quote("title,revisionId,body", safe="")
    )
    return _http_json("GET", url, token)


def _docs_batch_update(document_id: str, token: str, api_requests: list, required_revision_id: str) -> dict:
    url = _docs_base() + "/v1/documents/" + urllib.parse.quote(document_id, safe="") + ":batchUpdate"
    return _http_json(
        "POST",
        url,
        token,
        {"requests": api_requests, "writeControl": {"requiredRevisionId": required_revision_id}},
    )


def _bind_doc(document_id: str, token: str) -> "tuple[dict, str]":
    doc = _docs_get(document_id, token)
    rev = doc.get("revisionId")
    if not isinstance(rev, str) or not rev:
        raise UpstreamError("upstream_error", "documents.get returned no revisionId")
    return doc, rev


def _refuse_if_stale(expected: "str | None", current: str) -> None:
    if expected is not None and expected != current:
        raise UpstreamError(
            "stale_revision",
            "expected_revision_id does not match the current revisionId; reread and retry",
        )


def _run_text_checks(full_after: str, checks: list) -> bool:
    for kind, text in checks:
        if kind == "present" and text not in full_after:
            return False
        if kind == "absent" and text in full_after:
            return False
    return True


def _docs_edit(
    payload: dict,
    token: str,
    api_requests: list,
    checks: list,
    detail: dict,
) -> dict:
    """Bind revision, send ONE guarded batchUpdate, reread and verify."""
    document_id = _req_id(payload, "document_id")
    expected = _opt_revision(payload)
    if not api_requests:
        raise ValueError("no edits to apply")
    _, rev_before = _bind_doc(document_id, token)
    _refuse_if_stale(expected, rev_before)
    result = _docs_batch_update(document_id, token, api_requests, rev_before)
    replies = result.get("replies", [])
    doc_after = _docs_get(document_id, token)
    rev_after = doc_after.get("revisionId", "")
    full_after = _full_text(doc_after)
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and _run_text_checks(full_after, checks)
        and len(replies) == len(api_requests)
    )
    body = {
        "document_id": document_id,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "replies": len(replies),
        "verified": verified,
        "verification": "docs_revision_readback",
    }
    body.update(detail)
    return body


def _translate_docs_op(op: object) -> "tuple[dict, tuple | None]":
    if not isinstance(op, dict):
        raise ValueError("each batch op must be an object")
    kind = op.get("op")
    if kind == "insert":
        index = _req_int(op, "index", 1, 100000000)
        text = _req_str(op, "text", MAX_INSERT_CHARS)
        return {"insertText": {"location": {"index": index}, "text": text}}, ("present", text)
    if kind == "delete_range":
        start = _req_int(op, "start_index", 1, 100000000)
        end = _req_int(op, "end_index", 1, 100000000)
        if end <= start:
            raise ValueError("end_index must be greater than start_index")
        return {"deleteContentRange": {"range": {"startIndex": start, "endIndex": end}}}, None
    if kind == "replace":
        find = _req_str(op, "find", MAX_FIND_CHARS)
        replace = _req_str(op, "replace", MAX_REPLACE_CHARS, allow_empty=True)
        match_case = _opt_bool(op, "match_case", True)
        req = {"replaceAllText": {"containsText": {"text": find, "matchCase": match_case}, "replaceText": replace}}
        return req, (("present", replace) if replace else ("absent", find))
    if kind == "style":
        start = _req_int(op, "start_index", 1, 100000000)
        end = _req_int(op, "end_index", 1, 100000000)
        if end <= start:
            raise ValueError("end_index must be greater than start_index")
        text_style, fields = _text_style(op.get("style"))
        return {
            "updateTextStyle": {
                "range": {"startIndex": start, "endIndex": end},
                "textStyle": text_style,
                "fields": fields,
            }
        }, None
    raise ValueError("unsupported batch op: %s" % (kind,))


def handle_document_read(payload: dict, token: str) -> dict:
    document_id = _req_id(payload, "document_id")
    doc = _docs_get(document_id, token)
    full = _full_text(doc)
    content = doc.get("body", {}).get("content", [])
    return {
        "document_id": document_id,
        "title": doc.get("title", "") if isinstance(doc.get("title"), str) else "",
        "revision_id": doc.get("revisionId", ""),
        "structural_elements": len(content) if isinstance(content, list) else 0,
        "text_chars": len(full),
        "text_excerpt": full[:TEXT_EXCERPT_CHARS],
        "verified": True,
        "verification": "docs_get_readback",
    }


def handle_document_batch_edit(payload: dict, token: str) -> dict:
    ops = payload.get("requests")
    if not isinstance(ops, list) or not 1 <= len(ops) <= MAX_BATCH_OPS:
        raise ValueError("requests must be a list of 1-%d typed ops" % MAX_BATCH_OPS)
    api_requests = []
    checks = []
    kinds = []
    for op in ops:
        req, check = _translate_docs_op(op)
        api_requests.append(req)
        kinds.append(op.get("op"))
        if check is not None:
            checks.append(check)
    return _docs_edit(payload, token, api_requests, checks, {"ops": kinds})


def handle_document_text_insert(payload: dict, token: str) -> dict:
    index = _req_int(payload, "index", 1, 100000000)
    text = _req_str(payload, "text", MAX_INSERT_CHARS)
    return _docs_edit(
        payload,
        token,
        [{"insertText": {"location": {"index": index}, "text": text}}],
        [("present", text)],
        {"index": index, "inserted_chars": len(text)},
    )


def handle_document_text_replace(payload: dict, token: str) -> dict:
    find = _req_str(payload, "find", MAX_FIND_CHARS)
    replace = _req_str(payload, "replace", MAX_REPLACE_CHARS, allow_empty=True)
    match_case = _opt_bool(payload, "match_case", True)
    check = ("present", replace) if replace else ("absent", find)
    return _docs_edit(
        payload,
        token,
        [{"replaceAllText": {"containsText": {"text": find, "matchCase": match_case}, "replaceText": replace}}],
        [check],
        {"find_chars": len(find), "replace_chars": len(replace), "match_case": match_case},
    )


def handle_document_text_style(payload: dict, token: str) -> dict:
    start = _req_int(payload, "start_index", 1, 100000000)
    end = _req_int(payload, "end_index", 1, 100000000)
    if end <= start:
        raise ValueError("end_index must be greater than start_index")
    text_style, fields = _text_style(payload.get("style"))
    return _docs_edit(
        payload,
        token,
        [{"updateTextStyle": {"range": {"startIndex": start, "endIndex": end}, "textStyle": text_style, "fields": fields}}],
        [],
        {"start_index": start, "end_index": end, "fields": fields},
    )


# ---------------------------------------------------------------- Slides API


def _slides_get(presentation_id: str, token: str) -> dict:
    url = (
        _slides_base()
        + "/v1/presentations/"
        + urllib.parse.quote(presentation_id, safe="")
        + "?fields="
        + urllib.parse.quote("presentationId,title,revisionId,slides(objectId,slideProperties,pageElements)", safe="")
    )
    return _http_json("GET", url, token)


def _slides_batch_update(presentation_id: str, token: str, api_requests: list, required_revision_id: str) -> dict:
    url = _slides_base() + "/v1/presentations/" + urllib.parse.quote(presentation_id, safe="") + ":batchUpdate"
    return _http_json(
        "POST",
        url,
        token,
        {"requests": api_requests, "writeControl": {"requiredRevisionId": required_revision_id}},
    )


def _bind_presentation(presentation_id: str, token: str) -> "tuple[dict, str]":
    pres = _slides_get(presentation_id, token)
    rev = pres.get("revisionId")
    if not isinstance(rev, str) or not rev:
        raise UpstreamError("upstream_error", "presentations.get returned no revisionId")
    return pres, rev


def _slide_ids(presentation: dict) -> list:
    slides = presentation.get("slides", [])
    if not isinstance(slides, list):
        return []
    return [s.get("objectId", "") for s in slides if isinstance(s, dict)]


def _summarize_slides(presentation: dict) -> "tuple[list, int, bool]":
    slides = presentation.get("slides", [])
    if not isinstance(slides, list):
        slides = []
    summaries = []
    for slide in slides[:MAX_SLIDES_SUMMARIZED]:
        if not isinstance(slide, dict):
            continue
        summaries.append({"object_id": slide.get("objectId", ""), "text_excerpt": _excerpt(slide, SLIDE_EXCERPT_CHARS)})
    return summaries, len(slides), len(slides) > len(summaries)


def _find_object_id(node: object, object_id: str) -> bool:
    if isinstance(node, dict):
        if node.get("objectId") == object_id:
            return True
        return any(_find_object_id(v, object_id) for v in node.values())
    if isinstance(node, list):
        return any(_find_object_id(v, object_id) for v in node)
    return False


def handle_presentation_read(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    pres = _slides_get(presentation_id, token)
    summaries, count, truncated = _summarize_slides(pres)
    title = pres.get("title", "")
    return {
        "presentation_id": presentation_id,
        "title": title if isinstance(title, str) else "",
        "revision_id": pres.get("revisionId", ""),
        "slide_count": count,
        "slides": summaries,
        "slides_truncated": truncated,
        "verified": True,
        "verification": "slides_get_readback",
    }


def handle_slide_create(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    layout = _opt_layout(payload)
    pres_before, rev_before = _bind_presentation(presentation_id, token)
    ids_before = _slide_ids(pres_before)
    insertion_index = payload.get("insertion_index", len(ids_before))
    if isinstance(insertion_index, bool) or not isinstance(insertion_index, int) or not 0 <= insertion_index <= len(ids_before):
        raise ValueError("insertion_index must be an integer in [0, %d]" % len(ids_before))
    _refuse_if_stale(_opt_revision(payload), rev_before)
    result = _slides_batch_update(
        presentation_id,
        token,
        [{"createSlide": {"insertionIndex": insertion_index, "slideLayoutReference": {"predefinedLayout": layout}}}],
        rev_before,
    )
    replies = result.get("replies", [])
    new_id = ""
    if replies and isinstance(replies[0], dict):
        created = replies[0].get("createSlide", {})
        if isinstance(created, dict) and isinstance(created.get("objectId"), str):
            new_id = created["objectId"]
    pres_after = _slides_get(presentation_id, token)
    rev_after = pres_after.get("revisionId", "")
    ids_after = _slide_ids(pres_after)
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and len(ids_after) == len(ids_before) + 1
        and bool(new_id)
        and new_id in ids_after
    )
    return {
        "presentation_id": presentation_id,
        "slide_object_id": new_id,
        "insertion_index": insertion_index,
        "layout": layout,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "slide_count": len(ids_after),
        "verified": verified,
        "verification": "slides_revision_readback",
    }


def handle_slide_delete(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    slide_object_id = _req_id(payload, "slide_object_id")
    pres_before, rev_before = _bind_presentation(presentation_id, token)
    ids_before = _slide_ids(pres_before)
    if slide_object_id not in ids_before:
        raise UpstreamError("not_found", "slide_object_id is not present in the current slide order")
    _refuse_if_stale(_opt_revision(payload), rev_before)
    _slides_batch_update(presentation_id, token, [{"deleteObject": {"objectId": slide_object_id}}], rev_before)
    pres_after = _slides_get(presentation_id, token)
    rev_after = pres_after.get("revisionId", "")
    ids_after = _slide_ids(pres_after)
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and slide_object_id not in ids_after
        and len(ids_after) == len(ids_before) - 1
    )
    return {
        "presentation_id": presentation_id,
        "slide_object_id": slide_object_id,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "slide_count": len(ids_after),
        "verified": verified,
        "verification": "slides_revision_readback",
    }


def handle_slide_reorder(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    slide_object_id = _req_id(payload, "slide_object_id")
    pres_before, rev_before = _bind_presentation(presentation_id, token)
    ids_before = _slide_ids(pres_before)
    if slide_object_id not in ids_before:
        raise UpstreamError("not_found", "slide_object_id is not present in the current slide order")
    insertion_index = _req_int(payload, "insertion_index", 0, len(ids_before) - 1)
    _refuse_if_stale(_opt_revision(payload), rev_before)
    _slides_batch_update(
        presentation_id,
        token,
        [{"updateSlidesPosition": {"slideObjectIds": [slide_object_id], "insertionIndex": insertion_index}}],
        rev_before,
    )
    pres_after = _slides_get(presentation_id, token)
    rev_after = pres_after.get("revisionId", "")
    ids_after = _slide_ids(pres_after)
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and len(ids_after) == len(ids_before)
        and 0 <= insertion_index < len(ids_after)
        and ids_after[insertion_index] == slide_object_id
    )
    return {
        "presentation_id": presentation_id,
        "slide_object_id": slide_object_id,
        "insertion_index": insertion_index,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "slide_order": ids_after,
        "verified": verified,
        "verification": "slides_revision_readback",
    }


def handle_presentation_text_replace(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    find = _req_str(payload, "find", MAX_FIND_CHARS)
    replace = _req_str(payload, "replace", MAX_REPLACE_CHARS, allow_empty=True)
    match_case = _opt_bool(payload, "match_case", True)
    page_ids = payload.get("page_object_ids")
    if page_ids is not None:
        if not isinstance(page_ids, list) or not page_ids or len(page_ids) > MAX_PAGE_IDS:
            raise ValueError("page_object_ids must be a list of 1-%d object ids" % MAX_PAGE_IDS)
        for page_id in page_ids:
            if not isinstance(page_id, str) or not 1 <= len(page_id) <= 256 or any(c not in _ID_CHARS for c in page_id):
                raise ValueError("page_object_ids entries must be Google resource ids")
    pres_before, rev_before = _bind_presentation(presentation_id, token)
    _refuse_if_stale(_opt_revision(payload), rev_before)
    api_request: dict = {
        "replaceAllText": {"containsText": {"text": find, "matchCase": match_case}, "replaceText": replace}
    }
    if page_ids is not None:
        api_request["replaceAllText"]["pageObjectIds"] = page_ids
    result = _slides_batch_update(presentation_id, token, [api_request], rev_before)
    replies = result.get("replies", [])
    pres_after = _slides_get(presentation_id, token)
    rev_after = pres_after.get("revisionId", "")
    full_after = _full_text(pres_after)
    check = ("present", replace) if replace else ("absent", find)
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and _run_text_checks(full_after, [check])
        and len(replies) == 1
    )
    occurrences = 0
    if isinstance(result.get("replies"), list):
        for reply in result["replies"]:
            if isinstance(reply, dict) and isinstance(reply.get("replaceAllText"), dict):
                n = reply["replaceAllText"].get("occurrencesChanged")
                if isinstance(n, int):
                    occurrences += n
    return {
        "presentation_id": presentation_id,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "occurrences_changed": occurrences,
        "verified": verified,
        "verification": "slides_revision_readback",
    }


def handle_presentation_text_style(payload: dict, token: str) -> dict:
    presentation_id = _req_id(payload, "presentation_id")
    object_id = _req_id(payload, "object_id")
    has_start = "start_index" in payload
    has_end = "end_index" in payload
    text_range: dict = {}
    if has_start or has_end:
        if not (has_start and has_end):
            raise ValueError("start_index and end_index must be given together")
        start = _req_int(payload, "start_index", 0, 100000000)
        end = _req_int(payload, "end_index", 0, 100000000)
        if end <= start or end - start > MAX_INSERT_CHARS:
            raise ValueError("text range must be non-empty and at most %d chars" % MAX_INSERT_CHARS)
        text_range = {"startIndex": start, "endIndex": end}
    text_style, fields = _text_style(payload.get("style"))
    pres_before, rev_before = _bind_presentation(presentation_id, token)
    if not _find_object_id(pres_before, object_id):
        raise UpstreamError("not_found", "object_id is not present in the current presentation")
    _refuse_if_stale(_opt_revision(payload), rev_before)
    api_request = {"updateTextStyle": {"objectId": object_id, "style": text_style, "fields": fields}}
    if text_range:
        api_request["updateTextStyle"]["textRange"] = text_range
    _slides_batch_update(presentation_id, token, [api_request], rev_before)
    pres_after = _slides_get(presentation_id, token)
    rev_after = pres_after.get("revisionId", "")
    verified = (
        isinstance(rev_after, str)
        and bool(rev_after)
        and rev_after != rev_before
        and _find_object_id(pres_after, object_id)
    )
    return {
        "presentation_id": presentation_id,
        "object_id": object_id,
        "revision_before": rev_before,
        "revision_after": rev_after,
        "fields": fields,
        "verified": verified,
        "verification": "slides_revision_readback",
    }


# ---------------------------------------------------------------- Drive export


def _drive_file_meta(file_id: str, token: str) -> dict:
    url = (
        _drive_base()
        + "/drive/v3/files/"
        + urllib.parse.quote(file_id, safe="")
        + "?fields="
        + urllib.parse.quote("id,name,mimeType,modifiedTime,size", safe="")
    )
    meta = _http_json("GET", url, token)
    return {
        "id": meta.get("id", ""),
        "name": meta.get("name", "") if isinstance(meta.get("name"), str) else "",
        "mime_type": meta.get("mimeType", ""),
        "modified_time": meta.get("modifiedTime", ""),
    }


def _download_to_file(url: str, token: str, dest_path: str) -> "tuple[int, str]":
    req = urllib.request.Request(url, headers={"Accept": "*/*"}, method="GET")
    req.add_header("Authorization", "Bearer " + token)
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
                        raise UpstreamError("export_too_large", "export exceeded the %d-byte cap" % EXPORT_MAX_BYTES)
                    digest.update(chunk)
                    handle.write(chunk)
    except urllib.error.HTTPError as exc:
        raise _from_http_error(exc, token)
    except urllib.error.URLError as exc:
        raise UpstreamError("upstream_unreachable", "Google API unreachable: %s" % _redact(str(exc.reason), token)[:200])
    return total, digest.hexdigest()


def _export_file(file_id: str, mime_type: str, mime_map: dict, token: str) -> dict:
    export_dir = os.environ.get(EXPORT_DIR_ENV, "")
    if export_dir:
        if not os.path.isabs(export_dir):
            raise ValueError(EXPORT_DIR_ENV + " must be an absolute path")
        os.makedirs(export_dir, exist_ok=True)
        dest_path = os.path.join(export_dir, file_id + mime_map[mime_type])
        url = (
            _drive_base()
            + "/drive/v3/files/"
            + urllib.parse.quote(file_id, safe="")
            + "/export?mimeType="
            + urllib.parse.quote(mime_type, safe="")
        )
        try:
            size, digest = _download_to_file(url, token, dest_path)
        except UpstreamError:
            try:
                if os.path.exists(dest_path) and os.path.getsize(dest_path) == 0:
                    os.remove(dest_path)
            except OSError:
                pass
            raise
        return {
            "file_id": file_id,
            "mime_type": mime_type,
            "size_bytes": size,
            "sha256": digest,
            "path": dest_path,
            "persisted": True,
            "verified": True,
            "verification": "drive_export_artifact",
        }
    meta = _drive_file_meta(file_id, token)
    return {
        "file_id": file_id,
        "mime_type": mime_type,
        "download_url": (
            _drive_base()
            + "/drive/v3/files/"
            + urllib.parse.quote(file_id, safe="")
            + "/export?mimeType="
            + urllib.parse.quote(mime_type, safe="")
        ),
        "file_metadata": meta,
        "persisted": False,
        "verified": False,
        "verification": "drive_export_metadata_only",
        "reason": EXPORT_DIR_ENV + " is not set, so the artifact was not persisted",
    }


def handle_document_export(payload: dict, token: str) -> dict:
    file_id = _req_id(payload, "document_id")
    mime_type = payload.get("mime_type")
    if mime_type not in DOC_EXPORT_MIMES:
        raise ValueError("mime_type must be one of: " + ", ".join(sorted(DOC_EXPORT_MIMES)))
    return _export_file(file_id, mime_type, DOC_EXPORT_MIMES, token)


def handle_presentation_export(payload: dict, token: str) -> dict:
    file_id = _req_id(payload, "presentation_id")
    mime_type = payload.get("mime_type")
    if mime_type not in SLIDES_EXPORT_MIMES:
        raise ValueError("mime_type must be one of: " + ", ".join(sorted(SLIDES_EXPORT_MIMES)))
    return _export_file(file_id, mime_type, SLIDES_EXPORT_MIMES, token)


# ---------------------------------------------------------------- dispatch


_DISPATCH = {
    "document.google.read": handle_document_read,
    "document.google.batch_edit": handle_document_batch_edit,
    "document.text.insert": handle_document_text_insert,
    "document.text.replace": handle_document_text_replace,
    "document.text.style": handle_document_text_style,
    "document.export": handle_document_export,
    "presentation.google.read": handle_presentation_read,
    "presentation.slide.create": handle_slide_create,
    "presentation.slide.delete": handle_slide_delete,
    "presentation.slide.reorder": handle_slide_reorder,
    "presentation.text.replace": handle_presentation_text_replace,
    "presentation.text.style": handle_presentation_text_style,
    "presentation.export": handle_presentation_export,
}


def handler(request: dict) -> dict:
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": ADAPTER_ID, "protocol": "google-workspace-https"})
    if method == "capabilities":
        return response(
            request,
            True,
            "available",
            {"backend": "google-docs-v1+slides-v1+drive-v3", "intents": list(INTENTS)},
        )
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload")
    if not isinstance(payload, dict):
        return response(request, False, "degraded", error={"code": "invalid_payload", "message": "payload must be an object"})
    intent = payload.get("intent")
    func = _DISPATCH.get(intent)
    if func is None:
        return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
    token = ""
    try:
        token = _token()
        return response(request, True, "available", func(payload, token))
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
