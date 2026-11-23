// SPDX-License-Identifier: MPL-2.0
//! [`Audio`]'s propagation state and voice plumbing (`docs/specs/16-audio.md` § 3c): the level's
//! medium, the listener's contact, interior volumes, and the path voices (airborne + ground) each
//! positional sound starts. A child module of `audio.rs` so it can reach the mixer's private state
//! while keeping the hooks in the facade narrow.

use std::collections::HashMap;
use std::time::Duration;

use kira::track::TrackHandle;
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use super::{Audio, PlayRequest, play_data_on_track};
use crate::coupling::{AudioPropagation, InteriorFrame, ListenerContact, ListenerCoupling};
use crate::error::AudioError;
use crate::instance::SoundInstance;
use crate::medium::{AudioMedium, MAX_CUTOFF_HZ};
use crate::path::{PathControl, PathData, PathSpec};
use crate::propagation::{
    PathFrame, Propagation, PropagationContext, propagate, propagate_into, step_coupling_weight,
};
use crate::sound::{PlayData, PlaybackParams, SoundAsset};
use crate::spatial::{AudioEmitter, EmitterFrame, ListenerFrame};
use crate::voices::{SoundInstanceId, VoiceKind};

/// Rumble strength per unit of felt ground amplitude (a −20 dB felt arrival ≈ 0.4 strength).
pub const HAPTIC_GAIN: f32 = 4.0;
/// Felt rumbles weaker than this are not sent to the pads.
pub const HAPTIC_THRESHOLD: f32 = 0.02;

/// Which path a voice renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathRole {
    Airborne,
    Ground,
}

/// The propagation of one live voice.
#[derive(Clone, Debug)]
pub(crate) struct VoicePath {
    role: PathRole,
    spec: PathSpec,
    /// Whether the gain follows the contact fade (false for emitters, re-solved every frame, and
    /// for sound radiated into an interior's air).
    follows_contact: bool,
    /// For the second path voice of one positional play (its ground voice next to an audible
    /// airborne one): the play's first voice. The two are one play: a cue's `max_concurrent`
    /// counts them once and a steal stops both (rev-kimi-10 finding 5, ADR-5926).
    primary: Option<SoundInstanceId>,
}

/// A rumble for the gamepads, produced by a ground arrival.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroundRumble {
    /// Motor strength 0..1.
    pub strength: f32,
    /// How long, seconds.
    pub duration_secs: f32,
}

/// What the ground path is doing to the gamepads (agent-visible through `audio.medium`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GroundHaptics {
    /// Strength being felt now (largest active rumble, continuous emitters included).
    pub strength: f32,
    /// Strength of the last arrival event that fired.
    pub last_event_strength: f32,
    /// Arrival events fired since start.
    pub events_fired: u64,
    /// Arrivals still travelling through the ground.
    pub pending: usize,
}

#[derive(Clone, Copy, Debug)]
struct ScheduledRumble {
    at_secs: f64,
    rumble: GroundRumble,
}

/// Everything [`Audio`] keeps for propagation.
#[derive(Debug)]
pub(crate) struct PropagationState {
    medium: AudioMedium,
    coupling: ListenerCoupling,
    weight: f32,
    last_contact: ListenerContact,
    interiors: Vec<InteriorFrame>,
    emitters: HashMap<Entity, AudioPropagation>,
    /// Generation stamped on each emitter the last time `sync_propagation_emitters` saw it.
    sync_generation: u32,
    seen: HashMap<Entity, u32>,
    emitter_ground: HashMap<Entity, Vec<SoundInstanceId>>,
    paths: HashMap<SoundInstanceId, VoicePath>,
    solved: HashMap<Entity, Propagation>,
    scheduled: Vec<ScheduledRumble>,
    /// Last volume (dB), pan and pitch sent to each live voice. NaN means that component has
    /// not been sent. A still emitter does not repeat a command inside the gate.
    sent: HashMap<SoundInstanceId, (f32, f32, f32)>,
    active: Vec<(f64, f32)>,
    continuous: f32,
    /// When the last continuous rumble command stops covering the pad.
    continuous_until: f64,
    out: Vec<GroundRumble>,
    haptics: GroundHaptics,
}

impl Default for PropagationState {
    fn default() -> Self {
        Self {
            medium: AudioMedium::default(),
            coupling: ListenerCoupling::default(),
            weight: 1.0,
            last_contact: ListenerContact::Boots,
            interiors: Vec::new(),
            emitters: HashMap::new(),
            sync_generation: 0,
            seen: HashMap::new(),
            emitter_ground: HashMap::new(),
            paths: HashMap::new(),
            solved: HashMap::new(),
            scheduled: Vec::new(),
            sent: HashMap::new(),
            active: Vec::new(),
            continuous: 0.0,
            continuous_until: 0.0,
            out: Vec::new(),
            haptics: GroundHaptics::default(),
        }
    }
}

fn spec_for(frame: &PathFrame, delay: bool) -> PathSpec {
    let control = PathControl::new(frame.cutoff_hz, frame.coupling_cutoff_hz);
    control.set_post_gain(frame.contact_weight);
    PathSpec {
        delay_secs: if delay { f64::from(frame.delay_secs) } else { 0.0 },
        coda_secs: frame.coda_secs,
        coda_mix: frame.coda_mix,
        control,
    }
}

/// The gain the voice handle applies (before the coda): the path gain without the contact fade,
/// which the path applies after the coda.
fn handle_gain(frame: &PathFrame) -> f32 {
    if frame.contact_weight > 1e-6 {
        let gain = frame.gain / frame.contact_weight;
        // `f32::min` would turn a NaN gain into 1 (full volume): NaN is silence.
        if gain.is_nan() { 0.0 } else { gain.min(1.0) }
    } else {
        0.0
    }
}

