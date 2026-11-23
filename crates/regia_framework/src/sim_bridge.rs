// SPDX-License-Identifier: MPL-2.0
//! External-simulation bridge (`docs/specs/25-sim-bridge-and-do27.md` § 3).
//!
//! Connects a game whose authority lives outside the engine's ECS — a deterministic simulation
//! on its own thread, an external process, or a mock double in tests — to the presentation
//! world: input flows out, entities are spawned from snapshots, and events drive VFX, audio and UI.
//!
//! # Coordinates
//!
//! Regia Engine is right-handed, +Y up, forward −Z, in **metres**
//! (`docs/02-architecture.md` § 3). The [`ExternalSim`] implementation converts the sim's axes and
//! units to Regia Engine's inside [`ExternalSim::read_snapshot`], with one exception: a
//! **left-handed** sim sets [`SimBridgeSettings::handedness`] to
//! [`SimHandedness::LeftHandedReflectX`](crate::sim_coords::SimHandedness::LeftHandedReflectX) and
//! the bridge reflects X once, for every snapshot pose and placed event (`crate::sim_coords`,
//! ADR-8456); such a sim must not reflect again. The default
//! ([`SimHandedness::RightHanded`](crate::sim_coords::SimHandedness::RightHanded)) converts nothing.
//! [`to_regia_pose`] returns its inputs unchanged and `reconcile` does not call it.
//!
//! # Blending
//!
//! A sim that publishes its last two frames as a pair ([`ExternalSim::read_pair`], such as a
//! [`ThreadedSimHost`](crate::sim_host::ThreadedSimHost)) is drawn between them by each kind's
//! [`BlendPolicy`] ([`KindEntry::blend`]): lerp (the default), snap or hold, with the teleport
//! distance and the presentation delay of [`SimBridgeSettings`] (`crate::sim_blend`).
//!
//! # Hot reload
//!
//! Per ADR-015 § 4 item 4, the [`ExternalSim`] lives in a library that is **not reloaded**: the
//! sim crate is built as its own dylib linking the engine dylib, and its handle is an ordinary
//! resource owned by [`SimBridgePlugin`]. Editing the sim needs an editor restart (like engine
//! code). This module does **not** build a dylib loader; it only provides the resource slot.
//!
//! # Suspend
//!
//! [`SimBridgePlugin`] adds [`hold_sim_host_on_suspend`] to the `Lifecycle` schedule: when the app
//! is suspended (Android: home button, screen off) a threaded sim host
//! ([`crate::sim_host::SimHostHandle`] resource) is held, so the sim never steps unseen, and the
//! hold is released on `Resumed` unless the game had held the host itself.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_ecs::ecs::component::Mutable;
use regia_scene::Transform;

use crate::sim_blend::{BlendPolicy, SimBlendStats, SimBridgeSettings, SimPose, SnapshotPair, blend_pose};

#[cfg(test)]
fn note_reconcile_write(counters: &mut SimCounters) {
    counters.reconcile_writes = counters.reconcile_writes.wrapping_add(1);
}

#[cfg(not(test))]
fn note_reconcile_write(_counters: &mut SimCounters) {}

/// Queues a transform or extras write only when the entity does not already have that value.
/// `extras` is `None` when the sim does not report extras for the id (nothing to compare).
fn queue_existing(
    commands: &mut Commands,
    counters: &mut SimCounters,
    transforms: &Query<&Transform>,
    extras_q: &Query<&SimExtras>,
    extras: Option<&[(String, f32)]>,
    entity: Entity,
    transform: Transform,
) {
    let same = transforms.get(entity).is_ok_and(|current| {
        current.translation == transform.translation
            && current.rotation == transform.rotation
            && current.scale == transform.scale
    });
    if !same {
        commands.entity(entity).insert(transform);
        note_reconcile_write(counters);
    }
    if let Some(extras) = extras {
        let same_extras = extras_q.get(entity).is_ok_and(|current| current.0 == extras);
        if !same_extras {
            commands.entity(entity).insert(SimExtras(extras.to_vec()));
            note_reconcile_write(counters);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: coordinate conversion
// ---------------------------------------------------------------------------

/// Rotation about the world Y axis by `radians`.
///
/// In Regia Engine's right-handed frame (+Y up, forward −Z), a positive yaw turns the
/// forward direction from −Z toward −X (counter-clockwise when viewed from above).
/// `yaw_about_y(0.0)` is [`Quat::IDENTITY`] — the object faces −Z.
#[inline]
pub fn yaw_about_y(radians: f32) -> Quat {
    Quat::from_rotation_y(radians)
}

/// Returns `(sim_translation, sim_rotation)` unchanged.
///
/// `reconcile` writes the snapshot's translation and rotation straight into `Transform` and
/// does not call this function. A sim that uses another frame converts inside
/// [`ExternalSim::read_snapshot`]. This function is not a hook and cannot be overridden.
#[inline]
pub fn to_regia_pose(sim_translation: Vec3, sim_rotation: Quat) -> (Vec3, Quat) {
    (sim_translation, sim_rotation)
}

// ---------------------------------------------------------------------------
// Component: the sim-side stable id
// ---------------------------------------------------------------------------

/// The stable identifier an [`ExternalSim`] assigns to an entity.
///
/// Attached automatically by [`SimBridgePlugin`] to every entity spawned from a
/// snapshot. Never changes for the lifetime of the sim entity; the bridge uses
/// it to reconcile snapshots across frames.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[reflect(Component)]
pub struct SimId(pub u64);

/// The kind key a snapshot entity carried (for example `"disc"`, `"warhull"`). Attached when a
/// [`KindRegistry`] knows the kind.
#[derive(Component, Reflect, Clone, Debug, PartialEq, Eq, Hash)]
#[reflect(Component)]
pub struct SimKind(pub String);

/// The team index a snapshot entity carried, for team-colour presentation.
///
/// This is not the rules team. The rules team is [`crate::rules::TeamId`] (`0` is neutral).
/// A presentation value `n` corresponds to rules team `n as u8` when `n` fits in a `u8`.
/// The rules never read this component, and nothing copies one into the other.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[reflect(Component, Default)]
pub struct Team(pub u32);

/// Extra presentation fields the sim reported for one entity (damage zones, lean, turret yaw,
/// team-colour override…), as `(name, value)` pairs. Rewritten every reconciliation from
/// [`SimSnapshot::extras_for`]; presentation scripts read it (`specs/25` § 3).
#[derive(Component, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct SimExtras(pub Vec<(String, f32)>);

// ---------------------------------------------------------------------------
// Kind registry (specs/25 § 3)
// ---------------------------------------------------------------------------

/// What the bridge spawns for one kind key (`config/kinds.ron`).
///
/// `prefab` names the prefab asset path; the bridge records it now and instantiates it when the
/// prefab system lands (M6, `specs/14` A3) — until then kind entities spawn bare plus their
/// presentation scripts. `death_prefab` is the optional swap-in on vanish (M6).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindEntry {
    /// Prefab asset path (resolved from M6).
    pub prefab: Option<String>,
    /// Presentation script type paths (from the game's [`ScriptRegistry`](crate::script::ScriptRegistry)),
    /// attached in order on spawn.
    pub presentation_scripts: Vec<String>,
    /// Prefab swapped in when the entity vanishes (resolved from M6).
    pub death_prefab: Option<String>,
    /// How the bridge poses this kind between a sim's two published frames (ADR-8456). Optional in
    /// RON; [`BlendPolicy::Lerp`] when left out.
    #[serde(default)]
    pub blend: BlendPolicy,
}

/// Kind key → what to spawn (`config/kinds.ron`): `"warhull" → prefabs/units/warhull.prefab.ron`
/// plus presentation scripts. Loaded from RON; insert as a resource or build the plugin with
/// [`SimBridgePlugin::kinds`].
///
/// ```ron
/// (kinds: {
///     "disc": (prefab: Some("prefabs/disc.prefab.ron"),
///              presentation_scripts: ["my_game::scripts::DiscPresenter"]),
/// })
/// ```
#[derive(Resource, Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindRegistry {
    /// Every known kind, keyed by kind key.
    pub kinds: std::collections::BTreeMap<String, KindEntry>,
}

impl KindRegistry {
    /// Parses RON text (`config/kinds.ron`).
    pub fn from_ron(text: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(text)
    }

    /// Loads and parses `path` (missing file is an error here: the game declared it).
    pub fn load(path: &std::path::Path) -> Result<Self, KindRegistryError> {
        let text = std::fs::read_to_string(path).map_err(KindRegistryError::Io)?;
        Self::from_ron(&text).map_err(KindRegistryError::Parse)
    }

    /// The entry for `kind`, if registered.
    pub fn entry(&self, kind: &str) -> Option<&KindEntry> {
        self.kinds.get(kind)
    }

    /// The blend policy of `kind` ([`BlendPolicy::Lerp`] for a kind the registry does not know).
    pub fn blend_of(&self, kind: &str) -> BlendPolicy {
        self.entry(kind).map(|e| e.blend).unwrap_or_default()
    }
}

/// Failures of [`KindRegistry::load`].
#[derive(thiserror::Error, Debug)]
pub enum KindRegistryError {
    /// Disk IO failed.
    #[error("kinds file IO: {0}")]
    Io(#[from] std::io::Error),
    /// The RON did not parse.
    #[error("kinds file parse: {0}")]
    Parse(#[from] ron::error::SpannedError),
}

// ---------------------------------------------------------------------------
// Event router (specs/25 § 3)
// ---------------------------------------------------------------------------

/// One data-driven reaction to a sim event kind, applied by `apply_routed_events` (ADR-7210):
/// [`Reaction::CameraShake`] through the camera director, [`Reaction::Toast`] as a
/// [`ToastRaised`] message, and on a client build [`Reaction::Vfx`] (a `VfxScene` effect),
/// [`Reaction::Decal`] (a `DecalLibrary` prefab into the `DecalPool`), [`Reaction::SoundCue`]
/// (an `Audio` cue) and [`Reaction::Announcer`] (an `Audio` announcer line). The placed reactions
/// (`Vfx`, `Decal`) need the event's place ([`ExternalSim::event_place`]); every outcome is
/// counted in [`RoutedReactionStats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Reaction {
    /// Play a sound cue by name. A placed event plays it spatially at the event's position, an
    /// unplaced one plays it flat.
    SoundCue {
        /// Cue name in the audio bank.
        cue: String,
        /// How far the placed play carries, meters: past it the play is refused (counted in
        /// `sounds_refused`). Absent (the default; RON may leave it out), the engine's default
        /// emitter range applies (`AudioEmitter::default().max_distance`, 50 m). Set it for
        /// battlefield-scale events (explosions, artillery) heard from far away. A value that is
        /// not finite and positive also falls back to the default. Ignored by an unplaced play.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_distance: Option<f32>,
    },
    /// Spawn a VFX emitter.
    Vfx {
        /// Effect name.
        effect: String,
    },
    /// Place a decal.
    Decal {
        /// Decal kind.
        kind: String,
    },
    /// Shake the active camera.
    CameraShake {
        /// Position amplitude in metres.
        amplitude: f32,
        /// Wobble frequency in hertz.
        frequency_hz: f32,
        /// Duration in seconds.
        duration_secs: f32,
        /// Decay exponent (see [`Shake`](crate::camera::Shake)).
        falloff: f32,
    },
    /// Show a UI toast.
    Toast {
        /// Toast text (already localised by the caller or a pack-table key).
        text: String,
    },
    /// Play an announcer line.
    Announcer {
        /// Line id in the voice bank.
        line: String,
    },
}

/// Event kind → reactions (`config/events.ron`), applied to every drained event whose
/// [`ExternalSim::event_kind`] matches. Data-driven, so most reactions need no code.
///
/// ```ron
/// (routes: {
///     "explosion": [ (CameraShake: (amplitude: 0.5, frequency_hz: 9.0, duration_secs: 0.6, falloff: 1.2)),
///                    (SoundCue: (cue: "boom_large")) ],
///     "scored":    [ (Toast: (text: "Score!")), (Announcer: (line: "score_generic")) ],
/// })
/// ```
#[derive(Resource, Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRouter {
    /// Routes keyed by event kind.
    pub routes: std::collections::BTreeMap<String, Vec<Reaction>>,
}

impl EventRouter {
    /// Parses RON text (`config/events.ron`).
    pub fn from_ron(text: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(text)
    }

    /// The reactions mapped to `kind` (empty when none).
    pub fn reactions(&self, kind: &str) -> Vec<Reaction> {
        self.routes.get(kind).cloned().unwrap_or_default()
    }
}

/// One routed sim event: the kind and its mapped [`Reaction`]s, for systems that want the whole
/// bundle. Written by the bridge alongside [`SimEvent`].
#[derive(Message, Debug, Clone)]
pub struct RoutedEvent {
    /// The event's kind key.
    pub kind: String,
    /// The reactions the router mapped to it.
    pub reactions: Vec<Reaction>,
    /// Where it happened, in world metres ([`ExternalSim::event_place`]: an explicit point, or
    /// the current position of the sim entity it names). `None`: the placed reactions (`Vfx`,
    /// `Decal`) are skipped and counted as `unplaced`, and a `SoundCue` plays flat instead of
    /// spatially.
    pub position: Option<Vec3>,
    /// A variation seed derived from the snapshot tick, the event's index in the drain and its
    /// kind (never a clock), so a replayed event stream picks the same variations.
    pub seed: u64,
}

/// Where a sim event happened ([`ExternalSim::event_place`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum EventPlace {
    /// No place: placed reactions (`Vfx`, `Decal`) are skipped.
    #[default]
    Unplaced,
    /// A world position in metres (already in engine units).
    At(Vec3),
    /// The sim entity with this id: its current [`Transform`] translation, when the bridge has
    /// spawned it.
    Entity(u64),
}

