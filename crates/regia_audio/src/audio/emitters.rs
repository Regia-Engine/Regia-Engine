// SPDX-License-Identifier: MPL-2.0
//! Entity emitters on the [`Audio`] facade (`docs/specs/16-audio.md` § 3): autoplay lifecycle,
//! per-emitter child tracks (air absorption + occlusion low-pass, binaural rendering),
//! per-frame spatial updates against the nearest listener, occlusion smoothing and reverb
//! zones.

use std::sync::Arc;
use std::time::Duration;

use kira::effect::filter::{FilterBuilder, FilterHandle, FilterMode};
use kira::track::{TrackBuilder, TrackHandle};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;

use super::propagation_impl::play_wrapped;
use super::{Audio, low_pass_track, play_data_on_track};
use crate::audio_zone::{RockTuning, ZoneField, scaled_range};
use crate::bus::{REVERB_BUS, tween};
use crate::cue::{CueSelector, SoundCue};
use crate::cues::CueAssets;
use crate::error::AudioError;
use crate::hrtf::{HrirSphere, HrtfControl, HrtfEffectBuilder, SphereSpectra};
use crate::occlusion::{AudioOcclusion, OcclusionRay, OcclusionTuning};
use crate::reverb_zone::ReverbMix;
use crate::sound::{LoopRegion, PlaybackParams, SoundAsset};
use crate::sounds::SoundAssets;
use crate::spatial::{AudioEmitter, EmitterFrame, ListenerFrame, spatialize};
use crate::voices::{SoundInstanceId, VoiceKind};

/// Short fade used when emitter voices stop or move between tracks.
const HANDOFF: Duration = Duration::from_millis(20);
/// Tween of per-frame parameter updates (hides the frame-rate stepping).
const MICRO: Duration = Duration::from_millis(30);

/// One emitter's placement for a sync step, built by the `AudioSync` system from the ECS.
#[derive(Clone, Debug)]
pub struct EmitterUpdate {
    /// The emitter entity.
    pub entity: Entity,
    /// Its emitter component.
    pub emitter: AudioEmitter,
    /// World-space position, velocity (m/s) and facing.
    pub frame: EmitterFrame,
}

/// A per-emitter child track under the emitter's bus.
pub(super) struct EmitterTrack {
    /// The kira track (dropping it removes the track from the mixer).
    pub(super) handle: TrackHandle,
    /// Low-pass for air absorption and occlusion.
    filter: FilterHandle,
    /// Binaural control when the track renders HRTF.
    hrtf: Option<Arc<HrtfControl>>,
    /// Last cutoff sent (commands are only sent on change).
    cutoff_hz: f64,
}

/// Live state of one entity's emitter.
#[derive(Default)]
pub(super) struct EmitterState {
    voice_ids: Vec<SoundInstanceId>,
    pub(super) track: Option<EmitterTrack>,
    /// The bus the track was built under.
    track_bus: String,
    /// The sound the autoplay voice was started for (`None` = nothing autoplayed yet).
    autoplay_key: Option<SoundKey>,
    autoplay_voice: Option<SoundInstanceId>,
    /// The cue the autoplay voice was started for, and its picker (variation memory).
    autoplay_cue_key: Option<CueKey>,
    cue_selector: Option<CueSelector>,
    autoplay_cue_voice: Option<SoundInstanceId>,
    /// The cue part of the cue voice's level, dB (the cue's volume plus the pick's variation);
    /// the emitter's `volume_db` is added on top every frame.
    autoplay_cue_db: f32,
    /// The authored cue low-pass of each voice that plays through one (`voice id`, cutoff Hz):
    /// its `VoiceEntry::filter_track` is a sub-track of this emitter's track, rebuilt under the
    /// new track when the track is (ADR-5977).
    cue_filters: Vec<(SoundInstanceId, f32)>,
    /// Last frame seen (start placement for voices started between syncs).
    frame: EmitterFrame,
    /// Smoothed occlusion 0..1.
    occlusion: f32,
    /// Smoothed rock thickness between listener and emitter, metres.
    rock_m: f32,
    /// Distance scale applied last sync (1 = none).
    zone_scale: f32,
    /// The emitter was heard through a zone mouth last sync.
    routed: bool,
    /// A missing sound was already warned about.
    warned_missing: bool,
}

/// Identity of the sound an emitter names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SoundKey {
    Handle(u32, u32),
    Name(String),
}

fn sound_matches(emitter: &AudioEmitter, key: &Option<SoundKey>) -> bool {
    match key {
        Some(SoundKey::Handle(index, generation)) => emitter
            .sound
            .as_ref()
            .is_some_and(|handle| handle.index == *index && handle.generation == *generation),
        Some(SoundKey::Name(name)) => {
            emitter
                .sound
                .as_ref()
                .is_none_or(|handle| handle.index == u32::MAX)
                && emitter.sound_name == name.as_str()
        }
        None => {
            emitter
                .sound
                .as_ref()
                .is_none_or(|handle| handle.index == u32::MAX)
                && emitter.sound_name.is_empty()
        }
    }
}

fn sound_key(emitter: &AudioEmitter) -> Option<SoundKey> {
    match &emitter.sound {
        Some(handle) if handle.index != u32::MAX => Some(SoundKey::Handle(handle.index, handle.generation)),
        _ if !emitter.sound_name.is_empty() => Some(SoundKey::Name(emitter.sound_name.clone())),
        _ => None,
    }
}

fn resolve(emitter: &AudioEmitter, sounds: Option<&SoundAssets>) -> Option<SoundAsset> {
    let sounds = sounds?;
    match &emitter.sound {
        Some(handle) if handle.index != u32::MAX => sounds.get(handle).cloned(),
        _ => sounds.by_name(&emitter.sound_name).cloned(),
    }
}