/// Plays prepared data through `track`, wrapped with a propagation path.
pub(super) fn play_wrapped(
    track: &mut TrackHandle,
    data: PlayData,
    spec: PathSpec,
) -> Result<SoundInstance, AudioError> {
    match data {
        PlayData::Static(data) => Ok(SoundInstance::from_static(
            track
                .play(PathData::new(data, spec))
                .map_err(crate::sound::play_error)?,
        )),
        PlayData::Streaming(data) => Ok(SoundInstance::from_streaming(
            track
                .play(PathData::new(data, spec))
                .map_err(crate::sound::play_error)?,
        )),
    }
}

fn rumble_for(frame: &PathFrame) -> Option<GroundRumble> {
    let strength = (frame.felt * HAPTIC_GAIN).clamp(0.0, 1.0);
    (strength >= HAPTIC_THRESHOLD).then(|| GroundRumble {
        strength,
        duration_secs: (0.15 + 0.25 * frame.coda_secs).clamp(0.15, 1.5),
    })
}

impl Audio {
    // ------------------------------------------------------------------ medium and contact

    /// Sets the level's acoustic medium (normally derived from `LevelSettings` by the plugin).
    pub fn set_medium(&mut self, medium: AudioMedium) {
        if medium != self.propagation.medium {
            tracing::info!(target: "regia_audio", medium = medium.kind.name(), "audio medium");
        }
        self.propagation.medium = medium;
    }

    /// The level's acoustic medium.
    pub fn medium(&self) -> &AudioMedium {
        &self.propagation.medium
    }

    /// Sets what the listener touches (the plugin copies the listener's [`ListenerCoupling`]).
    pub fn set_listener_coupling(&mut self, coupling: ListenerCoupling) {
        if coupling.contact.coupled() {
            self.propagation.last_contact = coupling.contact;
        }
        self.propagation.coupling = coupling;
    }

    /// What the listener touches.
    pub fn listener_coupling(&self) -> &ListenerCoupling {
        &self.propagation.coupling
    }

    /// The contact fade 0..1 (1 = coupled to the ground; reaches 0 within 50 ms of leaving it).
    pub fn coupling_weight(&self) -> f32 {
        self.propagation.weight
    }

    /// Replaces this frame's interior volumes.
    pub fn set_interiors(&mut self, interiors: Vec<InteriorFrame>) {
        self.propagation.interiors = interiors;
    }

    /// This frame's interior volumes.
    pub fn interiors(&self) -> &[InteriorFrame] {
        &self.propagation.interiors
    }

    /// Replaces the propagation settings of attached emitters (entities without an entry are
    /// plain world sounds without ground coupling).
    ///
    /// An unchanged list is left in place, so a steady frame does not clone the settings.
    pub fn sync_propagation(&mut self, entries: &[(Entity, AudioPropagation)]) {
        if self.propagation_matches(entries) {
            return;
        }
        self.sync_propagation_owned(
            entries
                .iter()
                .map(|(entity, propagation)| (*entity, propagation.clone()))
                .collect(),
        );
    }

    /// Moves `entries` into the emitter map. The caller has already cloned; this does not clone
    /// again.
    pub fn sync_propagation_owned(&mut self, entries: Vec<(Entity, AudioPropagation)>) {
        self.propagation.emitters.clear();
        self.propagation.emitters.extend(entries);
    }

    /// Folds this frame's emitter components into the map. An emitter whose settings compare
    /// equal is left in place, so its ground name is not cloned again. Emitters absent from
    /// `emitters` are removed.
    pub(crate) fn sync_propagation_emitters<'a>(
        &mut self,
        emitters: impl IntoIterator<Item = (Entity, &'a AudioPropagation)>,
    ) {
        self.propagation.sync_generation = self.propagation.sync_generation.wrapping_add(1);
        let generation = self.propagation.sync_generation;
        for (entity, propagation) in emitters {
            self.propagation.seen.insert(entity, generation);
            let unchanged = matches!(
                self.propagation.emitters.get(&entity),
                Some(stored) if stored == propagation
            );
            if !unchanged {
                self.propagation.emitters.insert(entity, propagation.clone());
            }
        }
        let PropagationState { emitters, seen, .. } = &mut self.propagation;
        emitters.retain(|entity, _| seen.get(entity) == Some(&generation));
        seen.retain(|_, stamped| *stamped == generation);
    }

    fn propagation_matches(&self, entries: &[(Entity, AudioPropagation)]) -> bool {
        self.propagation.emitters.len() == entries.len()
            && entries
                .iter()
                .all(|(entity, propagation)| self.propagation.emitters.get(entity) == Some(propagation))
    }

    /// How many emitters have propagation settings.
    #[must_use]
    pub fn emitter_propagation_count(&self) -> usize {
        self.propagation.emitters.len()
    }

    /// Propagation settings stored for `entity` (not the solved paths).
    #[must_use]
    pub fn emitter_propagation_settings(&self, entity: Entity) -> Option<&AudioPropagation> {
        self.propagation.emitters.get(&entity)
    }

    /// Sets one attached emitter's propagation settings.
    pub fn set_emitter_propagation(&mut self, entity: Entity, propagation: AudioPropagation) {
        self.propagation.emitters.insert(entity, propagation);
    }

    /// The paths last solved for an attached emitter (updated every `AudioSync`).
    pub fn emitter_propagation(&self, entity: Entity) -> Option<&Propagation> {
        self.propagation.solved.get(&entity)
    }

    /// Solves the paths a sound at `emitter.position` would take right now to its nearest
    /// listener (no voice is started).
    pub fn solve_propagation(
        &self,
        emitter: &EmitterFrame,
        params: &AudioEmitter,
        propagation: &AudioPropagation,
        bus: &str,
    ) -> Propagation {
        let listener = self.listener_for(emitter.position);
        self.solve_propagation_for(&listener, emitter, params, propagation, bus)
    }

