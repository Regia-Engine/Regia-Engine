# Video

<!-- index: Video panel -->
<!-- index: media-codecs -->

Panel id: `video`. The **Video** tab opens a `.webm`, `.mkv` or `.video.ron` from the project, shows the decoded
picture, plays, pauses, scrubs and steps single frames, lists the file's streams and decode
counters, and edits the boot movie list of the project's `Regia.toml`. It opens a file the way a game's
video player does, so what the tab shows is what the runtime decodes. Depth:
[Video panel (agents)](../../agents/video-panel.md), [Video playback](../../agents/video-playback.md).

## Open it

The tab is in the default layout's bottom group; **View ▸ Reset layout** brings back a closed one.
Double-clicking a video file in the **Content** tab opens it here (and brings the tab forward). An
agent brings the tab forward with `editor.layout_apply {"open": "video"}`. There is no palette command
for this tab.

## Watch a video

1. Type a project-relative path such as `movies/intro.webm` and press **Open**, or double-click the file
   in the Content tab. A project must be open; without one the tab says so.
2. The picture appears with the transport row: **⏮** (previous frame), **Play** or **Pause**, **⏭**
   (next frame), a **frame** slider and the time over the duration.
3. Open **Stream** for the codec, size, frame rate, duration and container, each audio track, and each
   caption or subtitle track with the cues read so far. The line under it reports decode time,
   frames presented, dropped and corrupt, and the decode pool use.
4. A file that cannot play leaves the tab in an error state shown in red (a missing file, a damaged
   container, a refused codec, a full decode pool).

## Edit the boot movies

Under **Boot movies (Regia.toml [boot])** the tab lists the movies a game plays at start, in order.

1. A red code beside an entry means it does not open.
2. **Up** and **Down** reorder, **Remove** deletes an entry.
3. Type a project-relative path in the box (for example `movies/studio.video.ron`) and press **Add**.
4. The `skip_video` line tells where the skip key binding comes from: the project's
   `config/input.ron`, the default keys (Escape, Space, Enter, gamepad South and Start), or an input
   file that cannot be read.
5. **Save** writes the list back to `Regia.toml`.

## Actions and agent methods

| Action | Control | Agent method |
|---|---|---|
| Open a video | Path field, **Open**; double-click in Content | `editor.video_open` |
| Seek, step, play, pause | **⏮**, **⏭**, **Play**, **Pause**, the **frame** slider | `editor.video_seek` |
| Read the player, stream and decode counters | The tab | `editor.video_state` |
| Reorder, remove, add boot movies; save | **Up**, **Down**, **Remove**, **Add**, **Save** | `editor.video_boot` |

## Keyboard

The tab handles no keys of its own; reach its controls with Tab and press them with Enter or Space.

## Known limits

- Compressed codecs are off unless the build turns them on. Cargo feature `media-codecs` (AV1 video, Opus and Vorbis audio) is off by default. Without it the tab refuses those files, the same refusal the runtime uses (`video_panel_codec_unsupported`, and `video_codec_unsupported` on `video.open`). Every build still decodes the reference codecs: uncompressed I420 video (`V_UNCOMPRESSED`) and float PCM audio (`A_PCM/FLOAT/IEEE`). See [Video playback](../../agents/video-playback.md).
- A project must be open to open a video or edit the boot list.
- Caption and subtitle cues are read as the decoder reaches them, so step or play to the end to see
  them all.
- A **Save** that changes the list rewrites the manifest, so its comments and key order are not
  preserved; a list that did not change is never written.

See also: [Content browser](content-browser.md), [Audio](../audio.md), [Editor guide](index.md).
