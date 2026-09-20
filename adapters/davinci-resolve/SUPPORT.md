# Support boundary

Exact support matrix. "Scripting" means the typed local external-scripting path in
`src/resolve_bridge.py`. Anything else falls back to accessibility/canvas control
outside this adapter.

| Intent | Backend | Status |
| --- | --- | --- |
| `video.project.list` | Scripting (`GetProjectListInCurrentFolder`) | Supported |
| `video.project.open` | Scripting (`LoadProject` + current-project readback) | Supported |
| `video.project.create` | Scripting (`CreateProject` + current-project readback) | Supported |
| `video.project.save` | Scripting (`SaveProject` + folder-list persistence check) | Supported |
| `video.media.import` | Scripting (`ImportMedia` + clip-list readback) | Supported |
| `video.media.bin.create` | Scripting (`AddSubFolder` + bin-list readback) | Supported |
| `video.media.list` | Scripting (`GetClipList`) | Supported |
| `video.timeline.list` | Scripting (`GetTimelineCount`/`GetTimelineByIndex`) | Supported |
| `video.timeline.open` | Scripting (`SetCurrentTimeline` + readback) | Supported |
| `video.timeline.create` | Scripting (`CreateEmptyTimeline`/`CreateTimelineFromBin`) | Supported |
| `video.timeline.items.list` | Scripting (`GetItemListInTrack`) | Supported |
| `video.timeline.append` | Scripting (`AppendToTimeline`) | Supported |
| `video.timeline.insert` | Scripting (`AppendToTimeline` pinned to `recordFrame`) | Supported |
| `video.timeline.batch` | Scripting (one session, typed op list) | Supported |
| `video.timeline.marker.add` | Scripting (`AddMarker` + `GetMarkers` readback) | Supported |
| `video.timeline.marker.delete` | Scripting (`DeleteMarkerAtFrame`/`DeleteMarkersByColor`) | Supported |
| `video.timeline.item.properties.get` | Scripting (`GetProperty`, closed key set) | Supported |
| `video.timeline.item.properties.set` | Scripting (`SetProperty` + property readback) | Supported |
| `video.render.preset.list` | Scripting (`GetRenderPresetList`) | Supported |
| `video.render.configure` | Scripting (`SetRenderSettings` + settings readback, closed keys) | Supported |
| `video.render.add_job` | Scripting (`AddRenderJob` + job-list readback) | Supported |
| `video.render.start` | Scripting (`StartRendering` + status and artifact check) | Supported |
| `video.render.status` | Scripting (`GetRenderJobStatus`/`GetRenderJobList`) | Supported |
| `video.render.cancel` | Scripting (`StopRendering` + idle readback) | Supported |

Falls back to accessibility/canvas control (not implemented here): Color page grading
controls and scopes, Fusion composition editing inside clips, Fairlight mixer
automation and audio effects, viewer transport/jog/shuttle, Cut page smart tools
(source-tape, sync-bin multicam selection), keyframe curve editing, multi-user
Blackmagic Cloud collaboration, Project Server / Postgres project libraries, and any
Studio-only feature when the free edition is detected. Remote control over the
network is unsupported by design and never enabled.
