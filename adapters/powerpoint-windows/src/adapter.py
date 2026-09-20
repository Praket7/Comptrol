#!/usr/bin/env python3
"""Deep desktop PowerPoint adapter over Windows COM (comtypes).

Binds the exact presentation by full path and never touches blind
ActivePresentation without a uniqueness check. Macros/VBA are never
executed. All comtypes imports are lazy so the file compiles on macOS.
"""
import hashlib
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))
from adapter_protocol import response, serve  # noqa: E402

ADAPTER_ID = "comptrol.powerpoint-windows"
OPEN_EXTS = {".pptx", ".pptm", ".ppt"}
MACRO_KEYS = {"macro", "macros", "vba", "vba_macro", "run_macro", "execute_macro", "vbproject", "oleobject"}

PP_FIXED_FORMAT_PDF = 2
PP_LAYOUT_BLANK = 12


def comtypes_available():
    try:
        import comtypes.client  # noqa: F401
        return True
    except ImportError:
        return False


def require_comtypes():
    try:
        import comtypes.client as client
        return client
    except ImportError as exc:
        raise RuntimeError("comtypes_unavailable: comtypes is not installed; Windows COM route unavailable (%s)" % exc)


def scope_root():
    raw = os.environ.get("COMPTROL_PRESENTATIONS_ROOT", "").strip()
    if not raw:
        return None
    return Path(raw).resolve()


def scoped_resolve(raw, allowed_exts=None, must_exist=True):
    if not isinstance(raw, str) or not raw.strip():
        raise ValueError("path is required")
    candidate = Path(raw.strip()).resolve()
    root = scope_root()
    if root is not None:
        try:
            candidate.relative_to(root)
        except ValueError:
            raise ValueError("path_out_of_scope: path is outside COMPTROL_PRESENTATIONS_ROOT")
    if allowed_exts is not None and candidate.suffix.lower() not in allowed_exts:
        raise ValueError("unsupported extension for %s (got %s)" % (sorted(allowed_exts), candidate.suffix))
    if must_exist and not candidate.is_file():
        raise ValueError("file not found: %s" % candidate)
    return candidate


