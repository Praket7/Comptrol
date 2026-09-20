#!/usr/bin/env python3
"""Local DaVinci Resolve scripting bridge for the comptrol.resolve adapter.

This module is the only place that touches Resolve's scripting API. It talks
to the already-running Resolve application through the official
``DaVinciResolveScript`` module using a **local** external-scripting session
(``scriptapp("Resolve")``). It never enables network scripting, never accepts
a remote host or port, and never executes model-supplied code: every operation
is a typed function call with validated arguments.

The scripting modules are located dynamically by probing the well-known
Resolve install directories on Windows, macOS, and Linux. No Resolve version
number is hardcoded anywhere: the lookup accepts whatever version the user
installed, as long as the ``DaVinciResolveScript`` module is present.

All failures surface as :class:`BridgeError` with a stable machine-readable
``code`` so ``adapter.py`` can map them to structured RPC errors.
"""

import os
import platform
from pathlib import Path

MODULE_NAME = "DaVinciResolveScript"
ENV_SCRIPTING_DIR = "COMPTROL_RESOLVE_SCRIPTING_DIR"

TRACK_TYPES = ("video", "audio", "subtitle")

MARKER_COLORS = frozenset({
    "Blue", "Cyan", "Green", "Yellow", "Red", "Pink", "Purple",
    "Fuchsia", "Rose", "Lavender", "Sky", "Sand", "Cocoa", "Cream",
})

# Closed set of TimelineItem property keys the bridge will read or write.
# Anything outside this set is rejected before any Resolve call is made.
ITEM_PROPERTY_KEYS = frozenset({
    "Pan", "Tilt", "ZoomX", "ZoomY", "ZoomGang", "RotationAngle",
    "AnchorPointX", "AnchorPointY", "PositionX", "PositionY",
    "CropLeft", "CropRight", "CropTop", "CropBottom",
    "Opacity",
})

# Closed render-settings schema: key -> expected kind.
# Only these keys are ever forwarded to SetRenderSettings.
RENDER_SETTING_KINDS = {
    "TargetDir": "dir",
    "CustomName": "name",
    "ExportVideo": "bool",
    "ExportAudio": "bool",
    "FormatWidth": "int",
    "FormatHeight": "int",
    "FrameRate": "number",
}

VIDEO_FORMATS = frozenset({"mp4", "mov", "mxf", "avi", "mkv"})


class BridgeError(Exception):
    """Typed Resolve bridge failure with a stable error code."""

    def __init__(self, code, message):
        super().__init__(str(message))
        self.code = str(code)
        self.message = str(message)

    def __str__(self):
        return "%s: %s" % (self.code, self.message)


# ---------------------------------------------------------------------------
# Module location (dynamic, version-agnostic)
# ---------------------------------------------------------------------------

def _platform_candidates():
    """Well-known scripting-module directories. No version is hardcoded."""
    program_data = os.environ.get("PROGRAMDATA", r"C:\ProgramData")
    program_files = os.environ.get("PROGRAMFILES", r"C:\Program Files")
    home = Path.home()
    return [
        # Windows
        Path(program_data) / "Blackmagic Design" / "DaVinci Resolve"
        / "Support" / "Developer" / "Scripting" / "Modules",
        Path(program_files) / "Blackmagic Design" / "DaVinci Resolve"
        / "Developer" / "Scripting" / "Modules",
        # macOS
        Path("/Library/Application Support/Blackmagic Design/DaVinci Resolve"
             "/Developer/Scripting/Modules"),
        home / "Library" / "Application Support" / "Blackmagic Design"
        / "DaVinci Resolve" / "Developer" / "Scripting" / "Modules",
        # Linux
        Path("/opt/resolve/Developer/Scripting/Modules"),
        Path("/opt/DaVinci_Resolve/Developer/Scripting/Modules"),
    ]


def candidate_scripting_dirs():
    """Ordered candidate directories, explicit override first, deduplicated."""
    candidates = []
    override = os.environ.get(ENV_SCRIPTING_DIR)
    if override:
        candidates.append(Path(override))
    candidates.extend(_platform_candidates())
    seen = set()
    ordered = []
    for candidate in candidates:
        key = os.path.normcase(str(candidate))
        if key not in seen:
            seen.add(key)
            ordered.append(candidate)
    return ordered


