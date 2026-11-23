# Sequence curves panel (editor)

The editor's **Sequence Curves** tab is the curve editor of `.seq.ron` sequences (ADR-1880). It edits one
curve at a time: one component of one float or vec3 track of a sequence player in the editor world (a player
loaded with [`sequence.player`](sequence-player.md) or spawned by the host). The curve is drawn with the
engine's own sampling, so Bezier tangents, handle weights, steps and smoothstep look exactly as they play.
Every edit is validated on a copy of the whole sequence and then swapped into the player (its clock and play
state stay); a refused edit changes nothing. Each edit is an undo entry (a drag is one entry, and `editor.undo`
takes it back), and the panel opens and saves project `.seq.ron` files.

## In the panel

- Type a project path and press **Open** to spawn a stopped player of that file (its sub-sequences join the
  sequence library), or **Save** to write the sequence there. Each player remembers its own file: picking
  another player shows that player's path, empty if it has none yet, so **Save** never writes one sequence over
  another's file.
- Pick the **Player**, the **Track** (float, vec3, clip, audio and sub-sequence tracks; a clip, audio or
  sub-sequence track shows the [section strip](#section-strip) instead of a curve) and, for a vec3 track, the
  component **x**, **y** or **z**.
- Drag a key to move it in time and value; the keys re-sort. Double-click empty space to add a key (a
  `Bezier` key with an `Auto` tangent; a vec3 key takes the other components from their curves at that time,
  so they keep their shape). Drag empty space to scrub the player.
- Tangent handles show on the sides of a key that a `Bezier` segment uses. Dragging a handle sets one slope
  on both sides (`Aligned`); hold **Alt** to move only that side (`Broken`); a broken key stays broken.
- Right-click a key to select it and open its menu: the ease of the segment that starts there (`Linear`, `Step`, `Smooth`,
  `Bezier`), the tangent mode (`Auto`, `Flat`, `Linear`, `Aligned`, `Broken`) and **Delete key** (a curve keeps
  at least one key). The grid pans with the secondary or middle button and zooms with the wheel (Shift: time
  only, Ctrl: value only). The playhead is the player's clock.

## Agent methods

- `editor.seq_curve_state { player?, track?, component?, samples? }` (read-only) selects a player (entity
  id), track (by name) and component if given and returns `{ players, player, path, dirty, time, tracks: [{ name, kind }],
  track, track_name, kind, component, components, has_curve, selected, keys: [{ time, value, ease, tangent,
  slopes: [in, out], weights }], samples, error }`. `slopes` are the resolved slopes (also for `auto` and
  `linear` keys); `samples: n` (2..=1024) adds `n` evenly spaced `[time, value]` points from the first key to
  the last, sampled the way the player plays them. `path` is the player's project `.seq.ron` (once opened or
  saved, else `null`) and `dirty` says it has edits that file does not hold yet; such a sequence is autosaved
  and offered for recovery after a crash (kind `sequence`, [Editor autosave and recovery](editor-autosave.md)). With no player the
  answer is `{ players, player: null, path: null, dirty: false, error }`.
- `editor.seq_curve_edit { op, player?, track?, component?, ... }` (needs `edit_scene`) does one edit and
  returns `{ applied, selected, keys }`:
  - `op: "add"` with `time`, `value`; `op: "move"` with `index`, `time`, `value`; `op: "remove"` with `index`.
  - `op: "ease"` with `index` and `ease`: `Linear`, `Step`, `Smooth` or `Bezier`.
  - `op: "tangent"` with `index` and `tangent`: `auto`, `flat`, `linear`, `aligned` (with `slope`) or
    `broken` (with `in_slope`, `out_slope`). On a vec3 key the slope is this component's; the other
    components keep the slopes they had.
  - `op: "weights"` with `index` and `weights`: `[incoming, outgoing]`, each in `(0, 1]`, or `null`.
- `editor.seq_curve_open { path }` (needs `edit_scene`) reads a project-relative `.seq.ron`, adds its
  sub-sequences to the sequence library, spawns a stopped player of it and selects it, on its first float or
  vec3 track. It returns `{ player, path, tracks: [{ name, kind }], sub_sequences }`.
- `editor.seq_curve_save { path?, player? }` (needs `edit_scene`) writes the sequence with an atomic write, to
  `path` or to the path this player was opened from or last saved to, and returns `{ saved, version }`. The file is saved at the current format
  version. Parent folders are created.

```json
{ "method": "editor.seq_curve_edit", "params": { "track": "fade", "op": "tangent", "index": 0,
  "tangent": "aligned", "slope": 1.5 } }
{ "method": "editor.seq_curve_state", "params": { "samples": 32 } }
{ "method": "editor.seq_curve_open", "params": { "path": "cine/intro.seq.ron" } }
{ "method": "editor.seq_curve_save", "params": {} }
```

## Error codes

| Code | Meaning |
|---|---|
| `seq_curve_no_player` | No sequence player exists, or `player` is not one. |
| `seq_curve_bad_track` | No track with that name. `track` is a name, not an index: a number or any other non-string is refused and the selected track stays. |
| `seq_curve_not_a_curve` | The track is a bool, clip, audio or sub-sequence track. |
| `seq_curve_bad_component` | The component is past the track's (0 for float, 0..3 for vec3). |
| `seq_curve_bad_key` | No key with that index. |
| `seq_curve_out_of_range` | A negative or non-finite time, or a non-finite value or slope. |
| `seq_curve_key_time_taken` | Another key already sits at that time. |
| `seq_curve_last_key` | Removing the only key of a curve. |
| `seq_curve_bad_weights` | Weights outside `(0, 1]`. |
| `seq_curve_invalid` | The edited sequence does not validate (the message says why). |
| `seq_curve_bad_path` | The path is not relative to the project, has `..`, leads out through a symlink, does not end in `.seq.ron`, or none was given and the panel has none. |
| `seq_curve_no_project` | No project is open, so there is no root for the path. |
| `seq_curve_io` | The file cannot be read or written. |
| `seq_curve_bad_file` | The file or one of its sub-sequences does not parse, or the sub-sequence tree is refused (a cycle, a missing file's content, too deep). |

A missing or malformed parameter (`op`, `index`, `time`, `slope`, ...) is a plain invalid-params error.

## From Rust

The edit model is `regia_framework::sequencer::edit::apply_key_edit(&mut channel, component, edit)`, the
same code the panel and the methods run. The panel's state is the `SeqCurvePanel` resource;
`seq_curve_panel::{edit_selected, open_sequence, save_sequence}` are the panel's own operations, and the undo
entry is `seq_curve_panel::SeqCurveEdit`.

## Section strip

Picking a clip, audio or sub-sequence track shows its sections as bars on a time strip (ADR-2135).
Overlapping sections take separate rows: each section takes the lowest row free at its start, visiting the
sections by start time then index, so the layout is the same on every machine. Darker wedges at a bar's ends
are its blend (clip) or fade (audio) in and out.

- Drag a bar to move it (its length and what it plays stay). Drag its left or right edge to trim it (the edge
  moves by the pointer's travel, so pressing beside it does not make it jump). A trim
  keeps the content where it was on the clock: trimming a clip's start one second later at speed 2 moves its
  offset two clip seconds on, so the same frames show at the same instants. A section made shorter than its
  ramps has both ramps scaled down by one factor so they fit.
- Drags snap to the sequence's frames (its `fps`) unless **Snap to frames** is off. Drag empty space to
  scrub the player; the wheel zooms the time axis and the secondary or middle button pans it.
- Right-click a bar to select it and open its menu: **Duplicate after it** (a copy that starts where it ends)
  and **Delete section** (a track keeps at least one section).
- The row under the strip edits the selected section's numbers: start and end (trims, like the edges),
  offset, speed (clip and sub-sequence), in and out (the blend or fade), the weight (clip) or dB (audio), and
  the clip, sound or sequence name (applied on Enter or when the field loses focus). Dragging a number is one
  undo entry.
- Every frame of a drag edits the track as the drag found it, so dragging an edge in past the ramps and back
  out restores them, and a trim dragged back to where it began leaves the offset exactly where it was. An
  agent call during a drag is its own undo entry.

Sections keep their index: edits never reorder them and an added or duplicated section goes last.

- `editor.seq_sections_state { player?, track? }` (read-only) selects a player and track if given and returns
  `{ players, player, time, fps, track, track_name, kind, selected, rows, sections: [{ index, row, start, end,
  label, rise, fall, section }], error }`. `section` is the section as the file has it (`clip`, `sound` or
  `sequence`, `offset`, `speed`, `bindings`, ...); a float, vec3 or bool track has no sections (`[]`, `rows: 0`).
- `editor.seq_section_edit { op, player?, track?, ... }` (needs `edit_scene`) does one edit, validated on a copy
  of the whole sequence first, and returns `{ applied, selected, sections }`. Each call is an undo entry.
  - `op: "add"` with `section`: an object as in the file (`{ "clip": "wave", "start": 4.0, "end": 5.0 }`;
    the other fields default), read as the track's kind.
  - `op: "duplicate"` with `index`, `start`; `op: "remove"` with `index`.
  - `op: "move"` with `index`, `start`; `op: "trim_start"` with `index`, `start`; `op: "trim_end"` with `index`,
    `end`.
  - `op: "speed"` with `index`, `speed` in `(0, 1000]` (clip and sub-sequence sections; start and end stay).
  - `op: "offset"` with `index`, `offset` (content seconds at the start; zero or more for a sound).
  - `op: "ramps"` with `index`, `rise`, `fall` (clip blend or audio fade; their sum must fit the section).
  - `op: "gain"` with `index`, `value`: a clip's weight in `(0, 1]` or a sound's volume in dB (at most 24).
  - `op: "rename"` with `index`, `name`: the clip, sound or sub-sequence the section plays (not empty).

```json
{ "method": "editor.seq_sections_state", "params": { "track": "keeper-body" } }
{ "method": "editor.seq_section_edit", "params": { "track": "keeper-body", "op": "trim_start", "index": 1,
  "start": 3.25 } }
{ "method": "editor.seq_section_edit", "params": { "track": "ambience", "op": "add",
  "section": { "sound": "chime", "start": 5.0, "end": 5.75, "volume_db": -6.0 } } }
```

The showcase's intro (`samples/showcase/example_scene/cinematics/intro.seq.ron`) has a clip track
(`keeper-body`), an audio track (`ambience`) and a sub-sequence track (`sparkle`) to try these on:
`editor.seq_curve_open { "path": "cinematics/intro.seq.ron" }` with the showcase scene as the project.

Section errors (the panel's `seq_curve_no_player`, `seq_curve_bad_track` and `seq_curve_invalid` apply too):

| Code | Meaning |
|---|---|
| `seq_section_not_sections` | The track is a float, vec3 or bool track (edit its curve with `editor.seq_curve_edit`). |
| `seq_section_bad_index` | No section with that index. |
| `seq_section_wrong_kind` | The added section is of another kind, or the op does not apply to it (a speed on a sound, ramps or a gain on a sub-sequence). |
| `seq_section_out_of_range` | A negative, reversed or non-finite time, a speed, weight, volume or offset out of range, ramps longer than the section, an empty name, or anything else the format refuses. |
| `seq_section_last` | Removing a track's only section. |
| `seq_section_limit` | The track already holds 1024 sections. |

From Rust, the edit model is `regia_framework::sequencer::sections::apply_section_edit(&mut channel, edit)` and
the layout `section_spans(&channel)`; `seq_sections::edit_section` is the panel's own operation and
`SeqSectionStrip` the strip's view resource.

## v3 section edits and binding renames (ADR-2405)

The number row under the strip shows, for an audio section, its **time scale** (varispeed: pitch
follows) and a **3D** switch (on: the sound plays from a fixed position at the origin; set the
source by agent call or in the file) with **min m**, **max m** and **3D blend** while it is on; for a
sub-sequence section, its **repeat** (once, count, until_stop) and the **passes** of a count. The
**Rename binding** row under it picks one binding of the sequence and renames it everywhere the
sequence refers to it (track targets, spatial sources, sub-sequence binding tables) and in the
player's binding table, as **one undo step**: undo brings the old name and the old table back.

The same edits by agent call, `editor.seq_section_edit` with the v3 ops:

```json
{ "method": "editor.seq_section_edit", "params": { "track": "music", "op": "time_scale", "index": 0, "scale": 1.5 } }
{ "method": "editor.seq_section_edit", "params": { "track": "music", "op": "spatial", "index": 0,
  "spatial": { "source": { "Binding": "hero" }, "max_distance": 30.0 } } }
{ "method": "editor.seq_section_edit", "params": { "track": "music", "op": "spatial", "index": 0, "spatial": null } }
{ "method": "editor.seq_section_edit", "params": { "track": "fx", "op": "repeat", "index": 0,
  "repeat": "count", "count": 3, "loop_length": 0.25 } }
{ "method": "editor.seq_section_edit", "params": { "op": "rename_binding", "from": "hero", "to": "guard" } }
{ "method": "editor.seq_section_edit", "params": { "track": "fx", "op": "sub_binding", "index": 0,
  "from": "lamp", "to": "hero" } }
```

`rename_binding` needs no track and returns `{ applied, from, to, tracks, sounds, sub_sections,
unresolved, bindings }`. `editor.seq_sections_state` lists the sequence's `bindings`. Codes:
`seq_section_wrong_kind` (a time scale or 3D placement on a non-audio section, a repeat or a
target rename on a non-sub-sequence section), `seq_section_out_of_range` (a value the format
refuses), and for the rename `seq_binding_empty`, `seq_binding_same`, `seq_binding_in_use` (the
sequence already binds `to`), `seq_binding_unknown` (nothing refers to `from`) and
`seq_binding_table_in_use` (the player's table already binds `to`). A refused edit changes
nothing. Outside the editor, `sequencer.rename_binding` does the same rename without undo
([sequence players](sequence-player.md)).

**Per-section target renames.** `op: "sub_binding"` edits one sub-sequence section's `bindings`
map alone (`from` is the sub-sequence's target name, `to` the name it binds as here; `to: null`
or absent removes the entry) — the other sections keep their tables, unlike the sequence-wide
`rename_binding`. One undo entry per call. Codes: `seq_binding_empty` (`from` or `to` empty),
`seq_binding_same` (`from == to` on a set), `seq_binding_unknown` (removing a target the section
does not bind), `seq_section_bad_index`, `seq_section_wrong_kind` (a non-sub-sequence track).
The strip draws the selected sub section's table as the **Bindings** row: each pair as
`target → binds as` with a remove button, and two fields that set a pair on Enter.

## Record take (ADR-2637)

The **Record take** section at the top of the panel records play into a take of the edited sequence.
Pick the edited player's bound targets and/or **Active camera**, then the channels: **Transform**,
**Animation**, and property paths as comma-separated text (`light.intensity, reflect:PointLight.radius`).
Set **Audio cues**, **Include driven** and **Countdown (frames)** as needed. **Record** starts the
take and **Stop** writes `<name>_take<N>.seq.ron` next to the panel's sequence (or the source typed
in the section), adds it to that sequence as a sub-sequence section, and reloads the edited player
when it plays that sequence. While PIE runs, the take records the play world, and only targets bound
by guid can be recorded. The agent methods are `sequence.record_start`, `sequence.record_stop` and
`sequence.record_state`; see [Take recorder](sequence-take-recorder.md). From Rust:
`regia_editor::seq_curve_panel::{SeqTakePanel, take_config, start_take, stop_take}`.

## Not done yet

- A spatial section's source binding is edited by agent call or in the file. (A sub-sequence
  section's individual target renames moved out of this list: the `sub_binding` op and the
  **Bindings** row edit them per section now.)
- The v3 controls were driven by tests with synthetic egui input; nobody has judged them on a screen
  yet.
- The Record take section was driven by tests through `start_take` and `stop_take`, not with egui input;
  nobody has judged it on a screen yet.

## Video sections

A `Video` track (`.seq.ron` v4, ADR-2862) shows on the strip like the others, and each bar starts with a still of
the picture the section begins on (at its `offset`). Stills decode on their own thread and are cached by
source and position, at most 128 pixels wide; they come from the project's `MediaFiles`, or the project root.
The number row edits start, end, offset, the source path, `dB` (the source's own sound) and **Loop**, and picks
the target: fullscreen (layer and fit), player, screen (slot) or UI image, with its binding name.

`editor.seq_section_edit` ops: `gain` and `rename` apply as for sounds, `video_loop { looping }` and
`video_target { target }` are new, with `target` as in the file:
`{ "Fullscreen": { "layer": 0, "fit": "Fill" } }`, `{ "Player": "kiosk" }`,
`{ "Screen": { "binding": "tv", "slot": "Emissive" } }` or `{ "UiImage": "slate" }`. `speed` and `ramps` answer
`seq_section_wrong_kind`. Each call is an undo entry. `editor.seq_sections_state` adds `thumbnails: [{ index, state:
pending | ready | failed, frame, width, height, code }]` for a video track: `frame` is the picture's index and
`code` the media error, like `source_not_found`.

`editor.seq_section_edit { op: "binding_kind", binding, kind }` makes a binding hand-bound (`kind: "bound"`)
or spawned (`kind: "spawnable"`, ADR-2948); a spawnable binding takes `prefab` (a project `.prefab.ron` path or
`guid:<32 hex>`) and `keep_on_stop` (leave the actor in the world when the player stops).

## Sequence authoring (ADR-9710)

The **Sequence authoring** rows at the top of the panel create what the curve and section editors work inside.
Every edit is validated on a copy of the whole sequence (a refused edit changes nothing and shows its message
under the rows) and is one undo entry; `editor.undo` restores the sequence bytes and the player's binding table.

- **New sequence**: a project path, a length (seconds) and a frame rate; **Create** writes an empty `.seq.ron`
  v5 there and opens it. A sequence ends at its last key, section or marker, so the length is an `end` marker
  (a zero length writes none). An existing file is refused (`seq_new_exists`).
- **Add track**: pick the kind and type the name, binding, property and asset. A track needs one key or
  section, so it starts with one at the playhead: a key with value zero (one for `transform.scale`), or a
  one-second section of the asset. Kinds: `transform` (a Vec3 track on `transform.position`, or the
  `transform.rotation` / `transform.scale` path typed as property), `float`, `vec3`, `bool` (a property path),
  `clip` (a clip on the bound rig), `audio`, `spatial_audio` (emitting from the binding), `sub` (a sub-sequence
  path) and `video` (on the bound entity's video player; fullscreen with no binding). Fades are not a
  `.seq.ron` track: they live in the `.shots.ron` shot list, and `fade` is refused with
  `seq_track_fade_in_shots`.
- **Track** row (the panel's track): **Up** and **Down** reorder it (of two tracks that write the same property,
  the later wins), a name field renames it on Enter, **Remove track** removes it.
- **Add marker at playhead** adds `markerN`; agents rename, move and remove markers.
- **Binding**: type a name and press **Bind selected** to bind the selected entity (by its scene guid when it has
  one, so the binding survives a save). **Remove binding** removes the name from the player's table and, if
  the binding is a spawnable, from the file. While tracks or sections use the name the removal is refused with
  `seq_binding_used`; tick **cascade** to remove those tracks, the spatial sound and video sections bound to it,
  and sub-sequence table entries mapping to it. A spawnable binding is added on the [Binding kind](#section-strip)
  row or by agent call.
- **Key** writes the selected entity's position, rotation and scale at the playhead on its binding, creating
  `<binding>.position`, `<binding>.rotation` and `<binding>.scale` when missing; a key at the same time is
  overwritten. **Auto-key** does the same when a gizmo drag or an inspector edit on a bound entity ends, and the
  key joins that edit's undo entry, so one undo reverts both. An inspector edit of another component keys
  `reflect:<Component>.<field>`. Auto-key works with this panel closed too: the editor checks once per frame
  (ADR-8730), and each finished edit is keyed exactly once whichever path sees it first.
- Right-click a field name in the **Inspector** and pick **Add to sequence** to key that reflected field (a float,
  vec3, `[f32; 3]` color or bool) at the playhead as `reflect:<Component>.<field>`; an entity the sequence does
  not bind is bound under its name first.
- **Preview in level** applies the sequence at the playhead to the edit world (keyed tracks through the
  property appliers; clip sections pose their rig; sound sections play; a video section shows its picture) and
  follows scrubbing. A clip section blends its clips (from the `SequenceClips` library, as at runtime) over the
  rig's own pose at every scrub, so a blend in or out shows as it will play. A rig with an `Animator` is posed the
  same way, its animator suspended while the preview runs (the edit world never evaluates animators; the
  animator is never written, so after Stop rig and animator are bit for bit as before; ADR-8940).
- **Sound sections in the preview (ADR-8940).** Each flat or spatial sound section under the playhead plays one
  voice on the editor's audio output (the edit world's own audio when it has one, else the output the Content
  Browser previews sounds on; never a play-in-editor world). Entering a section starts the sound at the section
  offset; the timeline's play keeps it playing (however long a frame takes); a scrub, seek or loop wrap restarts it
  at the new offset; leaving the section, editing its sound, bus, rate or timing, and Stop stop it. Reverse play
  (J) is silent: a sound cannot play backward, so its first tick stops the voices and none start until the
  transport runs forward or the playhead moves. A voice whose sound has ended inside its section stops and is
  listed with `playing: false` and `past_end`. A spatial section plays from its bound emitter (or its fixed
  position), panned and attenuated by the runtime's spatialization against the nearest listener; when the editor's
  output has no listener (the edit world runs none), the editor camera is the listener (its eye, facing where it
  looks), so a section far from the world origin is heard as the viewport shows it.
- **Pitch-preserving, Doppler and occlusion in the preview (ADR-8591).** The preview hears a section the way the
  runtime and the render-out play it. A pitch-preserving section (a rate other than 1 within 0.5..2) plays the
  runtime's stretch of its range of the sound (`regia_audio::stretch_asset`, the same WSOLA), not varispeed: a
  range up to 0.25 s of sound renders when the playhead needs it, a longer one renders off the UI thread (from
  3 s ahead while playing, or when the playhead lands in it) and the section starts at the playhead's offset as
  soon as it is ready (listed meanwhile with `stretch_pending`); a rate outside 0.5..2 plays by varispeed and is
  listed with `stretch_rate`. A spatial section with Doppler shifts its pitch by the runtime's Doppler ratio:
  the emitter's velocity is its binding's position-track slope on the sequence clock (else its motion over the
  tick while playing), the listener's the editor camera's motion over the tick; a scrub hears no motion. A
  spatial section with occlusion asks the runtime provider's question, how many solid colliders stand between
  the listener and the emitter, against the edit world's colliders (the preview mirrors them into a physics world
  of its own, rebuilt when a collider or its pose changes), and plays quieter and muffled by the occlusion
  tuning, like the runtime voice.
- **Video sections in the preview (ADR-8940, ADR-7847).** A video section targeting a bound entity's video player
  shows the picture of the section's source time on that entity (decoded frame-exact with `regia_media`'s seek);
  the section's own audio is muted. The picture lives in the entity's playback session, never in its authored
  `VideoPlayer`, which the preview does not write: an undo entry, a duplicate, a prefab, a level save, an autosave,
  an asset-watch reload check and Play all see the authored player, and none of them closes and re-opens the
  previewed video. Leaving the section and Stop close the session.
  A **fullscreen** section and a **mesh** (`screen`) section are previewed too (ADR-8591): the preview runs
  `regia_media`'s frame publisher over sessions it keeps in a world of its own (never the edit world's entities,
  never a play-in-editor world), only while such a section is under the playhead. The fullscreen picture is drawn
  over the perspective viewport with the section's fit (letterbox, fill or stretch; the highest layer wins); the
  mesh section's picture goes into the bound mesh's albedo or emissive slot, and the mesh gets its own material
  back when the section ends and on Stop (the renderer component is never saved, so a save sees no video
  material). A **UI image** section is listed with `target_not_previewed` and a `message`: it shows in the running
  game's UI, which the edit world does not draw (preview it with Play).
- **Restore on stop (ADR-7726)**: the sequence's `restore_on_stop` (`.seq.ron` v5, ADR-2948). With it on, a
  player of the sequence that stops (or lets go at its end, or is despawned) puts back the value each keyed
  property had before the player first wrote it; with it off the last sampled values stay. Each change is one
  undo entry and **Save** writes it (off is the default and is left out of the file). It changes what a play
  session does when the sequence stops, not the edit-world preview, which always puts its values back.
- Unticking it, or closing
  the panel, puts back every value it changed, bit for bit (it snapshots each touched entity's transform and
  reflected components before the first write). A level save made during the preview saves the level as it
  was, without the previewed values.

### Agent methods

- `editor.seq_new { path, length?, fps?, name? }` (needs `edit_scene`) returns `{ player, path, length, fps }`
  (defaults: 5 s, 30 fps, the file stem). Codes: `seq_new_exists`, `seq_new_name_empty` (an empty or blank `name`,
  or file stem), `seq_new_bad_fps`, `seq_new_bad_length`,
  `seq_curve_bad_path`, `seq_curve_no_project`, `seq_curve_io`.
- `editor.seq_track_edit { op, player?, ... }` (needs `edit_scene`) returns `{ applied, index, player, tracks:
  [{ name, kind, target, property, len }], markers: [{ name, time }], spawnables, bindings, track }`:
  - `op: "add"` with `kind`, `name?` (default `<binding>.<kind>`), `binding`, `property`, `asset`, `time?`,
    `end?`, `value?` (a number, a bool or `[x, y, z]`), `at?` (insert index).
  - `op: "remove"`, `op: "rename"` (with `new_name`) and `op: "move"` (with `to`) act on `track` (a name;
    default the panel's track).
  - `kind: "marker"`: `add` with `name`, `time`, `payload?`, `at?`; `remove`, `rename` (`new_name`) and `move`
    (`to`) on the marker `name`.
  - Codes: `seq_track_kind_unknown`, `seq_track_fade_in_shots`, `seq_track_name_empty`, `seq_track_name_taken`,
    `seq_track_needs_binding`, `seq_track_needs_property`, `seq_track_needs_asset`, `seq_track_bad_index`,
    `seq_track_invalid`, `seq_marker_bad_index`, `seq_marker_name_empty`, `seq_marker_name_taken`,
    `seq_marker_invalid`, `seq_curve_no_player`, `seq_curve_bad_track`.
  - Track ops (`.seq.ron` v6, ADR-8105), each on `track` (default the panel's track): `op: "duplicate"` (a copy
    after it, named `<name>.2`, unlocked); `op: "lock"`, `op: "mute"` and `op: "solo"` with `on?` (default
    `true`; `false` turns it off); `op: "select_keys"` (selects every key or section mark of the track in the
    timeline; returns `selected`, the count); `op: "event_key"` with `action` `add` (`time`, `event`,
    `payload?`), `set` (`key`, the key index, and the new `time`, `event`, `payload?`) or `remove` (`key`) on an
    event track (returns `key`, the key's index). `kind: "event"` adds an event track whose first key fires
    `event` (default: the track name) with `payload?` at `time`; `binding` is optional (empty: a sequence-wide
    event). Every track in the result carries `locked`, `muted` and `solo`, and an event track its `events: [{
    time, event, payload }]`. Lock and mute are saved and are undo entries; solo and select are session state.
  - More codes: `seq_track_locked` (the edit would change a locked track's keys, sections, name, binding or
    position: unlock it first; every editor path refuses it, including `editor.seq_key`, `editor.seq_curve_edit`,
    `editor.seq_section_edit`, `editor.seq_keys` and the ripple of `editor.seq_cut_edit`), `seq_event_wrong_kind`
    (not an event track), `seq_event_bad_index` (no such key), `seq_event_invalid` (an empty event name, a
    negative or non-finite time, or the last key removed).
- `editor.seq_binding_edit { op, binding, player?, ... }` (needs `edit_scene`): `op: "add"` with `entity` (an
  entity id), `guid` (a scene guid) or `prefab` (a spawnable: `start?`, `end?`, `keep_on_stop?`); `op: "remove"`
  with `cascade?`. Returns the summary above plus `removed: { tracks, sounds, videos, sub_entries, spawnable }`
  for a removal. Codes: `seq_binding_empty`, `seq_binding_in_use`, `seq_binding_no_entity`,
  `seq_binding_used`, `seq_binding_unknown`, `seq_spawnable_invalid`.
- `editor.seq_key { binding?, entity?, channel?, time?, auto?, player? }` (needs `edit_scene`): `channel` is `transform`
  (default: position, rotation, scale), `position`, `rotation`, `scale` or a property path (a registered reader's
  path such as `light.intensity`, or `reflect:<Component>.<field>`); `binding` defaults to the selected entity's
  binding, `time` to the playhead. `entity` (a scene entity id) keys that entity's binding instead of the
  selection's, so an agent need not select it first (ADR-7747); it is refused together with `binding`.
  `auto: true | false` sets **Auto-key** (with no `binding` or `entity`, nothing is keyed).
  Returns `{ binding, keys: [{ track, property, time, value, created }], auto_key }`. Codes: `seq_key_unbound`,
  `seq_key_unreadable`, `seq_key_no_selection`, `seq_key_wrong_kind`, `seq_curve_out_of_range`,
  `seq_key_no_entity` (`entity` does not exist), `seq_key_binding_and_entity` (both given). Agents: an
  auto key lands at the next editor frame (panel open or closed, windowed or `--headless`) or at the next `editor.seq_*` call.
- `editor.seq_preview { on, time?, player? }` (needs `edit_scene`): `on: true` seeks to `time` if given and
  applies the sequence (with no `time` it reads the state), returning `{ on, time, touched: [entity], animators:
  [entity], sounds, videos, voices }`; `on: false` stops and returns `{ on: false, restored, voices }`
  (ADR-8940):
  - `animators`: the `Animator` rigs whose animator the preview suspended to pose them.
  - `sounds: [{ path, track, sound, position, spatial, emitter, volume_db, pan, pitch, doppler, velocity,
    occlusion, cutoff_hz, stretched, playing, error }]`: the sound sections under the playhead; `position` is the
    sound's offset in seconds, `emitter` the spatial source's world position, `volume_db`, `pan` and `pitch` what
    the voice plays with (`pitch` is the rate times `doppler`, or `doppler` alone for a `stretched` section),
    `doppler` the Doppler ratio (1 without Doppler), `velocity` the emitter velocity Doppler used (`null` with
    Doppler off), `occlusion` the occlusion amount `0..=1` and `cutoff_hz` the voice's low-pass cutoff (`null` for
    a section that does not occlude), `stretched` whether the voice plays the pitch-preserving stretch, `playing`
    whether a voice runs. `error` is `null`, `unknown_sound` (no sound of that name in the world's sounds or the
    project), `past_end` (the offset is past the sound's end), `stretch_pending` (the section's stretch is
    rendering; it starts when ready), `stretch_rate` (a pitch-preserving section's rate is outside 0.5..2: it plays
    by varispeed), a stretch error (`stretch_empty: ...`, `stretch_too_long: ...`) or the mixer's message.
  - `videos: [{ path, track, video, target, entity, source_time, frame, pts_ns, publishes, error, message }]`: the
    video sections under the playhead; `frame` and `pts_ns` are the index and presentation time (nanoseconds) of
    the picture the session holds for `source_time`; `publishes` (fullscreen and mesh sections) is the frame
    publisher's count of pictures published for the section so far. `error` is `null`, `unbound`,
    `target_not_previewed` (a UI image target; `message` says why), `no_mesh` (a mesh section's binding has no
    mesh renderer), `no_project`, `target_gone` or a `regia_media` error code (such as `source_not_found` or
    `decode_failed`).
  - `voices: { started, stopped }`: the preview voices started and stopped this editor session (counters).
  - `video_opens`: how many times the preview opened a video section's source this editor session (a counter;
    a save, autosave, asset-watch pass or Play does not add to it).
  - `media: { stretches: { rendered, inline, pending }, occlusion_mirror_builds }`: the preview's stretch cache
    counters (stretches rendered, of those rendered on the apply, and rendering now) and how many times it built
    its occlusion mirror of the edit world's colliders (counters for tests and soaks).
  - `path` in both lists is the section's index path in the sequence (sub-sequence entries first).

- `editor.seq_restore_on_stop { value, player? }` (needs `edit_scene`): sets **Restore on stop** to `value` as one
  undo entry (nothing when it already is) and returns `{ player, restore_on_stop, changed }`. Codes:
  `seq_curve_no_player`.

```json
{ "method": "editor.seq_restore_on_stop", "params": { "value": true } }
{ "method": "editor.seq_new", "params": { "path": "cine/cube.seq.ron", "length": 3.0 } }
{ "method": "editor.seq_binding_edit", "params": { "op": "add", "binding": "cube", "entity": 4294967301 } }
{ "method": "editor.seq_key", "params": { "binding": "cube", "channel": "position" } }
{ "method": "editor.seq_preview", "params": { "on": true, "time": 2.0 } }
{ "method": "editor.seq_key", "params": { "binding": "cube", "channel": "position" } }
{ "method": "editor.seq_track_edit", "params": { "op": "add", "kind": "marker", "name": "door", "time": 1.5 } }
{ "method": "editor.seq_key", "params": { "entity": 4294967301, "channel": "position", "time": 1.0 } }
{ "method": "editor.seq_binding_edit", "params": { "op": "remove", "binding": "cube", "cascade": true } }
```

From Rust: `regia_framework::sequencer::edit::tracks::{starter_track, apply_track_edit, apply_marker_edit,
key_property, set_key, remove_binding, new_sequence}` and `regia_editor::seq_tracks`.

Not done: a Content Browser entry for new sequences (no create-kind registry in this base); UI image video
targets in the preview (they show with Play); the rows and the preview's fullscreen overlay were driven by tests
through their functions, not with egui input, and nobody has judged them on a screen yet.

## Track list, lock, mute, solo and event keys (ADR-8105)

The authoring rows list every track of the sequence as a focusable row named `<name> (<kind>)` plus `,
locked`, `, muted` and `, solo` when they apply. Click a row (or press Enter or Space on it) to make it the
panel's track. Right-click a row, or press **Shift+F10** while it has the keyboard focus, for its menu (Tab walks
the items, Enter runs one):

- **Rename** (the name field at the top: type and press Enter), **Duplicate**, **Delete**, **Move up** and **Move
  down**: the `editor.seq_track_edit` ops, one undo entry each.
- **Lock** / **Unlock**: `locked` in the file (`.seq.ron` v6). A locked track refuses every edit of its keys,
  sections, name, binding or position, from every editor path and agent method, with `seq_track_locked`; Delete,
  Rename and the moves are disabled in its menu. Mute and Duplicate still work on it.
- **Mute** / **Unmute**: the track's saved `enabled` switch (`false` mutes): a muted track is not evaluated in the
  preview or at runtime, and a muted event track fires nothing.
- **Solo** / **Unsolo**: editor session state, never saved (ADR-8105: a saved solo would silence the other tracks
  in a shipped game). While any track of the player is soloed, the edit-world preview evaluates only the soloed
  tracks.
- **Select all keys**: selects every key (or section start and end) of the track in the timeline below.

When the panel's track is an **event track**, its keys are listed under the rows: each key's time, event name and
payload (a change commits when the drag or the edit ends, one undo entry), **Remove key**, and **Add event key at
playhead** (named after the track). When playback crosses an event key the player sends `SequenceEventFired`
([sequence player](sequence-player.md)).

```json
{ "method": "editor.seq_track_edit", "params": { "op": "add", "kind": "event", "name": "cues", "event": "door_open", "payload": "fast", "time": 2.0 } }
{ "method": "editor.seq_track_edit", "params": { "op": "event_key", "track": "cues", "action": "add", "time": 4.0, "event": "scene_end" } }
{ "method": "editor.seq_track_edit", "params": { "op": "lock", "track": "door-angle" } }
{ "method": "editor.seq_track_edit", "params": { "op": "solo", "track": "door-angle", "on": true } }
{ "method": "editor.seq_track_edit", "params": { "op": "select_keys", "track": "door-angle" } }
```

Event keys are timeline keys too (ADR-8628): an event track is a dope-sheet row (`editor.seq_timeline_state`
lists it with `kind: "Event"` and its key times), **Select all keys** and a box select pick its keys, and
`editor.seq_keys` moves, scales, deletes, copies and pastes them with the other keys. Event keys may share a time
(they fire in file order): a move or a scale onto another event's time is not refused (`seq_keys_time_taken` is
for the float, vec3 and bool tracks), and a pasted event key goes after the keys already at its time instead of
replacing one. A rippling `editor.seq_cut_edit` shifts the event keys after the edge with the other keys, and a
shortening ripple whose removed span holds an event key is refused `seq_ripple_keys_in_span`.