/// Identity of the cue an emitter names (a cue is used only when no sound is set).
#[derive(Clone, Debug, PartialEq, Eq)]
enum CueKey {
    Handle(u32, u32),
    Name(String),
}

fn cue_key(emitter: &AudioEmitter) -> Option<CueKey> {
    if emitter.has_sound() {
        return None;
    }
    match &emitter.cue {
        Some(handle) if handle.index != u32::MAX => Some(CueKey::Handle(handle.index, handle.generation)),
        _ if !emitter.cue_name.is_empty() => Some(CueKey::Name(emitter.cue_name.clone())),
        _ => None,
    }
}

fn resolve_cue<'a>(emitter: &AudioEmitter, cues: Option<&'a CueAssets>) -> Option<&'a SoundCue> {
    let cues = cues?;
    if emitter.has_sound() {
        return None;
    }
    match &emitter.cue {
        Some(handle) if handle.index != u32::MAX => cues.get(handle),
        _ => cues.by_name(&emitter.cue_name),
    }
}

/// The HRIR sphere binaural tracks use (built on first need).
#[derive(Default)]
pub(super) struct HrirState {
    spectra: Option<Arc<SphereSpectra>>,
    failed: bool,
    /// The rate the output's mixer runs binaural effects at, as the last one reported it (a
    /// device opens at its own rate, not the settings'); tables are prepared for it before a
    /// track is built (ADR-7461).
    mixer_rate: Option<u32>,
    /// A mixer rate past [`crate::hrtf::HRTF_MAX_RATE_TABLES`] was already logged.
    capped_logged: bool,
}

/// Reverb-zone state: what was last applied to the reverb sends.
#[derive(Default)]
pub(super) struct ReverbState {
    applied: Option<ReverbMix>,
}

/// `mix` with every non-finite field replaced by the dry mix's value (counted as a refusal). The
/// reverb send is shared by every bus that sends to it and its feedback state never forgets a
/// NaN: one bad zone value (a NaN in a scene file) would silence the whole mix through it.
fn finite_reverb_mix(mix: ReverbMix) -> ReverbMix {
    let dry = ReverbMix::DRY;
    let pick = |value: f32, fallback: f32| {
        if value.is_finite() {
            value
        } else {
            crate::sound::note_rejected("reverb zone value");
            fallback
        }
    };
    ReverbMix {
        weight: pick(mix.weight, dry.weight),
        send_db: pick(mix.send_db, dry.send_db),
        feedback: pick(mix.feedback, dry.feedback),
        damping: pick(mix.damping, dry.damping),
        stereo_width: pick(mix.stereo_width, dry.stereo_width),
    }
}

/// A snapshot of one emitter for tools and the agent protocol.
#[derive(Clone, Debug, PartialEq)]
pub struct EmitterStatus {
    /// The emitter entity.
    pub entity: Entity,
    /// Live (real or virtual) voices.
    pub voices: usize,
    /// Smoothed occlusion 0..1.
    pub occlusion: f32,
    /// Smoothed metres of rock between the emitter and its listener.
    pub rock_m: f32,
    /// Zone distance scale in effect (1 = none; 1.5 = audible 1.5× further).
    pub zone_scale: f32,
    /// Heard through a zone mouth instead of straight through rock.
    pub routed: bool,
    /// The voices render binaurally (HRTF).
    pub binaural: bool,
    /// The rate of the HRIR table the binaural voices convolve with (the mixer's rate once a
    /// table exists for it, ADR-7461); `None` when not binaural or not yet running.
    pub hrtf_rate: Option<u32>,
    /// Last synced position.
    pub position: Vec3,
}

impl Audio {
    // ----------------------------------------------------------------------- listeners

    /// Sets a single listener (player 0); see [`Audio::set_listeners`] for split-screen.
    pub fn set_listener(&mut self, listener: ListenerFrame) {
        self.listeners = vec![(None, listener)];
    }

    /// Sets every listener (split-screen: each emitter is heard by the nearest one, spec § 3).
    /// The first is player 0 (reverb zones follow it). An empty list restores the default
    /// listener at the origin facing −Z.
    pub fn set_listeners(&mut self, listeners: &[(Option<Entity>, ListenerFrame)]) {
        self.listeners = if listeners.is_empty() {
            vec![(None, ListenerFrame::default())]
        } else {
            listeners.to_vec()
        };
    }

    /// The listeners in effect (at least one).
    pub fn listeners(&self) -> &[(Option<Entity>, ListenerFrame)] {
        &self.listeners
    }

    /// The listener nearest to `position`.
    pub(super) fn listener_for(&self, position: Vec3) -> ListenerFrame {
        self.listeners
            .get(self.listener_index_for(position))
            .map(|(_, frame)| *frame)
            .unwrap_or_default()
    }

    /// Index of the listener nearest to `position` (split-screen, spec § 3).
    fn listener_index_for(&self, position: Vec3) -> usize {
        if self.listeners.len() == 1 {
            return 0;
        }
        self.listeners
            .iter()
            .enumerate()
            .min_by(|(_, (_, a)), (_, (_, b))| {
                a.position
                    .distance_squared(position)
                    .total_cmp(&b.position.distance_squared(position))
            })
            .map_or(0, |(index, _)| index)
    }

    // ----------------------------------------------------------------------- HRTF

    /// Replaces the HRIR sphere binaural emitters use with a measured one (the `.hrir` format;
    /// see [`crate::hrtf`] for the coordinate convention). Emitters already rendering
    /// binaurally keep their sphere until their track is rebuilt.
    pub fn load_hrir_sphere(&mut self, bytes: &[u8]) -> Result<(), AudioError> {
        let sphere = HrirSphere::from_bytes(bytes).map_err(|e| AudioError::load("HRIR sphere", e))?;
        self.set_hrir_sphere(&sphere);
        Ok(())
    }