    /// [`Audio::solve_propagation`] against a given listener.
    pub(super) fn solve_propagation_for(
        &self,
        listener: &ListenerFrame,
        emitter: &EmitterFrame,
        params: &AudioEmitter,
        propagation: &AudioPropagation,
        bus: &str,
    ) -> Propagation {
        let state = &self.propagation;
        let ctx = PropagationContext {
            medium: &state.medium,
            listener,
            coupling: &state.coupling,
            coupling_weight: state.weight,
            last_contact: state.last_contact,
            interiors: &state.interiors,
        };
        propagate(
            &ctx,
            emitter,
            params,
            propagation,
            self.bus_layout.bypasses_medium(bus),
        )
    }

    /// Ground haptics state (agent-visible).
    pub fn haptics(&self) -> GroundHaptics {
        self.propagation.haptics
    }

    /// Takes the rumbles due this frame (the plugin forwards them to every connected gamepad).
    pub fn take_rumbles(&mut self) -> Vec<GroundRumble> {
        std::mem::take(&mut self.propagation.out)
    }

    /// Queues `asset` at a world position with emitter parameters and propagation settings (ground
    /// coupling, path class): up to two voices start, the airborne and the ground arrival.
    pub fn play_asset_propagated(
        &mut self,
        asset: &SoundAsset,
        params: PlaybackParams,
        position: Vec3,
        emitter: &AudioEmitter,
        propagation: &AudioPropagation,
    ) -> Result<(), AudioError> {
        self.queue.push(PlayRequest {
            asset: asset.clone(),
            params,
            position: Some(position),
            kind: VoiceKind::OneShot,
            cue: None,
            emitter: Some(emitter.clone()),
            propagation: Some(propagation.clone()),
            filter_cutoff_hz: None,
        });
        Ok(())
    }

    // ------------------------------------------------------------------ voices

