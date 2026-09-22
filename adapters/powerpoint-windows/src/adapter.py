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
MAX_BATCH_OPS = 64
IMAGE_EXTS = {".png", ".jpg", ".jpeg", ".gif", ".bmp", ".tif", ".tiff", ".svg", ".emf", ".wmf"}
MSO_TEXT_ORIENTATION_HORIZONTAL = 1
MSO_FALSE = 0
MSO_TRUE = -1


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


def number_param(payload, key, *, required=False, minimum=None, maximum=None):
    value = payload.get(key)
    if value is None and not required:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError("%s must be a number" % key)
    value = float(value)
    if minimum is not None and value < minimum:
        raise ValueError("%s must be >= %s" % (key, minimum))
    if maximum is not None and value > maximum:
        raise ValueError("%s must be <= %s" % (key, maximum))
    return value


def hex_rgb(value):
    if not isinstance(value, str):
        raise ValueError("font_color must be a #RRGGBB string")
    raw = value.strip().lstrip("#")
    if len(raw) != 6:
        raise ValueError("font_color must be a #RRGGBB string")
    try:
        red = int(raw[0:2], 16)
        green = int(raw[2:4], 16)
        blue = int(raw[4:6], 16)
    except ValueError:
        raise ValueError("font_color must be a #RRGGBB string")
    return red + (green << 8) + (blue << 16)


def apply_text_style(shape, payload):
    try:
        font = shape.TextFrame.TextRange.Font
    except Exception as exc:
        raise ValueError("shape does not expose a text font: %s" % exc)
    if "font_size" in payload:
        font.Size = number_param(payload, "font_size", required=True, minimum=1, maximum=400)
    if "font_name" in payload:
        name = payload.get("font_name")
        if not isinstance(name, str) or not name.strip() or len(name) > 100:
            raise ValueError("font_name must be a non-empty string up to 100 chars")
        font.Name = name.strip()
    if "bold" in payload:
        if not isinstance(payload.get("bold"), bool):
            raise ValueError("bold must be boolean")
        font.Bold = MSO_TRUE if payload["bold"] else MSO_FALSE
    if "italic" in payload:
        if not isinstance(payload.get("italic"), bool):
            raise ValueError("italic must be boolean")
        font.Italic = MSO_TRUE if payload["italic"] else MSO_FALSE
    if "font_color" in payload:
        font.Color.RGB = hex_rgb(payload["font_color"])


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