    /// Uses `sphere` for binaural emitters (a measured one, possibly rotated with
    /// [`HrirSphere::rotate`]).
    pub fn set_hrir_sphere(&mut self, sphere: &HrirSphere) {
        self.hrir = HrirState {
            spectra: Some(sphere.spectra()),
            failed: false,
            mixer_rate: self.hrir.mixer_rate,
            capped_logged: false,
        };
    }

    /// The rate of the HRIR table binaural voices convolve with now (every binaural emitter
    /// shares the one mixer, so one rate; a track still waiting for its rebuild onto a new
    /// table counts only when no other track runs); `None` while no binaural track runs.
    pub fn hrtf_table_rate(&self) -> Option<u32> {
        let controls = || {
            self.emitters
                .values()
                .filter_map(|s| s.track.as_ref()?.hrtf.as_deref())
        };
        controls()
            .find(|c| c.table_rate().is_some() && c.stale_rate().is_none())
            .or_else(|| controls().next())?
            .table_rate()
    }

    /// The rate binaural effects will run at: a running mixdown's, else the rate the output's
    /// effects last reported (`None` before any binaural effect ran on it).
    fn hrtf_rate_hint(&self) -> Option<u32> {
        self.mixdown().map(|job| job.sample_rate).or(self.hrir.mixer_rate)
    }

    /// The shared spectra with a table at `rate` added when there is room (ADR-7461).
    fn hrir_spectra_at(&mut self, rate: u32) -> Option<Arc<SphereSpectra>> {
        let spectra = self.hrir_spectra()?;
        if spectra.has_rate(rate) {
            return Some(spectra);
        }
        let grown = spectra.with_rate(rate);
        if grown.has_rate(rate) {
            tracing::debug!(target: "regia_audio", rate, "HRIR table prepared for the mixer rate");
            self.hrir.spectra = Some(Arc::clone(&grown));
        } else if !self.hrir.capped_logged {
            self.hrir.capped_logged = true;
            tracing::warn!(
                target: "regia_audio",
                rate,
                tables = ?grown.rates(),
                "HRIR tables are full; binaural voices use the nearest table's rate"
            );
        }
        Some(grown)
    }

    /// The shared HRIR spectra (the synthesized sphere, built on first need).
    fn hrir_spectra(&mut self) -> Option<Arc<SphereSpectra>> {
        if self.hrir.spectra.is_none() && !self.hrir.failed {
            let sphere = HrirSphere::synthesized();
            if sphere.hrir_len() == 0 {
                tracing::error!(target: "regia_audio", "binaural rendering unavailable: empty HRIR sphere");
                self.hrir.failed = true;
            } else {
                self.hrir.spectra = Some(sphere.spectra());
            }
        }
        self.hrir.spectra.clone()
    }

    // ----------------------------------------------------------------------- emitter voices