```json
{ "method": "editor.seq_keys", "params": { "op": "select", "range": [0.9, 1.1], "tracks": [3] } }
{ "method": "editor.seq_keys", "params": { "op": "copy" } }
{ "method": "editor.seq_keys", "params": { "op": "paste", "time": 4.5 } }
```

Not done: the track rows' menu is not on the timeline's name column; the event tracks of a sub-sequence do not
fire.

## Sequencer timeline (ADR-7322)

The **Timeline** section of the Sequence Curves tab shows the panel's player on one time ruler: a marker row, a
cut row with the shot list's fade curve, and a dope sheet of every keyed track. Every edit is validated on a copy
(a refused edit changes nothing and shows its message under the timeline), and each gesture (a whole drag, a
button, an agent call) is one undo entry.

- **Dope sheet**: each float, vec3 and bool track is a row of keys, and each clip, audio,
  sub-sequence and video track is a row of its sections' start and end marks (squares; blue
  unselected, yellow selected). Click a vec3 track's name to open its x, y and z rows (they show
  the same keys). Click a key or a section mark to select it, Shift-click to add or remove it,
  drag across empty space to box-select marks on several rows. Drag a selected mark to move every
  selected mark (whole frames unless **Snap** is off): a key moves in time, a section end trims
  (the content stays on the clock), and a section with both marks selected moves whole; a moved
  section may not newly overlap another of its track (refused `seq_section_overlap`, nothing changes; an
  overlap the track already had, such as a cross-fade, may grow or shrink).
  Alt-drag scales the selected marks about the playhead (a section with both marks selected
  stretches, a single section end trims). **Delete keys** (or Delete), **Copy** and **Paste** (or
  the copy and paste shortcuts) at the playhead: the clipboard belongs to the panel and holds keys
  and whole sections (a section with either mark selected), so pick another player and paste
  there. A pasted track goes to the track of the same name and kind, else the one with the same binding,
  property and kind; a pasted key replaces a key at the same time. **Open in curve editor** shows
  the first selected key's track in the curve editor below.