def find_scripting_dir():
    """Return the directory holding DaVinciResolveScript.py or raise."""
    for candidate in candidate_scripting_dirs():
        try:
            if (candidate / (MODULE_NAME + ".py")).is_file():
                return candidate
        except OSError:
            continue
    raise BridgeError(
        "resolve_not_found",
        "DaVinci Resolve scripting modules were not found. Install DaVinci "
        "Resolve (any recent version) on this machine; no specific version "
        "is required.",
    )


_script_module = None


def load_script_module():
    """Import DaVinciResolveScript from its install directory (cached)."""
    global _script_module
    if _script_module is not None:
        return _script_module
    directory = find_scripting_dir()
    path_entry = str(directory)
    if path_entry not in __import__("sys").path:
        __import__("sys").path.insert(0, path_entry)
    try:
        module = __import__(MODULE_NAME, fromlist=["*"])
    except ImportError as exc:
        raise BridgeError(
            "scripting_disabled",
            "Found Resolve scripting modules at %s but the import failed; "
            "external scripting support may be broken or disabled: %s"
            % (directory, exc),
        ) from exc
    _script_module = module
    return module


# ---------------------------------------------------------------------------
# Connection (local external scripting only)
# ---------------------------------------------------------------------------

_LOCAL_ONLY_HINT = (
    "Start DaVinci Resolve and enable Preferences > System > General > "
    "External scripting using: Local. The adapter uses the local scripting "
    "session only and never enables network scripting."
)


def connect():
    """Return the Resolve scripting object for the running application.

    Uses ``scriptapp("Resolve")`` against the local session. No host, port,
    or network-scripting flag is ever passed: remote control stays disabled.
    """
    module = load_script_module()
    try:
        resolve = module.scriptapp("Resolve")
    except Exception as exc:
        raise BridgeError(
            "resolve_unreachable",
            "Could not attach to DaVinci Resolve (%s). %s" % (exc, _LOCAL_ONLY_HINT),
        ) from exc
    if resolve is None:
        raise BridgeError("resolve_unreachable", _LOCAL_ONLY_HINT)
    return resolve


def _invoke(label, func, *args):
    try:
        return func(*args)
    except BridgeError:
        raise
    except Exception as exc:
        raise BridgeError(
            "resolve_call_failed", "%s failed: %s" % (label, exc)
        ) from exc


def _maybe(func):
    """Call func(); return None instead of raising on any failure."""
    try:
        return func()
    except Exception:
        return None


# ---------------------------------------------------------------------------
# Read-only probe
# ---------------------------------------------------------------------------

def _version_string(resolve):
    getter = getattr(resolve, "GetVersionString", None)
    if callable(getter):
        value = _maybe(getter)
        if isinstance(value, str) and value.strip():
            return value.strip()
    legacy = getattr(resolve, "GetVersion", None)
    if callable(legacy):
        value = _maybe(legacy)
        if isinstance(value, (list, tuple)) and value:
            first = value[0]
            if isinstance(first, dict):
                parts = [str(first.get(k, "")) for k in ("Major", "Minor", "Patch", "Build")]
                text = ".".join(p for p in parts[:3] if p).strip()
                build = parts[3].strip()
                return ("%s (build %s)" % (text, build)).strip() if build else (text or "unknown")
            return str(first)
        if value is not None:
            return str(value)
    return "unknown"


def probe():
    """Read-only connection probe. Never mutates application state.

    Returns a dict describing reachability, edition, and version. Local
    external scripting is the only transport; network scripting is reported
    as not enabled by this adapter.
    """
    base = {"scripting": "local_external", "network_scripting": "not_enabled"}
    try:
        resolve = connect()
    except BridgeError as exc:
        return {**base, "reachable": False, "code": exc.code, "message": exc.message}
    product = _maybe(lambda: resolve.GetProductName()) or "DaVinci Resolve"
    edition = "studio" if "studio" in str(product).lower() else "free"
    manager = _maybe(lambda: resolve.GetProjectManager())
    current = _maybe(lambda: manager.GetCurrentProject()) if manager is not None else None
    project_name = _maybe(lambda: current.GetName()) if current is not None else None
    timeline_count = _maybe(
        lambda: current.GetTimelineCount()
    ) if current is not None else 0
    return {
        **base,
        "reachable": True,
        "product": str(product),
        "edition": edition,
        "version": _version_string(resolve),
        "project": project_name,
        "timeline_count": timeline_count if isinstance(timeline_count, int) else 0,
    }


