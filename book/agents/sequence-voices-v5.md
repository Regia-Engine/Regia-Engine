# Sequence voices: Doppler, occlusion and pitch-preserving sections (agents)

`.seq.ron` v5 (ADR-3197) gives a sequence's sound sections three things the game's own emitters
already had or that cinematics need: a Doppler shift on spatial sections, occlusion through the
audio occlusion query, and a time scale that keeps the pitch. Everything here is deterministic on
the sequence clock, so a seek and a fixed-step render-out sound the same as playing through.

## The fields

```ron
(sound: "car_pass", start: 0.0, end: 6.0,
    spatial: Some((source: Binding("car"), max_distance: 200.0, doppler: 1.0, occlusion: true))),
(sound: "line_04", start: 2.0, end: 5.0, time_scale: 0.75, time_scale_mode: PreservePitch),
```

| Field | Default | Meaning |
|---|---|---|
| `spatial.doppler` | 1 in a v5 file; 0 after migrating a v4 file | Doppler factor: 0 off, 1 physical, at most 4. The pitch ratio is clamped to 0.5..2. |
| `spatial.occlusion` | false | Ask the occlusion query; play quieter and low-passed when occluded. |
| `time_scale_mode` | `Varispeed` | `PreservePitch` keeps the pitch; the section's `time_scale` must be in 0.5..=2. |

The format is described in [`docs/formats/seq-ron.md`](../../docs/formats/seq-ron.md) (Version 5).

## How the player hears them

**Doppler.** The emitter's velocity comes from the section's motion on the sequence clock. For a
binding with a `transform.position` track, it is the slope of that track: the sequence is sampled
5 ms on either side of the clock (one-sided at the ends) and the difference goes through the bound
entity's parent transform. For a binding that something else moves, it is the entity's position
change over the frame step (zero on a voice's first frame). A fixed position is still. The listener's
velocity is its own position change over the step. On the frame of a clock jump, and when the
listener moves faster than 120 m/s in one step (a camera cut), the listener counts as still. The
voice's pitch is its rate times the Doppler ratio, and glides over the step. A seek lands on the ratio
that playing through gives at that clock, because the track slope does not see the jump. A
Doppler-shifted voice runs slightly ahead of or behind the section's clock mapping (that is the
effect); the next seek or loop puts it back in sync.

**Occlusion.** Each frame, an occluding section queues a ray from its nearest listener to its source
on `AudioOcclusion`, under the owner `sequencer` (`AudioOcclusion::set_extra_rays_for`). The ray is
keyed by the bound entity, or by a proxy entity the player spawns for a fixed position and despawns
when the section ends. The physics provider answers it as it answers an emitter's ray. The voice plays
on a low-pass voice (`Audio::start_asset_lowpass`). Its gain is `OcclusionTuning::gain` and its cutoff
`OcclusionTuning::cutoff_hz` of the occlusion amount, smoothed with `OcclusionTuning::smooth` over the
frame step (the render clock in a render-out). A voice that starts or restarts begins at the reported
amount. A section without `occlusion` plays on the plain voice path, so a v4 file renders exactly as
before.

**Pitch-preserving sections.** The section's range of the sound (`offset` for
`(end - start) × time_scale` sound seconds) is stretched once by WSOLA (`regia_audio::stretch_asset`)
at the voice rate, which is the time scale times the speeds of the sub-sequence sections above it.
The stretch is kept in `SequenceStretches` and played as a plain voice at pitch 1 (times Doppler) from
`(position - offset) / rate`. A seek or a loop therefore gives exactly the samples playing through
gave. Pan, volume, Doppler and the occlusion low-pass apply as for any voice.

- A range of up to 0.25 s of sound renders on the frame that needs it (one WSOLA pass costs about
  70 ms of a core per output second on a loaded host). A longer range renders on the
  async compute pool, starting 3 s before the clock reaches it, and its voice starts at the clock's
  offset when the stretch is ready.
- A running render-out renders every stretch on the frame, so its mix never depends on worker timing.
- The cache holds at most 256 MiB, least recently used out first. One range holds at most 10 minutes
  at 48 kHz.
- A voice rate of exactly 1 plays the sound itself, bit for bit the varispeed voice.
- A voice rate outside 0.5..=2 (a sub-sequence speed on top of the section's time scale) plays by
  varispeed and reports an `apply_failed` issue whose reason starts with `stretch_rate`.
- A stretch that fails reports `apply_failed` once, with its `stretch_*` code (`stretch_rate`,
  `stretch_empty`, `stretch_too_long`).

Not applied to sequence voices: binaural (HRTF) rendering and sound propagation (paths, portals,
reverb sends). They need the per-entity emitter voice path (ADR-2405) and are deferred.

## Agent surface

`sequencer.spatial_voices { player?, all? }` reports what the player hears:

- per spatial section: `doppler_factor`, `occlusion` and `occlusion_key` (the entity the occlusion
  query is asked about, once a voice runs);
- per section: `time_scale_mode` (`varispeed` or `preserve_pitch`) and `rate`. With `all: true`, flat
  sections are listed too, with `spatial: false`;
- per voice: `pitch` (the ratio set), `doppler` (the ratio), `velocity` (the emitter velocity Doppler
  used, m/s, or null with Doppler off), `occlusion` (the smoothed amount), `cutoff_hz` (null without
  occlusion) and `stretched`;
- `stretch_cache`: `{ entries, bytes, pending, rendered, inline, evicted, failed }`.

```json
{ "method": "sequencer.spatial_voices", "params": { "all": true } }
```

Errors: `bad_entity`, `bad_setting` (`all` is not a boolean), `no_player`, `several_players`,
`unknown_player`.

`editor.seq_section_edit` edits the fields as one undo entry each:

- `{ "op": "time_scale_mode", "index": 0, "mode": "preserve_pitch" }` (or `"varispeed"`);
- `{ "op": "spatial", "index": 0, "spatial": { "source": { "Binding": "car" }, "doppler": 0.5, "occlusion": true } }`.

A pitch-preserving section's `time_scale` outside 0.5..=2, and a `doppler` outside 0..=4, are refused
with `seq_section_out_of_range`. The section strip shows a **Doppler** number and an **Occlusion**
switch for a 3D sound, and a **Keep pitch** switch for any sound section.

Guide topic: `sequencer_v5` (`agent.guide`).

## Checking it

`crates/regia_framework/tests/seq_voice_doppler_stretch.rs` renders mixdowns with a fixed 60 Hz step
and checks the following, offline:

- a passing car's tone by frequency measurement (within 0.5 % of `440 × c / (c − v)`; no shift with
  `doppler: 0.0`);
- an occluded voice's level (the tuning's dB within 0.05 dB) and filter (a 6 kHz tone loses more
  than 30 dB);
- a stretched 440 Hz tone at 0.5, 0.75, 1.5 and 2 (pitch within 0.5 %, duration within 25 ms);
- no clicks on a speech-like line;
- rate 1 bit for bit;
- a seek against play-through;
- a render-out hash on repeat, and a v4 file against the v4 build's hash.