- **Markers**: **Add marker** at the playhead (`markerN`); click a marker to select it and edit its name and
  payload in the fields; drag it to move it; **Delete marker**. **Import CSV** and **Export CSV** read and write
  the project-relative `.csv` in the path field (the marker sheet: `time_seconds,name,payload`).
- **Cuts**: the shots as bars. **Add cut** cuts at the playhead to the selected camera (else the active one): the
  shot playing there ends at the playhead and a still shot of the camera plays on. Drag a cut to move it, drag
  the end of the last shot to trim it, click a shot to edit its fades (the white line is the fade's opacity) or
  **Delete cut** (the one at its start). With **Ripple** on, a cut move or trim shifts every later cut and the
  sequence's keys, markers and sections (those that start after the edge's old place) by the edit's delta, as one
  undo entry; a rippling cut move
  keeps the second shot's length, so it may go past that shot's end. Shortening removes the time between the new
  and the old edge: a key or marker there refuses the whole edit (`seq_ripple_keys_in_span`; move or delete it, or
  turn Ripple off), so nothing changes order. A drag is one ripple from where it was pressed, whatever the mouse
  speed. An add splits a shot in place, so it never ripples. The cut row edits the player's own shot list (what its camera takeover plays), else the shot list
  loaded in the `ShotPlayer` (`render.sequence`, the Shots panel); with neither, **Add cut** makes one on the
  player. A shortened Catmull-Rom camera path keeps its keys past the new end as control keys, so the curve before
  the cut is exactly the uncut shot's and a later move or trim that gives the time back restores it.
