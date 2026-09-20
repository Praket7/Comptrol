# Verification contract

A successful Resolve API return is delivery evidence, never task completion. Each
intent below carries its verifier; a failed readback returns `verification_failed`.

| Intent | Verifier |
| --- | --- |
| `video.project.list` | `resolve_state_readback`: names come from `GetProjectListInCurrentFolder`. |
| `video.project.open` | `resolve_project_readback`: `GetCurrentProject().GetName()` equals the requested name. |
| `video.project.create` | `resolve_project_readback`: current project equals the created name. |
| `video.project.save` | `resolve_project_persisted`: `SaveProject()` true AND the project is listed in its folder (database persistence; Resolve saves to its project database, not a file). |
| `video.media.import` | `resolve_media_pool_readback`: every imported clip name reappears in the target bin clip list. |
| `video.media.bin.create` | `resolve_media_pool_readback`: the new bin name reappears in the folder walk. |
| `video.media.list` | `resolve_state_readback`: entries come from the live `GetClipList`. |
| `video.timeline.list` | `resolve_state_readback`: entries come from `GetTimelineCount`/`GetTimelineByIndex`. |
| `video.timeline.open` | `resolve_timeline_readback`: current timeline equals the requested name. |
| `video.timeline.create` | `resolve_timeline_readback`: the new name reappears in the timeline list. |
| `video.timeline.items.list` | `resolve_state_readback`: items come from the live `GetItemListInTrack`. |
| `video.timeline.append` | `resolve_timeline_readback`: re-listed items contain the appended clip. |
| `video.timeline.insert` | `resolve_timeline_readback`: re-listed items contain the clip pinned at `recordFrame`. |
| `video.timeline.batch` | `resolve_batch_readback`: every op applied in one session, then item and marker counts re-read. |
| `video.timeline.marker.add` | `resolve_marker_readback`: `GetMarkers()` contains the frame. |
| `video.timeline.marker.delete` | `resolve_marker_readback`: frame gone, or zero markers of the color remain. |
| `video.timeline.item.properties.get` | `resolve_state_readback`: value comes from the live `GetProperty`. |
| `video.timeline.item.properties.set` | `resolve_item_property_readback`: `GetProperty` equals the written value. |
| `video.render.preset.list` | `resolve_state_readback`: presets come from `GetRenderPresetList`. |
| `video.render.configure` | `resolve_render_settings_readback`: `GetRenderSettings()` matches every configured key. |
| `video.render.add_job` | `resolve_render_readback`: the job id reappears in `GetRenderJobList`. |
| `video.render.start` | `resolve_render_artifact_readback`: job status confirms start; when a file target is configured the output must exist with nonzero size and an allowlisted video suffix — mandatory once the job reports complete. Poll `video.render.status` until completion. |
| `video.render.status` | `resolve_render_readback`: status comes from the live job query. |
| `video.render.cancel` | `resolve_render_readback`: `IsRenderingInProgress()` is false and no job remains rendering/queued. |
