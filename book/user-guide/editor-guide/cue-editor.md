# Cue editor

The **Cue Editor** tab edits a sound cue, a `.cue.ron` file that names a set of sound variations and how the game
picks and plays one: weights, pick mode, what happens at the instance limit, the volume and pitch ranges, a low-pass
filter and a caption for subtitles. Its **Audition** button draws a variation the way the cue would and plays the
file. Depth: [Audio mixer and cue editor (agents)](../../agents/audio-mixer.md), [Audio](../audio.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "cue_editor"}`. It has no palette command
and no key.

## Walk-through: tune a footstep cue

1. Type a project path such as `audio/footstep.cue.ron` in the box and click **Open**. The cue's name and path
   appear on a line with **Save** and **Audition**; **(edited)** is added to the path while there are unsaved changes.
2. Under **Variations**, each row shows a sound path, a weight (drag the number) and **Remove**. To add a sound,
   type its path (for example `audio/step_03.wav`) and click **Add variation**.
3. Choose the **mode** (random, shuffle or sequential) and the **steal** policy (oldest, quietest or none) from
   the two drop-downs.
4. Set **volume dB** and its **range**, the **pitch** range, and **max instances** (0 means unlimited).
5. Tick **low-pass** to filter the cue: the cutoff in Hz and an octave offset appear. Tick **caption** to attach a
   subtitle: type the **caption key**, pick its kind (sound, speech or narration), set the priority and the planned
   length in milliseconds, and click **Set caption**.
6. Click **Audition**. The line under the controls reports the variation drawn, its volume in dB and its pitch;
   it adds **(not played)** when the file could not be played.
7. Click **Save**. **Save** is enabled only while the cue is edited.

Every change is one undo step (Ctrl+Z), and scrubbing one value is a single step. A cue you did not edit is never
rewritten: its bytes stay as they were. An edited cue is written with the engine's own serializer, so a
hand-formatted file is reformatted on its first saved edit.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Open a cue | Path box, **Open** | `editor.cue_edit` (`path`) |
| Read the open cue | The panel | `editor.cue_edit` (`op: "get"`) |
| Add or remove a variation, change a weight | **Add variation**, **Remove**, the weight drag value | `editor.cue_edit` (`add_sound`, `remove_sound`, `set_weight`) |
| Mode, steal, volume, pitch, instances, filter, caption | The drop-downs, drag values, **low-pass**, **caption** | `editor.cue_edit` (`set`) |
| Bus, cooldown, no repeat of the last variation | none (not in the panel) | `editor.cue_edit` (`set`) |
| Save | **Save** | `editor.cue_edit` (`save`) |
| Audition | **Audition** | `editor.cue_edit` (`audition`) |
| Undo | Ctrl+Z | `editor.undo` |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between its controls, and Space or Enter presses the
focused button ([Editor guide](index.md)).

## Known limits

- A cue holds at most 256 variations.
- **Audition** plays the drawn file at its own level and pitch: the drawn volume and pitch are reported, not applied
  to the preview.
- The cue's bus, cooldown and no-repeat setting are read and written through the agent method only.
- A cue file newer than this engine cannot be opened.

## See also

[Audio mixer](audio-mixer.md), [Audio](../audio.md), [Audio mixer and cue editor (agents)](../../agents/audio-mixer.md).
