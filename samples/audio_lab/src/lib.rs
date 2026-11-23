// SPDX-License-Identifier: MIT-0
//! `audio_lab` (M8, `docs/specs/16-audio.md` § 6): a guided tour of the audio engine. One
//! scenario at a time, advanced automatically or by pressing `1`–`7`:
//!
//! 1. **Orbit** — a 3D emitter circles the listener; panning and attenuation are audible. The
//!    emitter is component-driven: the scenario only sets its `sound`, autoplay starts it and
//!    clearing the sound stops it.
//! 2. **Crossfade** — streaming music crossfades between two tracks.
//! 3. **Snapshot** — the `Paused` snapshot blends the music down and back up.
//! 4. **Cue** — a 5-variation cue never repeats the last 2 picks.
//! 5. **Burst** — 200 one-shots in one frame; the voice budget virtualises the rest.
//! 6. **Patch** — a parametric engine patch sweeps its `speed` parameter with no zipper noise.
//! 7. **Announcer** — rotation, pre-emption and −6 dB ducking under the line.
//!
//! All sounds are synthesised at startup (no asset files). Run `audio_lab --headless` for a
//! CI-friendly pass over every scenario on the offline backend: no window, deterministic 60 Hz
//! steps, a state log per scenario, exit code 0 after the full tour. Headless verifies state
//! transitions, not audible output — that is what the acceptance tests and a real device are for.

use std::time::Duration;

use regia::prelude::*;
use regia_audio::prelude::*;
use regia_scene::Transform;

/// The scenarios, in tour order. The index of a scenario is its hotkey minus one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// A 3D emitter circles the listener.
    Orbit,
    /// Streaming music crossfades between two tracks.
    Crossfade,
    /// The `Paused` snapshot blends the music down and back.
    Snapshot,
    /// A 5-variation cue fires a burst of picks.
    Cue,
    /// 200 one-shots in one frame, virtualised to the voice budget.
    Burst,
    /// A parametric engine patch sweeps its `speed` parameter.
    Patch,
    /// Announcer rotation, pre-emption and ducking.
    Announcer,
}

/// Every scenario, in tour order.
pub const ALL: [Scenario; 7] = [
    Scenario::Orbit,
    Scenario::Crossfade,
    Scenario::Snapshot,
    Scenario::Cue,
    Scenario::Burst,
    Scenario::Patch,
    Scenario::Announcer,
];

impl Scenario {
    /// The name shown in logs.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Orbit => "orbit",
            Scenario::Crossfade => "crossfade",
            Scenario::Snapshot => "snapshot",
            Scenario::Cue => "cue",
            Scenario::Burst => "burst",
            Scenario::Patch => "patch",
            Scenario::Announcer => "announcer",
        }
    }

    /// How long the scenario runs before the tour advances.
    pub fn duration(self) -> f64 {
        match self {
            Scenario::Orbit => 8.0,
            Scenario::Crossfade => 6.0,
            Scenario::Snapshot => 6.0,
            Scenario::Cue => 5.0,
            Scenario::Burst => 4.0,
            Scenario::Patch => 8.0,
            Scenario::Announcer => 8.0,
        }
    }

    /// From a hotkey (`1`–`7`), or `None`.
    pub fn from_hotkey(digit: u8) -> Option<Self> {
        ALL.get(usize::from(digit).checked_sub(1)?).copied()
    }
}

/// Exit after this many frames (`--exit-after-frames`, automated runs).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ExitAfterFrames(pub u64);

/// Exit after the tour completed every scenario once (headless mode).
#[derive(Resource, Debug, Clone, Copy)]
pub struct ExitAfterTour;

/// The lab's persistent state.
#[derive(Resource)]
pub struct LabState {
    /// The running scenario.
    pub scenario: Scenario,
    /// Seconds elapsed in the current scenario.
    pub elapsed: f64,
    /// The entity carrying the orbiting emitter.
    pub emitter_entity: Entity,
    /// The spawned engine patch, during the patch scenario.
    pub patch: Option<u64>,
    /// The orbit loop, registered in [`SoundAssets`] (the emitter plays it while orbiting).
    pub orbit_sound: regia_asset::Handle<SoundAsset>,
    /// Whether the `Paused` snapshot is pushed (snapshot scenario).
    pub snapshot_pushed: bool,
    /// Whether the snapshot scenario already popped it again.
    pub snapshot_done: bool,
    /// Whether the crossfade already happened (crossfade scenario).
    pub crossfaded: bool,
    /// Whether the burst already fired (burst scenario).
    pub burst_fired: bool,
    /// How many announcer lines the scenario has fired.
    pub announces_fired: usize,
    /// Throttle for the periodic status log (counts down; ≤ 0 logs).
    pub log_cooldown: f64,
}