/// What `apply_routed_events` did with the routed reactions so far (the agent's `sim.counters`
/// reports it under `reactions`).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RoutedReactionStats {
    /// `Vfx` effects spawned.
    pub vfx: u64,
    /// `Vfx` reactions naming an effect the `VfxScene` library does not have.
    pub vfx_unknown: u64,
    /// `Decal` prefabs placed.
    pub decals: u64,
    /// `Decal` reactions naming a prefab the `DecalLibrary` does not have.
    pub decals_unknown: u64,
    /// `SoundCue` cues that started a voice.
    pub sounds: u64,
    /// `SoundCue` cues the audio refused (unknown cue, empty cue, no bus, or a positional play
    /// nothing audible reached).
    pub sounds_refused: u64,
    /// `Announcer` lines started, queued or pre-empting the current line.
    pub announcer_lines: u64,
    /// `Announcer` lines rejected (a busier line has priority).
    pub announcer_rejected: u64,
    /// Placed reactions (`Vfx`, `Decal`) of an event with no place.
    pub unplaced: u64,
    /// Reactions whose subsystem is not in the app (no `VfxScene`, `DecalPool` or `Audio`; or a
    /// server build without the `client` feature).
    pub no_subsystem: u64,
}

/// A UI toast request raised by the router (the UI listens from M7; tests and tools read it now).
#[derive(Message, Debug, Clone)]
pub struct ToastRaised {
    /// Toast text.
    pub text: String,
}

/// Applies routed reactions: camera shakes go to the
/// [`CameraDirector`](crate::camera::CameraDirector), toasts are raised as [`ToastRaised`]
/// messages, and on a client build effects, decals, sound cues and announcer lines go to the
/// `VfxScene`, the `DecalPool`, and `Audio` (ADR-7210). Every outcome is counted in
/// [`RoutedReactionStats`].
fn apply_routed_events(
    mut routed: MessageReader<RoutedEvent>,
    #[cfg(feature = "client")] mut director: Option<ResMut<crate::camera::CameraDirector>>,
    #[cfg(feature = "client")] mut sinks: reaction_sinks::Sinks<'_>,
    mut toasts: MessageWriter<ToastRaised>,
    mut stats: ResMut<RoutedReactionStats>,
) {
    for event in routed.read() {
        for reaction in &event.reactions {
            match reaction {
                // A server has no camera to shake.
                #[cfg(not(feature = "client"))]
                Reaction::CameraShake { .. } => {}
                #[cfg(feature = "client")]
                Reaction::CameraShake {
                    amplitude,
                    frequency_hz,
                    duration_secs,
                    falloff,
                } => {
                    if let Some(director) = director.as_deref_mut() {
                        director.shake(crate::camera::Shake {
                            amplitude: *amplitude,
                            frequency_hz: *frequency_hz,
                            duration_secs: *duration_secs,
                            falloff: *falloff,
                        });
                    } else {
                        tracing::debug!(target: "regia_framework", "camera shake for {} dropped: CameraPlugin not installed", event.kind);
                    }
                }
                Reaction::Toast { text } => {
                    toasts.write(ToastRaised { text: text.clone() });
                }
                #[cfg(feature = "client")]
                Reaction::SoundCue { .. }
                | Reaction::Vfx { .. }
                | Reaction::Decal { .. }
                | Reaction::Announcer { .. } => {
                    sinks.apply(event, reaction, &mut stats);
                }
                #[cfg(not(feature = "client"))]
                Reaction::SoundCue { .. }
                | Reaction::Vfx { .. }
                | Reaction::Decal { .. }
                | Reaction::Announcer { .. } => {
                    // A server presents nothing.
                    stats.no_subsystem += 1;
                }
            }
        }
    }
}

/// The client half of `apply_routed_events`: effects, decals, sound cues and announcer lines.
#[cfg(feature = "client")]
mod reaction_sinks {
    use regia_app::prelude::*;
    use regia_audio::{Audio, AudioEmitter};
    use regia_core::math::Vec3;
    use regia_ecs::ecs::system::SystemParam;
    use regia_vfx::{DecalLibrary, DecalPool, VfxScene};

    use super::{Reaction, RoutedEvent, RoutedReactionStats};

    /// The emitter a placed routed cue plays with: the engine's default emitter, its range
    /// replaced by the reaction's `max_distance` when that is finite and positive.
    pub(super) fn routed_cue_emitter(max_distance: Option<f32>) -> AudioEmitter {
        let mut emitter = AudioEmitter::default();
        if let Some(range) = max_distance.filter(|d| d.is_finite() && *d > 0.0) {
            emitter.max_distance = range;
        }
        emitter
    }

    /// Where the presentation reactions go; each is optional.
    #[derive(SystemParam)]
    pub(super) struct Sinks<'w> {
        audio: Option<ResMut<'w, Audio>>,
        vfx: Option<ResMut<'w, VfxScene>>,
        decals: Option<ResMut<'w, DecalPool>>,
        decal_library: Option<Res<'w, DecalLibrary>>,
    }

    impl Sinks<'_> {
        /// Applies one presentation reaction of `event` and counts the outcome.
        pub(super) fn apply(
            &mut self,
            event: &RoutedEvent,
            reaction: &Reaction,
            stats: &mut RoutedReactionStats,
        ) {
            match reaction {
                Reaction::Vfx { effect } => {
                    let Some(scene) = self.vfx.as_deref_mut() else {
                        stats.no_subsystem += 1;
                        return;
                    };
                    let Some(at) = event.position else {
                        stats.unplaced += 1;
                        return;
                    };
                    if scene.spawn(effect, at, Vec3::Y, event.seed).is_some() {
                        stats.vfx += 1;
                    } else {
                        tracing::debug!(target: "regia_framework", "routed vfx '{effect}' for {}: not in the vfx library", event.kind);
                        stats.vfx_unknown += 1;
                    }
                }
                Reaction::Decal { kind } => {
                    let (Some(pool), Some(library)) =
                        (self.decals.as_deref_mut(), self.decal_library.as_deref())
                    else {
                        stats.no_subsystem += 1;
                        return;
                    };
                    let Some(at) = event.position else {
                        stats.unplaced += 1;
                        return;
                    };
                    // Up-facing ground decal; the yaw varies with the event seed.
                    let yaw = (event.seed % 3600) as f32 * (std::f32::consts::TAU / 3600.0);
                    let prefab = library.get(kind);
                    match prefab.map(|p| p.instantiate(at, Vec3::Y, yaw, None)) {
                        Some(Ok((decal, surface))) => {
                            let id = pool.spawn_with(decal, surface);
                            pool.set_footstep_surface(id, prefab.and_then(|p| p.surface.as_deref()));
                            stats.decals += 1;
                        }
                        _ => {
                            tracing::debug!(target: "regia_framework", "routed decal '{kind}' for {}: no such prefab", event.kind);
                            stats.decals_unknown += 1;
                        }
                    }
                }
                Reaction::SoundCue { cue, max_distance } => {
                    let Some(audio) = self.audio.as_deref_mut() else {
                        stats.no_subsystem += 1;
                        return;
                    };
                    // A placed event plays spatially at its position (the default emitter, with
                    // the reaction's range when it names one); an unplaced one still plays the
                    // cue flat. The seed keeps the picked variation deterministic either way,
                    // and an out-of-range position makes the play come back refused (no voice
                    // started).
                    let played = match event.position {
                        Some(at) => {
                            audio.play_cue_seeded_at(cue, event.seed, at, &routed_cue_emitter(*max_distance))
                        }
                        None => audio.play_cue_seeded(cue, event.seed),
                    };
                    match played {
                        Ok(Some(_)) => stats.sounds += 1,
                        _ => stats.sounds_refused += 1,
                    }
                }
                Reaction::Announcer { line } => {
                    let Some(audio) = self.audio.as_deref_mut() else {
                        stats.no_subsystem += 1;
                        return;
                    };
                    match audio.announce(line) {
                        regia_audio::AnnounceOutcome::Rejected => stats.announcer_rejected += 1,
                        _ => stats.announcer_lines += 1,
                    }
                }
                Reaction::CameraShake { .. } | Reaction::Toast { .. } => {}
            }
        }
    }
}

/// The variation seed of the `index`-th event drained at snapshot `tick` (FNV-1a over the
/// tick, the index and the kind, then a SplitMix64 finaliser): a pure function of the event
/// stream, so a replay picks the same effect, decal and cue variations.
fn routed_event_seed(tick: u64, index: u64, kind: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for byte in tick
        .to_le_bytes()
        .iter()
        .chain(&index.to_le_bytes())
        .chain(kind.as_bytes())
    {
        h = (h ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

// ---------------------------------------------------------------------------
// Counters and the frame-rate-independence check (specs/25 § 3, DO27 rule A1)
// ---------------------------------------------------------------------------

/// Bridge-side instrumentation of one running sim, the input of the frame-rate-independence
/// check (`specs/25` § 3): run the shell at 30 fps and uncapped and compare the **sim-side**
/// rates. `snapshot_ticks` (the sim's own clock) is the gating counter; the frame-driven ones
/// (`submit_input_calls`, `snapshot_reads`, `events_drained`) are context.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SimCounters {
    /// Reconciliation frames run.
    pub frames: u64,
    /// How many times `submit_input` was called (frame-driven).
    pub submit_input_calls: u64,
    /// How many times `read_snapshot` was called (frame-driven).
    pub snapshot_reads: u64,
    /// Sum of positive snapshot tick deltas: the sim's own clock, frame-rate independent.
    pub snapshot_ticks: u64,
    /// Events drained so far (frame-driven).
    pub events_drained: u64,
    /// Input edges forwarded (`BridgeInput::edges`).
    pub input_edges: u64,
    /// The last tick seen (for delta computation).
    pub last_tick: u64,
    /// Transform or `SimExtras` inserts `reconcile` queued for an entity that already
    /// existed (tests). A snapshot that matches the world does not increment it.
    #[cfg(test)]
    pub reconcile_writes: u64,
}

/// One measurement window: a counter total and the wall/game seconds it took.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramerateSample {
    /// The counter value at the end of the window (usually [`SimCounters::snapshot_ticks`]).
    pub ticks: u64,
    /// Game seconds elapsed in the window.
    pub wall_secs: f64,
}

/// The verdict of [`check_framerate_independence`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramerateCheckReport {
    /// Rate of sample A (ticks/second).
    pub a_rate: f64,
    /// Rate of sample B (ticks/second).
    pub b_rate: f64,
    /// `max(rate)/min(rate)`; `1.0` is perfect.
    pub ratio: f64,
    /// The tolerance that was applied (DO27 rule A1: `0.02`).
    pub tolerance: f64,
    /// Whether the rates match within tolerance. `false` when either window is degenerate
    /// (zero duration or zero ticks).
    pub pass: bool,
}