    /// Starts a positional one-shot through the medium: its airborne voice (delayed by `r / c`,
    /// absorbed) and its ground voice (delayed by `r / v`, with coda), each only when audible.
    /// `Ok(None)` when no path reaches the listener (airless, floating).
    pub(super) fn start_propagated(
        &mut self,
        request: PlayRequest,
    ) -> Result<Option<SoundInstanceId>, AudioError> {
        let mut request = request;
        let position = request.position.unwrap_or(Vec3::ZERO);
        let emitter = request.emitter.clone().unwrap_or_default();
        let propagation = request.propagation.clone().unwrap_or_default();
        // Resolve and record the bus (params, asset default, else Master) so the voices' booking
        // names the track they play on.
        let bus = super::resolved_params_bus(&mut request.params, Some(&request.asset));
        let frame = EmitterFrame {
            position,
            velocity: Vec3::ZERO,
            forward: Vec3::ZERO,
        };
        // Positional one-shots pan: binaural rendering needs a per-emitter track, which entity
        // emitters have and a fire-and-forget one-shot does not.
        let mut listener = self.listener_for(position);
        listener.hrtf = false;
        let solved = self.solve_propagation_for(&listener, &frame, &emitter, &propagation, &bus);
        let radiated = !self.propagation.coupling.contact.coupled() && self.propagation.weight <= 0.0;
        // A failure on the second path (a full bus track, a stream factory that fails on its
        // second call) must not leave the first path's voice running: roll back everything this
        // call registered before returning the error.
        let played_log_mark = self.played_log_mark();
        let scheduled_len = self.propagation.scheduled.len();
        let mut registered: Vec<SoundInstanceId> = Vec::new();
        let result = (|| -> Result<Option<SoundInstanceId>, AudioError> {
            let mut first = None;
            for (role, path) in [
                (PathRole::Airborne, solved.airborne),
                (PathRole::Ground, solved.ground),
            ] {
                let Some(path) = path.filter(PathFrame::audible) else {
                    continue;
                };
                let mut params = request.params.clone();
                params.panning = (path.pan != 0.0).then_some(path.pan);
                if (path.pitch_ratio - 1.0).abs() > 1e-4 {
                    params.pitch *= path.pitch_ratio;
                }
                let spec = spec_for(&path, true);
                let data = request.asset.play_data(&params)?;
                // A cue's authored low-pass: each path voice plays on its own one-off filtered
                // child track of the bus, held on the voice like a flat cue's (ADR-5926).
                let child = request
                    .filter_cutoff_hz
                    .and_then(|cutoff| self.buses.add_child_track(&bus, super::low_pass_track(cutoff)));
                if request.filter_cutoff_hz.is_some() && child.is_none() && self.buses.contains(&bus) {
                    tracing::warn!(target: "regia_audio", "cue filter track unavailable on `{bus}`, playing unfiltered");
                }
                let (instance, filter_track) = match child {
                    Some(mut child) => (play_wrapped(&mut child, data, spec.clone())?, Some(child)),
                    None => {
                        let track = self
                            .buses
                            .track(&bus)
                            .ok_or_else(|| AudioError::UnknownBus(bus.clone()))?;
                        (play_wrapped(track, data, spec.clone())?, None)
                    }
                };
                let logged = self.played_log_mark();
                let id = self.register_voice(
                    request.asset.clone(),
                    params,
                    request.kind,
                    request.cue.clone(),
                    handle_gain(&path),
                    instance,
                    Some(position),
                );
                registered.push(id);
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.filter_track = filter_track;
                }
                if first.is_some() {
                    // One played entry per request, however many paths it takes.
                    self.truncate_played_log(logged);
                }
                self.propagation.paths.insert(
                    id,
                    VoicePath {
                        role,
                        spec,
                        follows_contact: role == PathRole::Ground && !radiated,
                        primary: first,
                    },
                );
                if role == PathRole::Ground
                    && let Some(rumble) = rumble_for(&path)
                {
                    self.propagation.scheduled.push(ScheduledRumble {
                        at_secs: self.now_secs + f64::from(path.delay_secs),
                        rumble,
                    });
                }
                // On failure the rollback truncates the log to its pre-call mark.
                if first.is_none() {
                    first = Some(id);
                }
            }
            Ok(first)
        })();
        match result {
            Ok(first) => Ok(first),
            Err(error) => {
                self.truncate_played_log(played_log_mark);
                self.propagation.scheduled.truncate(scheduled_len);
                for id in registered {
                    self.propagation.paths.remove(&id);
                    if let Some(entry) = self.voices.remove(id)
                        && let Some(mut instance) = entry.instance
                    {
                        instance.stop(Duration::ZERO);
                    }
                }
                Err(error)
            }
        }
    }

    /// The first voice of the positional play `id` belongs to when `id` is that play's second
    /// path voice (its ground voice next to an audible airborne one); `None` otherwise.
    pub(super) fn path_primary(&self, id: SoundInstanceId) -> Option<SoundInstanceId> {
        self.propagation.paths.get(&id).and_then(|path| path.primary)
    }

    /// The airborne path wrapper of an attached emitter's voice for its first frame (no delay:
    /// a loop's arrival time is not perceptible, and a moving delay would double the Doppler
    /// shift). `AudioSync` retargets it every frame.
    pub(super) fn emitter_air_spec(
        &self,
        entity: Entity,
        listener: &ListenerFrame,
        frame: &EmitterFrame,
        emitter: &AudioEmitter,
        bus: &str,
    ) -> PathSpec {
        let propagation = self
            .propagation
            .emitters
            .get(&entity)
            .cloned()
            .unwrap_or_default();
        let solved = self.solve_propagation_for(listener, frame, emitter, &propagation, bus);
        let frame = solved.airborne.unwrap_or(PathFrame {
            delay_secs: 0.0,
            gain: 0.0,
            cutoff_hz: MAX_CUTOFF_HZ,
            coupling_cutoff_hz: None,
            coda_secs: 0.0,
            coda_mix: 0.0,
            pan: 0.0,
            pitch_ratio: 1.0,
            felt: 0.0,
            contact_weight: 1.0,
        });
        spec_for(&frame, false)
    }

    /// The path wrapper a live voice is re-played with (moved to another track, revived):
    /// its path without the arrival delay.
    pub(super) fn replay_spec(&self, id: SoundInstanceId) -> Option<PathSpec> {
        self.propagation.paths.get(&id).map(|p| PathSpec {
            delay_secs: 0.0,
            ..p.spec.clone()
        })
    }

    /// Records the airborne path of a just-registered emitter voice.
    pub(super) fn note_emitter_air(&mut self, id: SoundInstanceId, spec: Option<PathSpec>) {
        if let Some(spec) = spec {
            self.propagation.paths.insert(
                id,
                VoicePath {
                    role: PathRole::Airborne,
                    spec,
                    follows_contact: false,
                    primary: None,
                },
            );
        }
    }

    /// Solves an attached emitter for this frame and pushes the airborne frame into `air_ids` and
    /// the ground frame into its ground voices (started lazily the first time a ground path
    /// exists).
    /// `air_gain` scales the airborne gain (occlusion); the ground path goes under walls.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_emitter_paths(
        &mut self,
        entity: Entity,
        listener: &ListenerFrame,
        frame: &EmitterFrame,
        emitter: &AudioEmitter,
        air_ids: &[SoundInstanceId],
        air_gain: f32,
        micro: Duration,
    ) {
        let bus = emitter.bus.as_deref().unwrap_or("World");
        let bus_bypass = self.bus_layout.bypasses_medium(bus);
        let mut solved = self
            .propagation
            .solved
            .remove(&entity)
            .unwrap_or_else(Propagation::empty);
        let owned_settings = self.propagation.emitters.remove(&entity);
        let default_settings = AudioPropagation::default();
        let settings = owned_settings.as_ref().unwrap_or(&default_settings);
        {
            let scratch = &mut self.ground_name_scratch;
            let state = &self.propagation;
            let ctx = PropagationContext {
                medium: &state.medium,
                listener,
                coupling: &state.coupling,
                coupling_weight: state.weight,
                last_contact: state.last_contact,
                interiors: &state.interiors,
            };
            propagate_into(&ctx, frame, emitter, settings, bus_bypass, scratch, &mut solved);
        }
        if let Some(settings) = owned_settings {
            self.propagation.emitters.insert(entity, settings);
        }
        let airborne = solved.airborne;
        let ground = solved.ground;
        self.propagation.solved.insert(entity, solved);
        for &id in air_ids {
            self.apply_frame(id, airborne.as_ref(), air_gain, micro);
        }
        let ground_ids = self
            .propagation
            .emitter_ground
            .remove(&entity)
            .unwrap_or_default();
        if ground_ids.is_empty() && ground.is_some_and(|g| g.audible()) {
            self.start_emitter_ground(entity, bus, air_ids, ground.as_ref());
        } else {
            for id in &ground_ids {
                self.apply_frame(*id, ground.as_ref(), 1.0, micro);
            }
            if !ground_ids.is_empty() {
                self.propagation.emitter_ground.insert(entity, ground_ids);
            }
        }
        if let Some(ground) = ground.as_ref()
            && let Some(rumble) = rumble_for(ground)
        {
            self.propagation.continuous = self.propagation.continuous.max(rumble.strength);
        }
    }

    fn start_emitter_ground(
        &mut self,
        entity: Entity,
        bus: &str,
        air_ids: &[SoundInstanceId],
        ground: Option<&PathFrame>,
    ) {
        let (Some(ground), Some(&source)) = (ground, air_ids.first()) else {
            return;
        };
        let Some(entry) = self.voices.get(source) else {
            return;
        };
        let (asset, mut params) = (entry.asset.clone(), entry.params.clone());
        params.panning = Some(ground.pan);
        let spec = spec_for(ground, false);
        // The ground copy of a filtered cue voice carries the cue's low-pass too, on its own
        // one-off filtered child track of the bus (like a positional cue play's ground path,
        // ADR-5926/ADR-5977).
        let mut filter_track = self
            .emitter_cue_cutoff(entity, source)
            .and_then(|cutoff| self.buses.add_child_track(bus, super::low_pass_track(cutoff)));
        let started = asset
            .play_data(&params)
            .and_then(|data| match filter_track.as_mut() {
                Some(filter) => play_wrapped(filter, data, spec.clone()),
                None => {
                    let track = self
                        .buses
                        .track(bus)
                        .ok_or_else(|| AudioError::UnknownBus(bus.into()))?;
                    play_wrapped(track, data, spec.clone())
                }
            });
        match started {
            Ok(instance) => {
                let id = self.register_voice(
                    asset,
                    params,
                    VoiceKind::Emitter(entity),
                    None,
                    handle_gain(ground),
                    instance,
                    None,
                );
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.filter_track = filter_track;
                }
                self.propagation.paths.insert(
                    id,
                    VoicePath {
                        role: PathRole::Ground,
                        spec,
                        follows_contact: false,
                        primary: None,
                    },
                );
                self.propagation
                    .emitter_ground
                    .entry(entity)
                    .or_default()
                    .push(id);
            }
            Err(error) => tracing::warn!(target: "regia_audio", "ground voice for {entity:?}: {error}"),
        }
    }

    /// Pushes one path frame into a live voice (gain × `scale`, pan, pitch, cutoffs, contact
    /// fade); `None` silences it, coda included.
    fn apply_frame(&mut self, id: SoundInstanceId, frame: Option<&PathFrame>, scale: f32, micro: Duration) {
        let (gain, pan, pitch) = frame.map_or((0.0, 0.0, 1.0), |f| {
            (handle_gain(f) * scale, f.pan, f.pitch_ratio)
        });
        if let Some(path) = self.propagation.paths.get(&id) {
            match frame {
                Some(frame) => {
                    path.spec.control.set_absorption_hz(frame.cutoff_hz);
                    path.spec.control.set_coupling_hz(frame.coupling_cutoff_hz);
                    path.spec.control.set_post_gain(frame.contact_weight);
                }
                None => path.spec.control.set_post_gain(0.0),
            }
        }
        let Some(entry) = self.voices.get_mut(id) else {
            return;
        };
        entry.gain = gain;
        let volume_db = entry.effective_volume_db();
        // The voice's own pitch (1 unless a layered sound set it, ADR-3271) and the asset's
        // default pitch (spec 16 § 2, ADR-5926), times Doppler.
        let pitch = pitch * entry.effective_pitch();
        let has_instance = entry.instance.is_some();
        if !has_instance {
            return;
        }
        let prev = self
            .propagation
            .sent
            .get(&id)
            .copied()
            .unwrap_or((f32::NAN, f32::NAN, f32::NAN));
        let send_volume = prev.0.is_nan() || (prev.0 - volume_db).abs() > 0.05;
        let send_pan = prev.1.is_nan() || (prev.1 - pan).abs() > 0.001;
        let send_pitch = prev.2.is_nan() || (prev.2 - pitch).abs() > 0.005;
        if let Some(instance) = self.voices.get_mut(id).and_then(|entry| entry.instance.as_mut()) {
            if send_volume {
                instance.set_volume_db(volume_db, micro);
            }
            if send_pan {
                instance.set_pan(pan, micro);
            }
            if send_pitch {
                instance.set_pitch(pitch, micro);
            }
        }
        if send_volume || send_pan || send_pitch {
            self.propagation.sent.insert(
                id,
                (
                    if send_volume { volume_db } else { prev.0 },
                    if send_pan { pan } else { prev.1 },
                    if send_pitch { pitch } else { prev.2 },
                ),
            );
        }
    }

    /// Forgets the volume, pan and pitch last sent to `id`'s handle. Call it whenever a voice gets
    /// a new handle (a revive, a move to another track): the new handle starts from the voice's
    /// raw settings (base volume, no distance gain, centred, no Doppler), so the next path frame
    /// must send all three again even when the frame itself did not change.
    pub(super) fn forget_sent_frame(&mut self, id: SoundInstanceId) {
        self.propagation.sent.remove(&id);
    }

    /// Stops an emitter's ground voices (with its airborne ones in `stop_emitter`).
    pub(super) fn stop_emitter_ground(&mut self, entity: Entity, fade: Duration) {
        self.propagation.solved.remove(&entity);
        if let Some(ids) = self.propagation.emitter_ground.remove(&entity) {
            for id in ids {
                self.stop_voice(id, fade);
            }
        }
    }

    /// Re-plays a revived voice's data with its path (no delay this time).
    pub(super) fn play_revived(
        &mut self,
        id: SoundInstanceId,
        bus: &str,
        data: PlayData,
    ) -> Result<SoundInstance, AudioError> {
        let spec = self.replay_spec(id);
        // An entity emitter's airborne voices live on its child track (occlusion, HRTF).
        let airborne = self
            .propagation
            .paths
            .get(&id)
            .is_none_or(|p| p.role == PathRole::Airborne);
        let emitter = match self.voices.get(id).map(|e| e.kind) {
            Some(VoiceKind::Emitter(entity)) if airborne => Some(entity),
            _ => None,
        };
        // A filtered cue voice replays on the low-pass track it holds (for an attached voice a
        // sub-track of its emitter's track): its one filter, neither dropped nor doubled
        // (ADR-5977).
        let filter = self.voices.get_mut(id).and_then(|e| e.filter_track.as_mut());
        let emitter_track = emitter
            .and_then(|entity| self.emitters.get_mut(&entity))
            .and_then(|state| state.track.as_mut());
        let track = match (filter, emitter_track) {
            (Some(filter), _) => filter,
            (None, Some(track)) => &mut track.handle,
            (None, None) => self
                .buses
                .track(bus)
                .ok_or_else(|| AudioError::UnknownBus(bus.into()))?,
        };
        match spec {
            Some(spec) => play_wrapped(track, data, spec),
            None => play_data_on_track(track, data),
        }
    }

    /// Per-tick propagation work: the contact fade, one-shot ground gains following it, rumble
    /// scheduling and pruning of finished voices.
    #[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
    pub(super) fn tick_propagation(&mut self, dt: f64) {
        let state = &mut self.propagation;
        let coupled = state.coupling.contact.coupled();
        let before = state.weight;
        state.weight = step_coupling_weight(state.weight, coupled, dt as f32);
        let weight = state.weight;

        // Forget voices that are gone.
        let voices = &self.voices;
        state.paths.retain(|id, _| voices.get(*id).is_some());
        state.sent.retain(|id, _| voices.get(*id).is_some());
        for ids in state.emitter_ground.values_mut() {
            ids.retain(|id| voices.get(*id).is_some());
        }

        // One-shot ground voices follow the contact fade after their coda (jumping mid-rumble
        // silences the ringing too).
        if (weight - before).abs() > 1e-6 {
            for path in state.paths.values() {
                if path.follows_contact && path.role == PathRole::Ground {
                    path.spec.control.set_post_gain(weight);
                }
            }
        }

        // Rumbles: arrivals that reached the listener this tick, plus continuous emitters.
        let state = &mut self.propagation;
        let now = self.now_secs;
        let mut due = Vec::new();
        state.scheduled.retain(|s| {
            if s.at_secs <= now {
                due.push(s.rumble);
                false
            } else {
                true
            }
        });
        for mut rumble in due {
            rumble.strength *= weight;
            if rumble.strength < HAPTIC_THRESHOLD {
                continue;
            }
            state.haptics.events_fired += 1;
            state.haptics.last_event_strength = rumble.strength;
            state
                .active
                .push((now + f64::from(rumble.duration_secs), rumble.strength));
            state.out.push(rumble);
        }
        // One command in flight at a time: the pad backend adds overlapping effects, so a new
        // command goes out only once the previous one has ended.
        if state.continuous >= HAPTIC_THRESHOLD && state.continuous_until <= now + 1e-6 {
            let rumble = GroundRumble {
                strength: state.continuous,
                duration_secs: (dt as f32 * 2.0).max(0.05),
            };
            state.continuous_until = now + f64::from(rumble.duration_secs);
            state.out.push(rumble);
        }
        state.active.retain(|(until, _)| *until > now);
        state.haptics.strength = state
            .active
            .iter()
            .map(|(_, s)| *s)
            .fold(state.continuous, f32::max);
        state.haptics.pending = state.scheduled.len();
        state.continuous = 0.0;
        // Rumbles nobody drained (no input plugin) do not pile up.
        if state.out.len() > 64 {
            let excess = state.out.len() - 64;
            state.out.drain(..excess);
        }
    }

    /// The live path voices (role name, voice id) — test and agent hook.
    pub fn path_voices(&self) -> Vec<(&'static str, SoundInstanceId)> {
        let mut out: Vec<_> = self
            .propagation
            .paths
            .iter()
            .map(|(id, p)| {
                let role = match p.role {
                    PathRole::Airborne => "airborne",
                    PathRole::Ground => "ground",
                };
                (role, *id)
            })
            .collect();
        out.sort_by_key(|(_, id)| id.0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::AudioSettings;

    fn headless() -> Audio {
        Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
    }

    impl Audio {
        /// Test helper: queue a rumble as if a ground arrival had produced it.
        pub(crate) fn push_rumble_for_test(&mut self, strength: f32, duration_secs: f32) {
            self.propagation.out.push(GroundRumble {
                strength,
                duration_secs,
            });
        }
    }

    #[test]
    fn a_still_emitter_does_not_retarget_every_tick() {
        let mut audio = headless();
        audio.set_listener(crate::spatial::ListenerFrame::default());
        let entity = regia_ecs::prelude::Entity::from_raw_u32(3).expect("entity");
        let emitter = crate::spatial::AudioEmitter {
            max_distance: 50.0,
            autoplay: false,
            ..crate::spatial::AudioEmitter::default()
        };
        let asset = {
            let clip = crate::clip::SoundClip::new(vec![0.2; 2_400], 48_000).expect("clip");
            crate::sound::SoundAsset::from_clip(&clip)
        };
        audio.start_emitter(entity, asset, &emitter, 0.0).expect("starts");
        let update = crate::audio::EmitterUpdate {
            entity,
            emitter,
            frame: crate::spatial::EmitterFrame {
                position: regia_core::math::Vec3::new(2.0, 0.0, 0.0),
                ..crate::spatial::EmitterFrame::default()
            },
        };
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
        crate::instance::command_counts::reset();
        for _ in 0..30 {
            audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
        }
        assert_eq!(
            crate::instance::command_counts::snapshot(),
            (0, 0, 0),
            "a still emitter must not send volume, pan and pitch every sync"
        );
    }

    /// An emitter 20 m to the side (distance gain < 1, panned) playing a 2 s 440 Hz tone, with
    /// its first voice id, after one sync.
    fn far_emitter(audio: &mut Audio) -> (crate::audio::EmitterUpdate, SoundInstanceId) {
        audio.set_listener(crate::spatial::ListenerFrame::default());
        let entity = regia_ecs::prelude::Entity::from_raw_u32(4).expect("entity");
        let emitter = crate::spatial::AudioEmitter {
            max_distance: 50.0,
            autoplay: false,
            ..crate::spatial::AudioEmitter::default()
        };
        let asset = {
            let tone = (0..96_000)
                .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25)
                .collect::<Vec<f32>>();
            let clip = crate::clip::SoundClip::new(tone, 48_000).expect("clip");
            crate::sound::SoundAsset::from_clip(&clip)
        };
        audio.start_emitter(entity, asset, &emitter, 0.0).expect("starts");
        let update = crate::audio::EmitterUpdate {
            entity,
            emitter,
            frame: crate::spatial::EmitterFrame {
                position: regia_core::math::Vec3::new(20.0, 0.0, 0.0),
                ..crate::spatial::EmitterFrame::default()
            },
        };
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
        // The fresh mixer holds this one voice.
        assert_eq!(audio.voices.entries.len(), 1, "one emitter voice");
        let id = audio.voices.entries[0].id;
        (update, id)
    }

    /// Renders 100 ms and returns the RMS of the left and right channels over its last 20 ms
    /// (after every fade and tween has settled).
    fn settled_rms(audio: &mut Audio) -> (f32, f32) {
        let out = audio.render_offline(4_800).expect("render");
        let tail = &out[out.len() - 960 * 2..];
        let rms = |channel: usize| {
            let sum: f32 = tail.iter().skip(channel).step_by(2).map(|s| s * s).sum();
            (sum / 960.0).sqrt()
        };
        (rms(0), rms(1))
    }

    fn assert_same_level(before: (f32, f32), after: (f32, f32), what: &str) {
        assert!(
            (before.0 - before.1).abs() > 0.1 * before.0.max(before.1),
            "the test needs a panned emitter: {before:?}"
        );
        for (b, a) in [(before.0, after.0), (before.1, after.1)] {
            assert!(
                (a - b).abs() <= 0.1 * b.max(1e-4),
                "{what}: the level must match the spatial level before ({before:?}), got {after:?}"
            );
        }
    }

    /// A revived voice gets a new handle that starts at the base volume, centred: the next sync
    /// must send its distance gain and pan again even though the emitter did not move.
    #[test]
    fn a_revived_voice_gets_its_volume_and_pan_again() {
        let mut audio = headless();
        let (update, id) = far_emitter(&mut audio);
        let before = settled_rms(&mut audio);
        audio.voices.get_mut(id).expect("voice").virtualise();
        audio.revive_voice(id);
        assert!(audio.voices.get(id).expect("voice").instance.is_some(), "revived");
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
        let after = settled_rms(&mut audio);
        assert_same_level(before, after, "revived voice");
    }

    /// Moving an emitter's voices to another bus's track replays them from their raw settings:
    /// the next sync must send the spatial values again.
    #[test]
    fn a_voice_moved_to_a_new_track_gets_its_volume_and_pan_again() {
        let mut audio = headless();
        let (mut update, _) = far_emitter(&mut audio);
        let before = settled_rms(&mut audio);
        update.emitter.bus = Some("Ui".into());
        audio.sync_emitter_frames(std::slice::from_ref(&update), 1.0 / 60.0, None, None, None);
        let after = settled_rms(&mut audio);
        assert_same_level(before, after, "voice moved to the Ui bus");
    }

    #[test]
    fn sync_system_keeps_unchanged_emitter_ground_strings() {
        use crate::coupling::EmitterCategory;
        use crate::propagation_sync::audio_propagation_sync;

        let mut world = World::new();
        world.insert_resource(headless());
        let spawn = |world: &mut World, name: &str| {
            world
                .spawn(AudioPropagation {
                    ground: Some(name.into()),
                    ..AudioPropagation::for_category(EmitterCategory::Machinery)
                })
                .id()
        };
        let steady = spawn(&mut world, "bedrock");
        let edited = spawn(&mut world, "bedrock");
        let mut schedule = Schedule::default();
        schedule.add_systems(audio_propagation_sync);
        schedule.run(&mut world);
        schedule.run(&mut world);
        let ptr = |world: &World, entity: Entity| {
            world
                .resource::<Audio>()
                .propagation
                .emitters
                .get(&entity)
                .and_then(|propagation| propagation.ground.as_ref().map(|name| name.as_ptr() as usize))
        };
        let steady_ptr = ptr(&world, steady);
        assert!(steady_ptr.is_some(), "the steady emitter is in the map");
        schedule.run(&mut world);
        assert_eq!(
            ptr(&world, steady),
            steady_ptr,
            "an unchanged emitter was cloned into a new ground string"
        );
        world.get_mut::<AudioPropagation>(edited).expect("edited").ground = Some("ice".into());
        schedule.run(&mut world);
        assert_eq!(
            ptr(&world, steady),
            steady_ptr,
            "editing one emitter recloned the other"
        );
        assert_eq!(
            world
                .resource::<Audio>()
                .propagation
                .emitters
                .get(&edited)
                .and_then(|propagation| propagation.ground.as_deref()),
            Some("ice")
        );
        world.despawn(edited);
        schedule.run(&mut world);
        assert!(
            !world
                .resource::<Audio>()
                .propagation
                .emitters
                .contains_key(&edited),
            "a removed emitter leaves the map"
        );
        assert_eq!(
            ptr(&world, steady),
            steady_ptr,
            "removing one emitter recloned the other"
        );
    }

    #[test]
    fn sync_propagation_replaces_the_emitter_map() {
        let mut world = World::new();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let mut audio = headless();
        audio.sync_propagation(&[(a, AudioPropagation::default())]);
        assert!(audio.propagation.emitters.contains_key(&a));
        audio.sync_propagation(&[(b, AudioPropagation::default())]);
        assert!(
            !audio.propagation.emitters.contains_key(&a),
            "A left the sync list"
        );
        assert!(audio.propagation.emitters.contains_key(&b));
        assert_eq!(audio.propagation.emitters.len(), 1);
        assert!(audio.emitter_propagation(a).is_none());
    }

    #[test]
    fn take_rumbles_drains_once() {
        let mut audio = headless();
        audio.push_rumble_for_test(0.2, 0.1);
        audio.push_rumble_for_test(0.9, 0.4);
        let rumbles = audio.take_rumbles();
        assert_eq!(rumbles.len(), 2);
        assert!(audio.take_rumbles().is_empty(), "the queue drains exactly once");
    }

    /// A streaming asset whose factory succeeds on odd calls and fails on even ones: the
    /// airborne path of a two-path request builds and its ground path fails. Returns the asset
    /// and the factory call counter.
    fn flaky_ground_path_asset() -> (SoundAsset, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use kira::sound::streaming::StreamingSoundData;

        // A mono 16-bit PCM WAV, 48 kHz, 50 ms.
        let wav = |seconds: f64| -> Vec<u8> {
            let rate = 48_000u32;
            let len = (seconds * f64::from(rate)) as usize;
            let mut data = Vec::with_capacity(len * 2);
            for i in 0..len {
                let s = (((i as f32 / rate as f32) * 440.0 * std::f32::consts::TAU).sin() * 8_000.0) as i16;
                data.extend_from_slice(&s.to_le_bytes());
            }
            let mut wav = Vec::with_capacity(44 + data.len());
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&rate.to_le_bytes());
            wav.extend_from_slice(&(rate * 2).to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            wav
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(wav(0.05));
        let asset = SoundAsset::from_streaming_factory(
            {
                let calls = Arc::clone(&calls);
                let bytes = Arc::clone(&bytes);
                move || {
                    // Succeed on the airborne path's call, fail on the ground path's: the
                    // first and third calls build, the second and fourth error.
                    if calls.fetch_add(1, Ordering::SeqCst) % 2 == 1 {
                        return Err(crate::error::AudioError::load(
                            "flaky.wav",
                            "factory exhausted by the test",
                        ));
                    }
                    StreamingSoundData::from_cursor(std::io::Cursor::new((*bytes).clone()))
                        .map_err(|e| crate::error::AudioError::load("flaky.wav", e))
                }
            },
            0.05,
            "flaky.wav",
        );

        (asset, calls)
    }

    /// A request whose second path fails (here a stream factory that succeeds for the airborne
    /// path and fails for the ground path) must leave no voice behind: the airborne voice is
    /// rolled back instead of leaking on the mixer, and a retry does not stack another one
    /// (correctness audit finding 5).
    #[test]
    fn a_failed_second_path_rolls_back_the_first_voice() {
        use std::sync::atomic::Ordering;

        let (asset, calls) = flaky_ground_path_asset();
        let mut audio = headless();
        let emitter = AudioEmitter {
            min_distance: 1.0,
            max_distance: 5_000.0,
            ..AudioEmitter::default()
        };
        let propagation =
            crate::coupling::AudioPropagation::for_category(crate::coupling::EmitterCategory::Explosion);
        let params = PlaybackParams::default();
        let position = Vec3::new(50.0, 0.0, 0.0);
        for round in 0..2 {
            audio
                .play_asset_propagated(&asset, params.clone(), position, &emitter, &propagation)
                .expect("queued");
            audio.drain();
            assert_eq!(
                audio.voices.entries.len(),
                0,
                "round {round}: the failed ground path leaves no airborne voice"
            );
            assert!(
                audio.propagation.paths.is_empty(),
                "round {round}: no path state either"
            );
        }
        assert_eq!(
            audio.played_count, 0,
            "a rolled-back request is not a played one-shot"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "each attempt built the airborne data and failed on the ground path"
        );
    }

    /// Rev-kimi-28 (audio2-k item 3): the `played_log` cap must not undo the rollback of a
    /// failed request. With the log at its cap, the airborne voice's entry overflows it and the
    /// half-drain runs inside that request; the rollback's saved length then pointed past the
    /// drained log and the failed request's entry stayed in `played()`.
    #[test]
    fn a_rolled_back_request_leaves_no_played_entry_across_the_log_cap() {
        let (asset, _calls) = flaky_ground_path_asset();
        let mut audio = headless();
        let filler = crate::backend::PlayedOneShot {
            clip_len: 1,
            sample_rate: 48_000,
            position: None,
        };
        audio.played_log = vec![filler; Audio::PLAYED_LOG_CAP];
        let emitter = AudioEmitter {
            min_distance: 1.0,
            max_distance: 5_000.0,
            ..AudioEmitter::default()
        };
        let propagation =
            crate::coupling::AudioPropagation::for_category(crate::coupling::EmitterCategory::Explosion);
        let position = Vec3::new(50.0, 0.0, 0.0);
        audio
            .play_asset_propagated(
                &asset,
                PlaybackParams::default(),
                position,
                &emitter,
                &propagation,
            )
            .expect("queued");
        audio.drain();
        assert_eq!(audio.voices.entries.len(), 0, "the request was rolled back");
        let kept = audio
            .played()
            .iter()
            .filter(|p| p.position == Some(position))
            .count();
        assert_eq!(kept, 0, "a rolled-back request leaves no played entry");
        assert!(audio.played().len() <= Audio::PLAYED_LOG_CAP);
    }

    /// A continuously felt emitter does not queue a new rumble every frame: the commands in
    /// flight never overlap, and the pad stays covered.
    #[test]
    fn continuous_ground_rumble_does_not_stack_every_frame() {
        let mut audio = Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        });
        let dt = 1.0 / 60.0;
        let mut ends: Vec<f64> = Vec::new();
        let mut emitted = 0;
        for frame in 0..30 {
            audio.propagation.continuous = 0.5;
            audio.tick(dt);
            let now = audio.now_secs;
            for rumble in audio.take_rumbles() {
                emitted += 1;
                ends.push(now + f64::from(rumble.duration_secs));
            }
            let in_flight = ends.iter().filter(|end| **end > now + 1e-6).count();
            assert!(in_flight <= 1, "frame {frame}: {in_flight} rumbles in flight");
        }
        assert!(emitted >= 5, "still re-issued as each command ends: {emitted}");
        assert!(emitted < 30, "not every frame: {emitted}");
    }

    /// Spec 16 § 3c haptics: strength `4 × felt` clamped to 0..=1; under 0.02 no rumble
    /// (ADR-5614 conformance).
    #[test]
    fn rumble_strength_is_four_times_felt_clamped_with_a_floor() {
        let frame = |felt: f32| PathFrame {
            delay_secs: 0.0,
            gain: 1.0,
            cutoff_hz: 20_000.0,
            coupling_cutoff_hz: None,
            coda_secs: 0.0,
            coda_mix: 0.0,
            pan: 0.0,
            pitch_ratio: 1.0,
            felt,
            contact_weight: 1.0,
        };
        let strength = |felt: f32| rumble_for(&frame(felt)).map(|r| r.strength);
        assert_eq!(strength(0.1), Some(0.4));
        assert_eq!(strength(1.0), Some(1.0), "clamped");
        assert_eq!(strength(0.004), None, "0.016 is under the floor");
        assert_eq!(strength(0.005), Some(0.02), "the floor itself is sent");
        assert_eq!(strength(f32::NAN), None, "a NaN felt level sends no rumble");
    }
}