- **Transport**: |< (Home), << (J, reverse), Play/Pause (Space; K pauses, L plays), Stop, >| (End), **Loop**,
  **In** and **Out** (the play range, panel state only: never saved), **Timecode** / **Frames**, **Follow**
  (keep the playhead in view while playing). `,` and `.` step a frame. Keys work while the pointer is over the
  timeline and no text field has focus. Ctrl+wheel (or a pinch) zooms, the wheel over the ruler zooms, a
  horizontal wheel or a middle-button drag pans, and a drag on the ruler scrubs.

### Agent methods

- `editor.seq_timeline_state { player? }` (read) returns `{ player, time, duration, fps, timecode, frame,
  transport: { playing, rate, loop, range, display, follow }, snap, ripple, rows: [{ track, name, kind, keys,
  sections: [{ index, start, end, label }] }], selection: [{ track, name, index, time } | { track, name, index,
  end, time }], clipboard, markers: [{ name, time, payload }], cuts: { host, cuts, shots: [{ name, start,
  duration, fade_in, fade_out }] } | null }`.
- `editor.seq_keys { op, keys?, player?, ... }` (needs `edit_scene`): `keys` is `[{ track, index }]` (a track index
  or name) or a section ref `[{ track, index, end: "start" | "end" }]`; the default selection holds both kinds.
  `op: "select"` with `mode?` (`replace`, `add`, `toggle`) or `range: [from, to]` and `tracks?` (a box over those
  track indices, default all, keys and section marks together); `"move"` with `delta` (seconds) and `snap?` (keys
  and section ends move together: a section moves whole when both its marks are given, else the given end trims;
  a move that creates an overlap is refused `seq_section_overlap`); `"scale"` with `factor` and `pivot?` (default the
  playhead; keys and section ends together: a section with both marks given stretches about the pivot, its content
  kept on the clock from its new start, and one with a single mark trims that end); `"delete"` (keys only);
  `"copy"` (keys, and every section with either mark given, whole; `clipboard` counts both); `"paste"` with
  `time?` (default the playhead; pass `player` to paste into another sequence; the earliest copied key or section
  start lands at `time`, each section is appended whole to the track of the same name and kind, and a pasted
  section over one the track already has is refused `seq_section_overlap`; the pasted keys and both ends of each
  pasted section become the selection);
  `"open_curve"`. Returns `{ op, selection, clipboard, curve_track }`.
