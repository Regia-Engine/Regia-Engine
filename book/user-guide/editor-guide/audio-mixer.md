# Audio mixer

Panel id: `audio_mixer`.

The **Audio Mixer** tab shows the audio bus tree with a fader, a mute and a solo button per bus, and live meters
(peak, RMS and short-term loudness per bus, plus the loudness of the whole output) while the game plays in the
editor. Use it to balance music, effects and voice against each other and to see which bus is loud, without
editing a file. Depth: [Audio mixer (agents)](../../agents/audio-mixer.md), [Audio](../audio.md),
[Loudness](../loudness.md).

## Opening it

The tab sits in the group under the viewport in the default layout; **View ▸ Reset layout** brings it back if you
closed it. An agent brings it forward with `editor.layout_apply {"open": "audio_mixer"}`. It has no palette command
and no key.

## Walk-through: balance a bus while playing

1. Press Alt+P to play. The line at the top reads **Meters: play**; before play it reads **Meters: none** with a
   reminder to press Play, or **Meters: edit** when the edit world has an audio engine.
2. Find the bus in the tree (children are indented under their parent). Drag its slider, which runs from -80 to
   +6 dB, to change its fader. The three bars under the bus (**peak**, **rms**, **LUFS**) show its level; they turn
   amber above -9 dB and red above -1 dB.
3. Click **M** to mute the bus. Click **S** to solo it: with any bus soloed, only the soloed buses, their
   ancestors and their descendants sound, and a bus silenced only by a solo is marked **(solo)**. A mute still wins.
4. The **Program** rows show the loudness of the whole output: **M** (momentary), **S** (short-term), **I**
   (integrated) and **TP** (true peak) as bars, the loudness range (LRA) and the measured seconds. **Reset** starts the
   measurement again.
5. Press Ctrl+Z to undo a change. Every fader move, mute and solo is one undo step, and consecutive moves of one
   fader are a single step.

The mixer starts from the project's bus layout in `config/audio.ron` (the default tree when there is none). It
changes the running session's mixer; **Save layout** writes the file.

## Walk-through: an effect chain, a send and a snapshot

1. Click **Chain** on a bus. Its effect chain opens under it. Pick a kind in the list (EQ, Low-pass, High-pass,
   Reverb, Delay, Compressor, Distortion, Convolution) and click **Add effect**: the effect is added at the end at a
   near-neutral setting.
2. **Bypass** skips an effect without losing its settings; **Move up** and **Move down** reorder the chain;
   **Remove** deletes the effect.
3. Under the chain, each send bus (such as **Reverb**) has a row: **Add send** routes the bus to it (silent at
   first), the slider sets the send level from -60 dB (silent) to +6 dB, and **Remove send** removes the route.
   A send is taken after the fader and mute unless **Pre** is on: a pre-fader send is taken before them, so the
   reverb keeps its feed when you pull the bus fader down or mute the bus.
4. Below the buses, type a name and click **Save snapshot**: every fader, mute and send level is stored under that
   name. **Apply** blends the mixer to a snapshot over its blend time (the seconds field on its row); **Release**
   blends back. **Delete** removes a snapshot.
5. Click **Save layout** to write everything to `config/audio.ron`; until then the panel shows **· unsaved
   changes**. The game builds that bus tree when it starts.

Changing a chain, adding or removing a send, toggling **Pre**, or bypassing an effect rebuilds the mixer's buses, which stops the
sounds playing at that moment; moving a send slider or applying a snapshot does not.

## Actions

| Action | Control / key | Agent method |
|---|---|---|
| Move a bus's fader | The bus's slider | `editor.audio_mixer_set` (`volume_db`) |
| Mute a bus | **M** | `editor.audio_mixer_set` (`mute`) |
| Solo a bus | **S** | `editor.audio_mixer_set` (`solo`) |
| Read the buses and meters | The panel | `editor.audio_mixer_get` |
| Restart the program loudness measurement | **Reset** | none (view only) |
| Open a bus's effect chain and sends | **Chain** | none (view only) |
| Add, remove, reorder or bypass an effect | **Add effect**, **Remove**, **Move up**, **Move down**, **Bypass** | `editor.audio_mixer_chain` |
| Set, add or remove a send | The send slider, **Add send**, **Remove send** | `editor.audio_mixer_send` |
| Take a send before or after the fader | **Pre** | `editor.audio_mixer_send` (`tap`) |
| Save, apply, release or delete a snapshot; set its blend time | **Save snapshot**, **Apply**, **Release**, **Delete**, the seconds field | `editor.audio_mixer_snapshot` |
| Write the layout to `config/audio.ron` | **Save layout** | `editor.audio_mixer_save` |
| Undo | Ctrl+Z | `editor.undo` |

## Keyboard

The panel handles no keys of its own. Tab and Shift+Tab move between sliders, lists, fields and buttons (the open
chain and the snapshot rows included); the arrow keys move a focused slider and Space or Enter presses a focused
button such as **M**, **S**, **Chain**, **Bypass**, **Pre**, **Add effect** or **Apply** ([Editor guide](index.md)).

## Known limits

- Meters read silence outside play unless the edit world has its own audio engine; the source is named in the top line.
- Effect settings other than the kind are not edited in the panel: a new effect starts near neutral; other
  settings are edited in `config/audio.ron`.
- The meter bars are drawn shapes; the agent read of the same numbers is `editor.audio_mixer_get`.

## See also

[Cue editor](cue-editor.md), [Audio mixer (agents)](../../agents/audio-mixer.md), [Audio
loudness (agents)](../../agents/audio-program-loudness.md).