# ---------------------------------------------------------------------------
# Projects
# ---------------------------------------------------------------------------

def _project_manager(resolve):
    manager = _invoke("GetProjectManager", resolve.GetProjectManager)
    if manager is None:
        raise BridgeError("resolve_unreachable", "Resolve project manager is unavailable")
    return manager


def current_project(resolve):
    manager = _project_manager(resolve)
    project = _invoke("GetCurrentProject", manager.GetCurrentProject)
    if project is None:
        raise BridgeError("project_not_found", "No Resolve project is currently open")
    return project


def list_projects(resolve):
    manager = _project_manager(resolve)
    names = _invoke("GetProjectListInCurrentFolder", manager.GetProjectListInCurrentFolder)
    return [str(name) for name in (names or [])]


def open_project(resolve, name):
    manager = _project_manager(resolve)
    project = _invoke("LoadProject", manager.LoadProject, name)
    if project is None:
        raise BridgeError("project_not_found", "Resolve project not found: %s" % name)
    current = _invoke("GetCurrentProject", manager.GetCurrentProject)
    current_name = _maybe(lambda: current.GetName()) if current is not None else None
    return {"project": current_name, "opened": current_name == name}


def create_project(resolve, name):
    manager = _project_manager(resolve)
    project = _invoke("CreateProject", manager.CreateProject, name)
    if project is None:
        raise BridgeError("resolve_call_failed", "Resolve refused to create project: %s" % name)
    current = _invoke("GetCurrentProject", manager.GetCurrentProject)
    current_name = _maybe(lambda: current.GetName()) if current is not None else None
    return {"project": current_name, "created": current_name == name}


def save_project(resolve):
    project = current_project(resolve)
    saved = _invoke("SaveProject", resolve.GetProjectManager().SaveProject)
    name = _maybe(lambda: project.GetName())
    folder_projects = _maybe(
        lambda: resolve.GetProjectManager().GetProjectListInCurrentFolder()
    ) or []
    persisted = bool(saved) and (name in [str(entry) for entry in folder_projects])
    return {"project": name, "saved": bool(saved), "persisted": persisted}


# ---------------------------------------------------------------------------
# Media pool
# ---------------------------------------------------------------------------

def _media_pool(resolve):
    project = current_project(resolve)
    pool = _invoke("GetMediaPool", project.GetMediaPool)
    if pool is None:
        raise BridgeError("resolve_unreachable", "Resolve media pool is unavailable")
    return pool


def _iter_subfolders(folder, depth=0):
    if depth > 8:
        return
    children = _maybe(lambda: folder.GetSubFolderList()) or []
    for child in children or []:
        yield child
        yield from _iter_subfolders(child, depth + 1)


def find_bin(pool, name):
    """Locate a media-pool bin by name (recursive); None means current folder."""
    if name is None:
        return None
    root = _invoke("GetRootFolder", pool.GetRootFolder)
    if root is None:
        raise BridgeError("resolve_unreachable", "Resolve media pool root is unavailable")
    if _maybe(lambda: root.GetName()) == name:
        return root
    for folder in _iter_subfolders(root):
        if _maybe(lambda: folder.GetName()) == name:
            return folder
    raise BridgeError("bin_not_found", "Media bin not found: %s" % name)


def _set_current_folder(pool, folder):
    if folder is None:
        return
    _invoke("SetCurrentFolder", pool.SetCurrentFolder, folder)


