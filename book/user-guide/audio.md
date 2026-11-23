# Audio

`regia_audio` is a runtime audio layer over kira, with buses, snapshots, event cues, parametric
patches, spatial audio and voice capture.

## Buses and snapshots

Audio routes through named buses (`Master`, `Music`, `Sfx`, …; names are case-sensitive), each with a volume. A **snapshot**
captures a whole set of bus volumes and crossfades between them (menu → gameplay, for example):

```rust
use regia::prelude::*;
use regia_audio::prelude::*;

fn duck_under_dialogue(mut audio: ResMut<Audio>) {
    let _ = audio.push_snapshot("dialogue", std::time::Duration::from_secs_f32(0.3));
}
```

A settings menu drives the buses with `Audio::apply_user_settings` and an `AudioUserSettings`
(per-bus dB volumes and a muted list). The `audio.master` / `audio.music` / `audio.sfx` fields of
`GameSettings` are saved with the settings file, and `SettingsPlugin` pushes them onto the `Master`,
`Music` and `Sfx` buses when they change (a bus your layout lacks is skipped). Use
`apply_user_settings` for a menu that keeps its own per-bus dB values. `audio.voice`, `audio.ambience` and
the mutes `mute_master`, `mute_music`, `mute_sfx`, `mute_voice` and `mute_ambience` are saved the same way
and pushed onto `Voice` and `Ambience` (ADR-3464). `World` and `Ui` stay runtime-only. Older settings files
load those volumes at full and the mutes off.

## Cues and patches

An **audio cue** is a named event the game fires (`play_cue("explosion")`); a **patch** is a
parametric sound (looped or crossfaded layers with volume, pitch and low-pass cutoff curves driven by parameters) for procedural audio. Both are authored as RON
(`docs/formats/audio-cue-v1.md`, `docs/formats/audio-patch-v2.md`) and imported.

Patch format v2 (ADR-7226) adds **one-shot layers** (sounds played in turn when a parameter crosses
a threshold or a step, or when `Audio::patch_trigger(id, "horn")` fires them), **granular layers**
(overlapping grains from a source sound at a parameter-driven position and density) and parameter
**bindings**: `binding: Some((component: "Rover", field: "speed"))` reads a reflected component
field on the entity that carries `AudioPatchSource { patch: id }`, every frame. v1 files still load;
`regia upgrade-project` stamps them as v2. Agents read patches with `audio.patch_state`
([Audio patches (agents)](../agents/audio-patch.md)).

```rust
use regia::prelude::*;
use regia_audio::prelude::*;

// Your game's own message: raise it when something explodes, and play the cue in response.
#[derive(Message, Debug, Clone, PartialEq)]
struct ExplosionEvent;

fn on_explosion(mut audio: ResMut<Audio>, mut reader: MessageReader<ExplosionEvent>) {
    for _ in reader.read() {
        let _ = audio.play_cue("explosion");
    }
}
```

(`ExplosionEvent` is your game's own message — registered with `add_message::<ExplosionEvent>()`;
`play_cue` fires a cue registered from `docs/formats/audio-cue-v1.md`.)

Two cue options for variation (`.cue.ron` v2, all optional, old files unchanged):

- `filter_cutoff_hz` / `filter_offset_octaves` put a low-pass on the cue; the offset moves the
  cutoff in octaves (`-1.0` halves it), so one set of sounds can be dulled per cue in data.
- `audio.play_cue_event("impact", event_id, tick)` seeds the pick and pitch/volume jitter from the
  gameplay event (`play_cue_seeded` takes a raw seed), so a replay of the same event sounds the same.
  Never derive the seed from the wall clock.

### Procedural loops (no recorded file)

A layer that needs a steady bed (engine rumble, wind, machinery hum) can be generated instead
of recorded. A `.loopgen.ron` (`docs/formats/loopgen-ron.md`) is a small graph of noise,
oscillators, filters, amplitude modulation and mixes with a seed and a length. It renders to
the same samples on every platform, loops without a click (a wrap-around crossfade), and is
cached in memory by file content.