/// The generated audio assets (no files needed: everything is synthesised at startup).
#[derive(Resource)]
pub struct LabAssets {
    /// The orbiting emitter's loop.
    pub orbit: SoundAsset,
    /// Music track A (crossfade source).
    pub track_a: MusicAsset,
    /// Music track B (crossfade target).
    pub track_b: MusicAsset,
    /// The burst one-shots, one per frequency.
    pub burst: Vec<SoundAsset>,
    /// The engine patch (speed-driven loop set).
    pub patch: AudioPatch,
}

/// The lab plugin: builds the audio system, generates the assets and runs the scenarios.
pub struct AudioLabPlugin {
    /// Advance scenarios automatically (headless runs; windowed runs too — hotkeys override).
    pub auto_advance: bool,
}

impl Plugin for AudioLabPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AudioSettings>();
        // Headless apps have no platform plugin; the hotkey reader needs the message registered.
        app.add_message::<regia::platform::KeyboardInput>();
        if self.auto_advance {
            app.insert_resource(AutoAdvance);
        }
        app.add_plugins(AudioPlugin);

        let assets = LabAssets::generate();
        let orbit_sound = app
            .world_mut()
            .resource_mut::<SoundAssets>()
            .add("orbit", assets.orbit.clone());
        app.world_mut()
            .spawn((Transform::default(), AudioListener::default()));
        // No sound yet: the orbit scenario gives it one (autoplay), every other scenario clears it.
        let emitter_entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                AudioEmitter {
                    min_distance: 2.0,
                    max_distance: 12.0,
                    rolloff: RolloffCurve::Linear,
                    occlusion: false,
                    ..AudioEmitter::default()
                },
            ))
            .id();
        app.insert_resource(LabState {
            scenario: Scenario::Orbit,
            elapsed: 0.0,
            emitter_entity,
            patch: None,
            orbit_sound,
            snapshot_pushed: false,
            snapshot_done: false,
            crossfaded: false,
            burst_fired: false,
            announces_fired: 0,
            log_cooldown: 0.0,
        });
        app.insert_resource(assets);

        {
            let mut audio = app.world_mut().resource_mut::<Audio>();
            audio.register_snapshot(AudioSnapshot::new("Paused").with_bus_volume("Music", -18.0));
            let entries: Vec<CueEntry> = (0..5)
                .map(|i| CueEntry {
                    asset: sine_clip(500.0 + i as f32 * 40.0, 0.12, 0.4),
                    weight: 1.0,
                })
                .collect();
            let mut cue = SoundCue::new("lab_hit", entries);
            cue.no_repeat_last = 2;
            audio.register_cue(cue);
            audio.register_announcer_bank(
                "lab_objective",
                AnnouncerBank::new(
                    (0..3)
                        .map(|i| sine_clip(660.0 + i as f32 * 30.0, 1.2, 0.5))
                        .collect(),
                    AnnouncePriority::High,
                ),
            );
        }

        app.add_systems(
            Update,
            (
                render_when_offline,
                select_scenario_keyboard,
                run_scenario,
                advance_scenario_timer,
                exit_after_frames,
            ),
        );
        info!(target: "audio_lab", "audio_lab ready — hotkeys 1..7 pick a scenario, space restarts");
    }
}

#[derive(Resource)]
struct AutoAdvance;