- `editor.seq_marker_edit { op, marker?, name?, time?, payload?, csv?, path?, player? }` (needs `edit_scene`):
  `add` (default name `markerN`, default time the playhead), `move`, `rename`, `payload`, `delete` (`marker`: an
  index or a name), `import_csv` (`csv` text or a project `path`), `export_csv` (to `path`, or the text back).
  Returns `{ op, marker, markers, csv }`.
- `editor.seq_cut_edit { op, time?, camera?, cut?, shot?, duration?, fade_in?, fade_out?, ripple?, player? }`
  (needs `edit_scene`): `add` (at `time`, default the playhead, from `camera`, default the selected or active
  camera), `move` (`cut` to `time`), `trim` (`shot` to `duration`), `delete` (`cut`), `fade` (`shot`; an omitted
  fade is kept). `ripple: true` (add, move, trim; default false) shifts every later cut and the sequence's keys
  and markers after the edge's old place (the old cut time, the shot's old end) by the edit's delta, and every
  clip, sound, sub-sequence and video section that starts after that place (whole), validated on one copy as one
  undo entry ("Ripple sequence cuts"; an edit without ripple is "Edit sequence cuts" and leaves the sequence
  alone). A section that starts before the edge keeps its span. A shortening ripple whose removed span (new edge,
  old edge] holds a key, marker or section start is refused `seq_ripple_keys_in_span`, and one that would move a
  section onto another section of its track is refused `seq_section_overlap`; a rippling `move` is bounded only by the first shot's start. An add splits a
  shot in place, so its ripple delta is zero. Returns `{ op, shot, cuts }`.
