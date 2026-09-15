#!/usr/bin/env python3
import os
import sys
from pathlib import Path

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
            return response(request, True, "available", {"values": values, "verified": True})
        if intent == "libreoffice.calc.range.write":
            document = select_document(current_desktop, payload)
            sheet = document.Sheets.getByName(str(request["payload"].get("sheet", "Sheet1")))
            cell_range = sheet.getCellRangeByName(str(request["payload"]["range"]))
            values = request["payload"].get("values")
            if not isinstance(values, list) or any(not isinstance(row, list) for row in values):
                raise ValueError("values must be a two-dimensional array")
            cell_range.setDataArray(tuple(tuple(row) for row in values))
            return response(request, True, "available", {"written": True, "values": [list(row) for row in cell_range.getDataArray()], "verified": True})
        if intent == "libreoffice.document.save":
            document = select_document(current_desktop, payload)
            document.store()
            return response(request, True, "available", {"saved": True, "document_url": str(document.getURL()), "document_title": str(document.getTitle()), "modified": document.isModified(), "verified": not document.isModified()})
        return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
    except ImportError as exc:
        return response(request, False, "unsupported", error={"code": "uno_unavailable", "message": str(exc)})
    except Exception as exc:
        global CONTEXT, DESKTOP
        CONTEXT = None
        DESKTOP = None
        return response(request, False, "unhealthy", error={"code": "uno_request_failed", "message": str(exc)})


serve(handler)