    /// Starts a looping sound attached to an entity's emitter. The `AudioSync` system updates
    /// gain/pan/pitch/filtering from the entity's transform every frame. Emitters with a sound
    /// and `autoplay` do not need this: they start themselves.
    pub fn start_emitter(
        &mut self,
        entity: Entity,
        asset: SoundAsset,
        emitter: &AudioEmitter,
        volume_db: f32,
    ) -> Result<SoundInstanceId, AudioError> {
        let _span = tracing::info_span!("audio_start_emitter").entered();
        let params = PlaybackParams {
            volume_db,
            loop_region: Some(LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            }),
            ..PlaybackParams::default()
        };
        self.start_emitter_voice(entity, asset, emitter, params, None)
    }

    /// Starts a sound attached to an entity's emitter with full playback parameters (bus, volume,
    /// pitch, loop region). The bus falls back to the emitter's bus, then `World`. The
    /// `AudioSync` system updates gain/pan/pitch/filtering from the entity's transform every
    /// frame. A caller's `params.pitch` is composed with the per-frame Doppler ratio, so a cue's
    /// variation survives (spec 16 § 3). `filter_cutoff_hz` is a cue's authored low-pass: the
    /// voice then plays on its own one-off filtered sub-track of the emitter's track (so the
    /// emitter's occlusion filter and binaural rendering still apply on top), held on the voice
    /// (ADR-5977).
    pub(crate) fn start_emitter_voice(
        &mut self,
        entity: Entity,
        asset: SoundAsset,
        emitter: &AudioEmitter,
        params: PlaybackParams,
        filter_cutoff_hz: Option<f32>,
    ) -> Result<SoundInstanceId, AudioError> {
        let bus = params
            .bus
            .clone()
            .or_else(|| emitter.bus.clone())
            .unwrap_or_else(|| "World".to_string());
        let params = PlaybackParams {
            bus: Some(bus.clone()),
            ..params
        };
        // The caller's pitch times the asset's default pitch (spec 16 § 2): the product the
        // start data carries and `apply_frame` keeps sending (ADR-5926).
        let pitch = params.pitch * asset.default_pitch();
        let frame = self.emitters.get(&entity).map(|s| s.frame).unwrap_or_default();
        let listener = self.listener_for(frame.position);
        let spatial = spatialize(&listener, &frame, emitter);
        let binaural = listener.hrtf && emitter.spatial_blend > 0.0 && !self.hrir.failed;
        self.ensure_track(entity, &bus, binaural);
        // The voice is wrapped with its airborne propagation path (medium absorption, wall
        // transmission); `AudioSync` retargets it every frame.
        let spec = self.emitter_air_spec(entity, &listener, &frame, emitter, &bus);
        let data = asset.play_data(&params)?;
        // A cue's low-pass: a one-off sub-track of the emitter's track (of the bus when the
        // emitter has no track). Unavailable: warn and play unfiltered, as the flat play does.
        let mut filter_track = match filter_cutoff_hz {
            Some(cutoff) => self.cue_filter_track(entity, &bus, cutoff),
            None => None,
        };
        let state = self.emitters.entry(entity).or_default();
        if let Some(track) = state.track.as_ref()
            && let Some(control) = &track.hrtf
        {
            control.set(spatial.head_direction, emitter.spatial_blend);
        }
        let instance = match (filter_track.as_mut(), state.track.as_mut()) {
            (Some(filter), _) => play_wrapped(filter, data, spec.clone())?,
            (None, Some(track)) => play_wrapped(&mut track.handle, data, spec.clone())?,
            (None, None) => {
                let track = self
                    .buses
                    .track(&bus)
                    .ok_or_else(|| AudioError::UnknownBus(bus.clone()))?;
                play_wrapped(track, data, spec.clone())?
            }
        };
        let filtered = filter_track.is_some();
        let id = self.register_voice(
            asset,
            params,
            VoiceKind::Emitter(entity),
            None,
            spatial.gain,
            instance,
            None,
        );
        self.note_emitter_air(id, Some(spec));
        if let Some(entry) = self.voices.get_mut(id) {
            entry.filter_track = filter_track;
        }
        if let Some(entry) = self.voices.get_mut(id)
            && let Some(instance) = entry.instance.as_mut()
        {
            instance.set_pan(spatial.pan, Duration::ZERO);
            // The caller's pitch (a cue's variation) times the Doppler ratio; a plain emitter
            // passes pitch 1.0 and keeps the old behaviour exactly.
            instance.set_pitch(pitch * spatial.pitch_ratio, Duration::ZERO);
        }
        let state = self.emitters.entry(entity).or_default();
        state.voice_ids.push(id);
        if let Some(cutoff) = filter_cutoff_hz.filter(|_| filtered) {
            state.cue_filters.push((id, cutoff));
        }
        Ok(id)
    }

    /// A one-off low-pass sub-track for a cue voice of `entity`: under the emitter's track, or
    /// under `bus` when the emitter has none. `None` (with a warning) when neither can host one;
    /// the voice then plays unfiltered.
    fn cue_filter_track(&mut self, entity: Entity, bus: &str, cutoff_hz: f32) -> Option<TrackHandle> {
        let parent = self.emitters.get_mut(&entity).and_then(|s| s.track.as_mut());
        let has_parent = parent.is_some();
        let track = match parent {
            Some(parent) => parent.handle.add_sub_track(low_pass_track(cutoff_hz)).ok(),
            None => self.buses.add_child_track(bus, low_pass_track(cutoff_hz)),
        };
        // An unknown bus is the play's own error (`UnknownBus`), not a filter warning.
        if track.is_none() && (has_parent || self.buses.contains(bus)) {
            tracing::warn!(target: "regia_audio", "cue filter track unavailable for emitter {entity:?}, playing unfiltered");
        }
        track
    }

    /// The authored cue low-pass `id` (a voice of `entity`'s emitter) plays through, if any.
    pub(super) fn emitter_cue_cutoff(&self, entity: Entity, id: SoundInstanceId) -> Option<f32> {
        let state = self.emitters.get(&entity)?;
        state
            .cue_filters
            .iter()
            .find(|(voice, _)| *voice == id)
            .map(|(_, cutoff)| *cutoff)
    }

    /// Stops every voice of an entity's emitter and forgets it.
    pub fn stop_emitter(&mut self, entity: Entity, fade: Duration) {
        self.stop_emitter_ground(entity, fade);
        if let Some(mut state) = self.emitters.remove(&entity) {
            for id in state.voice_ids.drain(..) {
                self.stop_voice(id, fade);
            }
            // Dropping the track handle removes the child track once its voices have faded.
            state.track = None;
        }
    }

    /// Builds (or rebuilds) `entity`'s child track for `binaural`, moving live voices onto it.
    fn ensure_track(&mut self, entity: Entity, bus: &str, binaural: bool) {
        let state = self.emitters.entry(entity).or_default();
        // `Some(stale)` when the track fits; `stale` is the mixer rate when its binaural effect
        // runs on a table of another rate (the device's rate had no table when it was built).
        let fits = state.track.as_ref().and_then(|track| {
            (track.hrtf.is_some() == binaural && state.track_bus == bus)
                .then(|| track.hrtf.as_ref().and_then(|c| c.stale_rate()))
        });
        match fits {
            Some(None) => return,
            // Rebuild onto the mixer's own table once there is one (ADR-7461); a full table
            // set keeps the nearest table and the track.
            Some(Some(rate)) if !self.hrir_spectra_at(rate).is_some_and(|s| s.has_rate(rate)) => return,
            Some(Some(_)) | None => {}
        }
        let Some((handle, filter, control)) = self.build_emitter_track(bus, binaural) else {
            return;
        };
        let state = self.emitters.entry(entity).or_default();
        state.track = Some(EmitterTrack {
            handle,
            filter,
            hrtf: control,
            cutoff_hz: 20_000.0,
        });
        state.track_bus = bus.to_string();
        // Move the live voices onto the new track, from where they are now. A voice with a cue
        // low-pass gets a new filter sub-track under the new track (one filter, not two: the old
        // sub-track goes with the old track once the fading handle has finished); a virtual one
        // gets it too, so it revives there (ADR-5977).
        let ids = state.voice_ids.clone();
        let cue_filters = state.cue_filters.clone();
        for id in ids {
            // A stopping voice is not moved (it has no handle) and needs no new filter.
            if self.voices.get(id).is_none_or(|e| e.stopping) {
                continue;
            }
            if let Some(&(_, cutoff)) = cue_filters.iter().find(|(voice, _)| *voice == id) {
                let filter = self.cue_filter_track(entity, bus, cutoff);
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.filter_track = filter;
                }
            }
            let Some(entry) = self.voices.get_mut(id) else {
                continue;
            };
            let Some(mut old) = entry.instance.take() else {
                continue; // virtual: revives on the new track
            };
            let mut params = entry.params.clone();
            params.start_offset_secs = old.position();
            params.fade_in = Some(HANDOFF);
            old.stop(HANDOFF);
            let asset = entry.asset.clone();
            let spec = self.replay_spec(id);
            let Some(track) = self.emitters.get_mut(&entity).and_then(|s| s.track.as_mut()) else {
                if let Some(entry) = self.voices.get_mut(id) {
                    entry.virtual_cursor_secs = params.start_offset_secs;
                }
                continue;
            };
            // A filtered cue voice plays on its (new) filter sub-track of the new track.
            let filter = self.voices.get_mut(id).and_then(|e| e.filter_track.as_mut());
            let target = match filter {
                Some(filter) => filter,
                None => &mut track.handle,
            };
            match asset.play_data(&params).and_then(|data| match spec {
                Some(spec) => play_wrapped(target, data, spec),
                None => play_data_on_track(target, data),
            }) {
                Ok(instance) => {
                    if let Some(entry) = self.voices.get_mut(id) {
                        entry.instance = Some(instance);
                        if entry.paused || entry.held {
                            // The host paused this voice: the rebuild must not un-pause it
                            // (a fresh handle has not been heard, so no declick fade).
                            if let Some(instance) = entry.instance.as_mut() {
                                instance.pause(Duration::ZERO);
                            }
                        }
                    }
                    self.forget_sent_frame(id);
                }
                Err(error) => {
                    // Now handle-less (virtual): keep the captured position so a later revive
                    // resumes there instead of at the old start offset.
                    if let Some(entry) = self.voices.get_mut(id) {
                        entry.virtual_cursor_secs = params.start_offset_secs;
                    }
                    tracing::warn!(target: "regia_audio", "emitter voice did not move tracks: {error}");
                }
            }
        }
    }

    /// Adds an emitter's child track under `bus`: the occlusion low-pass, then the binaural
    /// effect when `binaural`. The binaural effect uses the HRIR table at the rate binaural
    /// effects are known to run at; when kira initialises it at a rate the spectra has no table
    /// for (the first binaural track on a device at a new rate), the table is prepared and the
    /// track built again, once, before any voice plays on it (ADR-7461).
    fn build_emitter_track(
        &mut self,
        bus: &str,
        binaural: bool,
    ) -> Option<(TrackHandle, FilterHandle, Option<Arc<HrtfControl>>)> {
        let mut spectra = match (binaural, self.hrtf_rate_hint()) {
            (false, _) => None,
            (true, Some(rate)) => self.hrir_spectra_at(rate),
            (true, None) => self.hrir_spectra(),
        };
        for attempt in 0..2 {
            let mut builder = TrackBuilder::new();
            let filter = builder.add_effect(
                FilterBuilder::new()
                    .mode(FilterMode::LowPass)
                    .cutoff(20_000.0)
                    .resonance(0.0),
            );
            let control = spectra.clone().map(|spectra| {
                let control = Arc::new(HrtfControl::default());
                builder.add_effect(HrtfEffectBuilder::new(spectra, Arc::clone(&control)));
                control
            });
            let handle = self.buses.add_child_track(bus, builder)?;
            // kira initialises a new track's effects as it is added, at the mixer's rate.
            if let Some(rate) = control.as_ref().and_then(|c| c.mixer_rate())
                && self.mixdown.is_none()
            {
                self.hrir.mixer_rate = Some(rate);
            }
            let stale = control.as_ref().and_then(|c| c.stale_rate());
            if let Some(rate) = stale
                && attempt == 0
            {
                let grown = self.hrir_spectra_at(rate);
                if grown.as_ref().is_some_and(|s| s.has_rate(rate)) {
                    // Dropping the handle removes the unused track.
                    drop(handle);
                    spectra = grown;
                    continue;
                }
            }
            return Some((handle, filter, control));
        }
        None
    }

    /// Clears emitter tracks after the mixer was rebuilt (resume): autoplay restarts.
    pub(super) fn reset_emitters_after_rebuild(&mut self) {
        let entities: Vec<Entity> = self.emitters.keys().copied().collect();
        for entity in entities {
            self.stop_emitter(entity, Duration::ZERO);
        }
        self.reverb.applied = None;
        self.reset_convolution_after_rebuild();
        self.reset_stretch_after_rebuild();
    }

    // ----------------------------------------------------------------------- per-frame sync

    /// Applies one frame of emitter positions (compatibility entry point: position only, no
    /// velocity, a 60 Hz step). The `AudioSync` system uses [`Audio::sync_emitter_frames`].
    pub fn sync_emitters(&mut self, updates: &[(Entity, AudioEmitter, Vec3)]) {
        let updates: Vec<EmitterUpdate> = updates
            .iter()
            .map(|(entity, emitter, position)| EmitterUpdate {
                entity: *entity,
                emitter: emitter.clone(),
                frame: EmitterFrame {
                    position: *position,
                    ..EmitterFrame::default()
                },
            })
            .collect();
        self.sync_emitter_frames(&updates, 1.0 / 60.0, None, None, None);
    }

    /// Applies one frame of emitters (called by the `AudioSync` system every frame with every
    /// live `AudioEmitter`): stops the emitters of entities that are gone, autoplays emitters
    /// whose sound or cue appeared or changed (resolved through `sounds` and `cues`), smooths
    /// occlusion toward the amounts reported on `occlusion` and queues this frame's occlusion
    /// rays there, and pushes gain, pan, pitch, low-pass and binaural direction into the live
    /// voices — each emitter against its nearest listener.
    #[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
    pub fn sync_emitter_frames(
        &mut self,
        updates: &[EmitterUpdate],
        dt: f32,
        sounds: Option<&SoundAssets>,
        cues: Option<&CueAssets>,
        occlusion: Option<&mut AudioOcclusion>,
    ) {
        // Drop emitters whose entities (or components) are gone.
        self.sync_live.clear();
        self.sync_live.extend(updates.iter().map(|u| u.entity));
        let gone: Vec<Entity> = self
            .emitters
            .keys()
            .copied()
            .filter(|e| !self.sync_live.contains(e))
            .collect();
        for entity in gone {
            self.stop_emitter(entity, Duration::from_millis(50));
        }

        let tuning = occlusion.as_ref().map(|o| o.tuning).unwrap_or_default();
        let rock_tuning = occlusion.as_ref().map(|o| o.rock).unwrap_or_default();
        // `sync_one` needs `&mut self`, so the scratch leaves `self` for the loop and comes back.
        let mut rays = std::mem::take(&mut self.occlusion_rays);
        rays.clear();
        for update in updates {
            self.autoplay(update, sounds, cues);
            let reported = match (&occlusion, update.emitter.occlusion) {
                (Some(occ), true) => occ.reported(update.entity),
                _ => 0.0,
            };
            let reported_rock = match (&occlusion, update.emitter.occlusion) {
                (Some(occ), true) => occ.reported_thickness(update.entity),
                _ => 0.0,
            };
            if let Some(ray) = self.sync_one(update, dt, (reported, reported_rock), (&tuning, &rock_tuning)) {
                rays.push(ray);
            }
        }
        if let Some(occ) = occlusion {
            occ.queue(&rays);
        }
        self.occlusion_rays = rays;
    }

    /// Starts, restarts or stops the autoplay voice of one emitter: a cue's variation when the
    /// emitter names a cue (and no sound), else its sound.
    fn autoplay(&mut self, update: &EmitterUpdate, sounds: Option<&SoundAssets>, cues: Option<&CueAssets>) {
        let state = self.emitters.entry(update.entity).or_default();
        state.frame = update.frame;
        if update.emitter.has_cue() {
            self.autoplay_cue(update, cues);
        } else {
            self.autoplay_sound(update, sounds);
        }
    }

    /// The cue branch of [`Audio::autoplay`]: picks one of the cue's variations when the emitter
    /// starts or its cue changes (a looping emitter loops it, a non-looping one plays it once).
    fn autoplay_cue(&mut self, update: &EmitterUpdate, cues: Option<&CueAssets>) {
        let emitter = &update.emitter;
        let key = cue_key(emitter);
        let state = self.emitters.entry(update.entity).or_default();
        if key == state.autoplay_cue_key {
            // Unchanged: the started variation stands. A looping emitter loops it; a
            // `looping: false` emitter is a one-shot and stays silent once it ends (spec 16 § 3,
            // like a sound emitter). A new variation is picked only when the cue changes or the
            // emitter is respawned.
            return;
        } else {
            // The cue changed (or was cleared): the old voice and its picker go.
            if let Some(old) = state.autoplay_cue_voice.take() {
                state.voice_ids.retain(|id| *id != old);
                self.stop_voice(old, HANDOFF);
            }
            let state = self.emitters.entry(update.entity).or_default();
            state.autoplay_cue_key = None;
            state.cue_selector = None;
        }
        let state = self.emitters.entry(update.entity).or_default();
        let Some(key) = key.filter(|_| emitter.autoplay) else {
            return;
        };
        let Some(cue) = resolve_cue(emitter, cues) else {
            if !state.warned_missing {
                state.warned_missing = true;
                tracing::warn!(
                    target: "regia_audio",
                    "emitter {:?}: cue {key:?} is not in CueAssets yet",
                    update.entity
                );
            }
            return;
        };
        // A cue built in Rust may have no entries; there is nothing to pick (the flat play path
        // guards this in `prepare_cue`). Without the guard `entries[pick]` below panics.
        if cue.entries.is_empty() {
            if !state.warned_missing {
                state.warned_missing = true;
                tracing::warn!(
                    target: "regia_audio",
                    "emitter {:?}: cue {key:?} has no entries",
                    update.entity
                );
            }
            return;
        }
        // Pick a variation: the selector lives on the emitter so no-repeat-last-N and the
        // shuffle/sequential modes hold across restarts. Taken out to satisfy the borrow checker
        // (the pick draws on the shared audio generator too).
        let mut selector = match state.cue_selector.take() {
            Some(selector) => selector,
            None => CueSelector::new(cue),
        };
        // The selector before the pick: a play that fails must not consume it (as for flat cue
        // plays, integ-v048c).
        let selector_before = selector.clone();
        let pick = selector.pick(&mut self.rng);
        let mut params = PlaybackParams {
            bus: cue.bus.clone(),
            volume_db: cue.volume_db,
            loop_region: emitter.looping.then_some(LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            }),
            ..PlaybackParams::default()
        };
        selector.vary(&mut params, &mut self.rng);
        // The cue's level (its volume plus this pick's variation) composes with the emitter's
        // `volume_db`, as a sound emitter's does; `sync_one` keeps the sum current every frame.
        let cue_db = params.volume_db;
        params.volume_db = cue_db + emitter.volume_db;
        let asset = cue.entries[pick].asset.clone();
        // The cue's authored low-pass, as on every other cue play (ADR-5977).
        let cutoff = cue.effective_filter_cutoff_hz();
        let started = self.start_emitter_voice(update.entity, asset, emitter, params, cutoff);
        let state = self.emitters.entry(update.entity).or_default();
        match started {
            Ok(id) => {
                state.cue_selector = Some(selector);
                state.autoplay_cue_key = Some(key);
                state.autoplay_cue_voice = Some(id);
                state.autoplay_cue_db = cue_db;
                state.warned_missing = false;
            }
            Err(error) => {
                // Keep the pre-pick selector for the retry, and leave the key unset so the next
                // frame tries again (a rejected play can be transient).
                state.cue_selector = Some(selector_before);
                if !state.warned_missing {
                    state.warned_missing = true;
                    tracing::warn!(target: "regia_audio", "emitter {:?} cue did not start: {error}", update.entity);
                }
            }
        }
    }

    /// The sound branch of [`Audio::autoplay`] (the original behaviour).
    fn autoplay_sound(&mut self, update: &EmitterUpdate, sounds: Option<&SoundAssets>) {
        let emitter = &update.emitter;
        // A sound takes over from a cue: drop the cue picker and voice (scoped borrows: the
        // state must not be held across `stop_voice` / a re-entry).
        let old_cue_voice = {
            let state = self.emitters.entry(update.entity).or_default();
            if state.autoplay_cue_key.is_some() || state.cue_selector.is_some() {
                state.autoplay_cue_key = None;
                state.cue_selector = None;
                state.autoplay_cue_voice.take()
            } else {
                None
            }
        };
        if let Some(old) = old_cue_voice {
            if let Some(state) = self.emitters.get_mut(&update.entity) {
                state.voice_ids.retain(|id| *id != old);
            }
            self.stop_voice(old, HANDOFF);
        }
        let state = self.emitters.entry(update.entity).or_default();
        if sound_matches(emitter, &state.autoplay_key) {
            return;
        }
        let key = sound_key(emitter);
        // The sound changed (or was cleared): the old autoplay voice goes.
        if let Some(old) = state.autoplay_voice.take() {
            state.voice_ids.retain(|id| *id != old);
            self.stop_voice(old, HANDOFF);
        }
        let state = self.emitters.entry(update.entity).or_default();
        state.autoplay_key = None;
        // No sound, or not playing on its own: nothing starts. Turning `autoplay` on later
        // starts the sound then; turning it off leaves a playing sound alone.
        let Some(key) = key.filter(|_| emitter.autoplay) else {
            return;
        };
        let Some(asset) = resolve(emitter, sounds) else {
            // Not registered (yet): retry next frame, warn once.
            if !state.warned_missing {
                state.warned_missing = true;
                tracing::warn!(
                    target: "regia_audio",
                    "emitter {:?}: sound {key:?} is not in SoundAssets yet",
                    update.entity
                );
            }
            return;
        };
        let params = PlaybackParams {
            volume_db: emitter.volume_db,
            loop_region: emitter.looping.then_some(LoopRegion {
                start_secs: 0.0,
                end_secs: None,
            }),
            ..PlaybackParams::default()
        };
        match self.start_emitter_voice(update.entity, asset, emitter, params, None) {
            Ok(id) => {
                let state = self.emitters.entry(update.entity).or_default();
                state.autoplay_key = Some(key);
                state.autoplay_voice = Some(id);
                state.warned_missing = false;
            }
            Err(error) => {
                // Leave `autoplay_key` unset so the next frame retries (a rejected play can be
                // transient: suspended mixer, voice limit); warn once per outage.
                let state = self.emitters.entry(update.entity).or_default();
                if !state.warned_missing {
                    state.warned_missing = true;
                    tracing::warn!(target: "regia_audio", "emitter {:?} did not start: {error}", update.entity);
                }
            }
        }
    }

    /// Spatial update of one emitter; returns its occlusion ray when it should be tested.
    fn sync_one(
        &mut self,
        update: &EmitterUpdate,
        dt: f32,
        (reported_occlusion, reported_rock): (f32, f32),
        (tuning, rock_tuning): (&OcclusionTuning, &RockTuning),
    ) -> Option<OcclusionRay> {
        let has_voices = self
            .emitters
            .get(&update.entity)
            .is_some_and(|s| !s.voice_ids.is_empty());
        if !has_voices {
            return None;
        }
        let listener_index = self.listener_index_for(update.frame.position);
        let (listener_entity, listener) = self.listeners[listener_index];
        let base = &update.emitter;
        let bus = base.bus.as_deref().unwrap_or("World");
        let binaural = listener.hrtf && base.spatial_blend > 0.0 && !self.hrir.failed;
        self.ensure_track(update.entity, bus, binaural);

        // Zones: a mouth route replaces the emitter's place with a virtual source; a shared zone
        // stretches its range.
        // A rock field over cooked geometry routes through its baked portals first (W-25).
        let field_route = self
            .sound_paths
            .as_ref()
            .and_then(|f| f.sound_route(listener.position, update.frame.position));
        let route = match field_route {
            Some(f) => f.route,
            None => self
                .zone_field
                .portal_route(listener.position, update.frame.position),
        };
        let mut frame = update.frame;
        if let Some(route) = &route {
            frame.position = route.position;
        }
        let zone_scale = match field_route {
            Some(f) => f.range_scale,
            None if route.is_some() => 1.0,
            None => self.zone_field.distance_scale(listener.position, frame.position),
        };
        let scaled = (zone_scale != 1.0).then(|| scaled_range(base, zone_scale));
        let emitter = scaled.as_ref().unwrap_or(base);
        let spatial = spatialize(&listener, &frame, emitter);
        let state = self.emitters.get_mut(&update.entity)?;
        state.zone_scale = zone_scale;
        state.routed = route.is_some();
        state.occlusion = tuning.smooth(state.occlusion, reported_occlusion, dt);
        state.rock_m = tuning.smooth(state.rock_m, reported_rock, dt);
        let occlusion = state.occlusion;
        let rock_m = state.rock_m;
        // The track's low-pass is occlusion and rock only: air absorption is the propagation
        // path's.
        let cutoff = tuning.cutoff_hz(occlusion).min(rock_tuning.cutoff_hz(rock_m));
        if let Some(track) = state.track.as_mut() {
            if (track.cutoff_hz - cutoff).abs() > track.cutoff_hz * 0.01 {
                track.cutoff_hz = cutoff;
                track.filter.set_cutoff(cutoff, tween(MICRO));
            }
            if let Some(control) = &track.hrtf {
                control.set(spatial.head_direction, emitter.spatial_blend);
            }
        }
        state.voice_ids.retain(|id| self.voices.get(*id).is_some());
        let live = &state.voice_ids;
        state.cue_filters.retain(|(id, _)| live.contains(id));
        let autoplay_voice = state.autoplay_voice;
        let cue_voice = state.autoplay_cue_voice.map(|id| (id, state.autoplay_cue_db));
        let ids = std::mem::take(&mut state.voice_ids);
        if let Some(id) = autoplay_voice
            && let Some(entry) = self.voices.get_mut(id)
        {
            entry.base_volume_db = emitter.volume_db;
        }
        // A cue voice: the cue's level plus the emitter's (rev-kimi-10 finding 6).
        if let Some((id, cue_db)) = cue_voice
            && let Some(entry) = self.voices.get_mut(id)
        {
            entry.base_volume_db = cue_db + emitter.volume_db;
        }
        // Gain, pan and Doppler come from the propagation paths (medium, walls, ground), the
        // airborne gain scaled by occlusion.
        self.apply_emitter_paths(
            update.entity,
            &listener,
            &frame,
            emitter,
            &ids,
            tuning.gain(occlusion) * rock_tuning.gain(rock_m),
            MICRO,
        );
        if let Some(state) = self.emitters.get_mut(&update.entity) {
            state.voice_ids = ids;
        }
        // A routed emitter is tested against its mouth: the sound arrives through it.
        let target = route.map_or(update.frame.position, |r| r.mouth);
        (emitter.occlusion && spatial.distance < emitter.max_distance).then_some(OcclusionRay {
            emitter: update.entity,
            listener: listener_entity,
            from: listener.position,
            to: target,
        })
    }

    /// Emitter snapshots, sorted by entity (tools and the agent protocol).
    pub fn emitter_status(&self) -> Vec<EmitterStatus> {
        let mut out: Vec<EmitterStatus> = self
            .emitters
            .iter()
            .map(|(entity, state)| EmitterStatus {
                entity: *entity,
                voices: state
                    .voice_ids
                    .iter()
                    .filter(|id| self.voices.get(**id).is_some())
                    .count(),
                occlusion: state.occlusion,
                rock_m: state.rock_m,
                zone_scale: if state.zone_scale > 0.0 {
                    state.zone_scale
                } else {
                    1.0
                },
                routed: state.routed,
                binaural: state.track.as_ref().is_some_and(|t| t.hrtf.is_some()),
                hrtf_rate: state
                    .track
                    .as_ref()
                    .and_then(|t| t.hrtf.as_ref())
                    .and_then(|c| c.table_rate()),
                position: state.frame.position,
            })
            .collect();
        out.sort_by_key(|s| s.entity);
        out
    }

    // ----------------------------------------------------------------------- reverb zones

    /// Applies the reverb-zone mix for player 0 (called by the `AudioSync` system). `None`
    /// means the world has no reverb zones: the reverb sends keep their layout levels (and
    /// return to them after the last zone disappears).
    pub fn set_reverb_mix(&mut self, mix: Option<ReverbMix>) {
        let mix = mix.map(finite_reverb_mix);
        let fade = Duration::from_millis(120);
        match mix {
            None => {
                if self.reverb.applied.take().is_some() {
                    for (bus, layout_db) in self.buses.sends_into(REVERB_BUS) {
                        self.buses.set_send_volume(&bus, REVERB_BUS, layout_db, fade);
                    }
                    self.set_zone_ir_sends(crate::bus::SILENT_SEND_DB, fade);
                }
            }
            Some(mix) => {
                let previous = self.reverb.applied;
                let send_changed =
                    previous.is_none_or(|p| (p.effective_send_db() - mix.effective_send_db()).abs() > 0.1);
                if send_changed || self.zone_flip_pending() {
                    // A zone with an impulse response plays through the zone-IR bus instead of the
                    // preset reverb; a change between the two crossfades the sends (ADR-2721).
                    let (preset_db, ir_db, preset_fade, ir_fade) =
                        self.zone_send_split(mix.effective_send_db());
                    for (bus, _) in self.buses.sends_into(REVERB_BUS) {
                        self.buses
                            .set_send_volume(&bus, REVERB_BUS, preset_db, preset_fade);
                    }
                    self.set_zone_ir_sends(ir_db, ir_fade);
                }
                let room_changed = previous.is_none_or(|p| {
                    (p.feedback - mix.feedback).abs() > 1e-3
                        || (p.damping - mix.damping).abs() > 1e-3
                        || (p.stereo_width - mix.stereo_width).abs() > 1e-3
                });
                if room_changed && mix.weight > 0.0 {
                    self.buses.set_send_reverb(
                        REVERB_BUS,
                        f64::from(mix.feedback),
                        f64::from(mix.damping),
                        f64::from(mix.stereo_width),
                        Duration::from_millis(300),
                    );
                }
                // Keep the previous room while fading out so the tail does not jump.
                let stored = if mix.weight > 0.0 || previous.is_none() {
                    mix
                } else {
                    ReverbMix {
                        weight: mix.weight,
                        send_db: mix.send_db,
                        ..previous.unwrap_or(mix)
                    }
                };
                self.reverb.applied = Some(stored);
            }
        }
    }

    /// Installs the zones placed this frame (the `AudioSync` system calls this before the emitter
    /// sync; tests and tools may call it directly).
    pub fn set_zone_field(&mut self, field: ZoneField) {
        self.zone_field = field;
    }

    /// Installs the rock field whose [`crate::RockField::sound_route`] routes emitters (the
    /// `AudioSync` system passes the [`crate::AudioRockField`]'s field every frame; `None` routes
    /// through the zones only).
    pub fn set_sound_paths(&mut self, field: Option<std::sync::Arc<dyn crate::RockField>>) {
        self.sound_paths = field;
    }

    /// The zones placed for the current frame.
    pub fn zone_field(&self) -> &ZoneField {
        &self.zone_field
    }

    /// The reverb-zone mix last applied (`None` without zones).
    pub fn reverb_mix(&self) -> Option<ReverbMix> {
        self.reverb.applied
    }
}