- `editor.seq_transport { op, time?, frames?, on?, in?, out?, mode?, dt?, player? }` (needs `edit_scene`): `play`,
  `reverse`, `pause`, `toggle`, `stop`, `seek` (`time`), `step` (`frames`, default 1), `home`, `end`, `loop`
  (`on`), `range` (`in` and `out`, or neither to clear), `display` (`mode`: `timecode` or `frames`), `snap` and
  `follow` (`on`: the **Snap** and **Follow** toggles), `tick` (`dt`:
  advance a playing transport as one drawn frame does). Returns `{ op, time, timecode, frame, transport }`. The
  transport is never an undo entry.

Codes: `seq_keys_empty`, `seq_keys_bad_track`, `seq_keys_bad_key`, `seq_keys_not_keyed`, `seq_keys_out_of_range`,
`seq_keys_bad_scale`, `seq_keys_time_taken`, `seq_keys_last_key`, `seq_keys_no_track`, `seq_keys_invalid`,
`seq_section_bad_index`, `seq_section_out_of_range`, `seq_section_overlap`, `seq_marker_bad_index`,
`seq_marker_name_empty`, `seq_marker_name_taken`, `seq_marker_invalid`, `seq_marker_csv`,
`seq_marker_bad_path`, `seq_marker_io`, `seq_cut_bad_index`, `seq_cut_out_of_range`, `seq_cut_time_taken`,
`seq_cut_invalid`, `seq_cut_no_camera`, `seq_cut_no_shots`, `seq_ripple_keys_in_span`, `seq_transport_bad_range`, `seq_transport_bad_time`,
`seq_curve_no_player`, `seq_curve_no_project`.