def import_media(resolve, paths, bin_name=None):
    pool = _media_pool(resolve)
    folder = find_bin(pool, bin_name) if bin_name is not None else None
    if folder is not None:
        _set_current_folder(pool, folder)
    items = _invoke("ImportMedia", pool.ImportMedia, list(paths))
    imported = []
    for item in items or []:
        label = _maybe(lambda: item.GetName())
        imported.append(str(label) if label else Path(str(paths[len(imported)])).name
                        if len(imported) < len(paths) else "clip")
    clips = [entry.get("name") for entry in list_media(resolve, bin_name)]
    missing = [label for label in imported if label not in clips]
    return {"imported": imported, "bin": bin_name, "verified_clips": missing == []}


def create_bin(resolve, name, parent=None):
    pool = _media_pool(resolve)
    if parent is not None:
        folder = find_bin(pool, parent)
        created = _invoke("AddSubFolder", pool.AddSubFolder, folder, name)
    else:
        try:
            created = _invoke("AddSubFolder", pool.AddSubFolder, name)
        except BridgeError:
            root = _invoke("GetRootFolder", pool.GetRootFolder)
            created = _invoke("AddSubFolder", pool.AddSubFolder, root, name)
    if created is None:
        raise BridgeError("resolve_call_failed", "Resolve refused to create bin: %s" % name)
    label = _maybe(lambda: created.GetName()) or name
    names = [entry for entry in _child_bin_names(pool)]
    return {"bin": label, "present": label in names}


def _child_bin_names(pool):
    root = _invoke("GetRootFolder", pool.GetRootFolder)
    names = []
    if root is not None:
        root_name = _maybe(lambda: root.GetName())
        if root_name:
            names.append(str(root_name))
        for folder in _iter_subfolders(root):
            folder_name = _maybe(lambda: folder.GetName())
            if folder_name:
                names.append(str(folder_name))
    return names


def list_media(resolve, bin_name=None):
    pool = _media_pool(resolve)
    previous = _maybe(lambda: pool.GetCurrentFolder())
    try:
        folder = find_bin(pool, bin_name) if bin_name is not None else None
        if folder is not None:
            _set_current_folder(pool, folder)
        clips = _invoke("GetClipList", pool.GetClipList) or []
    finally:
        if previous is not None:
            _maybe(lambda: pool.SetCurrentFolder(previous))
    result = []
    for clip in clips:
        name = _maybe(lambda: clip.GetName())
        clip_id = _maybe(lambda: clip.GetMediaId())
        entry = {"name": str(name) if name else "unknown"}
        if clip_id:
            entry["clip_id"] = str(clip_id)
        result.append(entry)
    return result


def find_clip(pool, clip_name, bin_name=None):
    """Find a media-pool item by clip name, searching the target bin or root."""
    root = _invoke("GetRootFolder", pool.GetRootFolder)
    folders = [root]
    if bin_name is not None:
        folders = [find_bin(pool, bin_name)]
    else:
        folders = [root] + list(_iter_subfolders(root))
    for folder in folders:
        clips = _maybe(lambda: folder.GetClipList()) or []
        for clip in clips or []:
            if _maybe(lambda: clip.GetName()) == clip_name:
                return clip
    raise BridgeError("clip_not_found", "Media clip not found: %s" % clip_name)


# ---------------------------------------------------------------------------
# Timelines
# ---------------------------------------------------------------------------

def list_timelines(resolve):
    project = current_project(resolve)
    count = _invoke("GetTimelineCount", project.GetTimelineCount)
    timelines = []
    for index in range(1, int(count or 0) + 1):
        timeline = _invoke("GetTimelineByIndex", project.GetTimelineByIndex, index)
        name = _maybe(lambda: timeline.GetName()) if timeline is not None else None
        timelines.append({"index": index, "name": str(name) if name else ""})
    return timelines


def get_timeline(resolve, name=None, index=None):
    project = current_project(resolve)
    if index is not None:
        timeline = _invoke("GetTimelineByIndex", project.GetTimelineByIndex, int(index))
        if timeline is None:
            raise BridgeError("timeline_not_found", "Timeline index not found: %s" % index)
        return timeline
    if name is not None:
        for entry in list_timelines(resolve):
            if entry["name"] == name:
                timeline = _invoke(
                    "GetTimelineByIndex", project.GetTimelineByIndex, entry["index"]
                )
                if timeline is None:
                    break
                return timeline
        raise BridgeError("timeline_not_found", "Timeline not found: %s" % name)
    timeline = _invoke("GetCurrentTimeline", project.GetCurrentTimeline)
    if timeline is None:
        raise BridgeError("timeline_not_found", "No timeline is currently open")
    return timeline