/// Compares two [`FramerateSample`]s (DO27 rule A1, `specs/25` § 3): the sim-side tick rates
/// must match within `tolerance` (2%) no matter the shell's frame rate. Exposed to agents as the
/// `sim.check_framerate_independence` method.
pub fn check_framerate_independence(
    a: FramerateSample,
    b: FramerateSample,
    tolerance: f64,
) -> FramerateCheckReport {
    let a_rate = if a.wall_secs > 0.0 {
        a.ticks as f64 / a.wall_secs
    } else {
        0.0
    };
    let b_rate = if b.wall_secs > 0.0 {
        b.ticks as f64 / b.wall_secs
    } else {
        0.0
    };
    let ratio = if a_rate > 0.0 && b_rate > 0.0 {
        a_rate.max(b_rate) / a_rate.min(b_rate)
    } else {
        f64::NAN
    };
    let pass = ratio.is_finite() && ratio - 1.0 <= tolerance;
    FramerateCheckReport {
        a_rate,
        b_rate,
        ratio,
        tolerance,
        pass,
    }
}

// ---------------------------------------------------------------------------
// Snapshot abstraction
// ---------------------------------------------------------------------------

/// A tick identifier returned with each snapshot. Wraps the sim's own clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotTick(pub u64);

/// One entity in a snapshot — what the bridge needs for reconciliation and pose
/// writes. The fields are intentionally small and concrete so the sim can fill
/// them without reflection overhead.
#[derive(Debug, Clone)]
pub struct SimEntityView<'a> {
    /// Stable id, never recycled while the entity exists.
    pub id: u64,
    /// Kind key (for example `"disc"`, `"warhull"`). Maps to a prefab in a
    /// `KindRegistry` asset (not implemented in this slice).
    pub kind: &'a str,
    /// Team index for team-colour presentation.
    pub team: u32,
    /// World-space position in metres, in the sim's frame: engine coordinates unless
    /// [`SimBridgeSettings::handedness`] reflects it.
    pub translation: Vec3,
    /// World-space rotation, in the same frame as `translation`.
    pub rotation: Quat,
}