def do_slide_duplicate(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    before = int(bound.Slides.Count)
    slide_num = check_slide_ref(payload.get("slide"), before)
    source_id = int(bound.Slides(slide_num).SlideID)
    duplicated = bound.Slides(slide_num).Duplicate()
    after = int(bound.Slides.Count)
    duplicate_id = int(duplicated(1).SlideID)
    target = payload.get("to")
    if target is not None:
        check_slide_ref(target, after, field="to")
        duplicated(1).MoveTo(target)
    order_after = slide_ids(bound)
    verified = after == before + 1 and duplicate_id != source_id and duplicate_id in order_after
    return {
        **presentation_state(bound),
        "slide_count_before": before,
        "slide_count_after": after,
        "source_slide_id": source_id,
        "duplicate_slide_id": duplicate_id,
        "moved_to": target,
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
    if any(key in payload for key in ("font_size", "font_name", "bold", "italic", "font_color")):
        apply_text_style(shape, payload)
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


def do_shape_textbox_create(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    slide_num = check_slide_ref(payload.get("slide"), int(bound.Slides.Count))
    left = number_param(payload, "left", required=True, minimum=-10000, maximum=20000)
    top = number_param(payload, "top", required=True, minimum=-10000, maximum=20000)
    width = number_param(payload, "width", required=True, minimum=1, maximum=20000)
    height = number_param(payload, "height", required=True, minimum=1, maximum=20000)
    text = payload.get("text", "")
    if not isinstance(text, str) or len(text) > 10000:
        raise ValueError("text must be a string up to 10000 chars")
    slide = bound.Slides(slide_num)
    before = int(slide.Shapes.Count)
    shape = slide.Shapes.AddTextbox(MSO_TEXT_ORIENTATION_HORIZONTAL, left, top, width, height)
    shape.TextFrame.TextRange.Text = text
    name = payload.get("name")
    if name is not None:
        if not isinstance(name, str) or not name.strip() or len(name) > 100:
            raise ValueError("name must be a non-empty string up to 100 chars")
        shape.Name = name.strip()
    if any(key in payload for key in ("font_size", "font_name", "bold", "italic", "font_color")):
        apply_text_style(shape, payload)
    after = int(slide.Shapes.Count)
    shape_name = str(shape.Name)
    verified = after == before + 1 and str(shape.TextFrame.TextRange.Text) == text
    return {
        **presentation_state(bound),
        "slide": slide_num,
        "shape": shape_name,
        "shape_count_before": before,
        "shape_count_after": after,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_shape_image_insert(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    slide_num = check_slide_ref(payload.get("slide"), int(bound.Slides.Count))
    image_path = scoped_resolve(payload.get("image_path", ""), allowed_exts=IMAGE_EXTS, must_exist=True)
    left = number_param(payload, "left", required=True, minimum=-10000, maximum=20000)
    top = number_param(payload, "top", required=True, minimum=-10000, maximum=20000)
    width = number_param(payload, "width", required=True, minimum=1, maximum=20000)
    height = number_param(payload, "height", required=True, minimum=1, maximum=20000)
    slide = bound.Slides(slide_num)
    before = int(slide.Shapes.Count)
    shape = slide.Shapes.AddPicture(str(image_path), MSO_FALSE, MSO_TRUE, left, top, width, height)
    name = payload.get("name")
    if name is not None:
        if not isinstance(name, str) or not name.strip() or len(name) > 100:
            raise ValueError("name must be a non-empty string up to 100 chars")
        shape.Name = name.strip()
    after = int(slide.Shapes.Count)
    verified = after == before + 1
    return {
        **presentation_state(bound),
        "slide": slide_num,
        "shape": str(shape.Name),
        "image_path": str(image_path),
        "shape_count_before": before,
        "shape_count_after": after,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_shape_delete(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    slide_num = check_slide_ref(payload.get("slide"), int(bound.Slides.Count))
    slide = bound.Slides(slide_num)
    before = int(slide.Shapes.Count)
    shape = find_shape(slide, payload.get("shape"))
    name = str(shape.Name)
    shape.Delete()
    after = int(slide.Shapes.Count)
    verified = after == before - 1
    return {
        **presentation_state(bound),
        "slide": slide_num,
        "deleted_shape": name,
        "shape_count_before": before,
        "shape_count_after": after,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": verified,
        "verification": "application_state",
    }


def do_shape_geometry_set(app, payload):
    bound = bind_presentation(app, payload)
    if bound is None:
        raise ValueError("presentation_not_open: provide presentation_path of an open deck or open it first")
    slide_num = check_slide_ref(payload.get("slide"), int(bound.Slides.Count))
    shape = find_shape(bound.Slides(slide_num), payload.get("shape"))
    requested = {}
    limits = {
        "left": (-10000, 20000),
        "top": (-10000, 20000),
        "width": (1, 20000),
        "height": (1, 20000),
        "rotation": (-3600, 3600),
    }
    for key, (minimum, maximum) in limits.items():
        if key in payload:
            requested[key] = number_param(payload, key, required=True, minimum=minimum, maximum=maximum)
    if not requested:
        raise ValueError("shape.geometry.set needs at least one of left, top, width, height, rotation")
    mapping = {"left": "Left", "top": "Top", "width": "Width", "height": "Height", "rotation": "Rotation"}
    for key, value in requested.items():
        setattr(shape, mapping[key], value)
    observed = {key: float(getattr(shape, mapping[key])) for key in requested}
    verified = all(abs(observed[key] - requested[key]) <= 0.5 for key in requested)
    return {
        **presentation_state(bound),
        "slide": slide_num,
        "shape": str(shape.Name),
        "requested": requested,
        "observed": observed,
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


def do_batch_edit(app, payload):
    ops = payload.get("ops")
    if not isinstance(ops, list) or not 1 <= len(ops) <= MAX_BATCH_OPS:
        raise ValueError("ops must contain between 1 and %d operations" % MAX_BATCH_OPS)
    presentation_path = payload.get("presentation_path", payload.get("path"))
    handlers = {
        "slide.create": do_slide_create,
        "slide.delete": do_slide_delete,
        "slide.reorder": do_slide_reorder,
        "slide.duplicate": do_slide_duplicate,
        "shape.text.set": do_shape_text_set,
        "shape.textbox.create": do_shape_textbox_create,
        "shape.image.insert": do_shape_image_insert,
        "shape.delete": do_shape_delete,
        "shape.geometry.set": do_shape_geometry_set,
        "save": do_save,
    }
    results = []
    for index, raw in enumerate(ops):
        if not isinstance(raw, dict):
            raise ValueError("batch operation %d must be an object" % index)
        kind = raw.get("op")
        func = handlers.get(kind)
        if func is None:
            raise ValueError("unsupported batch operation: %s" % kind)
        params = dict(raw)
        params.pop("op", None)
        if presentation_path and "presentation_path" not in params and "path" not in params:
            params["presentation_path"] = presentation_path
        reject_macro_requests(params)
        result = func(app, params)
        if not result.get("verified"):
            raise ValueError("batch operation %d was not verified" % index)
        results.append({
            "index": index,
            "op": kind,
            "verified": True,
            "slide_count": result.get("slide_count"),
            "slide_count_after": result.get("slide_count_after"),
            "saved_path": result.get("saved_path"),
        })
    bound = bind_presentation(
        app,
        {"presentation_path": presentation_path} if presentation_path else {},
    )
    if bound is None:
        raise ValueError("presentation_not_open: exact deck is no longer open")
    state = presentation_state(bound)
    return {
        **state,
        "applied": len(results),
        "results": results,
        "backend": "windows-com",
        "macros_executed": False,
        "verified": True,
        "verification": "application_state_batch_readback",
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
                    "presentation.desktop.batch_edit",
                    "presentation.slide.create",
                    "presentation.slide.delete",
                    "presentation.slide.reorder",
                    "presentation.slide.duplicate",
                    "presentation.shape.text.set",
                    "presentation.shape.textbox.create",
                    "presentation.shape.image.insert",
                    "presentation.shape.delete",
                    "presentation.shape.geometry.set",
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
        elif intent == "presentation.desktop.batch_edit":
            result = do_batch_edit(app, payload)
        elif intent == "presentation.slide.create":
            result = do_slide_create(app, payload)
        elif intent == "presentation.slide.delete":
            result = do_slide_delete(app, payload)
        elif intent == "presentation.slide.reorder":
            result = do_slide_reorder(app, payload)
        elif intent == "presentation.slide.duplicate":
            result = do_slide_duplicate(app, payload)
        elif intent == "presentation.shape.text.set":
            result = do_shape_text_set(app, payload)
        elif intent == "presentation.shape.textbox.create":
            result = do_shape_textbox_create(app, payload)
        elif intent == "presentation.shape.image.insert":
            result = do_shape_image_insert(app, payload)
        elif intent == "presentation.shape.delete":
            result = do_shape_delete(app, payload)
        elif intent == "presentation.shape.geometry.set":
            result = do_shape_geometry_set(app, payload)
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