def current_timeline_name(resolve):
    timeline = get_timeline(resolve)
    return _maybe(lambda: timeline.GetName())


def open_timeline(resolve, name=None, index=None):
    project = current_project(resolve)
    timeline = get_timeline(resolve, name=name, index=index)
    ok = _invoke("SetCurrentTimeline", project.SetCurrentTimeline, timeline)
    current = _maybe(lambda: project.GetCurrentTimeline().GetName())
    expected = name if name is not None else _maybe(lambda: timeline.GetName())
    return {"timeline": current, "opened": bool(ok) and current == expected}


def create_timeline(resolve, name, bin_name=None):
    pool = _media_pool(resolve)
    timeline = None
    if bin_name is not None:
        folder = find_bin(pool, bin_name)
        creator = getattr(pool, "CreateTimelineFromBin", None)
        if not callable(creator):
            raise BridgeError(
                "unsupported",
                "Creating a timeline inside a bin is not supported by this Resolve build",
            )
        timeline = _invoke("CreateTimelineFromBin", creator, folder, name)
    else:
        creator = getattr(pool, "CreateEmptyTimeline", None)
        if callable(creator):
            timeline = _invoke("CreateEmptyTimeline", creator, name)
        else:
            fallback = getattr(pool, "CreateTimelineFromBin", None)
            if not callable(fallback):
                raise BridgeError(
                    "unsupported",
                    "Timeline creation is not supported by this Resolve build",
                )
            root = _invoke("GetRootFolder", pool.GetRootFolder)
            timeline = _invoke("CreateTimelineFromBin", fallback, root, name)
    if timeline is None:
        raise BridgeError("resolve_call_failed", "Resolve refused to create timeline: %s" % name)
    label = _maybe(lambda: timeline.GetName()) or name
    names = [entry["name"] for entry in list_timelines(resolve)]
    return {"timeline": label, "present": label in names}


def _items_in_track(timeline, track_type, track_index):
    getter = getattr(timeline, "GetItemListInTrack", None)
    if not callable(getter):
        raise BridgeError(
            "unsupported",
            "Listing timeline items is not supported by this Resolve build",
        )
    items = _invoke("GetItemListInTrack", getter, track_type, int(track_index))
    return list(items or [])


def summarize_item(item, index):
    name = _maybe(lambda: item.GetName())
    start = _maybe(lambda: item.GetStart())
    end = _maybe(lambda: item.GetEnd())
    duration = _maybe(lambda: item.GetDuration())
    summary = {"index": index, "name": str(name) if name else "unknown"}
    if isinstance(start, (int, float)):
        summary["start"] = int(start)
    if isinstance(end, (int, float)):
        summary["end"] = int(end)
    if isinstance(duration, (int, float)):
        summary["duration"] = int(duration)
    return summary


def list_timeline_items(resolve, track_type, track_index, name=None, index=None):
    timeline = get_timeline(resolve, name=name, index=index)
    items = _items_in_track(timeline, track_type, int(track_index))
    label = _maybe(lambda: timeline.GetName())
    return {
        "timeline": label,
        "track_type": track_type,
        "track_index": int(track_index),
        "items": [summarize_item(item, pos) for pos, item in enumerate(items)],
    }


def get_track_item(timeline, track_type, track_index, item_index):
    items = _items_in_track(timeline, track_type, int(track_index))
    if int(item_index) >= len(items):
        raise BridgeError(
            "item_not_found",
            "Item %s is out of range: %s track %s holds %s item(s)"
            % (item_index, track_type, track_index, len(items)),
        )
    return items[int(item_index)], len(items)


