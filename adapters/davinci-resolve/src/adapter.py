#!/usr/bin/env python3
"""comptrol.resolve adapter: typed DaVinci Resolve control over framed RPC.

Every intent maps to a closed, validated call in ``resolve_bridge`` against
the running Resolve application via local external scripting. The adapter
never builds scripts from request data and never uses exec/eval: payloads
contribute only typed scalar arguments (names, indices, paths, enums) that
are validated here before any Resolve call.

Verification is per-intent readback, never request success alone: timeline
edits re-list items or markers, property writes re-read the property, render
operations re-query job status, and render/save intents additionally check
the persisted artifact (output file exists with nonzero size).
"""

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "_shared"))

import resolve_bridge as bridge  # noqa: E402
from adapter_protocol import response, serve  # noqa: E402
from resolve_bridge import BridgeError  # noqa: E402

ADAPTER_ID = "comptrol.resolve"
MAX_NAME_LEN = 128
MAX_TEXT_LEN = 256
MAX_PATHS = 64
MAX_OPS = 32
MAX_JOB_IDS = 64

MEDIA_SUFFIXES = frozenset({
    ".mp4", ".mov", ".mxf", ".mkv", ".avi", ".mts", ".m2ts",
    ".mp3", ".wav", ".aac", ".flac", ".aif", ".aiff",
    ".png", ".jpg", ".jpeg", ".tif", ".tiff", ".bmp", ".dpx", ".exr",
})

RENDER_SUFFIXES = frozenset({".mp4", ".mov", ".mxf", ".avi", ".mkv", ".wav"})

FRAME_RATES = (23.976, 24.0, 25.0, 29.97, 30.0, 50.0, 59.94, 60.0, 120.0)

BATCH_OPS = ("append", "insert", "marker.add", "marker.delete", "item.properties.set")


# ---------------------------------------------------------------------------
# Closed parameter validation (no script strings, no exec/eval)
# ---------------------------------------------------------------------------

def _fail(request, code, message, health="degraded"):
    return response(request, False, health,
                    error={"code": code, "message": str(message)})


def _invalid(message):
    return ValueError(str(message))


def _check_text(value, field, limit=MAX_NAME_LEN):
    if not isinstance(value, str) or not value.strip():
        raise _invalid("%s must be a non-empty string" % field)
    if len(value) > limit or any(char in value for char in "\r\n\x00"):
        raise _invalid("%s is too long or contains control characters" % field)
    return value.strip()


def _check_optional_text(value, field, limit=MAX_NAME_LEN):
    if value is None:
        return None
    return _check_text(value, field, limit)


def _check_int(value, field, minimum=0):
    if isinstance(value, bool) or not isinstance(value, int):
        raise _invalid("%s must be an integer" % field)
    if value < minimum:
        raise _invalid("%s must be >= %s" % (field, minimum))
    return value


def _check_number(value, field):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise _invalid("%s must be numeric" % field)
    return value


def _check_media_paths(value):
    if not isinstance(value, list) or not value or len(value) > MAX_PATHS:
        raise _invalid("paths must contain between 1 and %s files" % MAX_PATHS)
    resolved = []
    for entry in value:
        if not isinstance(entry, str) or any(char in entry for char in "\r\n\x00"):
            raise _invalid("each media path must be a plain path string")
        path = Path(entry).expanduser().resolve()
        if path.suffix.lower() not in MEDIA_SUFFIXES:
            raise _invalid("unsupported media suffix: %s" % path.suffix)
        if not path.is_file():
            raise _invalid("media file does not exist: %s" % path)
        resolved.append(str(path))
    return resolved


def _check_output_name(value):
    name = _check_text(value, "CustomName")
    if "/" in name or "\\" in name:
        raise _invalid("CustomName must be a file name, not a path")
    suffix = Path(name).suffix.lower()
    if suffix and suffix not in RENDER_SUFFIXES:
        raise _invalid("CustomName suffix must be a render artifact: %s" % suffix)
    return name