```ron
(
    version: 1, name: "wind", length_secs: 3.0, seed: 5,
    nodes: [
        (id: "n", node: Noise(color: Pink, gain: 0.8)),
        (id: "lp", node: Filter(input: "n", mode: LowPass, cutoff_hz: 600.0)),
    ],
    output: "lp",
)
```

Name it as a patch layer's `sound` (any path ending in `.loopgen.ron`) and build the patch
with `AudioPatch::from_def_with_loopgen`; the patch's volume and pitch curves then drive it like
a recorded loop. Or register it by name: `SoundAssets::add_loopgen("wind", text)`. Check a
file without playing it with `audio.loopgen_render` ([agent guide](../agents/audio.md)).

## Spatial audio

Put an `AudioEmitter` on an entity and it plays — no code. Register the sound once, then spawn:

```rust,wrap
# use regia::prelude::*;
# use regia_core::math::{Vec3, Quat};
# use regia_scene::Transform;
# let mut commands: Commands = todo!();
# let world: &mut World = todo!();
# let app: &mut App = todo!();
# use regia_audio::{AudioEmitter, SoundAsset, SoundAssets};
let hum = app.world_mut().resource_mut::<SoundAssets>()
    .add("generator_hum", SoundAsset::from_file_static("sfx/generator.ogg")?);
commands.spawn((
    AudioEmitter { min_distance: 2.0, max_distance: 40.0, ..AudioEmitter::new(hum) },
    Transform::from_translation(Vec3::new(4.0, 0.0, -10.0)),
));
```

`autoplay` (default on) starts the sound when the emitter appears, `looping` (default on) keeps it
going, `volume_db` and `bus` (default `World`) set the level and routing. Changing the sound
restarts it; despawning the entity, or removing the component, stops it. Scenes, the editor and
agents name sounds with `sound_name` instead of a handle. `Audio::start_emitter` still starts extra
voices on an entity by hand.

Everything comes from the entity's **world** transform (`GlobalTransform`), so an engine sound
parented to a vehicle, or a listener parented to a camera rig, is heard where it is drawn:

- **Distance**: full volume inside `min_distance`, silence beyond `max_distance`, along the
  `rolloff` curve (inverse, linear, log, custom). `spatial_blend` mixes 2D (0) and 3D (1). The
  inverse and log curves still have some level at `max_distance`, so a sound cuts out there; set
  `fade_start` (say `Some(0.8)`, a fraction of `max_distance`) and they fade smoothly to silence.
- **Cones**: the emitter faces its −Z axis; inside `cone_inner_degrees` it is at full volume,
  beyond `cone_outer_degrees` at `cone_outer_gain`. Rotate the entity to aim it.
- **Doppler**: `doppler_factor` (0 = off, 1 = physical). Velocities come from the motion of both
  the emitter and the listener over the real frame time, so a car passing a moving player shifts
  correctly at any frame rate. Teleports do not produce pitch spikes.
- **Air absorption**: `air_absorption` low-passes farther sounds.

**Listeners.** The ears follow the active camera (the lowest `order` drawing to the window) with
no setup. To place them yourself, put `AudioListener` on an entity: any listener takes over from
the camera. With several (split-screen) each emitter is heard by the nearest listener; the first
(lowest entity id) is player 0, whose position drives reverb zones. Insert
`AudioListenerFollow::Off` to stop the follow: without a listener the ears then sit at the origin
facing −Z. `audio.listener_info` says where they are and why.

**Occlusion.** With `occlusion` on (the default), walls between the listener and the emitter
muffle (low-pass) and quieten it. `regia::DefaultPlugins` casts the rays through physics: a
per-frame budget (`AudioOcclusion::rays_per_frame`, 16) is spread round-robin over the emitters
in range, and each emitter glides toward its answer (`OcclusionTuning::smoothing_secs`), so the
budget never shows as steps. The emitter's own body and its parents (the car carrying the engine
sound) and the listener's body and its parents never occlude. Without physics, or in your own geometry query,
call `AudioOcclusion::report(entity, amount)`.