def append_to_timeline(resolve, clip_name, bin_name=None, start_frame=None,
                       end_frame=None, record_frame=None, track_index=None):
    pool = _media_pool(resolve)
    clip = find_clip(pool, clip_name, bin_name)
    clip_info = {"mediaPoolItem": clip}
    if start_frame is not None:
        clip_info["startFrame"] = int(start_frame)
    if end_frame is not None:
        clip_info["endFrame"] = int(end_frame)
    if record_frame is not None:
        clip_info["recordFrame"] = int(record_frame)
    if track_index is not None:
        clip_info["trackIndex"] = int(track_index)
    created = _invoke("AppendToTimeline", pool.AppendToTimeline, [clip_info])
    if not created:
        raise BridgeError("resolve_call_failed", "Resolve refused to append clip: %s" % clip_name)
    first = created[0]
    label = _maybe(lambda: first.GetName())
    return {"clip": clip_name, "timeline_item": str(label) if label else clip_name}


def add_marker(resolve, frame, color, name, note, duration,
               name_selector=None, index_selector=None):
    timeline = get_timeline(resolve, name=name_selector, index=index_selector)
    adder = getattr(timeline, "AddMarker", None)
    if not callable(adder):
        raise BridgeError(
            "unsupported", "Timeline markers are not supported by this Resolve build"
        )
    ok = _invoke("AddMarker", adder, int(frame), color, name, note, int(duration), "")
    markers = get_markers(resolve, timeline=timeline)
    present = str(int(frame)) in markers or int(frame) in markers
    return {"frame": int(frame), "color": color, "added": bool(ok), "present": bool(present)}


def get_markers(resolve, name=None, index=None, timeline=None):
    active = timeline if timeline is not None else get_timeline(
        resolve, name=name, index=index
    )
    getter = getattr(active, "GetMarkers", None)
    if not callable(getter):
        raise BridgeError(
            "unsupported", "Timeline markers are not supported by this Resolve build"
        )
    markers = _invoke("GetMarkers", getter) or {}
    label = _maybe(lambda: active.GetName())
    normalized = {str(frame): dict(info or {}) for frame, info in dict(markers).items()}
    return {"timeline": label, "markers": normalized} if timeline is None else normalized


def delete_marker(resolve, frame=None, color=None, name_selector=None,
                  index_selector=None):
    timeline = get_timeline(resolve, name=name_selector, index=index_selector)
    if frame is not None:
        remover = getattr(timeline, "DeleteMarkerAtFrame", None)
        if not callable(remover):
            raise BridgeError(
                "unsupported",
                "Deleting a marker by frame is not supported by this Resolve build",
            )
        ok = _invoke("DeleteMarkerAtFrame", remover, int(frame))
        markers = get_markers(resolve, timeline=timeline)
        gone = str(int(frame)) not in markers and int(frame) not in markers
        return {"frame": int(frame), "deleted": bool(ok), "gone": bool(gone)}
    remover = getattr(timeline, "DeleteMarkersByColor", None)
    if not callable(remover):
        raise BridgeError(
            "unsupported",
            "Deleting markers by color is not supported by this Resolve build",
        )
    ok = _invoke("DeleteMarkersByColor", remover, color)
    markers = get_markers(resolve, timeline=timeline)
    remaining = [info for info in markers.values()
                 if str(info.get("color", "")) == color]
    return {"color": color, "deleted": bool(ok), "remaining": len(remaining)}


def get_item_property(resolve, track_type, track_index, item_index, key,
                      name=None, index=None):
    timeline = get_timeline(resolve, name=name, index=index)
    item, _ = get_track_item(timeline, track_type, track_index, item_index)
    getter = getattr(item, "GetProperty", None)
    if not callable(getter):
        raise BridgeError(
            "unsupported",
            "Timeline item properties are not supported by this Resolve build",
        )
    value = _invoke("GetProperty", getter, key)
    return {"key": key, "value": value}


def set_item_property(resolve, track_type, track_index, item_index, key, value,
                      name=None, index=None):
    timeline = get_timeline(resolve, name=name, index=index)
    item, _ = get_track_item(timeline, track_type, track_index, item_index)
    setter = getattr(item, "SetProperty", None)
    if not callable(setter):
        raise BridgeError(
            "unsupported",
            "Timeline item properties are not supported by this Resolve build",
        )
    ok = _invoke("SetProperty", setter, key, value)
    readback = _invoke("GetProperty", item.GetProperty, key)
    return {"key": key, "value": readback, "applied": bool(ok)}


