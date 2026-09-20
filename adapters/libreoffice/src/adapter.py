#!/usr/bin/env python3
import os
import sys
from pathlib import Path
from urllib.parse import unquote, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

CONTEXT = None
DESKTOP = None

def connect():
    import uno
    local = uno.getComponentContext()
    resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
    endpoint = os.environ.get("COMPTROL_LIBREOFFICE_UNO", "socket,host=127.0.0.1,port=2002;urp;StarOffice.ComponentContext")
    return resolver.resolve("uno:" + endpoint)


def desktop():
    global CONTEXT, DESKTOP
    if DESKTOP is None:
        CONTEXT = connect()
        DESKTOP = CONTEXT.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", CONTEXT)
    return DESKTOP


def documents(current_desktop):
    enumeration = current_desktop.getComponents().createEnumeration()
    result = []
    while enumeration.hasMoreElements():
        result.append(enumeration.nextElement())
    return result


def select_document(current_desktop, payload):
    candidates = documents(current_desktop)
    requested_url = str(payload.get("document_url", "")).strip()
    requested_title = str(payload.get("document_title", "")).strip()
    if requested_url:
        candidates = [document for document in candidates if str(document.getURL()) == requested_url]
    if requested_title:
        candidates = [document for document in candidates if str(document.getTitle()) == requested_title]
    if len(candidates) != 1:
        raise ValueError("document_identity_required: provide a unique document_url or document_title")
    return candidates[0]


def file_url_to_path(url):
    """Convert a file:// URL to a local path; return None for remote URLs."""
    parsed = urlparse(url)
    if parsed.scheme == "file":
        return unquote(parsed.path)
    if not parsed.scheme:
        return url
    return None


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.libreoffice", "backend": "UNO"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "official_uno_api"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    try:
        current_desktop = desktop()
        intent = request.get("payload", {}).get("intent")
        payload = request.get("payload", {})
        if intent == "libreoffice.calc.range.read":
            document = select_document(current_desktop, payload)
            sheet = document.Sheets.getByName(str(request["payload"].get("sheet", "Sheet1")))
            cell_range = sheet.getCellRangeByName(str(request["payload"]["range"]))
            values = [list(row) for row in cell_range.getDataArray()]
            return response(request, True, "available", {"values": values, "verified": True, "verification": "uno_range_readback"})
        if intent == "libreoffice.calc.range.write":
            document = select_document(current_desktop, payload)
            sheet = document.Sheets.getByName(str(request["payload"].get("sheet", "Sheet1")))
            cell_range = sheet.getCellRangeByName(str(request["payload"]["range"]))
            values = request["payload"].get("values")
            if not isinstance(values, list) or any(not isinstance(row, list) for row in values):
                raise ValueError("values must be a two-dimensional array")
            cell_range.setDataArray(tuple(tuple(row) for row in values))
            return response(request, True, "available", {"written": True, "values": [list(row) for row in cell_range.getDataArray()], "verified": True, "verification": "uno_range_readback"})
        if intent == "libreoffice.document.save":
            document = select_document(current_desktop, payload)
            document.store()
            return response(request, True, "available", {"saved": True, "document_url": str(document.getURL()), "document_title": str(document.getTitle()), "modified": document.isModified(), "verified": not document.isModified(), "verification": "uno_persistence_readback"})
        if intent == "libreoffice.document.open":
            import uno
            target = str(payload.get("path", "")).strip()
            url = str(payload.get("document_url", "")).strip()
            if target:
                url = uno.systemPathToFileUrl(target)
            if not url:
                raise ValueError("path or document_url is required")
            component = current_desktop.loadComponentFromURL(url, "_blank", 0, ())
            if component is None:
                return response(request, False, "available", error={
                    "code": "document_open_failed",
                    "message": f"LibreOffice did not open {url}",
                })
            opened_url = str(component.getURL())
            verified = opened_url == url
            return response(request, True, "available", {
                "opened": True,
                "document_url": opened_url,
                "document_title": str(component.getTitle()),
                "verified": verified,
                "verification": "uno_url_readback",
            })
        if intent == "libreoffice.writer.text.replace":
            document = select_document(current_desktop, payload)
            if not document.supportsService("com.sun.star.text.TextDocument"):
                raise ValueError("writer.text.replace requires a text document")
            search = str(payload.get("find", ""))
            replacement = str(payload.get("replace", ""))
            if not search:
                raise ValueError("find is required")
            descriptor = document.createReplaceDescriptor()
            descriptor.SearchString = search
            descriptor.ReplaceString = replacement
            replaced = int(document.replaceAll(descriptor))
            # Independent readback: the search string must no longer occur
            # unless the replacement reintroduced it.
            probe = document.createSearchDescriptor()
            probe.SearchString = search
            remaining = 0
            while document.findNext(document.Text.Start, probe) is not None:
                remaining += 1
                if remaining > 1000:
                    break
            verified = remaining == 0 or search in replacement
            return response(request, True, "available", {
                "replaced": replaced,
                "remaining_occurrences": remaining,
                "verified": verified,
                "verification": "uno_replace_readback",
            })
        if intent == "libreoffice.document.export":
            document = select_document(current_desktop, payload)
            output_url = str(payload.get("output_url", "")).strip()
            if not output_url:
                raise ValueError("output_url is required")
            document.storeToURL(output_url, ())
            # Persisted-artifact verification: the exported file must exist
            # with nonzero size on the local filesystem when observable.
            path = file_url_to_path(output_url)
            verified = False
            details = {"exported": True, "output_url": output_url, "modified": document.isModified(), "verification": "uno_export_readback"}
            if path is not None:
                exported = Path(path)
                exists = exported.is_file()
                size = exported.stat().st_size if exists else 0
                verified = exists and size > 0
                details["output_size"] = size
            else:
                details["verification"] = "uno_export_dispatch_only"
            details["verified"] = verified
            return response(request, True, "available", details)
        return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
    except ImportError as exc:
        return response(request, False, "unsupported", error={"code": "uno_unavailable", "message": str(exc)})
    except Exception as exc:
        global CONTEXT, DESKTOP
        CONTEXT = None
        DESKTOP = None
        return response(request, False, "unhealthy", error={"code": "uno_request_failed", "message": str(exc)})


serve(handler)