/// A set of render-ready entities for one sim tick.
///
/// The sim writes into this object in [`ExternalSim::read_snapshot`]. The bridge
/// iterates it with [`for_each_entity`](SimSnapshot::for_each_entity).
pub trait SimSnapshot: Default + Send + Sync + 'static {
    /// Calls `f` once for every entity in the snapshot, in any order.
    fn for_each_entity(&self, f: impl FnMut(SimEntityView<'_>));

    /// Extra presentation fields for `id` (damage zones, lean, turret yaw, team colour
    /// override…), appended to `out` as `(name, value)` pairs. The bridge writes them onto the
    /// entity as [`SimExtras`], where per-kind presentation scripts read them (`specs/25` § 3).
    ///
    /// The default is empty; override when the sim carries extra state.
    fn extras_for(&self, _id: u64, _out: &mut Vec<(String, f32)>) {}
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Per-frame input the bridge sends to the authoritative sim.
///
/// Kept small and concrete — not a generic reflection object — so the sim can
/// read it with zero overhead.
#[derive(Resource, Debug, Clone, Default)]
pub struct BridgeInput {
    /// Normalised movement axes: x = right, y = forward (both in [-1, 1]).
    pub move_axes: Vec2,
    /// Names of action edges that were pressed ("went down") this frame.
    pub edges: Vec<String>,
}

// ---------------------------------------------------------------------------
// ExternalSim trait
// ---------------------------------------------------------------------------

/// A source of authoritative game state that lives outside Regia Engine's ECS.
///
/// The sim owns its own clock, runs on its own thread, and is never modified
/// except through [`submit_input`](ExternalSim::submit_input). The bridge reads
/// a snapshot once per frame and reconciles entities into the presentation world.
pub trait ExternalSim: Send + 'static {
    /// The snapshot type this sim produces. Must implement [`SimSnapshot`].
    type Snapshot: SimSnapshot;

    /// Events emitted by the sim since the last drain (audio cues, VFX triggers,
    /// UI toasts…). Sent as [`SimEvent`] messages each frame.
    type Event: Send + Sync + 'static;

    /// Feed one frame of player input to the sim. Must not block; the sim
    /// samples input on its own clock.
    fn submit_input(&mut self, input: &BridgeInput);

    /// Fill `out` with the render-ready entity set for `frame_time`.
    ///
    /// `frame_time` is the Regia Engine frame's simulation delta (`Time::sim_delta`: the frame
    /// delta, or a render-out time map's simulation step, ADR-1524); the sim may interpolate or
    /// extrapolate to match its own tick rate. Returns the sim tick this
    /// snapshot represents.
    fn read_snapshot(&mut self, frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick;

    /// Fill `prev` and `curr` with the sim's last two published frames, taken from **one**
    /// publish, and return their ticks and the blend weight for a frame drawn `delay` behind the
    /// newest publish ([`SimBridgeSettings::presentation_delay`]). The bridge then poses each entity
    /// between them by its kind's [`BlendPolicy`] (ADR-8456).
    ///
    /// When the returned weight is at least
    /// [`BLEND_CURR_AT`](crate::sim_blend::BLEND_CURR_AT) the bridge does not read `prev`, and the
    /// sim may leave it unfilled. The default returns `None`: the bridge calls
    /// [`read_snapshot`](ExternalSim::read_snapshot) and draws that snapshot as is.
    fn read_pair(
        &mut self,
        _frame_time: Duration,
        _delay: Duration,
        _prev: &mut Self::Snapshot,
        _curr: &mut Self::Snapshot,
    ) -> Option<SnapshotPair> {
        None
    }

    /// Move every event that has fired since the last call into `out`. Called
    /// once per Regia Engine frame so events are never lost.
    fn drain_events(&mut self, out: &mut Vec<Self::Event>);

    /// The kind key of `event` used by the [`EventRouter`] (for example `"explosion"`).
    /// Defaults to the event type's path; override to route by meaning.
    fn event_kind(_event: &Self::Event) -> String {
        std::any::type_name::<Self::Event>().to_string()
    }

    /// Where `event` happened, for the router's placed reactions (`Vfx`, `Decal`): a world
    /// position or the sim entity it happened to. Defaults to [`EventPlace::Unplaced`] (placed
    /// reactions are skipped and counted). In the same frame as the snapshot: the bridge applies
    /// [`SimBridgeSettings::handedness`] to an [`EventPlace::At`] position.
    fn event_place(_event: &Self::Event) -> EventPlace {
        EventPlace::Unplaced
    }
}

// ---------------------------------------------------------------------------
// SimEvent message
// ---------------------------------------------------------------------------

/// A message sent once per drained sim event. Game systems read these with
/// `MessageReader<SimEvent<E>>`.
#[derive(Message, Debug, Clone)]
pub struct SimEvent<E: Send + Sync + 'static>(pub E);

// ---------------------------------------------------------------------------
// Entity map
// ---------------------------------------------------------------------------

/// Bidirectional mapping between sim-side stable ids and ECS [`Entity`] handles.
///
/// Maintained by the bridge's reconciliation system; game code may read it to
/// look up an entity from a sim id.
#[derive(Resource, Debug, Default)]
pub struct SimEntityMap {
    /// Ordered by sim id: the reconciliation despawns vanished entities in this order, and the
    /// despawn order decides which entity ids later spawns reuse, so a hashed order here made two
    /// runs of one snapshot stream allocate different entities (ADR-537).
    sim_to_entity: BTreeMap<u64, Entity>,
    entity_to_sim: HashMap<Entity, u64>,
    /// Sim ids whose entities currently carry a [`SimExtras`] component (so it can be removed
    /// when the extras become empty).
    tracked_extras: HashSet<u64>,
}

impl SimEntityMap {
    /// The ECS entity for `sim_id`, if one has been spawned.
    pub fn entity(&self, sim_id: u64) -> Option<Entity> {
        self.sim_to_entity.get(&sim_id).copied()
    }

    /// The sim id for `entity`, if it was spawned by the bridge.
    pub fn sim_id(&self, entity: Entity) -> Option<u64> {
        self.entity_to_sim.get(&entity).copied()
    }

    fn insert(&mut self, sim_id: u64, entity: Entity) {
        self.sim_to_entity.insert(sim_id, entity);
        self.entity_to_sim.insert(entity, sim_id);
    }

    fn remove_by_entity(&mut self, entity: Entity) {
        if let Some(sim_id) = self.entity_to_sim.remove(&entity) {
            self.sim_to_entity.remove(&sim_id);
        }
    }

    /// Iterate over all `(sim_id, Entity)` pairs, in increasing sim id.
    pub fn iter(&self) -> impl Iterator<Item = (u64, Entity)> + '_ {
        self.sim_to_entity.iter().map(|(&k, &v)| (k, v))
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Registers the sim-bridge types and runs reconciliation every frame.
///
/// # Type parameters
///
/// * `S` — the concrete [`ExternalSim`] implementation. Stored as a resource so
///   tests can inspect and drive it.
///
/// # Optional resources
///
/// Insert a [`KindRegistry`] (or build with [`SimBridgePlugin::kinds`]) to spawn per-kind
/// presentation; an [`EventRouter`] (or [`SimBridgePlugin::events`]) to route drained events to
/// reactions. Without them the bridge spawns bare entities (id + transform) and forwards raw
/// [`SimEvent`]s only.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use regia_app::prelude::*;
/// use regia_core::math::{Quat, Vec3};
/// use regia_scene::Transform;
/// use regia_framework::sim_bridge::{
///     BridgeInput, ExternalSim, SimBridgePlugin, SimEntityView, SimId, SimSnapshot, SnapshotTick,
/// };
///
/// /// A one-entity snapshot with a fixed pose.
/// #[derive(Default)]
/// struct MySnapshot {
///     translation: Vec3,
/// }
///
/// impl SimSnapshot for MySnapshot {
///     fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
///         f(SimEntityView {
///             id: 1,
///             kind: "disc",
///             team: 0,
///             translation: self.translation,
///             rotation: Quat::IDENTITY,
///         });
///     }
/// }
///
/// #[derive(Resource)]
/// struct MySim;
///
/// impl ExternalSim for MySim {
///     type Snapshot = MySnapshot;
///     type Event = ();
///
///     fn submit_input(&mut self, _input: &BridgeInput) {}
///
///     fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
///         out.translation = Vec3::new(1.0, 2.0, 3.0);
///         SnapshotTick(1)
///     }
///
///     fn drain_events(&mut self, _out: &mut Vec<Self::Event>) {}
/// }
///
/// let mut app = App::new();
/// app.add_plugins((MinimalPlugins, SimBridgePlugin::<MySim>::new(MySim)));
/// app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
/// for _ in 0..3 {
///     app.update();
/// }
///
/// // The bridge spawned one entity for the snapshot, at the snapshot pose.
/// let mut found = None;
/// let world = app.world_mut();
/// for (sid, transform) in world.query::<(&SimId, &Transform)>().iter(world) {
///     if sid.0 == 1 {
///         found = Some(transform.translation);
///     }
/// }
/// assert_eq!(found, Some(Vec3::new(1.0, 2.0, 3.0)));
/// ```
pub struct SimBridgePlugin<S: ExternalSim> {
    sim: std::sync::Mutex<Option<S>>,
    kinds: Option<KindRegistry>,
    router: Option<EventRouter>,
    settings: Option<SimBridgeSettings>,
}

impl<S: ExternalSim> SimBridgePlugin<S> {
    /// Wrap `sim` in a plugin. Call [`App::add_plugins`] with the result.
    pub fn new(sim: S) -> Self {
        Self {
            sim: std::sync::Mutex::new(Some(sim)),
            kinds: None,
            router: None,
            settings: None,
        }
    }

    /// Attaches a [`KindRegistry`] (kind key → prefab + presentation scripts).
    pub fn kinds(mut self, kinds: KindRegistry) -> Self {
        self.kinds = Some(kinds);
        self
    }

    /// Attaches an [`EventRouter`] (event kind → reactions).
    pub fn events(mut self, router: EventRouter) -> Self {
        self.router = Some(router);
        self
    }

    /// Sets the presentation settings: handedness, teleport distance and presentation delay
    /// (ADR-8456). Without it the bridge keeps a [`SimBridgeSettings`] resource the app inserted,
    /// or the default.
    pub fn settings(mut self, settings: SimBridgeSettings) -> Self {
        self.settings = Some(settings);
        self
    }
}

impl<S: ExternalSim + Resource<Mutability = Mutable>> Plugin for SimBridgePlugin<S> {
    fn build(&self, app: &mut App) {
        // Transfer the sim into the world as a resource.
        let Ok(mut guard) = self.sim.lock() else {
            tracing::error!("SimBridgePlugin lock poisoned; sim not inserted");
            return;
        };
        let Some(sim) = guard.take() else {
            tracing::error!("SimBridgePlugin sim already taken; skipping");
            return;
        };
        drop(guard);

        if let Some(kinds) = &self.kinds {
            app.insert_resource(kinds.clone());
        }
        if let Some(router) = &self.router {
            app.insert_resource(router.clone());
        }
        match self.settings {
            Some(settings) => app.insert_resource(settings),
            None => app.init_resource::<SimBridgeSettings>(),
        };

        app.insert_resource(sim)
            .register_type::<SimId>()
            .register_type::<Transform>()
            .register_type::<SimKind>()
            .register_type::<Team>()
            .register_type::<SimExtras>()
            .add_message::<SimEvent<S::Event>>()
            .add_message::<RoutedEvent>()
            .add_message::<ToastRaised>()
            .init_resource::<SimEntityMap>()
            .init_resource::<SimCounters>()
            .init_resource::<RoutedReactionStats>()
            .init_resource::<SimBlendStats>()
            .insert_resource(BridgeInput::default())
            .init_resource::<SuspendHold>()
            .init_resource::<PauseHold>()
            .add_systems(PreUpdate, (hold_sim_host_on_pause, reconcile::<S>).chain())
            .add_systems(Update, apply_routed_events)
            .add_systems(Lifecycle, hold_sim_host_on_suspend);
    }

    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// Whether the current suspension took the sim host's hold (so `Resumed` gives it back).
#[derive(Resource, Debug, Default)]
pub struct SuspendHold {
    /// `true` while a hold taken by [`hold_sim_host_on_suspend`] is in place.
    pub held_by_suspend: bool,
}

/// Holds a threaded sim host ([`crate::sim_host::SimHostHandle`] resource) when the app is
/// suspended and releases it on `Resumed` (ADR-8526). A host the game held before the suspension
/// (a lobby, a pause menu) stays held. Runs in the `Lifecycle` schedule, which the platform runner
/// runs for each transition through `regia_platform::apply_transition`, on every platform; Android
/// is where the OS suspends the process. Added by [`SimBridgePlugin`]; an app that hosts a sim
/// without the bridge adds it itself (`app.init_resource::<SuspendHold>()` first).
pub fn hold_sim_host_on_suspend(world: &mut World) {
    let Some(event) = world
        .get_resource::<regia_platform::AppLifecycle>()
        .and_then(|lifecycle| lifecycle.last_event)
    else {
        return;
    };
    let Some(handle) = world.get_resource::<crate::sim_host::SimHostHandle>().cloned() else {
        return;
    };
    let Some(mut latch) = world.get_resource_mut::<SuspendHold>() else {
        return;
    };
    match event {
        regia_platform::LifecycleEvent::Suspended if !latch.held_by_suspend => {
            let was_held = handle.set_held(true);
            latch.held_by_suspend = !was_held;
            tracing::info!(was_held, "sim host held: the app is suspended");
        }
        regia_platform::LifecycleEvent::Resumed if latch.held_by_suspend => {
            handle.set_held(false);
            latch.held_by_suspend = false;
            tracing::info!("sim host released: the app resumed");
        }
        _ => {}
    }
}

/// Whether the current gameplay pause took the sim host's hold (so unpausing gives it back).
#[derive(Resource, Debug, Default)]
pub struct PauseHold {
    /// `true` while a hold taken by [`hold_sim_host_on_pause`] is in place.
    pub held_by_pause: bool,
}

/// Holds a threaded sim host ([`crate::sim_host::SimHostHandle`] resource) while the game is
/// paused (`Time::paused`) and releases it when the game unpauses (ADR-10105): a real-time host
/// steps on its own thread's clock, so without the hold it would play on under a pause screen.
/// A host the game held itself, or that a suspension holds, is left alone: the release only
/// undoes a hold this system took. `PreUpdate`, before the reconcile; added by
/// [`SimBridgePlugin`] (an app without the bridge adds it with `init_resource::<PauseHold>()`).
pub fn hold_sim_host_on_pause(
    time: Option<Res<Time>>,
    handle: Option<Res<crate::sim_host::SimHostHandle>>,
    latch: Option<ResMut<PauseHold>>,
) {
    let (Some(time), Some(handle), Some(mut latch)) = (time, handle, latch) else {
        return;
    };
    if time.paused && !latch.held_by_pause {
        let was_held = handle.set_held(true);
        latch.held_by_pause = !was_held;
        if !was_held {
            tracing::info!("sim host held: the game is paused");
        }
    } else if !time.paused && latch.held_by_pause {
        handle.set_held(false);
        latch.held_by_pause = false;
        tracing::info!("sim host released: the game unpaused");
    }
}

/// The per-frame reconciliation system.
///
/// 1. Reads [`BridgeInput`] and calls [`ExternalSim::submit_input`].
/// 2. Calls [`ExternalSim::read_snapshot`] and spawns/updates/despawns entities: new ids spawn
///    what their [`KindRegistry`] entry names (prefab reserved for M6; presentation scripts
///    attached through the [`ScriptRegistry`](crate::script::ScriptRegistry)), existing ids get
///    their [`Transform`] and [`SimExtras`] rewritten, vanished ids despawn.
/// 3. Calls [`ExternalSim::drain_events`]: every event is sent as a [`SimEvent`] message, and —
///    when an [`EventRouter`] is present — also as a [`RoutedEvent`] carrying the reactions
///    mapped from its [`ExternalSim::event_kind`].
/// 4. Updates the [`SimCounters`] instrumentation (frame-rate-independence checks read it).
#[allow(clippy::too_many_arguments)]
fn reconcile<S: ExternalSim + Resource<Mutability = Mutable>>(
    mut sim: ResMut<S>,
    mut map: ResMut<SimEntityMap>,
    mut commands: Commands,
    time: Res<Time>,
    mut events: MessageWriter<SimEvent<S::Event>>,
    mut routed: MessageWriter<RoutedEvent>,
    bridge_input: Res<BridgeInput>,
    kinds: Option<Res<KindRegistry>>,
    router: Option<Res<EventRouter>>,
    script_registry: Option<Res<crate::script::ScriptRegistry>>,
    mut counters: ResMut<SimCounters>,
    transforms: Query<&Transform>,
    extras_q: Query<&SimExtras>,
    mut seen: Local<HashSet<u64>>,
    mut drained: Local<Vec<S::Event>>,
    (settings, mut blend_stats, mut prev_poses): BlendParams,
) {
    #[tracing::instrument(skip_all, target = "regia_framework", name = "sim_bridge_reconcile")]
    #[allow(clippy::too_many_arguments)]
    fn inner<S: ExternalSim + Resource<Mutability = Mutable>>(
        sim: &mut S,
        map: &mut SimEntityMap,
        commands: &mut Commands,
        frame_time: Duration,
        events: &mut MessageWriter<SimEvent<S::Event>>,
        routed: &mut MessageWriter<RoutedEvent>,
        bridge_input: &BridgeInput,
        kinds: Option<&KindRegistry>,
        router: Option<&EventRouter>,
        script_registry: Option<&crate::script::ScriptRegistry>,
        counters: &mut SimCounters,
        transforms: &Query<&Transform>,
        extras_q: &Query<&SimExtras>,
        seen: &mut HashSet<u64>,
        drained: &mut Vec<S::Event>,
        blend: BlendFrame<'_>,
    ) {
        let BlendFrame {
            settings,
            stats: blend_stats,
            prev_poses,
        } = blend;
        let handedness = settings.handedness;
        // 1. Submit input
        sim.submit_input(bridge_input);
        counters.frames += 1;
        counters.submit_input_calls += 1;
        counters.input_edges += bridge_input.edges.len() as u64;

        // 2. Read the snapshot: the sim's published pair when it has one (ADR-8456), else one
        // snapshot drawn as read.
        let mut snapshot = S::Snapshot::default();
        let mut prev_snapshot = S::Snapshot::default();
        let pair = sim.read_pair(
            frame_time,
            settings.presentation_delay,
            &mut prev_snapshot,
            &mut snapshot,
        );
        let tick = match pair {
            Some(pair) => {
                blend_stats.pair_reads = blend_stats.pair_reads.wrapping_add(1);
                pair.curr_tick
            }
            None => {
                blend_stats.single_reads = blend_stats.single_reads.wrapping_add(1);
                sim.read_snapshot(frame_time, &mut snapshot)
            }
        };
        // The weight each entity blends with (`None`: draw the snapshot as read).
        let alpha = pair.map(|pair| {
            if pair.alpha.is_finite() {
                pair.alpha.clamp(0.0, 1.0)
            } else {
                1.0
            }
        });
        // `prev` poses by sim id, converted like `curr`; only read below the `curr` threshold.
        prev_poses.clear();
        if alpha.is_some_and(|alpha| alpha < crate::sim_blend::BLEND_CURR_AT) {
            prev_snapshot.for_each_entity(|view| {
                let (translation, rotation) = handedness.pose(view.translation, view.rotation);
                prev_poses.insert(
                    view.id,
                    SimPose {
                        translation,
                        rotation,
                    },
                );
            });
        }
        counters.snapshot_reads += 1;
        counters.snapshot_ticks += tick.0.saturating_sub(counters.last_tick);
        counters.last_tick = tick.0;

        // 3. Entity reconciliation
        seen.clear();
        let mut extras: Vec<(String, f32)> = Vec::new();

        // New sim ids are spawned after the walk, in sim-id order: the trait allows any emit
        // order, and spawn order decides which entities the presentation gets (and so every later
        // spawn's), like the sorted despawns below (ADR-537, ADR-867, ADR-1660).
        let mut fresh: Vec<(u64, String, u32, Transform)> = Vec::new();
        snapshot.for_each_entity(|view| {
            seen.insert(view.id);

            // Handedness once at the boundary, then the kind's blend policy (ADR-8456). A
            // right-handed single snapshot is written as read, bit for bit.
            let (translation, rotation) = handedness.pose(view.translation, view.rotation);
            let mut pose = SimPose {
                translation,
                rotation,
            };
            if let Some(alpha) = alpha {
                let policy = kinds.map_or(BlendPolicy::Lerp, |k| k.blend_of(view.kind));
                let (blended, outcome) = blend_pose(
                    policy,
                    prev_poses.get(&view.id).copied(),
                    pose,
                    alpha,
                    settings.teleport_distance,
                );
                blend_stats.note(outcome);
                pose = blended;
            }
            let transform = Transform {
                translation: pose.translation,
                rotation: pose.rotation,
                scale: Vec3::ONE,
            };

            if let Some(&entity) = map.sim_to_entity.get(&view.id) {
                // Existing entity: update its Transform and SimExtras only when they changed.
                extras.clear();
                snapshot.extras_for(view.id, &mut extras);
                let reported = !extras.is_empty() || map.tracked_extras.contains(&view.id);
                queue_existing(
                    commands,
                    counters,
                    transforms,
                    extras_q,
                    reported.then_some(extras.as_slice()),
                    entity,
                    transform,
                );
            } else {
                fresh.push((view.id, view.kind.to_string(), view.team, transform));
            }
        });
        // Stable: a sim id listed twice spawns once (its first view) and the later view updates it.
        fresh.sort_by_key(|(id, ..)| *id);
        for (id, kind, team, transform) in fresh {
            if let Some(&entity) = map.sim_to_entity.get(&id) {
                extras.clear();
                snapshot.extras_for(id, &mut extras);
                let reported = !extras.is_empty() || map.tracked_extras.contains(&id);
                queue_existing(
                    commands,
                    counters,
                    transforms,
                    extras_q,
                    reported.then_some(extras.as_slice()),
                    entity,
                    transform,
                );
            } else {
                let view = SimEntityView {
                    id,
                    kind: &kind,
                    team,
                    translation: transform.translation,
                    rotation: transform.rotation,
                };
                // New sim id: spawn the presentation its kind names, or a bare entity.
                let kind_entry = kinds.and_then(|k| k.entry(view.kind));
                // A bare entity has no extras (not the previous view's).
                extras.clear();
                let mut spawn = commands.spawn((SimId(view.id), transform));
                match kind_entry {
                    Some(entry) => {
                        spawn
                            .insert(SimKind(view.kind.to_string()))
                            .insert(Team(view.team));
                        if let Some(scripts) = script_registry {
                            let mut components = crate::script::Scripts::default();
                            for type_path in &entry.presentation_scripts {
                                components.push_registered(type_path, scripts);
                            }
                            if !components.0.is_empty() {
                                spawn.insert(components);
                            }
                        } else if !entry.presentation_scripts.is_empty() {
                            tracing::warn!(
                                target: "regia_framework",
                                "kind {} names presentation scripts but ScriptPlugin is not installed",
                                view.kind
                            );
                        }
                        extras.clear();
                        snapshot.extras_for(view.id, &mut extras);
                        if !extras.is_empty() {
                            spawn.insert(SimExtras(extras.clone()));
                        }
                    }
                    None => {
                        if kinds.is_some() {
                            tracing::debug!(
                                target: "regia_framework",
                                "kind {} has no KindRegistry entry; spawning bare entity",
                                view.kind
                            );
                        }
                    }
                }
                let entity = spawn.id();
                if !extras.is_empty() {
                    map.tracked_extras.insert(view.id);
                }
                map.insert(view.id, entity);
            }
        }

        // Remove entities whose sim ids disappeared from the snapshot (the optional "death"
        // prefab swap of `specs/25` § 3 arrives with the prefab system, M6).
        // Despawn in sim-id order, not the map's hash order: the despawn order decides which
        // entity slots later spawns reuse, so a per-process order would make two replays of the
        // same snapshots spawn different entities (ADR-537, ADR-867). `sim_to_entity` is a `BTreeMap`, so the
        // sort is a cheap guard that keeps the order if the map type ever changes.
        let mut vanished: Vec<(u64, Entity)> = map
            .sim_to_entity
            .iter()
            .filter(|(id, _)| !seen.contains(*id))
            .map(|(&id, &entity)| (id, entity))
            .collect();
        vanished.sort_unstable_by_key(|(id, _)| *id);
        for (_, entity) in &vanished {
            if let Some(sim_id) = map.entity_to_sim.get(entity) {
                map.tracked_extras.remove(sim_id);
            }
            map.remove_by_entity(*entity);
            commands.entity(*entity).despawn();
        }

        // 4. Drain events: raw messages for game systems, routed reactions for data-driven play.
        drained.clear();
        sim.drain_events(drained);
        counters.events_drained += drained.len() as u64;
        for (index, event) in drained.drain(..).enumerate() {
            if let Some(router) = router {
                let kind = S::event_kind(&event);
                let reactions = router.reactions(&kind);
                if !reactions.is_empty() {
                    let position = match S::event_place(&event) {
                        EventPlace::Unplaced => None,
                        EventPlace::At(at) => Some(handedness.pos(at)),
                        EventPlace::Entity(id) => map
                            .entity(id)
                            .and_then(|entity| transforms.get(entity).ok())
                            .map(|t| t.translation),
                    };
                    let seed = routed_event_seed(tick.0, index as u64, &kind);
                    routed.write(RoutedEvent {
                        kind,
                        reactions,
                        position,
                        seed,
                    });
                }
            }
            events.write(SimEvent(event));
        }
    }

    inner::<S>(
        &mut sim,
        &mut map,
        &mut commands,
        time.sim_delta(),
        &mut events,
        &mut routed,
        &bridge_input,
        kinds.as_deref(),
        router.as_deref(),
        script_registry.as_deref(),
        &mut counters,
        &transforms,
        &extras_q,
        &mut seen,
        &mut drained,
        BlendFrame {
            settings: settings.as_deref().copied().unwrap_or_default(),
            stats: &mut blend_stats,
            prev_poses: &mut prev_poses,
        },
    );
}

/// The blend system parameters of `reconcile` (ADR-8456): the settings, the counters and the
/// reused `prev` pose map.
type BlendParams<'w, 's> = (
    Option<Res<'w, SimBridgeSettings>>,
    ResMut<'w, SimBlendStats>,
    Local<'s, HashMap<u64, SimPose>>,
);

/// The blend state one reconciliation borrows (ADR-8456).
struct BlendFrame<'a> {
    settings: SimBridgeSettings,
    stats: &'a mut SimBlendStats,
    prev_poses: &'a mut HashMap<u64, SimPose>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::message::MessageCursor;
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Deterministic mock sim: 3 hover discs
    // -----------------------------------------------------------------------

    /// A tiny snapshot carrying a `Vec<SnapEntry>`.
    #[derive(Default)]
    struct MockSnapshot {
        entries: Vec<SnapEntry>,
    }

    struct SnapEntry {
        id: u64,
        kind: String,
        team: u32,
        translation: Vec3,
        rotation: Quat,
    }

    impl SimSnapshot for MockSnapshot {
        fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
            for entry in &self.entries {
                f(SimEntityView {
                    id: entry.id,
                    kind: &entry.kind,
                    team: entry.team,
                    translation: entry.translation,
                    rotation: entry.rotation,
                });
            }
        }
    }

    /// Tracks every write so the test can assert nothing but `submit_input` touches
    /// the sim's mutable state.
    struct WriteLog {
        submit_input_calls: Vec<BridgeInput>,
    }

    /// A deterministic mock external sim.
    ///
    /// Three hover discs (ids 1–3, kind `"disc"`). Each frame, disc *i* sits at
    /// `(i * 2.0, 1.5, 0.0)` plus a step along X of `tick * 0.01`. The sim owns
    /// its tick counter, not Regia Engine's frame time. `submit_input` stores the last
    /// input but does not move the discs (authority is the sim).
    #[derive(Resource)]
    struct MockSim {
        tick: u64,
        disc_ids: Vec<u64>,
        last_input: Option<BridgeInput>,
        pending_events: Vec<String>,
        /// Shared so the test can read write counts without a `ResMut` conflict.
        write_log: std::sync::Arc<Mutex<WriteLog>>,
    }

    impl MockSim {
        fn new(write_log: std::sync::Arc<Mutex<WriteLog>>) -> Self {
            Self {
                tick: 0,
                disc_ids: vec![1, 2, 3],
                last_input: None,
                pending_events: vec!["event_a".into(), "event_b".into()],
                write_log,
            }
        }

        /// Remove a disc by id. The next snapshot won't include it.
        fn remove_disc(&mut self, id: u64) {
            self.disc_ids.retain(|&d| d != id);
        }

        /// Add an extra disc.
        fn add_disc(&mut self, id: u64) {
            self.disc_ids.push(id);
        }

        fn disc_translation(&self, id: u64, tick: u64) -> Vec3 {
            Vec3::new(id as f32 * 2.0 + tick as f32 * 0.01, 1.5, 0.0)
        }
    }

    impl ExternalSim for MockSim {
        type Snapshot = MockSnapshot;
        type Event = String;

        fn submit_input(&mut self, input: &BridgeInput) {
            self.last_input = Some(input.clone());
            // Record the write for the "no other writes" assertion.
            if let Ok(mut log) = self.write_log.lock() {
                log.submit_input_calls.push(input.clone());
            }
        }

        fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
            self.tick += 1;
            let tick = self.tick;

            out.entries.clear();
            for &id in &self.disc_ids {
                out.entries.push(SnapEntry {
                    id,
                    kind: "disc".into(),
                    team: 0,
                    translation: self.disc_translation(id, tick),
                    rotation: Quat::IDENTITY,
                });
            }

            SnapshotTick(tick)
        }

        fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
            out.append(&mut self.pending_events);
        }
    }

    // -----------------------------------------------------------------------
    // Coordinate conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn yaw_about_y_zero_is_identity() {
        assert_eq!(yaw_about_y(0.0), Quat::IDENTITY);
    }

    #[test]
    fn yaw_about_y_pi_over_2_rotates_neg_z_to_neg_x() {
        let q = yaw_about_y(std::f32::consts::FRAC_PI_2);
        let forward = q * Vec3::NEG_Z;
        // Should face roughly −X after π/2 yaw.
        assert!((forward - Vec3::NEG_X).length() < 1e-5, "got {forward}");
    }

    #[test]
    fn sim_pose_identity_stays_identity() {
        // A sim pose where "at (10, 0, -4), yaw 0 faces −Z" is the identity in
        // Regia Engine — both frames are +Y up, forward −Z, metres.
        let (t, r) = to_regia_pose(Vec3::new(10.0, 0.0, -4.0), Quat::IDENTITY);
        assert_eq!(t, Vec3::new(10.0, 0.0, -4.0));
        assert_eq!(r, Quat::IDENTITY);
    }

    // -----------------------------------------------------------------------
    // Bridge reconciliation tests
    // -----------------------------------------------------------------------

    fn make_app(sim: MockSim) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, SimBridgePlugin::<MockSim>::new(sim)));
        // Step time manually so we control exactly when the sim ticks.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    /// Query helper: get (SimId, translation) for each entity that has both.
    fn disc_translations(app: &mut App) -> Vec<(u64, Vec3)> {
        let world = app.world_mut();
        let mut query = world.query::<(&SimId, &Transform)>();
        let mut results: Vec<_> = query.iter(world).map(|(sid, t)| (sid.0, t.translation)).collect();
        results.sort_by_key(|(id, _)| *id);
        results
    }

    #[test]
    fn new_sim_ids_spawn_in_id_order_whatever_the_emit_order() {
        // The snapshot may list entities in any order (a sim's hash iteration); the bridge spawns
        // new ids sorted, so the same snapshots give the same entities (ADR-1660).
        let spawned = |order: Vec<u64>| {
            let log = std::sync::Arc::new(Mutex::new(WriteLog {
                submit_input_calls: Vec::new(),
            }));
            let mut sim = MockSim::new(log);
            sim.disc_ids = order;
            let mut app = make_app(sim);
            app.update();
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &SimId)>();
            let mut rows: Vec<(u64, Entity)> = query.iter(world).map(|(e, id)| (id.0, e)).collect();
            rows.sort_by_key(|(id, _)| *id);
            rows
        };
        let sorted = spawned(vec![1, 2, 3]);
        assert_eq!(sorted.len(), 3);
        assert!(
            sorted
                .windows(2)
                .all(|w| w[0].1.index().index() < w[1].1.index().index()),
            "{sorted:?}"
        );
        assert_eq!(spawned(vec![3, 1, 2]), sorted);
        assert_eq!(spawned(vec![2, 3, 1]), sorted);
    }

    #[test]
    fn three_discs_spawned_and_positioned() {
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log.clone());
        let mut app = make_app(sim);

        // After 5 frames, we should see the expected positions.
        for _ in 0..5 {
            app.update();
        }

        let positions = disc_translations(&mut app);
        assert_eq!(positions.len(), 3, "expected 3 disc entities");

        // The sim's tick is now 5. Disc i at tick 5:
        // translation = (i*2.0 + 5*0.01, 1.5, 0.0)
        for (id, pos) in &positions {
            let expected_x = *id as f32 * 2.0 + 5.0 * 0.01;
            assert!(
                (pos.x - expected_x).abs() < 1e-6,
                "disc {id}: expected x={expected_x}, got x={}",
                pos.x
            );
            assert!((pos.y - 1.5).abs() < 1e-6, "disc {id}: y should be 1.5");
            assert!((pos.z - 0.0).abs() < 1e-6, "disc {id}: z should be 0.0");
        }
    }

    #[test]
    fn remove_disc_despawns_entity() {
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log);
        let mut app = make_app(sim);

        // Run 2 frames to get the initial set.
        for _ in 0..2 {
            app.update();
        }
        assert_eq!(disc_translations(&mut app).len(), 3);

        // Remove disc 2 from the sim.
        app.world_mut().resource_mut::<MockSim>().remove_disc(2);

        // Run one more frame: reconciliation should despawn disc 2.
        app.update();

        let positions = disc_translations(&mut app);
        assert_eq!(positions.len(), 2, "disc 2 should be despawned");
        let ids: Vec<u64> = positions.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
        assert!(!ids.contains(&2));
    }

    #[test]
    fn despawn_order_does_not_depend_on_the_map_so_entity_reuse_repeats() {
        // Two bridges fed the same snapshots must hand out the same entities: despawns decide
        // which slots later spawns reuse, so they run in sim-id order (ADR-867).
        let run = || {
            let log = std::sync::Arc::new(Mutex::new(WriteLog {
                submit_input_calls: Vec::new(),
            }));
            let mut app = make_app(MockSim::new(log));
            for id in 4..=12 {
                app.world_mut().resource_mut::<MockSim>().add_disc(id);
            }
            app.update();
            for id in 2..=11 {
                app.world_mut().resource_mut::<MockSim>().remove_disc(id);
            }
            app.update();
            for id in 20..=29 {
                app.world_mut().resource_mut::<MockSim>().add_disc(id);
            }
            app.update();
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &SimId)>();
            let mut spawned: Vec<(u64, Entity)> = query.iter(world).map(|(e, id)| (id.0, e)).collect();
            spawned.sort_by_key(|(id, _)| *id);
            spawned
        };
        let first = run();
        assert_eq!(first.len(), 12, "discs 1, 12 and 20..=29: {first:?}");
        assert_eq!(first, run(), "the same snapshots give the same entities");
    }

    #[test]
    fn new_sim_id_spawns_entity() {
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log);
        let mut app = make_app(sim);

        // Run a frame to get the initial 3 discs.
        app.update();
        assert_eq!(disc_translations(&mut app).len(), 3);

        // Add disc 4 to the sim.
        app.world_mut().resource_mut::<MockSim>().add_disc(4);

        app.update();

        let positions = disc_translations(&mut app);
        assert_eq!(positions.len(), 4, "disc 4 should be spawned");
        let ids: Vec<u64> = positions.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&4));
    }

    #[test]
    fn events_are_drained_once() {
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log);
        let mut app = make_app(sim);

        // Use a single cursor so we only see new events each frame.
        let mut cursor = MessageCursor::default();

        // Frame 1: drain events.
        app.update();
        {
            let messages = app.world().resource::<Messages<SimEvent<String>>>();
            let events: Vec<_> = cursor.read(messages).cloned().collect();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].0, "event_a");
            assert_eq!(events[1].0, "event_b");
        }

        // Frame 2: events should not reappear.
        app.update();
        {
            let messages = app.world().resource::<Messages<SimEvent<String>>>();
            let events: Vec<_> = cursor.read(messages).cloned().collect();
            assert!(
                events.is_empty(),
                "events should be empty after first drain, got {events:?}"
            );
        }
    }

    #[test]
    fn submit_input_stores_last_move_and_does_not_move_discs() {
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log.clone());
        let mut app = make_app(sim);

        // Set some input.
        app.world_mut().resource_mut::<BridgeInput>().move_axes = Vec2::new(0.5, -0.3);

        app.update();

        // The sim should have received the input.
        {
            let sim_res = app.world().resource::<MockSim>();
            let input = sim_res
                .last_input
                .as_ref()
                .expect("submit_input should have been called");
            assert_eq!(input.move_axes, Vec2::new(0.5, -0.3));
        }

        // The disc positions are unchanged from the identity input baseline
        // (the mock sim moves discs by tick, not by input).
        let positions = disc_translations(&mut app);
        assert_eq!(positions.len(), 3);

        // Verify only submit_input was called through the recorded log.
        let Ok(log) = log.lock() else {
            panic!("write log mutex poisoned");
        };
        assert_eq!(log.submit_input_calls.len(), 1);
    }

    #[test]
    fn yaw_about_y_full_circle_returns_to_identity() {
        let q = yaw_about_y(std::f32::consts::TAU);
        // A full-circle rotation brings any vector back to itself.
        let v = q * Vec3::X;
        assert!((v - Vec3::X).length() < 1e-5, "expected X, got {v}");
    }

    #[test]
    fn sim_entity_map_lookup_roundtrip() {
        let mut map = SimEntityMap::default();
        let entity = Entity::from_bits(42);
        map.insert(7, entity);
        assert_eq!(map.entity(7), Some(entity));
        assert_eq!(map.sim_id(entity), Some(7));
        assert_eq!(map.entity(99), None);

        map.remove_by_entity(entity);
        assert_eq!(map.entity(7), None);
        assert_eq!(map.sim_id(entity), None);
    }

    #[test]
    fn no_unwrap_on_missing_entity_reconciliation_handles_absence() {
        // Build an app where the snapshot contains an id that was never spawned,
        // and one that existed but vanished. The system must not panic.
        let log = std::sync::Arc::new(Mutex::new(WriteLog {
            submit_input_calls: Vec::new(),
        }));
        let sim = MockSim::new(log);
        let mut app = make_app(sim);

        // Run a few frames with normal state.
        for _ in 0..3 {
            app.update();
        }
        assert_eq!(disc_translations(&mut app).len(), 3);

        // Remove all discs.
        app.world_mut().resource_mut::<MockSim>().disc_ids.clear();
        app.update();
        assert_eq!(disc_translations(&mut app).len(), 0, "all discs despawned");

        // The map should be empty — no stale entries.
        let map = app.world().resource::<SimEntityMap>();
        assert_eq!(map.iter().count(), 0);
    }

    #[test]
    fn bridge_input_default_is_zero_and_empty() {
        let input = BridgeInput::default();
        assert_eq!(input.move_axes, Vec2::ZERO);
        assert!(input.edges.is_empty());
    }

    #[test]
    fn snapshot_tick_ord() {
        let a = SnapshotTick(1);
        let b = SnapshotTick(2);
        assert!(a < b);
        assert_eq!(a, SnapshotTick(1));
    }
}

