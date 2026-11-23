# Sequencer

The editor's sequencer is the **Sequence Curves** tab: pick a `SequencePlayer` and one of its
float or vec3 tracks, edit the curve's keys and tangents, scrub, and open or save the
`.seq.ron`. Its section strip shows the clip, audio, sub-sequence and video tracks as bars you
move and trim. The **Shots** tab edits the camera curves (field of view, roll, focus, aperture)
of the loaded shot sequence. Depth: [Sequencer](../sequencer.md), [Camera shot
sequences](../shot-sequences.md), [Sequence curves panel](../../agents/seq-curve-panel.md),
[Shot panel](../../agents/shot-panel.md), [Sequence player](../../agents/sequence-player.md),
[Take recorder](../../agents/sequence-take-recorder.md).

## Sequence Curves tab

| Action | Control / gesture / key | Agent method |
|---|---|---|
| Pick the player and track | **Player**, **Track** | `editor.seq_curve_state` |
| Open, save | **Open**, **Save** | `editor.seq_curve_open`, `editor.seq_curve_save` |
| Add a key | Double-click the curve | `editor.seq_curve_edit` |
| Move a key or a tangent | Drag it; hold Alt to break the tangent | `editor.seq_curve_edit` |
| Key menu | Right-click a key: ease and tangent modes, **Delete key**; on empty space **Zoom to fit** | `editor.seq_curve_edit` |
| Zoom, pan | Scroll (Shift: time only, Ctrl: value only); right- or middle-drag pans | none |
| Move or trim a section | Drag a bar; drag its edge | `editor.seq_section_edit` |
| Section menu | Right-click a bar ▸ **Duplicate after it**, **Delete section** | `editor.seq_section_edit` |
| Snap to frames | **Snap to frames** | none (an agent passes exact times to `editor.seq_section_edit`) |
| Scrub | Drag on empty strip space | none; `editor.seq_sections_state` reads the strip, `editor.seq_curve_state` the curve |
| Edit a section's numbers | The row under the strip (keyboard reachable) | `editor.seq_section_edit` |
| Record a take | **Record**, **Record (PIE)**, **Stop** | `sequence.record_start`, `sequence.record_stop` |
| Undo, redo | Ctrl+Z, Ctrl+Shift+Z / Ctrl+Y | `editor.undo`, `editor.redo` |

## Shots tab

| Action | Control / gesture | Agent method |
|---|---|---|
| Pick the shot and track | **Shot**, **Track** | `editor.shot_panel_state` |
| Edit the curve | Double-click adds a key, drag moves it, right-click opens the key menu | `editor.shot_panel_edit` |

## Keyboard and accessibility notes

A float or vec3 track shows a **Keys of <track>** list left of the curve, and a section track a
**Sections of <track>** list under the strip (ADR-7383). Rows are named `<track> key <i> at <time>
s` and `<track> section <i> at <time> s, <what it plays>`. With a row focused:

| Key | Does | Agent method |
|---|---|---|
| Up / Down | Focus the previous / next key or section | none |
| Enter or Space | Select it (the curve and the number row follow) | `editor.seq_curve_state`, `editor.seq_sections_state` |
| Left / Right | Move it one frame (1/fps) earlier / later, one undo step per press | `editor.seq_curve_edit` `move`, `editor.seq_section_edit` `move` |
| Delete | Remove it | `editor.seq_curve_edit` `remove`, `editor.seq_section_edit` `remove` |

The curve and the strip are announced as "Sequence curve, N keys" and "<kind> section strip, N
sections". Tangent handles, trims and scrubbing stay mouse-only (the number row and the agent
methods cover them). Findings: [Editor accessibility](accessibility.md).

More sequencer authoring (track add and remove, keyframes from the viewport) is on the branch
`lane/sprint-v06-seq-authoring`; this page gains it at the v0.6 integration.
