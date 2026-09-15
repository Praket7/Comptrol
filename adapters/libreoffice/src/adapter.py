#!/usr/bin/env python3
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402


def connect():
    import uno
    local = uno.getComponentContext()
    resolver = local.ServiceManager.createInstanceWithContext("com.sun.star.bridge.UnoUrlResolver", local)
    endpoint = os.environ.get("COMPTROL_LIBREOFFICE_UNO", "socket,host=127.0.0.1,port=2002;urp;StarOffice.ComponentContext")
    return resolver.resolve("uno:" + endpoint)


def handler(request):
    method = request.get("method")
    if method == "handshake":
        return response(request, True, "available", {"adapter": "comptrol.libreoffice", "backend": "UNO"})
    if method == "capabilities":
        return response(request, True, "available", {"backend": "official_uno_api"})
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    try:
        context = connect()
        desktop = context.ServiceManager.createInstanceWithContext("com.sun.star.frame.Desktop", context)
        intent = request.get("payload", {}).get("intent")
        if intent == "libreoffice.calc.range.read":
            document = desktop.getCurrentComponent()
            sheet = document.Sheets.getByName(str(request["payload"].get("sheet", "Sheet1")))
            cell_range = sheet.getCellRangeByName(str(request["payload"]["range"]))
            values = [list(row) for row in cell_range.getDataArray()]
            return response(request, True, "available", {"values": values, "verified": True})
        if intent == "libreoffice.calc.range.write":
            document = desktop.getCurrentComponent()
            sheet = document.Sheets.getByName(str(request["payload"].get("sheet", "Sheet1")))
            cell_range = sheet.getCellRangeByName(str(request["payload"]["range"]))
            values = request["payload"].get("values")
            if not isinstance(values, list) or any(not isinstance(row, list) for row in values):
                raise ValueError("values must be a two-dimensional array")
            cell_range.setDataArray(tuple(tuple(row) for row in values))
            return response(request, True, "available", {"written": True, "values": [list(row) for row in cell_range.getDataArray()], "verified": True})
        if intent == "libreoffice.document.save":
            document = desktop.getCurrentComponent()
            document.store()
            return response(request, True, "available", {"saved": True, "modified": document.isModified(), "verified": not document.isModified()})
        return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
    except ImportError as exc:
        return response(request, False, "unsupported", error={"code": "uno_unavailable", "message": str(exc)})
    except Exception as exc:
        return response(request, False, "unhealthy", error={"code": "uno_request_failed", "message": str(exc)})


serve(handler)