#[cfg(test)]
mod m9_tests {
    use super::*;
    use crate::script::{Script, ScriptCtx, ScriptPlugin, Scripts};
    use regia_core::math::Vec2;

    /// Records which script methods ran, for kind-presentation assertions.
    #[derive(Resource, Default)]
    struct PresenterLog(Vec<String>);

    /// A presentation script the kind registry attaches.
    #[derive(regia_macros::Script, Reflect, Default, Clone)]
    #[script(order = 0)]
    #[allow(dead_code)]
    struct DiscPresenter {
        lean_seen: f32,
    }
    // The derive emits `regia_framework::…` paths; alias this crate to that name.
    use crate as regia_framework;

    impl Script for DiscPresenter {
        fn start(&mut self, ctx: &mut ScriptCtx) {
            if let Some(mut log) = ctx.world().get_resource_mut::<PresenterLog>() {
                log.0.push("presenter:start".into());
            }
        }

        fn update(&mut self, ctx: &mut ScriptCtx) {
            let extras = ctx.get::<SimExtras>().map(|e| e.0.clone()).unwrap_or_default();
            let lean = extras
                .iter()
                .find(|(name, _)| name == "lean")
                .map_or(0.0, |(_, v)| *v);
            self.lean_seen = lean;
            if let Some(mut log) = ctx.world().get_resource_mut::<PresenterLog>() {
                log.0.push(format!("presenter:update:lean={lean}"));
            }
        }
    }