fn sine_clip(hz: f32, seconds: f64, amp: f32) -> SoundAsset {
    const RATE: u32 = 48_000;
    let len = (seconds * f64::from(RATE)) as usize;
    let samples: Vec<f32> = (0..len)
        .map(|i| ((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * amp)
        .collect();
    SoundAsset::from_clip(&SoundClip::new(samples, RATE).expect("constant parameters"))
}

/// WAV (PCM16 mono) bytes of a sine, for the streaming music tracks.
fn wav_bytes(hz: f32, seconds: f64) -> std::sync::Arc<Vec<u8>> {
    const RATE: u32 = 48_000;
    let len = (seconds * f64::from(RATE)) as usize;
    let mut data = Vec::with_capacity(len * 2);
    for i in 0..len {
        let sample = (((i as f32 / RATE as f32) * hz * std::f32::consts::TAU).sin() * 0.35 * 32_000.0) as i16;
        data.extend_from_slice(&sample.to_le_bytes());
    }
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::sync::Arc::new(wav)
}

fn mono_track(name: &str, hz: f32) -> MusicAsset {
    MusicAsset::from_stems(
        name,
        110.0,
        4,
        vec![MusicStem {
            name: "base".into(),
            asset: SoundAsset::streaming_from_bytes(wav_bytes(hz, 2.0), name).expect("a valid WAV"),
            base_db: 0.0,
            intensity: vec![(0.0, 0.0), (1.0, 0.0)],
        }],
    )
}

impl LabAssets {
    /// Synthesises every asset the lab plays.
    pub fn generate() -> Self {
        let def = AudioPatchDef {
            version: 1,
            name: "lab_engine".into(),
            bus: Some("Sfx".into()),
            parameters: vec![PatchParamDef {
                name: "speed".into(),
                min: 0.0,
                max: 100.0,
                smoothing_secs: 0.08,
                binding: None,
            }],
            layers: vec![PatchLayerDef {
                name: "engine_set".into(),
                sound: None,
                keys: vec![
                    PatchKeyDef {
                        value: 0.0,
                        sound: "idle".into(),
                        gain_db: 0.0,
                        pitch: 1.0,
                    },
                    PatchKeyDef {
                        value: 50.0,
                        sound: "mid".into(),
                        gain_db: 0.0,
                        pitch: 1.0,
                    },
                    PatchKeyDef {
                        value: 100.0,
                        sound: "fast".into(),
                        gain_db: 0.0,
                        pitch: 1.0,
                    },
                ],
                parameter: Some("speed".into()),
                volume_db: -8.0,
                volume_curves: vec![],
                pitch_curves: vec![PatchCurveDef {
                    parameter: "speed".into(),
                    points: vec![(0.0, 0.6), (100.0, 1.4)],
                }],
                filter_curves: vec![],
                one_shot: None,
                granular: None,
            }],
        };
        let mut key_sounds = std::collections::HashMap::new();
        key_sounds.insert(("engine_set".to_string(), 0), sine_clip(90.0, 0.5, 0.3));
        key_sounds.insert(("engine_set".to_string(), 1), sine_clip(180.0, 0.5, 0.3));
        key_sounds.insert(("engine_set".to_string(), 2), sine_clip(360.0, 0.5, 0.3));
        Self {
            orbit: sine_clip(440.0, 0.5, 0.3),
            track_a: mono_track("lab_a", 220.0),
            track_b: mono_track("lab_b", 330.0),
            burst: (0..200)
                .map(|i| sine_clip(400.0 + i as f32 * 3.0, 0.25, 0.05))
                .collect(),
            patch: AudioPatch {
                def,
                layer_sounds: Default::default(),
                key_sounds,
            },
        }
    }
}

/// Hotkeys `1`–`7` jump to a scenario, `Space` restarts the current one.
fn select_scenario_keyboard(
    mut keys: MessageReader<KeyboardInput>,
    mut state: ResMut<LabState>,
    mut audio: ResMut<Audio>,
) {
    for input in keys.read() {
        if input.state != ButtonState::Pressed || input.repeat {
            continue;
        }
        let digit = match input.physical_key.as_str() {
            "Digit1" => 1,
            "Digit2" => 2,
            "Digit3" => 3,
            "Digit4" => 4,
            "Digit5" => 5,
            "Digit6" => 6,
            "Digit7" => 7,
            _ => 0,
        };
        if let Some(scenario) = Scenario::from_hotkey(digit) {
            leave_scenario(&mut state, &mut audio);
            state.scenario = scenario;
            state.elapsed = 0.0;
            info!(target: "audio_lab", "scenario → {}", scenario.name());
        } else if input.physical_key == "Space" {
            leave_scenario(&mut state, &mut audio);
            state.elapsed = 0.0;
            info!(target: "audio_lab", "scenario {} restarted", state.scenario.name());
        }
    }
}

/// Runs the current scenario's per-frame logic.
fn run_scenario(
    time: Res<Time>,
    mut state: ResMut<LabState>,
    assets: Res<LabAssets>,
    mut audio: ResMut<Audio>,
    mut transforms: Query<&mut Transform>,
    mut emitters: Query<&mut AudioEmitter>,
) {
    let dt = time.delta_secs() as f64;
    let t = state.elapsed;
    // The orbit emitter plays only during the orbit scenario: give it its sound (autoplay
    // starts it) or clear it (stops it). Written only on change.
    if let Ok(mut emitter) = emitters.get_mut(state.emitter_entity) {
        let wanted = (state.scenario == Scenario::Orbit).then(|| state.orbit_sound.clone());
        if emitter.sound != wanted {
            emitter.sound = wanted;
        }
    }
    match state.scenario {
        Scenario::Orbit => {
            // Half a revolution per second, 5 m out.
            let angle = t as f32 * std::f32::consts::PI;
            let position = Vec3::new(angle.cos() * 5.0, 0.0, angle.sin() * 5.0);
            if let Ok(mut transform) = transforms.get_mut(state.emitter_entity) {
                transform.translation = position;
            }
            if state.log_cooldown <= 0.0 {
                let frame = spatialize(
                    &ListenerFrame::looking_at(Vec3::ZERO, -Vec3::Z),
                    &EmitterFrame {
                        position,
                        ..EmitterFrame::default()
                    },
                    &AudioEmitter {
                        min_distance: 2.0,
                        max_distance: 12.0,
                        rolloff: RolloffCurve::Linear,
                        ..AudioEmitter::default()
                    },
                );
                info!(
                    target: "audio_lab",
                    "orbit: gain {:.2}, pan {:+.2}, distance {:.1} m",
                    frame.gain, frame.pan, frame.distance
                );
            }
        }
        Scenario::Crossfade => {
            if t < dt {
                let _ = audio.play_music(&assets.track_a, Duration::ZERO);
                info!(target: "audio_lab", "music: track a");
            } else if !state.crossfaded && t > 2.5 {
                state.crossfaded = true;
                let _ = audio.play_music(&assets.track_b, Duration::from_secs_f32(1.5));
                info!(target: "audio_lab", "music: crossfading to track b over 1.5 s");
            } else if state.log_cooldown <= 0.0 {
                state.log_cooldown = 0.5;
                info!(
                    target: "audio_lab",
                    "music: playing {:?}, crossfading {}",
                    audio.current_music(),
                    audio.music_crossfading()
                );
            }
        }
        Scenario::Snapshot => {
            if !state.snapshot_pushed && !state.snapshot_done && t > 1.0 {
                state.snapshot_pushed = true;
                let _ = audio.push_snapshot("Paused", Duration::from_secs_f32(0.5));
                info!(target: "audio_lab", "snapshot: Paused pushed (music −18 dB over 0.5 s)");
            } else if state.snapshot_pushed && t > 3.5 {
                state.snapshot_pushed = false;
                state.snapshot_done = true;
                let _ = audio.pop_snapshot(Duration::from_secs_f32(0.5));
                info!(target: "audio_lab", "snapshot: Paused popped");
            } else if state.log_cooldown <= 0.0 {
                state.log_cooldown = 0.5;
                info!(
                    target: "audio_lab",
                    "snapshot: active {:?}, music bus {:.1} dB",
                    audio.active_snapshot(),
                    audio.bus_volume("Music").unwrap_or(f32::NAN)
                );
            }
        }
        Scenario::Cue => {
            const FIRE_EVERY: f64 = 0.4;
            if t % FIRE_EVERY < dt
                && let Err(error) = audio.play_cue("lab_hit")
            {
                warn!(target: "audio_lab", "cue failed: {error}");
            }
            if state.log_cooldown <= 0.0 {
                state.log_cooldown = 0.5;
                info!(
                    target: "audio_lab",
                    "cue: {} plays so far, {} real voices",
                    audio.played_count(),
                    audio.real_voice_count()
                );
            }
        }
        Scenario::Burst => {
            if !state.burst_fired && t > 0.5 {
                state.burst_fired = true;
                for asset in &assets.burst {
                    let _ = audio.play_asset(asset, PlaybackParams::on_bus("Sfx"));
                }
                audio.drain();
                info!(
                    target: "audio_lab",
                    "burst: 200 one-shots → {} real, {} virtual voices",
                    audio.real_voice_count(),
                    audio.virtual_voice_count()
                );
            } else if state.log_cooldown <= 0.0 {
                state.log_cooldown = 0.5;
                info!(
                    target: "audio_lab",
                    "burst: {} real, {} virtual voices",
                    audio.real_voice_count(),
                    audio.virtual_voice_count()
                );
            }
        }
        Scenario::Patch => {
            if state.patch.is_none() {
                match audio.spawn_patch(assets.patch.clone()) {
                    Ok(id) => {
                        state.patch = Some(id);
                        info!(target: "audio_lab", "patch: engine spawned");
                    }
                    Err(error) => warn!(target: "audio_lab", "patch failed to spawn: {error}"),
                }
            } else if let Some(id) = state.patch {
                // One sweep up and down over the scenario.
                let phase = (t / 4.0).min(2.0);
                let speed = if phase <= 1.0 {
                    phase * 100.0
                } else {
                    (2.0 - phase) * 100.0
                };
                let _ = audio.patch_set_param(id, "speed", speed as f32);
                if state.log_cooldown <= 0.0 {
                    state.log_cooldown = 0.5;
                    info!(target: "audio_lab", "patch: speed {speed:.0}");
                }
            }
        }
        Scenario::Announcer => {
            // Line 1 at 0.5 s, line 2 at 2.0 s (pre-emption or rejection), and a look at the
            // duck state while the line plays.
            let fire_times = [0.5, 2.0];
            if state.announces_fired < fire_times.len() && t >= fire_times[state.announces_fired] {
                state.announces_fired += 1;
                info!(target: "audio_lab", "announcer: {:?}", audio.announce("lab_objective"));
            } else if state.log_cooldown <= 0.0 {
                state.log_cooldown = 0.5;
                info!(
                    target: "audio_lab",
                    "announcer: queued {:?}, music bus {:.1} dB",
                    audio.announcer_queued(),
                    audio.bus_volume("Music").unwrap_or(f32::NAN)
                );
            }
        }
    }
    state.elapsed += dt;
    state.log_cooldown -= dt;
}

/// Ends the current scenario and moves to the next one.
fn advance_scenario_timer(
    mut state: ResMut<LabState>,
    mut audio: ResMut<Audio>,
    auto: Option<Res<AutoAdvance>>,
    done: Option<Res<ExitAfterTour>>,
    mut exit: MessageWriter<AppExit>,
) {
    if state.elapsed < state.scenario.duration() {
        return;
    }
    let next_index = ALL
        .iter()
        .position(|s| *s == state.scenario)
        .map_or(0, |i| (i + 1) % ALL.len());
    debug!(
        target: "audio_lab",
        "end of {}: voices {:?}",
        state.scenario.name(),
        audio.voice_kinds()
    );
    leave_scenario(&mut state, &mut audio);
    state.scenario = ALL[next_index];
    state.elapsed = 0.0;
    info!(target: "audio_lab", "scenario → {}", state.scenario.name());
    if auto.is_some() && done.is_some() && next_index == 0 {
        info!(target: "audio_lab", "tour complete");
        exit.write(AppExit::Success);
    }
}

/// Stops whatever the current scenario started (called on transitions and restarts).
fn leave_scenario(state: &mut LabState, audio: &mut Audio) {
    if let Some(patch) = state.patch.take() {
        audio.despawn_patch(patch);
    }
    if state.snapshot_pushed {
        state.snapshot_pushed = false;
        state.snapshot_done = true;
        let _ = audio.pop_snapshot(Duration::from_secs_f32(0.5));
    }
    state.crossfaded = false;
    state.burst_fired = false;
    state.announces_fired = 0;
}

/// The offline backend renders only when asked: on headless runs this pumps the mixer so kira
/// processes stop commands and streaming decoders advance (a real output device renders itself).
fn render_when_offline(mut audio: ResMut<Audio>) {
    if audio.is_headless() {
        let _ = audio.render_offline((48_000.0 / 60.0) as usize);
    }
}

/// Exits after `ExitAfterFrames` frames (automated smoke runs).
fn exit_after_frames(limit: Option<Res<ExitAfterFrames>>, time: Res<Time>, mut exit: MessageWriter<AppExit>) {
    if let Some(limit) = limit
        && time.frame_count() >= limit.0
    {
        info!(target: "audio_lab", "{} frames done, exiting", time.frame_count());
        exit.write(AppExit::Success);
    }
}