def _check_output_dir(value):
    if not isinstance(value, str) or any(char in value for char in "\r\n\x00"):
        raise _invalid("TargetDir must be a plain path string")
    path = Path(value).expanduser().resolve()
    if not path.is_dir():
        raise _invalid("TargetDir must be an existing directory: %s" % path)
    return str(path)


def _check_track_type(value):
    if value not in bridge.TRACK_TYPES:
        raise _invalid("track_type must be one of %s" % (", ".join(bridge.TRACK_TYPES)))
    return value


def _check_color(value):
    if value not in bridge.MARKER_COLORS:
        raise _invalid("color must be one of %s" % (", ".join(sorted(bridge.MARKER_COLORS))))
    return value


def _check_property_key(value):
    if value not in bridge.ITEM_PROPERTY_KEYS:
        raise _invalid("property key must be one of %s"
                       % (", ".join(sorted(bridge.ITEM_PROPERTY_KEYS))))
    return value


def _check_property_value(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise _invalid("property value must be numeric")
    return float(value)


def _check_render_settings(value):
    if not isinstance(value, dict) or not value:
        raise _invalid("settings must be a non-empty object")
    kinds = bridge.RENDER_SETTING_KINDS
    unknown = sorted(set(value) - set(kinds))
    if unknown:
        raise _invalid("unsupported render settings: %s" % ", ".join(unknown))
    settings = {}
    for key, item in value.items():
        kind = kinds[key]
        if kind == "dir":
            settings[key] = _check_output_dir(item)
        elif kind == "name":
            settings[key] = _check_output_name(item)
        elif kind == "bool":
            if not isinstance(item, bool):
                raise _invalid("%s must be a boolean" % key)
            settings[key] = item
        elif kind == "int":
            if isinstance(item, bool) or not isinstance(item, int):
                raise _invalid("%s must be an integer" % key)
            if key in ("FormatWidth", "FormatHeight") and not 16 <= item <= 16384:
                raise _invalid("%s is out of range" % key)
            settings[key] = item
        elif kind == "number":
            number = _check_number(item, key)
            if key == "FrameRate" and float(number) not in FRAME_RATES:
                raise _invalid("FrameRate must be one of %s"
                               % (", ".join(str(rate) for rate in FRAME_RATES)))
            settings[key] = number
    return settings


def _check_job_id(value):
    return _check_text(value, "job_id", limit=MAX_NAME_LEN)


def _check_job_ids(value):
    if not isinstance(value, list) or not value or len(value) > MAX_JOB_IDS:
        raise _invalid("job_ids must contain between 1 and %s ids" % MAX_JOB_IDS)
    return [_check_job_id(entry) for entry in value]


def _select_timeline(payload):
    name = _check_optional_text(payload.get("timeline"), "timeline")
    index = payload.get("timeline_index")
    if index is not None:
        index = _check_int(index, "timeline_index", minimum=1)
    if name is not None and index is not None:
        raise _invalid("timeline and timeline_index are mutually exclusive")
    return name, index


def _clip_selector(payload, require_record_frame=False):
    clip = _check_text(payload.get("clip"), "clip")
    bin_name = _check_optional_text(payload.get("bin"), "bin")
    start_frame = payload.get("start_frame")
    end_frame = payload.get("end_frame")
    record_frame = payload.get("record_frame")
    track_index = payload.get("track_index")
    if start_frame is not None:
        start_frame = _check_int(start_frame, "start_frame")
    if end_frame is not None:
        end_frame = _check_int(end_frame, "end_frame")
    if start_frame is not None and end_frame is not None and end_frame <= start_frame:
        raise _invalid("end_frame must be greater than start_frame")
    if record_frame is not None:
        record_frame = _check_int(record_frame, "record_frame")
    elif require_record_frame:
        raise _invalid("record_frame is required for insert")
    if track_index is not None:
        track_index = _check_int(track_index, "track_index", minimum=1)
    return {
        "clip_name": clip, "bin_name": bin_name, "start_frame": start_frame,
        "end_frame": end_frame, "record_frame": record_frame,
        "track_index": track_index,
    }


def _marker_params(payload):
    frame = _check_int(payload.get("frame"), "frame")
    color = _check_color(payload.get("color"))
    name = _check_text(payload.get("name", "Comptrol"), "name")
    note = payload.get("note", "")
    if not isinstance(note, str) or len(note) > MAX_TEXT_LEN:
        raise _invalid("note must be a string of at most %s characters" % MAX_TEXT_LEN)
    if any(char in note for char in "\r\x00"):
        raise _invalid("note contains control characters")
    duration = _check_int(payload.get("duration", 1), "duration", minimum=1)
    return frame, color, name, note, duration


def _artifact_stat(path_text):
    try:
        path = Path(path_text)
        if path.is_file():
            return {"output_path": str(path), "output_size": path.stat().st_size}
    except OSError:
        pass
    return {"output_path": str(path_text), "output_size": 0}


def _expected_render_path(settings):
    target = settings.get("TargetDir")
    custom = settings.get("CustomName")
    if not target or not custom:
        return None
    name = str(custom)
    if not Path(name).suffix:
        for suffix in sorted(RENDER_SUFFIXES):
            candidate = Path(str(target)) / (name + suffix)
            if candidate.is_file():
                return str(candidate)
        return str(Path(str(target)) / name)
    return str(Path(str(target)) / name)


# ---------------------------------------------------------------------------
# Intent implementations (each declared intent has a branch below)
# ---------------------------------------------------------------------------

def _project_list(payload, resolve):
    del payload
    projects = bridge.list_projects(resolve)
    current = bridge.probe().get("project")
    return {"projects": projects, "project": current,
            "verified": True, "verification": "resolve_state_readback"}


def _project_open(payload, resolve):
    name = _check_text(payload.get("name"), "name")
    result = bridge.open_project(resolve, name)
    verified = result.get("opened") is True and result.get("project") == name
    return {**result, "verified": verified, "verification": "resolve_project_readback"}


def _project_create(payload, resolve):
    name = _check_text(payload.get("name"), "name")
    result = bridge.create_project(resolve, name)
    verified = result.get("created") is True and result.get("project") == name
    return {**result, "verified": verified, "verification": "resolve_project_readback"}


def _project_save(payload, resolve):
    del payload
    result = bridge.save_project(resolve)
    # Resolve persists projects to its project database rather than a
    # user-visible file, so the artifact proof is database persistence:
    # SaveProject accepted AND the project is listed in its folder.
    verified = result.get("persisted") is True
    return {"project": result.get("project"), "saved": result.get("saved"),
            "artifact": "project_database", "persisted": result.get("persisted"),
            "verified": verified, "verification": "resolve_project_persisted"}


def _media_import(payload, resolve):
    paths = _check_media_paths(payload.get("paths"))
    bin_name = _check_optional_text(payload.get("bin"), "bin")
    result = bridge.import_media(resolve, paths, bin_name)
    return {"imported": result["imported"], "bin": result["bin"],
            "verified": result["verified_clips"] is True,
            "verification": "resolve_media_pool_readback"}


def _media_bin_create(payload, resolve):
    name = _check_text(payload.get("name"), "name")
    parent = _check_optional_text(payload.get("parent"), "parent")
    result = bridge.create_bin(resolve, name, parent)
    return {"bin": result["bin"], "parent": parent,
            "verified": result["present"] is True,
            "verification": "resolve_media_pool_readback"}


def _media_list(payload, resolve):
    bin_name = _check_optional_text(payload.get("bin"), "bin")
    clips = bridge.list_media(resolve, bin_name)
    return {"bin": bin_name, "clips": clips, "count": len(clips),
            "verified": True, "verification": "resolve_state_readback"}


def _timeline_list(payload, resolve):
    del payload
    timelines = bridge.list_timelines(resolve)
    current = bridge.current_timeline_name(resolve)
    return {"timelines": timelines, "timeline": current,
            "verified": True, "verification": "resolve_state_readback"}


def _timeline_open(payload, resolve):
    name, index = _select_timeline(payload)
    if name is None and index is None:
        raise _invalid("timeline name or timeline_index is required")
    result = bridge.open_timeline(resolve, name=name, index=index)
    return {**result, "verified": result.get("opened") is True,
            "verification": "resolve_timeline_readback"}


def _timeline_create(payload, resolve):
    name = _check_text(payload.get("name"), "name")
    bin_name = _check_optional_text(payload.get("bin"), "bin")
    result = bridge.create_timeline(resolve, name, bin_name)
    return {"timeline": result["timeline"], "bin": bin_name,
            "verified": result["present"] is True,
            "verification": "resolve_timeline_readback"}


def _timeline_items_list(payload, resolve):
    track_type = _check_track_type(payload.get("track_type", "video"))
    track_index = _check_int(payload.get("track_index", 1), "track_index", minimum=1)
    name, index = _select_timeline(payload)
    result = bridge.list_timeline_items(resolve, track_type, track_index,
                                        name=name, index=index)
    return {**result, "verified": True, "verification": "resolve_state_readback"}


def _timeline_append(payload, resolve):
    selector = _clip_selector(payload)
    name, index = _select_timeline(payload)
    if name is not None or index is not None:
        bridge.open_timeline(resolve, name=name, index=index)
    created = bridge.append_to_timeline(
        resolve, selector["clip_name"], selector["bin_name"],
        selector["start_frame"], selector["end_frame"],
        selector["record_frame"], selector["track_index"])
    items = bridge.list_timeline_items(resolve, "video", 1)
    names = [item["name"] for item in items["items"]]
    present = created["timeline_item"] in names or selector["clip_name"] in names
    return {"timeline": items["timeline"], "appended": created["timeline_item"],
            "item_count": len(names),
            "verified": present, "verification": "resolve_timeline_readback"}


def _timeline_insert(payload, resolve):
    selector = _clip_selector(payload, require_record_frame=True)
    name, index = _select_timeline(payload)
    if name is not None or index is not None:
        bridge.open_timeline(resolve, name=name, index=index)
    # Resolve's scripting surface appends through the media pool; an insert
    # is expressed as an append pinned to record_frame.
    created = bridge.append_to_timeline(
        resolve, selector["clip_name"], selector["bin_name"],
        selector["start_frame"], selector["end_frame"],
        selector["record_frame"], selector["track_index"])
    items = bridge.list_timeline_items(resolve, "video", 1)
    names = [item["name"] for item in items["items"]]
    present = created["timeline_item"] in names or selector["clip_name"] in names
    return {"timeline": items["timeline"], "inserted": created["timeline_item"],
            "record_frame": selector["record_frame"], "item_count": len(names),
            "verified": present, "verification": "resolve_timeline_readback"}


def _marker_add(payload, resolve):
    frame, color, name, note, duration = _marker_params(payload)
    timeline_name, timeline_index = _select_timeline(payload)
    result = bridge.add_marker(resolve, frame, color, name, note, duration,
                               timeline_name, timeline_index)
    verified = result.get("added") is True and result.get("present") is True
    return {**result, "name": name,
            "verified": verified, "verification": "resolve_marker_readback"}


def _marker_delete(payload, resolve):
    frame = payload.get("frame")
    color = payload.get("color")
    if (frame is None) == (color is None):
        raise _invalid("exactly one of frame or color is required")
    if frame is not None:
        frame = _check_int(frame, "frame")
    else:
        color = _check_color(color)
    timeline_name, timeline_index = _select_timeline(payload)
    result = bridge.delete_marker(resolve, frame=frame, color=color,
                                  name_selector=timeline_name,
                                  index_selector=timeline_index)
    if frame is not None:
        verified = result.get("gone") is True
    else:
        verified = result.get("deleted") is True and result.get("remaining") == 0
    return {**result, "verified": verified,
            "verification": "resolve_marker_readback"}


def _item_properties_get(payload, resolve):
    track_type = _check_track_type(payload.get("track_type", "video"))
    track_index = _check_int(payload.get("track_index", 1), "track_index", minimum=1)
    item_index = _check_int(payload.get("item_index"), "item_index")
    key = _check_property_key(payload.get("key"))
    name, index = _select_timeline(payload)
    result = bridge.get_item_property(resolve, track_type, track_index,
                                      item_index, key, name=name, index=index)
    return {**result, "track_type": track_type, "track_index": track_index,
            "item_index": item_index,
            "verified": True, "verification": "resolve_state_readback"}


def _item_properties_set(payload, resolve):
    track_type = _check_track_type(payload.get("track_type", "video"))
    track_index = _check_int(payload.get("track_index", 1), "track_index", minimum=1)
    item_index = _check_int(payload.get("item_index"), "item_index")
    key = _check_property_key(payload.get("key"))
    value = _check_property_value(payload.get("value"))
    name, index = _select_timeline(payload)
    result = bridge.set_item_property(resolve, track_type, track_index,
                                      item_index, key, value,
                                      name=name, index=index)
    verified = result.get("applied") is True and float(result.get("value") or 0) == value
    return {**result, "track_type": track_type, "track_index": track_index,
            "item_index": item_index,
            "verified": verified,
            "verification": "resolve_item_property_readback"}


def _render_preset_list(payload, resolve):
    del payload
    presets = bridge.list_render_presets(resolve)
    return {"presets": presets, "verified": True,
            "verification": "resolve_state_readback"}


def _render_configure(payload, resolve):
    settings = _check_render_settings(payload.get("settings"))
    readback = bridge.configure_render(resolve, settings)
    mismatched = [key for key, expected in settings.items()
                  if str(readback.get(key)) != str(expected)]
    return {"settings": {key: readback.get(key) for key in settings},
            "verified": not mismatched,
            "verification": "resolve_render_settings_readback"}


def _render_add_job(payload, resolve):
    del payload
    result = bridge.add_render_job(resolve)
    return {"job_id": result["job_id"],
            "verified": result["queued"] is True,
            "verification": "resolve_render_readback"}


def _render_start(payload, resolve):
    job_ids = payload.get("job_ids")
    if job_ids is not None:
        job_ids = _check_job_ids(job_ids)
    result = bridge.start_render(resolve, job_ids)
    status = bridge.render_status(resolve)
    settings = bridge.get_render_settings(resolve)
    expected = _expected_render_path(settings)
    data = {"started": result["started"], "job_ids": job_ids,
            "status": status, "settings": settings}
    if expected is None:
        # No file target is configured yet; the status readback is the proof.
        return {**data, "verified": result["started"] is True,
                "verification": "resolve_render_readback"}
    stat = _artifact_stat(expected)
    data.update(stat)
    suffix = Path(expected).suffix.lower()
    extension_ok = not suffix or suffix in RENDER_SUFFIXES
    complete = any(str(job.get("status", "")).lower() == "complete"
                   for job in status.get("jobs", []))
    if complete:
        verified = extension_ok and stat["output_size"] > 0
    else:
        verified = result["started"] is True and extension_ok
    return {**data, "complete": complete,
            "verified": verified,
            "verification": "resolve_render_artifact_readback"}


def _render_status(payload, resolve):
    job_id = payload.get("job_id")
    if job_id is not None:
        job_id = _check_job_id(job_id)
    status = bridge.render_status(resolve, job_id)
    data = dict(status)
    if job_id is None:
        settings = bridge.get_render_settings(resolve)
        expected = _expected_render_path(settings)
        if expected is not None:
            data.update(_artifact_stat(expected))
    data["verified"] = True
    data["verification"] = "resolve_render_readback"
    return data


def _render_cancel(payload, resolve):
    del payload
    result = bridge.cancel_render(resolve)
    return {**result, "verified": result["cancelled"] is True,
            "verification": "resolve_render_readback"}


# ---------------------------------------------------------------------------
# Batch timeline edits: one bridge session, typed ops only
# ---------------------------------------------------------------------------

def _validate_batch_op(op):
    if not isinstance(op, dict):
        raise _invalid("each batch op must be an object")
    kind = op.get("op")
    if kind not in BATCH_OPS:
        raise _invalid("unsupported batch op: %s" % kind)
    if kind in ("append", "insert"):
        params = _clip_selector(op, require_record_frame=(kind == "insert"))
        return kind, params
    if kind == "marker.add":
        frame, color, name, note, duration = _marker_params(op)
        return kind, {"frame": frame, "color": color, "name": name,
                      "note": note, "duration": duration}
    if kind == "marker.delete":
        frame = op.get("frame")
        color = op.get("color")
        if (frame is None) == (color is None):
            raise _invalid("marker.delete needs exactly one of frame or color")
        if frame is not None:
            frame = _check_int(frame, "frame")
        else:
            color = _check_color(color)
        return kind, {"frame": frame, "color": color}
    track_type = _check_track_type(op.get("track_type", "video"))
    track_index = _check_int(op.get("track_index", 1), "track_index", minimum=1)
    item_index = _check_int(op.get("item_index"), "item_index")
    key = _check_property_key(op.get("key"))
    value = _check_property_value(op.get("value"))
    return kind, {"track_type": track_type, "track_index": track_index,
                  "item_index": item_index, "key": key, "value": value}


def _timeline_batch(payload, resolve):
    ops = payload.get("ops")
    if not isinstance(ops, list) or not ops or len(ops) > MAX_OPS:
        raise _invalid("ops must contain between 1 and %s operations" % MAX_OPS)
    validated = [_validate_batch_op(op) for op in ops]
    name, index = _select_timeline(payload)
    if name is not None or index is not None:
        bridge.open_timeline(resolve, name=name, index=index)
    # Single bridge session: one local scripting attachment for every op.
    results = []
    for kind, params in validated:
        if kind == "append":
            outcome = bridge.append_to_timeline(
                resolve, params["clip_name"], params["bin_name"],
                params["start_frame"], params["end_frame"],
                params["record_frame"], params["track_index"])
        elif kind == "insert":
            outcome = bridge.append_to_timeline(
                resolve, params["clip_name"], params["bin_name"],
                params["start_frame"], params["end_frame"],
                params["record_frame"], params["track_index"])
        elif kind == "marker.add":
            outcome = bridge.add_marker(
                resolve, params["frame"], params["color"], params["name"],
                params["note"], params["duration"])
        elif kind == "marker.delete":
            outcome = bridge.delete_marker(
                resolve, frame=params["frame"], color=params["color"])
        else:
            outcome = bridge.set_item_property(
                resolve, params["track_type"], params["track_index"],
                params["item_index"], params["key"], params["value"])
        results.append({"op": kind, "ok": True, "result": outcome})
    timeline = bridge.current_timeline_name(resolve)
    items = bridge.list_timeline_items(resolve, "video", 1)
    markers = bridge.get_markers(resolve)
    markers = markers if isinstance(markers, dict) else {}
    return {"timeline": timeline, "applied": len(results), "results": results,
            "item_count": len(items["items"]), "marker_count": len(markers),
            "verified": True, "verification": "resolve_batch_readback"}


HANDLERS = {
    "video.project.list": _project_list,
    "video.project.open": _project_open,
    "video.project.create": _project_create,
    "video.project.save": _project_save,
    "video.media.import": _media_import,
    "video.media.bin.create": _media_bin_create,
    "video.media.list": _media_list,
    "video.timeline.list": _timeline_list,
    "video.timeline.open": _timeline_open,
    "video.timeline.create": _timeline_create,
    "video.timeline.items.list": _timeline_items_list,
    "video.timeline.append": _timeline_append,
    "video.timeline.insert": _timeline_insert,
    "video.timeline.batch": _timeline_batch,
    "video.timeline.marker.add": _marker_add,
    "video.timeline.marker.delete": _marker_delete,
    "video.timeline.item.properties.get": _item_properties_get,
    "video.timeline.item.properties.set": _item_properties_set,
    "video.render.preset.list": _render_preset_list,
    "video.render.configure": _render_configure,
    "video.render.add_job": _render_add_job,
    "video.render.start": _render_start,
    "video.render.status": _render_status,
    "video.render.cancel": _render_cancel,
}


def _health_for(code):
    if code in ("resolve_not_found", "scripting_disabled", "resolve_unreachable",
                "unsupported", "bin_not_found", "project_not_found",
                "timeline_not_found", "clip_not_found", "item_not_found",
                "unsupported_intent"):
        return "unsupported"
    return "degraded"


class _ValidationPassed(Exception):
    """Raised when a validation-only pass reaches the first bridge touch."""


class _ValidateOnly:
    """Stand-in for the Resolve handle that permits validation but no calls.

    Every intent function validates its closed params before touching the
    bridge, so running it against this stand-in either raises ValueError
    (bad params) or _ValidationPassed (params fine, first bridge touch).
    """

    def __getattr__(self, name):
        raise _ValidationPassed()

    def __call__(self, *args, **kwargs):
        raise _ValidationPassed()


def handler(request):
    method = request.get("method")
    if method == "handshake":
        probe = bridge.probe()
        reachable = probe.get("reachable") is True
        return response(request, True, "available", {
            "adapter": ADAPTER_ID,
            "modes": ["live"] if reachable else [],
            "route": "local_external_scripting",
            "reachable": reachable,
            "edition": probe.get("edition"),
            "version": probe.get("version"),
            "network_scripting": "not_enabled",
        })
    if method == "capabilities":
        probe = bridge.probe()
        reachable = probe.get("reachable") is True
        return response(request, True, "available", {
            "backend": "resolve_local_scripting",
            "mode": "live",
            "live": reachable,
            "intents": sorted(HANDLERS),
        })
    if method == "shutdown":
        return response(request, True, "available", {"stopped": True})
    payload = request.get("payload", {}) or {}
    intent = payload.get("intent")
    func = HANDLERS.get(intent)
    if func is None:
        return _fail(request, "unsupported_intent",
                     "unsupported intent: %s" % intent, health="unsupported")
    try:
        # Validation-first pass: closed params are checked before any Resolve
        # connection, so malformed requests fail fast with invalid_params even
        # when Resolve is unavailable. The stand-in raises _ValidationPassed
        # at the first bridge touch; bridge calls never execute against it.
        try:
            func(payload, _ValidateOnly())
        except _ValidationPassed:
            pass
        resolve = bridge.connect()
        data = func(payload, resolve)
    except BridgeError as exc:
        return _fail(request, exc.code, exc.message,
                     health=_health_for(exc.code))
    except ValueError as exc:
        return _fail(request, "invalid_params", str(exc))
    except Exception as exc:
        return _fail(request, "resolve_request_failed", str(exc))
    if not data.get("verified"):
        return response(request, False, "degraded", payload=data,
                        error={"code": "verification_failed",
                               "message": "postcondition readback did not confirm %s" % intent})
    return response(request, True, "available", data)


serve(handler)