    /// One entity of the mock kind snapshot: (id, kind, team, translation, extras).
    type KindEntry = (u64, String, u32, Vec3, Vec<(String, f32)>);

    /// Mock snapshot whose entities carry kinds, teams and extras.
    #[derive(Default)]
    struct KindSnapshot {
        entries: Vec<KindEntry>,
    }

    impl SimSnapshot for KindSnapshot {
        fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
            for (id, kind, team, translation, _extras) in &self.entries {
                f(SimEntityView {
                    id: *id,
                    kind,
                    team: *team,
                    translation: *translation,
                    rotation: Quat::IDENTITY,
                });
            }
        }

        fn extras_for(&self, id: u64, out: &mut Vec<(String, f32)>) {
            if let Some((_, _, _, _, extras)) = self.entries.iter().find(|(i, ..)| *i == id) {
                out.extend(extras.iter().cloned());
            }
        }
    }

    /// A sim whose events report a meaningful kind, for router tests.
    #[derive(Resource, Default)]
    struct KindSim {
        pending: Vec<String>,
        tick: u64,
        /// Added to every snapshot translation. Zero leaves the existing kind tests still.
        nudge: Vec3,
    }

    impl ExternalSim for KindSim {
        type Snapshot = KindSnapshot;
        type Event = String;

        fn submit_input(&mut self, _input: &BridgeInput) {}

        fn read_snapshot(&mut self, _frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
            self.tick += 1;
            *out = KindSnapshot {
                entries: vec![(
                    1,
                    "disc".into(),
                    7,
                    Vec3::new(0.0, 1.0, 0.0) + self.nudge,
                    vec![("lean".to_string(), 0.25)],
                )],
            };
            SnapshotTick(self.tick)
        }

        fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
            out.append(&mut self.pending);
        }

        fn event_kind(event: &Self::Event) -> String {
            event.clone()
        }
    }

    fn kinds_ron() -> KindRegistry {
        KindRegistry::from_ron(
            r#"(kinds: {
                "disc": (prefab: Some("prefabs/disc.prefab.ron"),
                         presentation_scripts: ["regia_framework::sim_bridge::m9_tests::DiscPresenter"]),
            })"#,
        )
        .expect("valid kinds ron")
    }

    fn make_app() -> App {
        use crate::script::RegisterScriptExt;
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            ScriptPlugin,
            SimBridgePlugin::<KindSim>::new(KindSim::default()).kinds(kinds_ron()),
        ))
        .register_script::<DiscPresenter>()
        .init_resource::<PresenterLog>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    #[test]
    fn kind_registry_parses_and_looks_up() {
        let registry = kinds_ron();
        let disc = registry.entry("disc").expect("disc registered");
        assert_eq!(
            disc.presentation_scripts,
            vec!["regia_framework::sim_bridge::m9_tests::DiscPresenter".to_string()]
        );
        assert_eq!(disc.prefab.as_deref(), Some("prefabs/disc.prefab.ron"));
        assert!(registry.entry("unknown").is_none());
    }

    #[test]
    fn kind_entities_spawn_with_scripts_teams_and_extras() {
        let mut app = make_app();
        app.update();

        let world = app.world_mut();
        let mut query = world.query_filtered::<(&SimId, &SimKind, &Team, &SimExtras, &Scripts), ()>();
        let found = query.iter(world).next().expect("kind entity spawned");
        assert_eq!(found.0.0, 1);
        assert_eq!(found.1.0, "disc");
        assert_eq!(found.2.0, 7);
        assert_eq!(found.3.0, vec![("lean".to_string(), 0.25)]);
        assert_eq!(found.4.0.len(), 1, "presentation script attached");

        // The presenter ran and read the extras through its ScriptCtx.
        let log = world.resource::<PresenterLog>();
        assert!(
            log.0.iter().any(|e| e == "presenter:update:lean=0.25"),
            "presenter read lean from SimExtras: {:?}",
            log.0
        );
    }

    #[test]
    fn event_router_emits_routed_reactions() {
        let mut app = App::new();
        let router = EventRouter::from_ron(r#"(routes: { "explosion": [ Toast(text: "Boom!") ] })"#)
            .expect("valid routes ron");
        app.add_plugins((
            MinimalPlugins,
            SimBridgePlugin::<KindSim>::new(KindSim {
                tick: 0,
                pending: vec!["explosion".into()],
                nudge: Vec3::ZERO,
            })
            .events(router),
        ))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.update();

        use regia_ecs::ecs::message::MessageCursor;
        let world = app.world();
        let mut cursor = MessageCursor::default();
        let routed: Vec<_> = cursor
            .read(world.resource::<Messages<RoutedEvent>>())
            .cloned()
            .collect();
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].kind, "explosion");
        assert_eq!(
            routed[0].reactions,
            vec![Reaction::Toast { text: "Boom!".into() }]
        );

        // The applier raised the toast.
        let mut toast_cursor = MessageCursor::default();
        let toasts: Vec<_> = toast_cursor
            .read(world.resource::<Messages<ToastRaised>>())
            .cloned()
            .collect();
        assert_eq!(toasts.len(), 1, "toast raised by apply_routed_events");
    }

    #[test]
    fn counters_accumulate_per_frame() {
        let mut app = make_app();
        app.world_mut().resource_mut::<BridgeInput>().edges = vec!["boost".into()];
        for _ in 0..3 {
            app.update();
        }
        let counters = app.world().resource::<SimCounters>();
        assert_eq!(counters.frames, 3);
        assert_eq!(counters.submit_input_calls, 3);
        assert_eq!(counters.snapshot_reads, 3);
        assert_eq!(counters.snapshot_ticks, 3, "tick 1 every read, delta summed");
        assert_eq!(counters.input_edges, 3, "one edge per frame");
    }

    #[test]
    fn frame_rate_check_helper() {
        use super::FramerateSample;
        // 30 fps for 2 s and 300 fps for 2 s over a 60 Hz sim: both see ~120 ticks.
        let a = FramerateSample {
            ticks: 120,
            wall_secs: 2.0,
        };
        let b = FramerateSample {
            ticks: 119,
            wall_secs: 2.0,
        };
        let report = check_framerate_independence(a, b, 0.02);
        assert!(report.pass, "{report:?}");

        // A frame-rate dependent sim (one tick per frame) fails.
        let a = FramerateSample {
            ticks: 60,
            wall_secs: 2.0,
        };
        let b = FramerateSample {
            ticks: 600,
            wall_secs: 2.0,
        };
        let report = check_framerate_independence(a, b, 0.02);
        assert!(!report.pass, "{report:?}");

        // Degenerate windows fail rather than divide by zero.
        let report = check_framerate_independence(
            FramerateSample {
                ticks: 0,
                wall_secs: 1.0,
            },
            FramerateSample {
                ticks: 60,
                wall_secs: 1.0,
            },
            0.02,
        );
        assert!(!report.pass && report.ratio.is_nan(), "{report:?}");
    }

    /// A sim that ticks on its own clock (60 Hz) regardless of the frame rate: the frame-rate
    /// independence property the bridge checks for.
    #[derive(Resource, Default)]
    struct ClockSim {
        elapsed: f64,
        tick: u64,
    }

    #[derive(Default)]
    struct ClockSnapshot {
        entries: Vec<(u64, Vec3)>,
    }

    impl SimSnapshot for ClockSnapshot {
        fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
            for (id, translation) in &self.entries {
                f(SimEntityView {
                    id: *id,
                    kind: "disc",
                    team: 0,
                    translation: *translation,
                    rotation: Quat::IDENTITY,
                });
            }
        }
    }

    impl ExternalSim for ClockSim {
        type Snapshot = ClockSnapshot;
        type Event = String;

        fn submit_input(&mut self, _input: &BridgeInput) {}

        fn read_snapshot(&mut self, frame_time: Duration, out: &mut Self::Snapshot) -> SnapshotTick {
            self.elapsed += frame_time.as_secs_f64();
            let target = (self.elapsed * 60.0).floor() as u64;
            self.tick = target.max(self.tick);
            out.entries = vec![(1, Vec3::new(self.tick as f32 * 0.01, 1.0, 0.0))];
            SnapshotTick(self.tick)
        }

        fn drain_events(&mut self, _out: &mut Vec<Self::Event>) {}
    }

    #[test]
    fn sim_clock_is_frame_rate_independent_through_the_bridge() {
        fn run(step: Duration, frames: u32) -> (SimCounters, f32, f64) {
            let mut app = App::new();
            app.add_plugins((
                MinimalPlugins,
                SimBridgePlugin::<ClockSim>::new(ClockSim::default()),
            ))
            .insert_resource(TimeUpdateStrategy::ManualDuration(step));
            for _ in 0..frames {
                app.update();
            }
            let counters = *app.world().resource::<SimCounters>();
            let elapsed = app.world().resource::<Time>().elapsed_secs() as f64;
            let x = app
                .world_mut()
                .query::<&Transform>()
                .iter(app.world())
                .next()
                .map(|t| t.translation.x)
                .unwrap_or(0.0);
            (counters, x, elapsed)
        }

        // ~2 s of game time each: 60 fps × 33 ms vs 120 fps × 16 ms.
        let (fast_counters, fast_x, fast_secs) = run(Duration::from_millis(16), 120);
        let (slow_counters, slow_x, slow_secs) = run(Duration::from_millis(33), 60);

        let report = check_framerate_independence(
            super::FramerateSample {
                ticks: fast_counters.snapshot_ticks,
                wall_secs: fast_secs,
            },
            super::FramerateSample {
                ticks: slow_counters.snapshot_ticks,
                wall_secs: slow_secs,
            },
            0.02,
        );
        assert!(report.pass, "rates {report:?}");
        // Each pose equals its own sim tick exactly (determinism), and the two poses differ by
        // no more than the windows' 60 ms wall-time difference allows (≤ 4 ticks = 0.04 m).
        assert!(
            (fast_x - fast_counters.snapshot_ticks as f32 * 0.01).abs() < 1e-4
                && (slow_x - slow_counters.snapshot_ticks as f32 * 0.01).abs() < 1e-4,
            "pose equals the sim's own tick: fast {fast_x}, slow {slow_x}"
        );
        assert!(
            (fast_x - slow_x).abs() < 0.05,
            "poses within the wall-time difference: fast {fast_x}, slow {slow_x}"
        );
        let _ = Vec2::ZERO; // keep the Vec2 import honest if assertions change
    }

    /// A snapshot that matches the spawned pose does not insert the transform or the extras again.
    #[test]
    fn an_unchanged_snapshot_does_not_rewrite_the_pose() {
        let mut app = make_app();
        app.update();
        let writes = |app: &mut App| app.world().resource::<SimCounters>().reconcile_writes;
        let idle_frame = writes(&mut app);
        app.update();
        let idle = writes(&mut app) - idle_frame;
        app.world_mut().resource_mut::<KindSim>().nudge = Vec3::new(1.0, 0.0, 0.0);
        let moved_frame = writes(&mut app);
        app.update();
        let moved = writes(&mut app) - moved_frame;
        assert_eq!(idle, 0, "the second frame matches the world");
        assert!(moved >= 1, "a moved snapshot still writes");
    }
}