# ---------------------------------------------------------------------------
# Render
# ---------------------------------------------------------------------------

def list_render_presets(resolve):
    project = current_project(resolve)
    presets = _invoke("GetRenderPresetList", project.GetRenderPresetList)
    return [str(entry) for entry in (presets or [])]


def get_render_settings(resolve):
    project = current_project(resolve)
    settings = _invoke("GetRenderSettings", project.GetRenderSettings)
    return dict(settings or {})


def configure_render(resolve, settings):
    project = current_project(resolve)
    ok = _invoke("SetRenderSettings", project.SetRenderSettings, dict(settings))
    if not ok:
        raise BridgeError("resolve_call_failed", "Resolve refused the render settings")
    return get_render_settings(resolve)


def _normalize_jobs(entries):
    jobs = []
    for entry in entries or []:
        if not isinstance(entry, dict):
            continue
        job_id = entry.get("RenderJobId", entry.get("JobId", entry.get("JobID", entry.get("Id"))))
        if job_id is None:
            continue
        record = {"job_id": str(job_id)}
        status = entry.get("JobStatus", entry.get("Status"))
        if status is not None:
            record["status"] = str(status)
        for extra in ("CompletionPercentage", "TargetDir", "CustomName", "OutputFilename"):
            if entry.get(extra) is not None:
                record[_snake(extra)] = entry.get(extra)
        jobs.append(record)
    return jobs


def _snake(name):
    out = []
    for pos, char in enumerate(name):
        if char.isupper() and pos:
            out.append("_")
        out.append(char.lower())
    return "".join(out)


def list_render_jobs(resolve):
    project = current_project(resolve)
    entries = _invoke("GetRenderJobList", project.GetRenderJobList)
    return _normalize_jobs(entries)


def add_render_job(resolve):
    project = current_project(resolve)
    job_id = _invoke("AddRenderJob", project.AddRenderJob)
    if not job_id:
        raise BridgeError("resolve_call_failed", "Resolve refused to queue a render job")
    job_id = str(job_id)
    queued = [job for job in list_render_jobs(resolve) if job["job_id"] == job_id]
    return {"job_id": job_id, "queued": bool(queued)}


def start_render(resolve, job_ids=None):
    project = current_project(resolve)
    if job_ids:
        started = _invoke("StartRendering", project.StartRendering, list(job_ids), False)
    else:
        try:
            started = _invoke("StartRendering", project.StartRendering)
        except BridgeError:
            started = _invoke("StartRendering", project.StartRendering, [], False)
    if not started:
        raise BridgeError("render_failed", "Resolve refused to start rendering")
    return {"started": True}


def render_status(resolve, job_id=None):
    project = current_project(resolve)
    rendering = bool(_maybe(lambda: project.IsRenderingInProgress()))
    if job_id is not None:
        getter = getattr(project, "GetRenderJobStatus", None)
        if not callable(getter):
            raise BridgeError(
                "unsupported",
                "Per-job render status is not supported by this Resolve build",
            )
        status = _invoke("GetRenderJobStatus", getter, job_id) or {}
        record = {"job_id": job_id, "rendering": rendering}
        if isinstance(status, dict):
            if status.get("JobStatus") is not None:
                record["status"] = str(status.get("JobStatus"))
            if status.get("CompletionPercentage") is not None:
                record["completion_percentage"] = status.get("CompletionPercentage")
        return record
    return {"rendering": rendering, "jobs": list_render_jobs(resolve)}


def cancel_render(resolve):
    project = current_project(resolve)
    _invoke("StopRendering", project.StopRendering)
    rendering = bool(_maybe(lambda: project.IsRenderingInProgress()))
    jobs = list_render_jobs(resolve)
    active = [job for job in jobs
              if str(job.get("status", "")).lower() in ("rendering", "queued", "running")]
    return {"cancelled": not rendering and not active, "rendering": rendering, "jobs": jobs}