**Reverb zones.** `AudioReverbZone` (sphere, oriented box, or `Global`) makes the world
reverberate while player 0 is inside: the `World` and `Ambience` sends into the `Reverb` bus rise
to the zone's `send_db` and the reverb takes its `feedback` (room size), `damping` and
`stereo_width`. Over `fade_distance` outside the volume the send fades, so walking into a hangar
blends the reverb in. Overlapping zones blend by weight: the more specific one (higher `priority`, then the
smaller volume) crossfades over the others as the listener enters it. Without zones the
default mix is dry. `AudioReverbZone::small_room` and `::hall` are starting points.

**Tunnels and mines: audio zones.** A zone also has a `preset` (`tube`, `mine_gallery`, `open`,
`custom`), a `distance_scale`, a `blend_distance` (the `fade_distance` field) and a `cavity` flag, and
`shape` may be a capsule (axis along the entity's local Y: rotate it to run along a tunnel). Overlapping
zones blend by weight instead of one winning, so the open reverb fades into the tube's over the tube's
blend distance. When an emitter and the listener are both inside a zone with `distance_scale: 1.5`,
the emitter is audible 1.5 times further. Rock between them is counted from the ground surface: install a
`RockField` (the ground height at `(x, z)`, `None` inside a terrain hole) on `AudioRockField`, and mark
tunnel zones `cavity` so their inside is not counted as rock. Each metre of rock takes 2.5 dB and closes
the low-pass (tuning: `AudioOcclusion::rock`). A cavity zone with `AudioZonePortals` (its mouths, local
to the entity) is heard from the surface only through its mouths. See [Audio zones](../agents/audio-zones.md)
for the agent calls.

<!-- ignore-reason: uses the game's own terrain and hole types -->
```rust,ignore
let zone = AudioReverbZone::tube(ReverbZoneShape::Capsule { radius: 3.0, half_length: 100.0 });
world.spawn((zone, Transform::from_translation(centre).with_rotation(Quat::from_rotation_z(FRAC_PI_2))));
world.resource_mut::<AudioRockField>().set(Arc::new(FnRockField(move |x, z| {
    (!holes.contains(x, z)).then(|| terrain.map.sample_bilinear(x, z))
})));
```

**Headphones (HRTF).** `AudioListener { hrtf: true }` renders that listener's emitters
binaurally: interaural time and level differences and pinna cues, so sounds sit in front, behind,
above and below — not just left and right. Make it the player's "headphones" setting; it switches
live. The convolver is adapted from the MIT `hrtf` crate; the built-in head model is synthesized
(Brown–Duda spherical head), and `Audio::load_hrir_sphere` loads a measured `.hrir` sphere instead.
Positional one-shots (`Audio::play_at`) keep stereo panning.

The editor viewport draws a marker on every emitter; select one to see its `min_distance` and
`max_distance` spheres and its cone, select a reverb zone to see its volume. Agents use
`audio.list_emitters` and `audio.set_emitter` ([agent guide](../agents/audio.md)).
`docs/specs/16-audio.md` covers the model and the budgets.

**Bad numbers stay out of the mix.** An emitter whose transform turns NaN (a physics blow-up)
goes silent and virtual until it has a real place again; a NaN or infinite volume, pitch, pan or
cutoff from code or a tool is replaced before it reaches the mixer, and a loop region the mixer
cannot wrap (zero length, inverted, past the end) loops the whole sound. One bad emitter never
silences the bus it shares. `regia_audio::rejected_value_count()` (and `rejected_values` in
`audio.list_emitters`) counts the refusals, and the first one is logged.

**Reproducible offline renders of streams.** A streaming sound is decoded by a background
thread, and the mixer holds it silent whenever that thread falls behind, which the offline
renderer (headless runs, tests, tools) can cause on a loaded machine. For a render that must be
identical every run, play `asset.decode_to_memory()?` instead: the same sound, fully decoded
(about 23 MB per minute of 48 kHz audio). Every whole decode (`from_file_static`, `decode_to_memory`,
loudness measurement) stops with `AudioError::DecodeLimit` past 30 minutes or 1 GiB of decoded frames, whichever comes first
(`SoundDecodeLimits`; the `*_with_limits` variants take others): stream long audio. `Audio::stream_stats()` counts underruns (a stream that
stood still for 0.1 s while playing) on the device and offline, and logs the first ones.

## Music

A track (`.music.ron`, `docs/formats/music-ron.md`) is a set of stems that play in sync, with
a tempo and a meter, and optionally stingers and rules for moving to other tracks. Everything
musical runs on a clock inside the mixer, so it lands on the exact sample at any frame rate.

<!-- ignore-reason: `audio`, `calm` and `combat` are the game's own resource and tracks -->
```rust,ignore
// Register the tracks, then start one.
audio.register_music(calm);
audio.play_music(&combat, Duration::ZERO)?;

// Let the rule decide: "at the end of the segment, crossfade over two seconds". Asking again for
// the track that is playing or on its way changes nothing, so this can run every frame.
audio.music_transition("calm", MusicTransitionRequest::default())?;

// A one-shot on the next bar.
audio.music_stinger("victory", None)?;

// Or on the next beat, over the stinger's own setting.
audio.music_stinger("hit", Some(MusicSync::NextBeat))?;
```

A rule picks when the switch happens (`Immediate`, `NextBeat`, `NextBar`, `EndOfSegment`), how long
the equal-power crossfade is, and an optional segment to play between the tracks. A request has to
reach the mixer ahead of time, so a boundary less than 50 ms away goes to the next one.

Gameplay follows the music through the `MusicBeat` message: bar, beat within the bar and beat index,
all from 0. It arrives up to a frame after the beat is heard, and `clock_secs` says exactly when
it was.

<!-- ignore-reason: a system the game registers itself; the light handles are elided -->
```rust,ignore
fn pulse_lights(mut beats: MessageReader<MusicBeat>) {
    for beat in beats.read() {
        if beat.beat == 0 {
            // A downbeat.
        }
    }
}
```

Agents use `audio.music_state`, `audio.music_transition` and `audio.music_stinger`
([agent guide](../agents/audio-music.md)).

## Sound on other worlds: propagation media

Sound follows the planet. A level's atmosphere preset picks its **medium**: on Earth sound
travels at 343 m/s; on Mars at 240 m/s, about 20 dB quieter, and high frequencies die within tens
of meters; on an airless world like Europa nothing travels through the air at all. You can refine
the preset per level with `LevelSettings.audio_medium` (speed of sound, level, absorption, default
ground material).

What still reaches you on Europa travels **through the ground**. Give emitters that hit or shake the
ground an `AudioPropagation` — `AudioPropagation::for_category(EmitterCategory::Explosion)` and
friends cover impacts, explosions, collapses, footsteps, landings, machinery and hull landings —
and keep the listener's `ListenerCoupling` up to date from your physics contacts:

```rust,wrap
# use regia_audio::ListenerContact;
# let coupling: &mut regia_audio::ListenerCoupling = todo!();
# let grounded: bool = todo!();
# let seated_in = todo!();
// On the player entity that carries the AudioListener:
coupling.contact = if grounded { ListenerContact::Boots } else { ListenerContact::None };
coupling.vehicle = seated_in; // the cockpit of this vehicle is heard directly
```

A thump through the boots arrives after `distance / wave speed` (ice carries it at 3.8 km/s,
loose regolith at ~150 m/s), low-passed by distance and by the boots, with a ringing tail that is
long in dry, rigid ground. Jump and it fades out within 50 ms; land and it returns. Explosions on
Earth are felt through the ground — and through the gamepad's rumble — before the air wave
arrives. Put an `AudioInteriorVolume` on a pressurised hab: inside, sounds have air again; outside
impacts come through the walls, muffled.

Entity emitters (above) travel the same way: their airborne voice follows the medium (and is
what occlusion muffles), and with an `AudioPropagation` they also start a ground voice.

Radio, UI, music and the pilot's own body (`EmitterCategory::Body`: breathing, suit) bypass the
medium. A bus is marked with `bypass_medium: true` in the bus layout. To hear each world, render the
scripted scenarios: `cargo run --release -p regia_audio --example propagation_render -- out/`.
The model and its sources are in `docs/specs/16-audio.md` § 3c; agents query it with
`audio.medium` / `audio.propagation` ([agent guide](../agents/audio.md)).

## Voice

Voice capture (announcer lines, comms) uses `cpal` input streams; announcer helpers mix captured
audio over the game. Captured input is a runtime resource, never written to disk unless your game
does so. The capture stream and loopback are covered in [Voice capture and loopback](../agents/voice.md);
player-to-player voice chat (`regia_audio::voice_chat::VoiceChatPlugin`, Opus with the `voice-opus`
feature, network transport with `voice-net`) in [Voice chat](../agents/voice-chat.md).

## More sound systems

- **Ambience**: a weather-driven bed (`AmbienceBed`), point ambiences (`AmbientSpot`) and surface
  footsteps (`FootstepBank`): [Audio ambience](../agents/audio-ambience.md).
- **Contact sounds** from physics impacts, scrapes, rolls and breaks
  (`regia_framework::physics_audio::PhysicsAudioPlugin`): [Contact sounds from physics](../agents/physics-audio.md).
- **Vehicle audio** (`regia_framework::vehicle_audio::VehicleAudioPlugin`): [Vehicle audio](../agents/vehicle-audio.md).
- **Mono output** for players who hear with one ear (`Audio::set_mono_output`): [Mono audio](../agents/audio-mono.md).

## The output stage: limiter and meter

Every output runs a master limiter after all buses (ADR-544). With the default 0 dBFS ceiling a
mix that stays below full scale is untouched; a louder one is turned down instead of clipped. Set
a lower ceiling, or bypass it, from code:

```rust
use regia::prelude::*;
use regia_audio::{Audio, MasterLimiter};

fn quieter_ceiling(mut audio: ResMut<Audio>) {
    audio.set_master_limiter(Some(MasterLimiter { ceiling_db: -1.0, release_secs: 0.1 }));
    let meter = audio.output_meter();
    if meter.max_reduction_db > 6.0 {
        // The mix is too hot: lower a bus instead of leaning on the limiter.
    }
}
```

Stopping or pausing a sound that has been heard always fades over at least 10 ms
(`regia_audio::DECLICK_FADE`), so a `Duration::ZERO` stop does not click. Agents read and set the
same state with `audio.mix`, `audio.set_bus`, `audio.set_limiter` and `audio.render_check`
([Audio: mixer, limiter, render check](../agents/audio-mix.md)).

## Convolution reverb, zone impulse responses, time stretch and ambience files

A measured room, spring or cabinet can replace the preset reverb (ADR-2721):

<!-- ignore-reason: fragments of a game's audio setup; `audio`, `bytes` and `layout` are the game's own mixer, IR file and bus layout -->
```rust,ignore
use regia_audio::{AudioReverbZone, BusEffect, ImpulseResponse};
audio.register_impulse_response("hall", ImpulseResponse::from_encoded_bytes(bytes)?)?;
// On a bus (config/audio.ron does the same):
layout.send_buses[0].effects.push(BusEffect::Convolution { ir: "hall".into(), mix: 1.0, predelay_secs: 0.02 });
// Or per zone: while this is the most specific zone around the listener, the hall IR plays.
let zone = AudioReverbZone::hall(regia_core::math::Vec3::splat(20.0)).with_ir("hall");
```

IRs are capped per tier (desktop 4 s, mobile 1.5 s; the low tier keeps every zone on its preset).
Mark an IR's sound `impulse_response: Some(true)` in its `.meta` settings so the cook checks it and
never loudness-normalizes it.

`regia_audio::time_stretch(samples, channels, rate)` changes speed without changing pitch (0.5 to
2); `Audio::play_time_stretched` does it on a playing voice whose rate can change.

An `.ambience.ron` (`docs/formats/ambience-ron.md`) describes a whole ambience: looping beds that
follow wind and weather, and seeded random one-shots around the listener. Play it with
`Audio::play_ambience(AmbienceDef::from_ron(text)?)`; `AudioPlugin` steps it every frame.