```json
{ "method": "editor.seq_keys", "params": { "op": "select", "range": [0.5, 2.5], "tracks": [0, 1, 2] } }
{ "method": "editor.seq_keys", "params": { "op": "move", "delta": 0.5 } }
{ "method": "editor.seq_transport", "params": { "op": "seek", "time": 1.0 } }
{ "method": "editor.seq_keys", "params": { "op": "scale", "factor": 2.0 } }
{ "method": "editor.seq_marker_edit", "params": { "op": "add", "name": "door", "payload": "creak" } }
{ "method": "editor.seq_cut_edit", "params": { "op": "add", "time": 1.0 } }
{ "method": "editor.seq_cut_edit", "params": { "op": "trim", "shot": 1, "duration": 1.5, "ripple": true } }
{ "method": "editor.seq_keys", "params": { "op": "move", "delta": 0.5, "keys": [{ "track": "anim", "index": 0, "end": "start" }] } }
{ "method": "editor.seq_transport", "params": { "op": "play" } }
```

From Rust: `regia_framework::sequencer::edit::timeline::{apply_key_batch, apply_mark_move, copy_keys, set_marker,
import_marker_csv, apply_cut_edit, new_shot_list, cut_times, fade_curve, ripple_shift, ripple_span,
snap_to_frame}` and `regia_editor::seq_timeline`.

Not done: nobody has judged the timeline on a screen yet (the tests drive it with headless egui input, including
the keyboard shortcuts and the Alt-drag scale); a Smooth-eased scalar camera track can still bend locally past a
trim (its end key is re-eased Linear).