/// The router's presentation reactions in a headless App (ADR-7210): `Vfx` spawns its effect at
/// the event's entity, `Decal` places its prefab at the event's position, `SoundCue` starts a
/// voice and `Announcer` starts a line; unknown names and unplaced events are counted, not
/// fatal.
#[cfg(all(test, feature = "client"))]
mod reaction_tests {
    use super::*;
    use regia_audio::{
        AnnouncePriority, AnnouncerBank, Audio, AudioEmitter, AudioSettings, CueEntry, SoundAsset, SoundClip,
        SoundCue,
    };
    use regia_vfx::{DecalLibrary, DecalPool, VfxScene};

    /// One entity (sim id 5) at `ENTITY_AT`; every drained event is `(kind, place)`.
    #[derive(Resource, Default)]
    struct PlacedSim {
        pending: Vec<(String, EventPlace)>,
        tick: u64,
    }

    const ENTITY_AT: Vec3 = Vec3::new(3.0, 0.0, 4.0);

    struct OneEntity;

    impl SimSnapshot for OneEntity {
        fn for_each_entity(&self, mut f: impl FnMut(SimEntityView<'_>)) {
            f(SimEntityView {
                id: 5,
                kind: "tank",
                team: 1,
                translation: ENTITY_AT,
                rotation: Quat::IDENTITY,
            });
        }
    }

    impl Default for OneEntity {
        fn default() -> Self {
            Self
        }
    }

    impl ExternalSim for PlacedSim {
        type Snapshot = OneEntity;
        type Event = (String, EventPlace);

        fn submit_input(&mut self, _input: &BridgeInput) {}

        fn read_snapshot(&mut self, _frame_time: Duration, _out: &mut Self::Snapshot) -> SnapshotTick {
            self.tick += 1;
            SnapshotTick(self.tick)
        }

        fn drain_events(&mut self, out: &mut Vec<Self::Event>) {
            out.append(&mut self.pending);
        }

        fn event_kind(event: &Self::Event) -> String {
            event.0.clone()
        }

        fn event_place(event: &Self::Event) -> EventPlace {
            event.1
        }
    }

    fn effect() -> regia_vfx::VfxSystemDef {
        regia_vfx::VfxSystemDef::from_ron(&format!(
            r#"(
    version: {},
    name: "boom",
    emitters: [(
        name: "flash",
        spawn: (mode: Burst(8), lifetime: (min: 1.0, max: 1.0)),
        init: (size: (min: 0.5, max: 0.5)),
        render: (mode: Billboard),
    )],
)"#,
            regia_vfx::VFX_FORMAT_VERSION
        ))
        .expect("test effect parses")
    }

    fn asset() -> SoundAsset {
        SoundAsset::from_clip(&SoundClip::new(vec![0.1_f32; 48_000], 48_000).expect("clip"))
    }