def sha256_of(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def reject_macro_requests(payload):
    if not isinstance(payload, dict):
        return
    for key in payload.keys():
        if isinstance(key, str) and key.strip().lower() in MACRO_KEYS:
            raise ValueError("macro_execution_refused: VBA/macros are never executed")


def normalize_full_name(full_name):
    try:
        return os.path.normcase(os.path.abspath(str(full_name)))
    except Exception:
        return os.path.normcase(str(full_name))


def get_app():
    client = require_comtypes()
    app = client.CreateObject("PowerPoint.Application")
    return app


def iter_presentations(app):
    try:
        count = int(app.Presentations.Count)
    except Exception:
        return []
    result = []
    for index in range(1, count + 1):
        try:
            result.append(app.Presentations(index))
        except Exception:
            continue
    return result


def presentation_state(pres):
    try:
        full_name = str(pres.FullName)
    except Exception:
        full_name = ""
    try:
        name = str(pres.Name)
    except Exception:
        name = ""
    try:
        slide_count = int(pres.Slides.Count)
    except Exception:
        slide_count = -1
    return {"name": name, "full_name": full_name, "slide_count": slide_count}


def bind_presentation(app, payload, allow_active_fallback=True):
    """Bind the exact presentation by full path.

    Never returns a blind ActivePresentation: without an explicit
    presentation_path the deck must be uniquely identifiable (exactly one
    open presentation), otherwise binding is refused as ambiguous.
    """
    requested_raw = payload.get("presentation_path", payload.get("path", ""))
    presentations = iter_presentations(app)
    if isinstance(requested_raw, str) and requested_raw.strip():
        requested = Path(requested_raw.strip()).resolve()
        matches = [pres for pres in presentations if normalize_full_name(presentation_state(pres)["full_name"]) == os.path.normcase(str(requested))]
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise ValueError("ambiguous_presentation: multiple open presentations match %s" % requested)
        return None
    if not allow_active_fallback:
        raise ValueError("presentation_identity_required: provide presentation_path")
    if len(presentations) == 1:
        return presentations[0]
    if not presentations:
        raise ValueError("presentation_not_open: no presentation is open")
    raise ValueError("ambiguous_presentation: %d presentations open; provide presentation_path" % len(presentations))


def check_slide_ref(value, slide_count, field="slide"):
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError("%s must be a 1-based slide number" % field)
    if value < 1 or value > slide_count:
        raise ValueError("%s %s out of range (1..%s)" % (field, value, slide_count))
    return value


def slide_ids(pres):
    ids = []
    count = int(pres.Slides.Count)
    for index in range(1, count + 1):
        try:
            ids.append(int(pres.Slides(index).SlideID))
        except Exception:
            ids.append(-1)
    return ids


def do_open(app, payload):
    raw_path = payload.get("path", payload.get("presentation_path", ""))
    path = scoped_resolve(raw_path, allowed_exts=OPEN_EXTS, must_exist=True)
    bound = bind_presentation(app, {"presentation_path": str(path)}, allow_active_fallback=False)
    if bound is None:
        opened = app.Presentations.Open(str(path))
        bound = opened
        # Re-bind by exact full path so a similarly named deck cannot be mistaken.
        rebound = bind_presentation(app, {"presentation_path": str(path)}, allow_active_fallback=False)
        if rebound is not None:
            bound = rebound
    state = presentation_state(bound)
    verified = normalize_full_name(state["full_name"]) == os.path.normcase(str(path))
    return {
        **state,
        "input_path": str(path),
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_slide_create(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    before = int(bound.Slides.Count)
    index = payload.get("index")
    if index is not None:
        check_slide_ref(index, before + 1, field="index")
    insert_at = index if isinstance(index, int) else before + 1
    new_slide = bound.Slides.Add(insert_at, PP_LAYOUT_BLANK)
    title = payload.get("title")
    if isinstance(title, str) and title:
        if len(title) > 300:
            raise ValueError("title must be at most 300 chars")
        try:
            new_slide.Shapes.Title.TextFrame.TextRange.Text = title
        except Exception:
            try:
                box = new_slide.Shapes.AddTextbox(1, 0, 0, 500, 100)
                box.TextFrame.TextRange.Text = title
            except Exception:
                pass
    if isinstance(index, int):
        try:
            new_slide.MoveTo(index)
        except Exception:
            pass
    after = int(bound.Slides.Count)
    state = presentation_state(bound)
    verified = after == before + 1
    return {
        **state,
        "slide_count_before": before,
        "slide_count_after": after,
        "inserted_at": index if isinstance(index, int) else after,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_slide_delete(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    before = int(bound.Slides.Count)
    slide_num = check_slide_ref(payload.get("slide"), before)
    if before <= 1:
        raise ValueError("slide.delete refused: presentation must keep at least one slide")
    bound.Slides(slide_num).Delete()
    after = int(bound.Slides.Count)
    state = presentation_state(bound)
    verified = after == before - 1
    return {
        **state,
        "slide_count_before": before,
        "slide_count_after": after,
        "deleted": slide_num,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_slide_reorder(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    count = int(bound.Slides.Count)
    from_idx = check_slide_ref(payload.get("slide"), count)
    to_idx = check_slide_ref(payload.get("to"), count)
    order_before = slide_ids(bound)
    moved_id = order_before[from_idx - 1]
    bound.Slides(from_idx).MoveTo(to_idx)
    order_after = slide_ids(bound)
    state = presentation_state(bound)
    verified = len(order_after) == count and order_after[to_idx - 1] == moved_id
    return {
        **state,
        "order_before": order_before,
        "order_after": order_after,
        "moved_slide_id": moved_id,
        "from": from_idx,
        "to": to_idx,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def find_shape(slide, selector):
    count = int(slide.Shapes.Count)
    if isinstance(selector, bool):
        raise ValueError("shape must be a shape name or 1-based index")
    if isinstance(selector, int):
        if selector < 1 or selector > count:
            raise ValueError("shape index %s out of range (1..%s)" % (selector, count))
        return slide.Shapes(selector)
    if isinstance(selector, str) and selector.strip():
        wanted = selector.strip()
        try:
            return slide.Shapes(wanted)
        except Exception:
            pass
        for index in range(1, count + 1):
            try:
                shape = slide.Shapes(index)
                if str(shape.Name) == wanted:
                    return shape
            except Exception:
                continue
        raise ValueError("shape not found: %s" % wanted)
    raise ValueError("shape must be a shape name or 1-based index")


def do_shape_text_set(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    count = int(bound.Slides.Count)
    slide_num = check_slide_ref(payload.get("slide"), count)
    text = payload.get("text")
    if not isinstance(text, str):
        raise ValueError("text must be a string")
    if len(text) > 2000:
        raise ValueError("text must be at most 2000 chars")
    slide = bound.Slides(slide_num)
    shape = find_shape(slide, payload.get("shape"))
    try:
        shape.TextFrame.TextRange.Text = text
    except Exception as exc:
        raise ValueError("shape does not accept text: %s" % exc)
    try:
        readback = str(shape.TextFrame.TextRange.Text)
    except Exception:
        readback = ""
    state = presentation_state(bound)
    verified = readback == text
    try:
        shape_name = str(shape.Name)
    except Exception:
        shape_name = ""
    return {
        **state,
        "slide": slide_num,
        "shape": shape_name,
        "text": text,
        "readback": readback,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def verify_saved_file(bound):
    try:
        full_name = str(bound.FullName)
    except Exception:
        full_name = ""
    path = Path(full_name) if full_name else None
    exists = path is not None and path.is_file()
    size = path.stat().st_size if exists else 0
    return path, exists, size


def do_save(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    sha_before = ""
    try:
        existing = Path(str(bound.FullName))
        if existing.is_file():
            sha_before = sha256_of(existing)
    except Exception:
        pass
    bound.Save()
    state = presentation_state(bound)
    path, exists, size = verify_saved_file(bound)
    sha_after = sha256_of(path) if exists and size > 0 else ""
    verified = exists and size > 0
    return {
        **state,
        "saved_path": str(path) if path is not None else "",
        "output_size": size,
        "sha256_before": sha_before,
        "sha256_after": sha_after,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "persisted_artifact",
    }


def do_export_pdf(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    output_path = scoped_resolve(payload.get("output_path", ""), allowed_exts={".pdf"}, must_exist=False)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    bound.ExportAsFixedFormat(str(output_path), PP_FIXED_FORMAT_PDF)
    exists = output_path.is_file()
    size = output_path.stat().st_size if exists else 0
    state = presentation_state(bound)
    verified = exists and size > 0
    return {
        **state,
        "output_path": str(output_path),
        "output_size": size,
        "sha256": sha256_of(output_path) if verified else "",
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "persisted_artifact",
    }


def handler(request):
    method = request.get("method")
    available = comtypes_available()
    if method == "handshake":
        return response(
            request,
            True,
            "available" if available else "unsupported",
            {
                "adapter": ADAPTER_ID,
                "backend": "windows-com" if available else "unavailable",
                "comtypes_available": available,
                "route": "comtypes_exact_path_binding",
                "macros_executed": False,
                "note": "" if available else "comtypes is not importable on this host; Windows COM route unavailable",
            },
        )
    if method == "capabilities":
        return response(
            request,
            True,
            "available" if available else "unsupported",
            {
                "backend": "windows-com" if available else "unavailable",
                "comtypes_available": available,
                "intents": [
                    "presentation.desktop.open",
                    "presentation.slide.create",
                    "presentation.slide.delete",
                    "presentation.slide.reorder",
                    "presentation.shape.text.set",
                    "presentation.save",
                    "presentation.export_pdf",
                ],
            },
        )
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload", {}) if isinstance(request.get("payload"), dict) else {}
    intent = payload.get("intent")
    try:
        reject_macro_requests(payload)
        if not available:
            return response(
                request,
                False,
                "unsupported",
                error={"code": "comtypes_unavailable", "message": "comtypes is not importable; Windows COM route unavailable"},
            )
        app = get_app()
        if intent == "presentation.desktop.open":
            result = do_open(app, payload)
        elif intent == "presentation.slide.create":
            result = do_slide_create(app, payload)
        elif intent == "presentation.slide.delete":
            result = do_slide_delete(app, payload)
        elif intent == "presentation.slide.reorder":
            result = do_slide_reorder(app, payload)
        elif intent == "presentation.shape.text.set":
            result = do_shape_text_set(app, payload)
        elif intent == "presentation.save":
            result = do_save(app, payload)
        elif intent == "presentation.export_pdf":
            result = do_export_pdf(app, payload)
        else:
            return response(request, False, "unsupported", error={"code": "unsupported_intent", "message": str(intent)})
        health = "available" if result.get("verified") else "degraded"
        return response(request, bool(result.get("verified")), health, result)
    except ValueError as exc:
        message = str(exc)
        code = "invalid_request"
        if message.startswith("macro_execution_refused"):
            code = "macro_execution_refused"
        elif message.startswith("ambiguous_presentation") or message.startswith("presentation_identity_required"):
            code = "presentation_identity_required"
        elif message.startswith("presentation_not_open"):
            code = "presentation_not_open"
        elif message.startswith("path_out_of_scope"):
            code = "path_out_of_scope"
        health = "unsupported" if code in ("presentation_not_open",) else "unhealthy"
        if code in ("presentation_identity_required", "path_out_of_scope", "macro_execution_refused"):
            health = "unhealthy"
        return response(request, False, health, error={"code": code, "message": message})
    except RuntimeError as exc:
        message = str(exc)
        code = "comtypes_unavailable" if message.startswith("comtypes_unavailable") else "com_request_failed"
        health = "unsupported" if code == "comtypes_unavailable" else "degraded"
        return response(request, False, health, error={"code": code, "message": message})
    except Exception as exc:
        return response(request, False, "unhealthy", error={"code": "com_request_failed", "message": str(exc)})


serve(handler)
