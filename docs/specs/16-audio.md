# Spec 16 — Audio (`regia_audio`, kira)

## 1. Model
- The `Audio` resource owns kira's `AudioManager` (cpal backend). If no output device exists
  (CI, headless), it falls back to the offline (null) backend and logs one warning (ADR-5614).
- **Buses** (kira tracks) defined in `config/audio.ron`: tree rooted at `Master`; defaults
  `Master → {Music, Sfx → {World, Ui}, Voice, Ambience}`. Each bus: volume (dB), mute, effects chain,
  sends (e.g. `World` → `Reverb` send bus). User settings expose chosen buses as sliders.
  The runtime builds the tree in `AudioSettings.buses` (default: the tree above); a host that ships a
  `config/audio.ron` layout parses it with `BusLayout::from_ron` and puts it there, and the editor mixer
  reads and writes the file (ADR-2530). `regia::DefaultPlugins` loads `<project>/config/audio.ron` (a
  `BusLayout`) into `AudioSettings.buses` at startup, overriding the default tree and a host-inserted
  `AudioSettings::buses`; a missing, unparseable or invalid file keeps the current tree with a warning
  (ADR-5699). The file is the bus tree — it is not the user settings (`AudioUserSettings`).
  The file is the frozen `audio-buses` format (v1 since ADR-8354: `version`, named `snapshots` with mutes, send
  levels and a blend time, the `Bypassed` effect; [`docs/formats/audio-buses-ron.md`](../formats/audio-buses-ron.md));
  `Audio::set_bus_layout` replaces a live layout (the editor mixer's chain, send and snapshot edits).
- Effects available per bus: EQ (3-band, kira filters), low/high-pass filter, reverb, delay, compressor,
  distortion (kira built-ins) — parameters reflected; only the low/high-pass cutoffs have live handles
  and are tweenable at runtime (other effect parameters are fixed when the bus tracks are built).
  **Convolution** (ADR-2721): `BusEffect::Convolution { ir, mix, predelay_secs }` convolves the bus with an impulse
  response registered by `Audio::register_impulse_response` (uniformly partitioned overlap-save, 256-frame partitions;
  the wet path is `max(predelay, 256 frames)` late, the dry path is not delayed; IRs resampled to the mixer rate at load;
  IR swaps crossfade equal-power; no allocation or lock on the audio thread). IR length caps per `ConvolutionTier`:
  desktop 4 s, mobile 1.5 s (Android default), low none. Refusals: `ir_empty`, `ir_truncated`, `ir_silent`,
  `ir_non_finite`, `ir_channels`, `ir_sample_rate`, `ir_too_long`, `ir_tier_disabled`. Pitch-preserving **time stretch**
  (WSOLA, rates 0.5 to 2, rate 1 a pass-through): `time_stretch` offline, `Audio::play_time_stretched` on a voice.
  Agent: `audio.bus_effects`, `audio.bus_effect_set`, `audio.convolution_stats`, `audio.time_stretch_probe`
  (`book/agents/audio-convolution.md`).
- **Snapshots** (Tier 1): named sets of bus volumes and low/high-pass cutoffs (`"Paused"`, `"Underwater"`) with
  blend time; `audio.push_snapshot("Underwater", Duration::from_millis(500))`.
- **Ducking**: rule list (`when Voice active, Music -6 dB, attack 0.2, release 1.0`) implemented by
  monitoring active sounds per bus.
- **Output stage** (ADR-544): a stereo-linked peak limiter on kira's main track, after every bus and send bus
  (`MasterLimiter`: ceiling 0 dBFS, release 80 ms by default; a mix below full scale passes bit for bit). It
  silences NaN/infinite samples and feeds an `OutputMeter` (peaks before and after, gain reduction). Mute is
  −100 dB (`MUTE_DB`), never −∞, so every volume tween stays finite; a layout bus with `mute` starts muted.
  Agent surface: `audio.mix`, `audio.set_bus`, `audio.set_limiter`, `audio.render_check` (`book/agents/audio-mix.md`).
  Loudness at import: `audio.loudness`, `audio.set_loudness_normalization` (`specs/95`, `book/agents/loudness-import.md`).

## 2. Sounds
- `Sound` asset: `Static` (decoded PCM in memory; for short SFX) or `Streaming` (decoded on the fly;
  music, ambience, long VO). Loop region, volume, pitch, pan and bus are set per play (`PlaybackParams`)
  and per cue (`SoundCue.volume_db`, `bus`); the asset itself may store defaults (spec § 2,
  `SoundAsset::with_default_*`): a play that does not set them uses the asset's — the default volume adds
  to the play's (like the loudness gain), the default pitch multiplies, the default loop region fills a
  play without one, and the default bus routes a play whose params name no bus (ADR-5699).
- `SoundCue` asset (Tier 1): randomised container — list of sounds with weights, random pitch/volume
  ranges, no-repeat-last-N, sequential/shuffle modes, max concurrent instances with steal policy
  (oldest/quietest/none), cooldown.
- Playback handle `SoundInstance` with `stop(fade)`, `pause/resume(fade)`, `set_volume/pitch/pan(tween)`,
  `seek`, `state()`. Game code holds a `SoundInstanceId` (from `play_cue`) and calls the same operations
  on `Audio` (`stop_voice`, `pause_voice`, `resume_voice`, `set_voice_volume_db`, `set_voice_pitch`,
  `set_voice_pan`, `seek_voice`, `voice_state`). A stop or pause of a sound that may have been heard, and every resume, fades over at least
  `DECLICK_FADE` (10 ms, ADR-544); a voice stopped before the mixer rendered it stops at once.
- Gameplay pause (ADR-10105): while `Time::paused` is set, `audio_sync` holds every voice on a bus the pause
  freezes (`Audio::set_game_paused`): it pauses in place with `DECLICK_FADE`, a voice started on such a bus during
  the pause is held at once, a held voice's age and virtual cursor do not advance, and unpausing resumes only what
  the pause held. `Ui` and `Music` (with their child buses) are exempt by default (`Audio::set_pause_exempt_buses`);
  voice chat is never held. While paused the mixer bookkeeping (ducking, snapshots, streams, voice chat, music)
  advances by `RealTime`, and propagation and patch layers by zero.

## 3. Spatial audio
- **Listener**: by default the ears follow the active camera (`regia::DefaultPlugins` copies the transform of the
  lowest-`order` window camera into `AudioActiveCamera` before the audio sync, ADR-2517). An `AudioListener` component on
  any entity takes over; `AudioListenerFollow::Off` opts out and leaves the origin facing −Z, as does a world with no
  camera. Split-screen: one `AudioListener` per player, nearest listener per emitter. Agent surface: `audio.listener_info`.
- `AudioEmitter` component: `sound: Handle<SoundAsset>` (and `sound_name`), or a `SoundCue` to play one
  of its variations when the emitter starts — `cue: Handle<SoundCue>` (and `cue_name`, resolved through
  `CueAssets`; used only when `sound`/`sound_name` are unset, ADR-5699). A looping cue emitter loops the
  variation it picked; with `looping: false` it plays it once, like a sound; a new variation is picked when
  the cue changes or the emitter respawns (ADR-5926) — `autoplay`, `looping`, `bus`,
  attenuation (`min_distance`, `max_distance`, curve: inverse/linear/log/custom `Curve`; `fade_start`, an optional fraction
  of `max_distance` where the inverse and log curves begin a smoothstep fade so they reach 0 at `max_distance` instead of
  cutting out, default none), spatial blend
  (0 = 2D, 1 = 3D), doppler factor (default 0), cone (inner/outer angle, outer gain), low-pass by
  distance (air absorption), occlusion (Tier 2: physics raycast → low-pass + volume). Delay, level,
  absorption and Doppler speed come from the level's propagation medium (§ 3c).
- Non-emitter sources join the occlusion query through `AudioOcclusion::set_extra_rays` (voice chat) and the
  owner-keyed `set_extra_rays_for(owner, rays)` (the sequencer's spatial sound sections, owner `sequencer`); a voice
  without an emitter track that needs a retunable low-pass plays through `Audio::start_asset_lowpass` /
  `set_voice_lowpass`. `stretch_asset` renders a range of a sound through the WSOLA core into a new in-memory sound
  (the sequencer's pitch-preserving sections, ADR-3197).
- One-shots at a position: `audio.play_asset_at(asset, params, position, emitter)` (`play_at(clip, position)` for
  a raw clip); attached: an `AudioEmitter` entity with `looping: false`. A *cue* plays in 3D too:
  `audio.play_cue_at(cue, position, emitter)` (a one-shot at a position, through the medium) and
  `audio.play_cue_on(cue, entity, emitter)` (attached to the entity's emitter, following it) — with
  seeded/event variants — pick a variation with the cue's weights, ranges and no-repeat rules, honour its
  cooldown, limit and steal policy, and (for `play_at`) return `None` when nothing audible reaches the
  listener. The cue's authored low-pass applies to every cue play: flat, `play_at`, `play_on` and an
  emitter's cue autoplay. An attached voice's low-pass track sits under its emitter's track, so the
  emitter's occlusion filter still applies on top (ADR-5977, superseding that part of ADR-5699). `play_cue`
  stays 2D.
- Position sync in `PostUpdate/AudioSync` using `GlobalTransform` (kira spatial scene: listener +
  emitter positions/orientations).
- Virtualisation (stop the voice, keep its time cursor): a voice below the audibility threshold (beyond
  `max_distance` included) is virtualised at once, whatever the count; audible voices are virtualised
  lowest priority first only while the count exceeds the project limit (default 64 desktop, 32 mobile)
  (ADR-5614).

## 3a. Parametric patches (sound driven by game state)
- `AudioPatch` asset (`.audiopatch.ron`): a small graph of layers (looping sources, granular/crossfaded
  loop sets keyed by a parameter, one-shots), per-layer volume/pitch/filter driven by **parameters**
  through curves (e.g. engine RPM ← speed, hover whine ← throttle, strain ← slope).
- Parameters set from Rust (`patch.set("speed", v)`) or bound to reflected fields (sim-bridge snapshot
  fields in DO27). Parameter changes are slewed on the game thread and handed to kira as short tweens,
  so the audio thread always interpolates (no zipper noise; ADR-5614).
- *As built (ADR-7226, `.audiopatch.ron` v2, [`docs/formats/audio-patch-v2.md`](../formats/audio-patch-v2.md)):*
  layers are `sound` loops, `keys` loop sets, `one_shot` layers (round-robin sounds fired when a smoothed
  parameter rises or falls through a threshold, crosses a multiple of a step, or by name through
  `Audio::patch_trigger`, with a cooldown) and `granular` layers (`regia_audio::granular`: periodic-Hann
  grains from a looping source at a curve-driven position and density, a 64-grain pool, seeded jitter).
  A parameter's `binding` names a reflected component field (`Component.field`, scale and offset) on the
  entity carrying `AudioPatchSource`; `patch_binding_sync` (PostUpdate, before AudioSync) sets the target
  each frame. A sim-bridge snapshot field binds by being mirrored into a reflected component; there is no
  direct snapshot binding. Smoothing stays on the game thread (slewed targets, short mixer tweens); the
  granular voice reads its controls at each grain start on the audio thread. v1 files migrate by a version
  stamp. Agent surface: `audio.patch_state`, `audio.patch_trigger` (`book/agents/audio-patch.md`).
- Music system: **stems** played in sync with per-stem volume driven by game state ("combat
  intensity"), intensity changes on bar boundaries (BPM/meter metadata on the music asset).
  All musical timing runs on the **audio clock**: one kira clock in the mixer, read as seconds, with a
  beat grid per track (ADR-2517). Scheduled sounds go into the mixer ahead of time and start on an exact
  sample. **Stingers** are one-shots on the next beat, bar or segment end. **Transition rules** between
  tracks choose immediate, next beat, next bar or end of segment; an equal-power crossfade length (the outgoing and incoming stems follow cosine/sine amplitudes, so the mix holds its level); an
  optional segment played between the two tracks. `MusicBeat` messages carry bar and beat to gameplay,
  up to a frame after the beat, with its exact clock time. Format: `.music.ron` v2
  ([`docs/formats/music-ron.md`](../formats/music-ron.md)). Agent surface: `audio.music_state`,
  `audio.music_transition`, `audio.music_stinger` (`book/agents/audio-music.md`).
  A track change with a fade is an equal-power crossfade: the outgoing and incoming stems follow cosine/sine
  amplitudes, so the mix holds its level through the fade. With a transition segment, that crossfade is from
  the outgoing track into the segment (the next track starts at full level when the segment ends). If the
  segment is shorter than the crossfade, the outgoing fade ends when the segment ends.
- Voice-line playback helpers used by announcers: per-id rotation without repeats in the last N plays
  (N = 2, fixed),
  priority classes where higher pre-empts lower and at most one low-priority line queued, automatic
  ducking rules (DO27: game −6 dB under announcer, 300 ms release; voice chat exempt).

## 3b. Voice capture
- `AudioCapture` (cpal input stream): device selection, 48 kHz mono frames delivered to the game on a
  lock-free ring, voice-activity detection (energy-based) and push-to-talk helpers, input level meter.
- Voice chat (codec, packet format, jitter buffer, channels, moderation) lives in `regia_audio::voice_chat`
  (`docs/specs/59-voice-chat.md`, ADR-1647); the network glue is the `VoiceChatTransport` trait, implemented
  by `regia_net::voice` (ADR-1911). A game that
  prefers its own voice core (DO27 routes voice through its own) ignores `VoiceChatPlugin` and uses only
  the capture and playback baseline here (a `VoiceStream` source type playing decoded frames with
  jitter buffering).
- 3D positioning for the baseline `VoiceStream` (built, post-v0.5.0): the stream takes a
  `VoicePlacement` (distance gain, equal-power pan, optional air-absorption low-pass) through
  `VoiceStreamHandle::set_placement`, ramped per processing batch; a `VoiceStreamAnchor` component
  on an entity places the loopback stream like an emitter at the entity's position against the
  nearest listener each `AudioSync`. On the frame the last anchor disappears (despawned or parked)
  the placement resets to the flat center, so an anchor that went silent past `max_distance`
  cannot leave the loopback muted; with no anchor ever, a placement set by the game or
  `audio.voice_place` is kept. Agent surface: `audio.voice_place`, and `audio.voice` reports
  `loopback.placement`.
- Android: requires `RECORD_AUDIO` permission request flow (JNI) — prompt only when the game asks.

## 3d. Audio zones (reverb presets, distance scale, rock occlusion, mouth routing)
ADR-250, DO27 ask 8. All of it lives in `regia_audio`; none of it needs physics or terrain crates.
- **Zone** (`AudioReverbZone`): shape box, sphere, capsule (axis = local Y) or global; `preset`
  (`custom`, `tube`, `mine_gallery`, `open`) fills `send_db` (wet), `feedback` (decay), `damping`,
  `stereo_width`, `fade_distance` (blend distance), `distance_scale` and `cavity`. Overlapping zones
  *blend*: layered least specific first (priority, then larger volume), each crossfading over what is
  below it by its own weight (1 inside, linear to 0 over the blend distance).
- **Distance scale**: an emitter and listener that share a zone hear the emitter at `distance / scale`
  (`scaled_range` stretches `min_distance`, `max_distance` and the rolloff, so every curve and the air
  absorption follow). A zone's share is the lower of the two memberships, so the scale fades out over the
  blend distance. Tube and gallery presets use 1.5 (DO27 ask 8: about 1.5x further underground).
- **Rock occlusion**: the host installs a `RockField` (ground height at `(x, z)`, `None` in a hole or
  off the map) on `AudioRockField`. `rock_thickness` marches listener to emitter (1 m step, at most 256
  samples) and sums the length below the surface that is not inside a `cavity` zone. `RockTuning` maps it:
  2.5 dB per metre (cap 60 dB) and a low-pass from 20 kHz to 350 Hz over 6 m. Rays come from the existing
  occlusion queue (same budget and round robin) and are smoothed like obstacle occlusion; both act together.
- **Mouth routing**: `AudioZonePortals` on a cavity zone lists its mouths. Emitter in the cavity, listener
  outside: the emitter is heard from a virtual position along the listener-to-mouth direction at the path
  length emitter, mouth, listener, with no scale, and the rock ray goes to the mouth. A listener within a
  mouth's `radius` (standing in it) is routed through that mouth even when another gives a shorter path; on
  the mouth centre the virtual source lies toward the emitter. No portals: nothing changes (rock occlusion
  alone).
- **Zone IRs** (ADR-2721): `ir: Some(name)` on the most specific zone around player 0 plays that impulse response
  instead of the preset: the reverb sends move to the hidden `ZoneIr` send bus over 0.25 s, and between two IR zones
  the convolver crossfades equal-power. Low tier or an unregistered IR: the preset (counted in `zone_ir_fallbacks`).
  A project with no zone IR mixes bit for bit as before.

## 3c. Propagation media (sound that follows the planet's atmosphere)
Every positional sound reaches the listener along up to two **paths**, each rendered as one voice
wrapped in a cheap path DSP (`regia_audio::path`: an arrival delay, two 2-pole RBJ low-passes,
an optional Schroeder coda, a post-coda gain; no tracks, no allocation on the audio thread).
- **Medium per level** (`AudioMedium`): derived from `LevelSettings.atmosphere.preset` and refined by
  `LevelSettings.audio_medium` (`LevelAudioMedium`, scene format v6; every field optional).
  Earth: c = 343.3 m/s (`SPEED_OF_SOUND`), level 0 dB, air absorption `α(f) = k f²`, `k = 0.0016 dB/m/kHz²` (ISO 9613-1
  fit). Mars: c = 240 m/s, −20 dB at equal source power, `k = 0.08 dB/m/kHz²` (Maurice et al. 2022;
  Bass & Chambers 2001) — the −3 dB point is ~3 kHz at 200 m on Earth, ~430 Hz on Mars. Airless:
  no airborne path. Default ground per preset: `soil` (Earth), `mars_regolith`, `ice` (Europa).
- **Airborne path**: delay `r / c` (one-shots at a position; voices attached to an emitter entity, looping
  or not, are not delayed — a moving delay would double the Doppler shift; ADR-5614), the emitter's authored rolloff and cone, the medium's level offset, an
  absorption low-pass at `f_c = √(3 / (k r))` kHz, Doppler with the medium's `c`. The `SPEED_OF_SOUND`
  constant remains the Earth value used by the medium-less `spatialize`.
- **Ground path** (structure-borne shockwaves): `AudioPropagation` on an emitter (or passed with a
  one-shot) sets `ground_coupling` 0..1 (scaled to 0 at `coupling_height` above the surface, so a
  hovering hull couples only within ~1 m), `ground_level_db`, `source_size_m`, surface (`1/√r`) or
  body (`1/r`) spreading and the material it touches. Per `GroundMaterial` (wave speed, Q, density;
  presets soil, sand, wet_soil, bedrock, basalt, regolith, mars_regolith, airless_rock, ice, concrete,
  steel): delay `r / v`, absorption `exp(−π f r / (Q v))` as a low-pass at `f_c = ln√2 · Q v / (π r)`,
  and a coda whose 60 dB decay is `ln 1000 · Q / (π · 100 Hz)` (clamped 0.15–8 s; dry high-Q
  regolith rings, wet soil thuds). Explosions on air worlds arrive through rock/ice before the air
  wave. `EmitterCategory` presets cover DO27's list: coupled impacts, explosions, collapses,
  footsteps, landings, machinery, hull landings/scrapes, planted weapons; weak hull hover; air-only
  projectiles; radio/UI bypass; cockpit and body.
- **Listener coupling** (`ListenerCoupling` on the listener, written by gameplay from physics
  contacts): `Boots` (low-pass 500 Hz, −6 dB), `Vehicle` (250 Hz, −3 dB, plus the hull's steel
  interface), `Structure` (700 Hz, −9 dB) or `None`. Leaving contact fades the ground path — coda
  included — to silence in 50 ms; landing restores it. Interfaces crossed (terrain → foundation →
  deck) each transmit `4 Z₁Z₂/(Z₁+Z₂)²` of the energy (`Z = ρ v`).
- **Interiors** (`AudioInteriorVolume`: an oriented box, `pressurised`, wall material): sources inside
  a pressurised volume are airborne in its own (Earth) air whatever the planet; exterior sounds get in
  through the wall when both sides have air (−35 dB, 400 Hz) and as ground vibration radiated into the
  interior air otherwise (−18 dB, 300 Hz) even for a floating listener.
- **Bypass**: buses with `bypass_medium` (default `Music`, `Ui`, `Voice`, and their children),
  `PathClass::Body` (breathing, suit, own footsteps by bone conduction) and the listener's own vehicle
  (`AudioPropagation.vehicle == ListenerCoupling.vehicle`: the hull-internal cockpit path) are heard
  directly, undelayed and unfiltered.
- **Haptics**: each ground arrival schedules a rumble at its arrival time with strength
  `4 × felt` clamped to 0..=1 (the arrival's amplitude at 30 Hz, absorption included; below 0.02 no rumble is scheduled) and continuous coupled emitters
  rumble while they run; `ground_haptics` forwards this frame to every connected pad through
  `regia_input::Gamepads`. A pad has one motor command, so each pad receives only the strongest due
  rumble and that rumble's duration (equal strengths keep the first). `Audio::haptics()` reports the
  ground haptics (agent-visible via `audio.medium`).
- **Cost**: per path voice one countdown, four biquad channels and (ground only) four combs + two
  all-passes on a mono sum; straight-line distance (no structure graph yet); the voice budget and
  virtualisation apply to path voices like any other (a positional one-shot with ground coupling
  starts two voices). Propagation runs in the `AudioSync` set under `tracing` spans and builds for
  Android.
- **Agent protocol**: `audio.medium` and `audio.propagation { entity | position, category }` →
  `{ paths: [{ path: airborne|ground|direct, delay_ms, gain_db, cutoff_hz, coupling_cutoff_hz, coda_s,
  felt }], coupled, … }` (`book/agents/audio.md`).

## 3e. Ambience that matches the picture (ADR-2442)
- `AmbienceBed` (opt-in resource): looping layers on the `Ambience` bus whose levels are polylines of one drive
  each: `constant`, `wind` (mean `SceneWind`), `gust` (wind now minus mean), `rustle` (the foliage sway stage's
  flutter factor `speed / (speed + 5)`), `rain`, `drips` (wetness after the rain), `snow` (`SurfaceWeather`).
  Wind drives are silent under `calm_m_per_s` (0.3 m/s), so still foliage is quiet. One-pole attack/release.
  `wind_override` (heard while no `SceneWind` exists) for hosts that set the renderer's foliage wind directly.
- `AmbientSpot` (component): a looping emitter (water) or scattered one-shot calls (birds) at random intervals and
  places, never the same variant twice in a row, through the emitter pipeline above (distance gain, distance
  low-pass, occlusion); weather gates silence a spot in rain or strong wind.
- `FootstepBank` (opt-in resource) and `regia_framework::footsteps`: animation notifies `Footstep*` on a
  `FootstepWalker` play the surface's variant set (walker override, host probe, terrain layer, default), no
  immediate repeat per walker, louder for faster and heavier walkers.
- `.ambience.ron` v1 (ADR-2721, `docs/formats/ambience-ron.md`): beds in the `BedLayer` shape plus random one-shots
  (interval, ring distance, height and level ranges, optional drive curve), all drawn from the file's `seed`, so the same
  file renders the same samples. Validated at cook. Agent: `audio.ambience_play`, `audio.ambience_stop`,
  `audio.ambience_list`.
  Agent surface: `audio.ambience`, `audio.set_ambience`, `audio.ambient_spots`, `audio.set_ambient_spot`,
  `audio.probe_ambience`, `audio.footsteps`, `audio.set_footsteps`, `audio.footstep`
  (`book/agents/audio-ambience.md`).

## 3f. Vehicle audio (ADR-3271)
- `regia_audio::vehicle_audio`: `.vehicleaudio.ron` v1 ([`docs/formats/vehicle-audio.md`](../formats/vehicle-audio.md)) and
  a pure fixed-step model from the drivetrain and wheel readout to voices: engine loops recorded at known rpm in on-load
  and off-load sets, the two nearest the smoothed rpm crossfaded with equal power and pitched `rpm / recorded rpm`; the sets
  blended by load (throttle and engine torque); a rev-limiter cut, shift one-shots with a dip, an optional whine; tire squeal
  from combined slip (0 below a threshold), rolling noise per surface, suspension bumps; wind by speed. One-pole smoothing on
  every parameter; no per-step allocation; a parameter digest.
- `regia_framework::vehicle_audio::VehicleAudioPlugin` plays it on emitter entities at the engine, exhaust and wheels through
  the emitter pipeline (the voice's own pitch multiplies Doppler), with the listener's own vehicle on the cockpit path, under
  a voice budget (nearest K vehicles full, the next ones one engine layer, the rest silent). Agent surface:
  `audio.vehicle_audio`, `audio.vehicle_audio_set`, `audio.vehicle_audio_probe` (`book/agents/vehicle-audio.md`).

## 4. Platform notes
- Android: cpal's Android backend (AAudio/Oboe). **Verify in M8** with a physical device: latency,
  resume after suspend, Bluetooth headset switch. Stop/restart the stream on `Suspended`/`Resumed`.
- Suspend releases every live mixer handle and its ducking activity and real voice-budget slot.
  Spawned patches retain their ids and parameter state; resume rebuilds their layers (phase restarts).
  `Audio::start_persistent_loop` opts a host-owned 2D loop into recovery with the same voice id and a
  frozen cursor, preserving pause and playback controls. Stop/despawn while suspended cancels recovery.
  Transient plays, voice-chat streams and music scheduling retain their existing teardown behavior.
  Beginning or ending a render-out mixdown ends preview loops and patches that are already playing
  (ADR-8052). While the mixer is down (suspended, no take), a new patch or persistent loop waits
  for resume, and `spawn_patch` rejects an unknown bus. A take on a suspended output still builds
  patch layers and persistent-loop voices, and suspending mid-take does not stop them. No content
  format changes.
  `suspend_output`/`resume_output` are idempotent and count only real transitions; resume rebuilds the
  buses with the current host volumes, mutes and snapshot cutoffs before restarting persistent sounds.
  The output gate does not resume an output a host already re-opened (ADR-185).
  Agent `audio.list_emitters.output` reports suspension, transition counts and persistent ownership.
  Public `regia_audio::{polyline, layer_outputs, LayerOutputs}` exposes the existing patch evaluation
  law for host probes. See `book/agents/audio-output-recovery.md`.
- Desktop output gate: the output pauses while every window is hidden (all modes), and after 5 consecutive
  frames ≥ 50 ms only when `AudioThrottle` arms it: `Auto` (default) only while a software adapter renders
  (`regia_platform::SoftwareRenderer`, published by the renderer), `On` always, `Off` never (ADR-187).
- Desktop: handle default device changes (recreate stream on `DeviceChanged` error, preserving playing
  sounds where possible).
- Stall watchdog (ADR-8830): kira's cpal backend restarts the stream itself on a device change; when that
  reopen fails, its restart thread panics (an abort under the shipping `panic = "abort"`, silence in a debug
  build). As built: `AudioStallWatchdog` (`regia_audio::stall`) counts engine frames in which the output's
  sample clock did not move on a live, unsuspended device output; after `stall_frames` (default 240) it
  suspends and resumes the output once (every voice ends, as on any suspend), counts it, warns, and doubles
  its patience up to `max_backoff`. A suspended output, the offline backend and a render-out mixdown are never
  touched. kira itself is vendored and patched (`vendor/kira`, ADR-8830): a failed reopen keeps the renderer,
  leaves the stream manager idle and is retried every 500 ms instead of panicking. ADR-7543: the frozen frames
  must also last `min_stall` of engine time (unscaled `RealTime` delta, default 3 s) before a recovery, a retry
  or a back-off reset, so the patience does not shrink with the frame rate (240 frames are 0.24 s at 1000 fps).
- Bad input never reaches the mixer (ADR-872): NaN/infinite volumes, rates, pans, cutoffs, reverb-zone values and
  voice-chat samples are replaced at the kira boundary (a NaN in a filter or reverb state would stay there), a
  non-finite emitter or listener place is silence until it is valid again, loop regions kira cannot wrap loop the
  whole sound, and tool or file durations never panic. Refusals are counted (`rejected_value_count`).
- Streaming underruns (a stream's decoder behind, the mixer holding it silent) are counted per stream
  (`Audio::stream_stats`, `audio.list_emitters` `streams`). Offline renders that must be reproducible decode their
  streams first (`SoundAsset::decode_to_memory`).

## 5. Editor
- Sound preview in content browser; bus mixer panel with live meters (peak/RMS per bus) during PIE;
  `SoundCue` editor; emitter gizmos (min/max spheres, cone).

## 6. Acceptance (M8)
- `samples/audio_lab`: 3D emitters orbiting the listener with audible panning/attenuation; music
  streaming with crossfade between two tracks; snapshot switch on pause menu; cue with 5 variations
  never repeating last 2; 200 simultaneous one-shots with virtualisation without glitches.
- Android: no audible glitch at 60 fps load on reference device; audio resumes after 20 suspend cycles.
- Parametric patch: a vehicle engine patch driven by a speed parameter sweeps smoothly 0→max→0 with no
  clicks; music stems change intensity on bar boundaries; announcer rotation/pre-emption/ducking rules
  pass unit tests.
- Music on the audio clock and the listener (ADR-2517; `crates/regia_audio/tests/music_clock.rs`,
  `listener_follow.rs`, `crates/regia/tests/audio_listener.rs`, offline mixer): a stinger starts on the next
  bar's exact sample at render steps of 800, 137 and 1024 frames; a next-bar transition switches on the
  boundary sample; crossfades are equal power; a transition segment plays between two tracks; gameplay gets
  every beat in order through a long frame; the listener matches the camera after one update unless a
  listener component or the setting says otherwise.
- Convolution, zone IRs, time stretch, ambience files (ADR-2721; `crates/regia_audio/src/convolution.rs`,
  `timestretch.rs`, `tests/convolution.rs`, `tests/ambience_file.rs`, `tests/unconfigured_identity.rs`, offline mixer): a unit
  impulse returns the IR and any signal matches direct convolution within −80 dB; `process` allocates nothing over 10,000
  blocks (counting allocator); an IR crossfade's largest sample step stays within 1.25× the steady outputs'; stretch at
  0.5, 0.75, 1.5 and 2 keeps a 440 Hz tone within 1 % and the duration within 1 %, rate 1 is bit-identical; a seeded
  `.ambience.ron` renders the same samples twice; truncated or empty IRs are refused with their codes; the default mix is
  bit-identical to the tree before convolution. On a 44.1 kHz mixer binaural voices use a 44.1 kHz HRIR table.
- Binaural rates (AU-14, ADR-7461; `crates/regia_audio/tests/hrtf_rates.rs`, `hrtf.rs` unit tests, offline mixer): a
  mixer at any rate convolves with an HRIR table at its own rate, built on first need from the 48 kHz sphere (band-limited
  sinc, every resampled tap kept above 48 kHz, FFT size rounded up to a multiple of the 32-sample block), at most six
  tables per sphere (44.1 and 48 kHz up front; past the cap the nearest table). A hard-left impulse rendered at 96 kHz
  and 192 kHz lags between the ears two and four times its 48 kHz lag, within one sample; the 44.1 and 48 kHz output is
  bit-identical to the tree before per-rate tables (pinned hashes); the effect allocates nothing per block at 44.1 to
  192 kHz. A device whose rate had no table is detected from the effect's own report (`HrtfControl::stale_rate`): the
  table is built and the emitter track rebuilt once. `audio.list_emitters` reports `hrtf_rate` per emitter and per
  headphone listener. Not verified here: a real 96 kHz output device.
- Voice capture: loopback test (capture → `VoiceStream` playback) on Linux, Windows, Android with
  < 60 ms added latency excluding network.
- Propagation media (§ 3c; `crates/regia_audio/tests/propagation.rs`, offline mixer): airless +
  floating listener → silent; airless + standing 500 m from an explosion on ice → arrives at
  `500 / 3800` s, low-passed, audible; jumping before the arrival → silent; Mars at 200 m vs Earth
  at 200 m → ~20 dB quieter, later (`r/240` vs `r/343`), darker; an explosion on Earth (bedrock) is
  heard and felt through the ground before the air wave; a pressurised interior has its own air and
  exterior impacts come through the structure, muffled; Doppler uses the level medium's `c`; an
  airless Europa pilot on foot hears only the suit (breathing, radio, ground thumps through the
  boots). Listen-check WAVs per medium in `docs/evidence/audio/<date>/`.

## 7. Audio cook (ADR-3230)

Audio ships as its source file by default. A project may transcode WAV and FLAC sources to Ogg Vorbis at cook and package time.

- **Project table** `<project>/audio_cook.toml` (all keys optional; unknown keys and a quality outside -0.1..=1.0 are a cook warning and the defaults apply): `mode = "keep" | "vorbis"` (default `keep`), `quality` (desktop, default 0.4), `android_quality` (default 0.3), `force = false` (transcode `.ogg` and `.mp3` too).
- **Per sound**, the `.meta` `settings` RON string: `(mode: Some(vorbis), quality: Some(0.5), android_quality: Some(0.2), force: Some(false))`, next to the loudness keys.
- **What ships**: under `vorbis`, a WAV or FLAC ships as Ogg Vorbis at the platform's quality, under its own path and name; the loudness table, asset ids and game code are unchanged. `.ogg` and `.mp3` ship as they are unless `force`. A sound whose channels are identical is encoded as one channel.
- **Loudness** is measured on the source, so the gain is the same under both modes.
- **Cache and reports**: `<cooked cache>/audio/…ogg`; `regia cook` prints each sound's mode and source vs shipped bytes; `audio_ship.json` holds the same. Cooks and encodes are byte-reproducible.
- Book page: `book/agents/audio-cook.md`.