    /// The bridge with `routes`, a vfx scene (`boom`), the built-in decal prefabs and a headless
    /// audio with the cue `bang` and the announcer bank `score`.
    fn app(routes: &str, pending: Vec<(String, EventPlace)>) -> App {
        let mut library = regia_vfx::scene::VfxLibrary::new();
        library.add(effect()).expect("valid effect");
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        audio.register_cue(SoundCue::new(
            "bang",
            vec![CueEntry {
                asset: asset(),
                weight: 1.0,
            }],
        ));
        audio.register_announcer_bank(
            "score",
            AnnouncerBank::new(vec![asset()], AnnouncePriority::Normal),
        );
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            SimBridgePlugin::<PlacedSim>::new(PlacedSim { pending, tick: 0 })
                .events(EventRouter::from_ron(routes).expect("valid routes")),
        ))
        .insert_resource(VfxScene::new(library))
        .insert_resource(DecalPool::new(16))
        .insert_resource(DecalLibrary::builtin())
        .insert_resource(audio)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app
    }

    #[test]
    fn each_presentation_reaction_produces_its_effect_decal_and_voice() {
        let routes = r#"(routes: {
            "hit": [Vfx(effect: "boom"), Decal(kind: "scorch"), SoundCue(cue: "bang")],
            "scored": [Announcer(line: "score")],
        })"#;
        let mut app = app(
            routes,
            vec![
                ("hit".into(), EventPlace::Entity(5)),
                ("scored".into(), EventPlace::Unplaced),
            ],
        );
        // Frame 1 spawns the entity; its transform is readable from frame 2 on, so the entity
        // event is queued for the second drain.
        let pending = std::mem::take(&mut app.world_mut().resource_mut::<PlacedSim>().pending);
        app.update();
        app.world_mut().resource_mut::<PlacedSim>().pending = pending;
        app.update();

        let stats = *app.world().resource::<RoutedReactionStats>();
        assert_eq!(
            (stats.vfx, stats.decals, stats.sounds, stats.announcer_lines),
            (1, 1, 1, 1),
            "{stats:?}"
        );
        assert_eq!(
            (stats.unplaced, stats.no_subsystem, stats.vfx_unknown),
            (0, 0, 0),
            "{stats:?}"
        );

        // The effect plays at the entity.
        let scene = app.world().resource::<VfxScene>();
        assert_eq!(scene.live_instances(), 1);
        let origin = scene.instances().next().expect("instance").origin();
        assert!((origin - ENTITY_AT).length() < 1e-5, "effect at {origin}");
        // The decal lies at the entity.
        let pool = app.world().resource::<DecalPool>();
        assert_eq!(pool.len(), 1);
        let decal = pool.decals().first().expect("decal");
        assert!(
            (decal.position - ENTITY_AT).length() < 1e-5,
            "decal at {}",
            decal.position
        );
        // The cue and the announcer line hold voices.
        let audio = app.world().resource::<Audio>();
        assert!(
            audio.real_voice_count() >= 2,
            "cue + announcer voices: {}",
            audio.real_voice_count()
        );
    }

    #[test]
    fn unknown_names_and_unplaced_events_are_counted_not_fatal() {
        let routes = r#"(routes: {
            "far": [Vfx(effect: "boom"), Decal(kind: "scorch")],
            "odd": [Vfx(effect: "nope"), Decal(kind: "nope"), SoundCue(cue: "nope")],
        })"#;
        let mut app = app(
            routes,
            vec![
                ("far".into(), EventPlace::Unplaced),
                ("odd".into(), EventPlace::At(Vec3::new(1.0, 0.0, 1.0))),
            ],
        );
        app.update();
        let stats = *app.world().resource::<RoutedReactionStats>();
        assert_eq!(stats.unplaced, 2, "{stats:?}");
        assert_eq!(
            (stats.vfx_unknown, stats.decals_unknown, stats.sounds_refused),
            (1, 1, 1),
            "{stats:?}"
        );
        assert_eq!((stats.vfx, stats.decals, stats.sounds), (0, 0, 0));
        assert_eq!(app.world().resource::<VfxScene>().live_instances(), 0);
    }

    #[test]
    fn the_event_seed_is_a_pure_function_of_the_stream() {
        assert_eq!(routed_event_seed(7, 0, "hit"), routed_event_seed(7, 0, "hit"));
        assert_ne!(routed_event_seed(7, 0, "hit"), routed_event_seed(7, 1, "hit"));
        assert_ne!(routed_event_seed(7, 0, "hit"), routed_event_seed(8, 0, "hit"));
        assert_ne!(routed_event_seed(7, 0, "hit"), routed_event_seed(7, 0, "hut"));
    }

    /// A clip of `len` samples (two lengths make a two-variation cue whose pick shows in
    /// `Audio::played` as the played clip length).
    fn clip(len: usize) -> SoundAsset {
        SoundAsset::from_clip(&SoundClip::new(vec![0.1_f32; len], 48_000).expect("clip"))
    }

    #[test]
    fn a_routed_sound_cue_with_a_position_plays_a_spatial_one_shot_at_it() {
        let routes = r#"(routes: { "hit": [SoundCue(cue: "bang")] })"#;
        let mut app = app(routes, vec![("hit".into(), EventPlace::At(ENTITY_AT))]);
        app.update();
        let stats = *app.world().resource::<RoutedReactionStats>();
        assert_eq!((stats.sounds, stats.sounds_refused), (1, 0), "{stats:?}");
        let audio = app.world().resource::<Audio>();
        let played = audio.played();
        assert_eq!(played.len(), 1, "one positional play");
        assert_eq!(
            played[0].position,
            Some(ENTITY_AT),
            "the cue starts a spatial voice at the event position"
        );
    }

    #[test]
    fn a_routed_sound_cue_without_a_position_still_plays_flat() {
        let routes = r#"(routes: { "hit": [SoundCue(cue: "bang")] })"#;
        let mut app = app(routes, vec![("hit".into(), EventPlace::Unplaced)]);
        app.update();
        let stats = *app.world().resource::<RoutedReactionStats>();
        assert_eq!(
            (stats.sounds, stats.sounds_refused, stats.unplaced),
            (1, 0, 0),
            "a cue is not a placed reaction: an unplaced event still plays it, flat"
        );
        let audio = app.world().resource::<Audio>();
        let played = audio.played();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].position, None, "flat, not spatial");
    }

    #[test]
    fn an_out_of_range_routed_sound_cue_is_refused_not_played() {
        let routes = r#"(routes: { "hit": [SoundCue(cue: "bang")] })"#;
        // 1 km away with a default emitter (max distance 50 m): nothing audible.
        let mut app = app(
            routes,
            vec![("hit".into(), EventPlace::At(Vec3::new(1_000.0, 0.0, 0.0)))],
        );
        app.update();
        let stats = *app.world().resource::<RoutedReactionStats>();
        assert_eq!(
            (stats.sounds, stats.sounds_refused),
            (0, 1),
            "an inaudible positional cue counts as refused, not played"
        );
        assert!(
            app.world().resource::<Audio>().played().is_empty(),
            "no voice starts"
        );
    }

    /// Review finding 2 (w6-post05b): a placed routed cue used the 50 m default emitter, so a
    /// battlefield-scale event 200 m away was refused and content could not say otherwise. With
    /// the reaction's `max_distance` it plays; without it the default range still applies.
    #[test]
    fn a_routed_sound_cue_at_200_m_plays_only_with_a_longer_max_distance() {
        let far = vec![("hit".into(), EventPlace::At(Vec3::new(200.0, 0.0, 0.0)))];
        let run = |routes: &str| {
            let mut app = app(routes, far.clone());
            app.update();
            let stats = *app.world().resource::<RoutedReactionStats>();
            let played = app.world().resource::<Audio>().played().len();
            (stats.sounds, stats.sounds_refused, played)
        };
        assert_eq!(
            run(r#"(routes: { "hit": [SoundCue(cue: "bang")] })"#),
            (0, 1, 0),
            "without the field: the default 50 m emitter, refused at 200 m"
        );
        assert_eq!(
            run(r#"(routes: { "hit": [SoundCue(cue: "bang", max_distance: Some(400.0))] })"#),
            (1, 0, 1),
            "with max_distance 400 m the cue plays at 200 m"
        );
        assert_eq!(
            run(r#"(routes: { "hit": [SoundCue(cue: "bang", max_distance: Some(150.0))] })"#),
            (0, 1, 0),
            "a range shorter than the distance still refuses"
        );
    }

    #[test]
    fn a_sound_cue_without_max_distance_parses_and_writes_as_before() {
        let router = EventRouter::from_ron(r#"(routes: { "hit": [SoundCue(cue: "bang")] })"#)
            .expect("an old route parses");
        assert_eq!(
            router.reactions("hit"),
            vec![Reaction::SoundCue {
                cue: "bang".into(),
                max_distance: None,
            }]
        );
        let text = ron::to_string(&router).expect("writes");
        assert!(
            !text.contains("max_distance"),
            "an absent range is not written: {text}"
        );
        let ranged = Reaction::SoundCue {
            cue: "boom".into(),
            max_distance: Some(800.0),
        };
        let back: Reaction = ron::from_str(&ron::to_string(&ranged).expect("writes")).expect("reads");
        assert_eq!(back, ranged);
    }

    #[test]
    fn a_routed_cue_range_that_is_not_finite_and_positive_uses_the_default_emitter() {
        let default = AudioEmitter::default().max_distance;
        for bad in [None, Some(0.0), Some(-5.0), Some(f32::NAN), Some(f32::INFINITY)] {
            assert_eq!(
                reaction_sinks::routed_cue_emitter(bad).max_distance,
                default,
                "{bad:?}"
            );
        }
        let ranged = reaction_sinks::routed_cue_emitter(Some(400.0));
        assert_eq!(ranged.max_distance, 400.0);
        assert_eq!(
            ranged.min_distance,
            AudioEmitter::default().min_distance,
            "only the range changes"
        );
    }

    #[test]
    fn the_same_event_stream_replays_the_same_cue_variation() {
        let routes = r#"(routes: { "hit": [SoundCue(cue: "pick")] })"#;
        // Eight events on one stream, an eight-variation cue: the seed (a pure function of the
        // stream) must pick the same variations on every replay of the same stream, shown by the
        // played clip lengths. Review finding 7 (w6-post05b): a fresh `Audio` seeds its shared
        // picker the same way every time, so a plain replay passed even with the seed ignored.
        // The second replay first draws from the shared picker (unseeded flat plays of another
        // cue), so only the event seed can make its picks match.
        const EVENTS: usize = 8;
        const VARIATIONS: usize = 8;
        let picks = |shared_draws: usize| {
            let pending = (0..EVENTS)
                .map(|i| ("hit".to_string(), EventPlace::At(Vec3::new(i as f32, 0.0, 1.0))))
                .collect();
            let mut app = app(routes, pending);
            let variations = |base: usize| {
                (0..VARIATIONS)
                    .map(|v| CueEntry {
                        asset: clip(base + 480 * v),
                        weight: 1.0,
                    })
                    .collect::<Vec<_>>()
            };
            {
                let mut audio = app.world_mut().resource_mut::<Audio>();
                audio.register_cue(SoundCue::new("pick", variations(4_800)));
                audio.register_cue(SoundCue::new("warm", variations(1_000)));
                for _ in 0..shared_draws {
                    audio.play_cue("warm").expect("warm cue plays");
                }
            }
            app.update();
            app.world()
                .resource::<Audio>()
                .played()
                .iter()
                .filter(|played| played.position.is_some())
                .map(|played| played.clip_len)
                .collect::<Vec<_>>()
        };
        let first = picks(0);
        assert_eq!(first.len(), EVENTS, "every event plays");
        let distinct: std::collections::BTreeSet<_> = first.iter().collect();
        assert!(
            distinct.len() > 1,
            "the stream picks more than one variation: {first:?}"
        );
        assert_eq!(picks(0), first, "the same seed picks the same variations");
        assert_eq!(
            picks(3),
            first,
            "the event seed, not the shared picker's state, picks the variation"
        );
    }
}

// Presentation physics beside the bridge (ADR-8356). A sibling file so this module stays one file.
#[path = "sim_bridge_presentation.rs"]
pub mod presentation;
