// SPDX-License-Identifier: MPL-2.0
//! The audio plugin and its systems (`docs/specs/16-audio.md` § 3–4).

use std::time::Duration;

use regia_app::{App, Lifecycle, Plugin, PostUpdate, PostUpdateSet, RealTime, Update};
use regia_ecs::prelude::*;
use regia_platform::{LifecycleEvent, SoftwareRenderer, SurfacesHidden};

use crate::audio::{Audio, EmitterUpdate};
use crate::audio_zone::{AudioRockField, AudioZonePortals, PlacedZone, ZoneField, rock_thickness};
use crate::cues::CueAssets;
use crate::occlusion::AudioOcclusion;
use crate::reverb_zone::{AudioReverbZone, ReverbPreset, ReverbZoneShape, reverb_mix};
use crate::settings::AudioSettings;
use crate::sounds::SoundAssets;
use crate::spatial::{
    AudioActiveCamera, AudioEmitter, AudioListener, AudioListenerFollow, EmitterFrame, ListenerFrame,
    MotionTracker,
};
use regia_core::math::{Quat, Vec3};
use regia_scene::{GlobalTransform, Transform};

/// Adds the [`Audio`] resource and its systems: the one-shot drain in `Update`, the
/// `AudioSync` pass (listener/emitter sync + subsystem ticks) in `PostUpdate`, and suspend/resume
/// handling on the `Lifecycle` schedule (Android: the output stream is freed on `Suspended` and
/// rebuilt on `Resumed`).
#[derive(Default)]
pub struct AudioPlugin;

impl Plugin for AudioPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AudioSettings>()
            .init_resource::<AudioThrottle>()
            .init_resource::<crate::stall::AudioStallWatchdog>()
            .init_resource::<AudioResumeRetry>()
            .register_type::<AudioThrottle>();
        let settings = app.world().resource::<AudioSettings>().clone();
        let audio = Audio::new(&settings);
        app.insert_resource(audio)
            .init_resource::<SoundAssets>()
            .init_resource::<CueAssets>()
            .init_resource::<AudioOcclusion>()
            .init_resource::<AudioRockField>()
            .init_resource::<AudioListenerFollow>()
            .init_resource::<AudioActiveCamera>()
            .register_type::<AudioListenerFollow>()
            .add_message::<regia_asset::SourceAssetChanged>()
            .add_systems(Update, crate::sounds::reload_sound_files)
            .register_type::<AudioListener>()
            .register_type::<AudioEmitter>()
            .register_type::<crate::voice::VoiceStreamAnchor>()
            .register_type::<AudioReverbZone>()
            .register_type::<ReverbZoneShape>()
            .register_type::<ReverbPreset>()
            .register_type::<crate::audio_zone::ZonePortal>()
            .register_type::<AudioZonePortals>()
            .register_type::<crate::coupling::ListenerCoupling>()
            .register_type::<crate::coupling::AudioPropagation>()
            .register_type::<crate::coupling::AudioInteriorVolume>()
            .add_message::<regia_scene::streaming::OriginShifted>()
            .add_message::<crate::music::MusicBeat>()
            .add_systems(Update, drain_one_shots)
            // Video players' PCM feeds onto their buses (ADR-1670).
            .init_resource::<crate::media_feed::MediaAudioRegistry>()
            .add_systems(Update, crate::media_feed::attach_media_feeds)
            .add_systems(
                PostUpdate,
                (
                    audio_sync.in_set(PostUpdateSet::AudioSync),
                    audio_output_gate.in_set(PostUpdateSet::AudioSync),
                    // A live output whose clock stopped is reopened (ADR-8830); after the gate,
                    // so a pause the gate makes this frame is seen as a suspend.
                    crate::stall::audio_stall_watchdog
                        .in_set(PostUpdateSet::AudioSync)
                        .after(audio_output_gate)
                        .after(audio_sync),
                    audio_resume_retry
                        .in_set(PostUpdateSet::AudioSync)
                        .after(audio_output_gate),
                    crate::propagation_sync::audio_propagation_sync
                        .in_set(PostUpdateSet::AudioSync)
                        .before(audio_sync),
                    crate::propagation_sync::ground_haptics
                        .in_set(PostUpdateSet::AudioSync)
                        .after(audio_sync),
                    crate::audio::send_music_beats
                        .in_set(PostUpdateSet::AudioSync)
                        .after(audio_sync),
                ),
            )
            .add_systems(Lifecycle, audio_lifecycle)
            // Ambience that follows the weather, point spots and their calls (ADR-2442); each
            // system runs only while its resource or a spot exists.
            .init_resource::<crate::ambience::AmbienceSpotState>()
            .add_systems(
                PostUpdate,
                (
                    crate::ambience::ambience_bed_sync
                        .run_if(resource_exists::<crate::ambience::AmbienceBed>),
                    crate::ambience::ambience_bed_cleanup.after(crate::ambience::ambience_bed_sync),
                    crate::ambience::ambient_spot_sync.run_if(
                        any_with_component::<crate::ambience::AmbientSpot>
                            .or_else(any_with_component::<crate::ambience::AmbientCall>),
                    ),
                    crate::ambience::ambience_file_sync,
                )
                    .before(PostUpdateSet::AudioSync),
            )
            // Patch parameters bound to reflected fields (`.audiopatch.ron` v2, ADR-7226).
            .init_resource::<crate::patch_binding::PatchBindingState>()
            .add_systems(
                PostUpdate,
                crate::patch_binding::patch_binding_sync
                    .run_if(any_with_component::<crate::patch_binding::AudioPatchSource>)
                    .before(PostUpdateSet::AudioSync),
            );
    }
}

/// `Update` system: drains the queued one-shots into the audio backend. Inert when the `Audio`
/// resource is absent.
///
/// Systems that enqueue one-shots and need them counted in the same frame should be ordered
/// `before(drain_one_shots)`; without ordering the drain may run first and the one-shots are then
/// handed to the backend on the next frame.
pub fn drain_one_shots(audio: Option<ResMut<Audio>>) {
    if let Some(mut audio) = audio {
        audio.drain();
    }
}

/// World-space position and rotation: the `GlobalTransform` (parented entities sit where they
/// are drawn), else the local `Transform`, else the origin. `Transform` requires a
/// `GlobalTransform`, so an app without the scene plugin's propagation has identity globals on
/// moved entities: an identity global next to a non-identity local is read as "not
/// propagated" and the local pose is used (exact for root entities).
pub(crate) fn pose(global: Option<&GlobalTransform>, local: Option<&Transform>) -> (Vec3, Quat) {
    let unpropagated = |g: &GlobalTransform| {
        *g == GlobalTransform::IDENTITY && local.is_some_and(|l| *l != Transform::IDENTITY)
    };
    match (global, local) {
        (Some(global), _) if !unpropagated(global) => {
            let (_, rotation, translation) = global.0.to_scale_rotation_translation();
            (translation, rotation.normalize())
        }
        (_, Some(local)) => (local.translation, local.rotation),
        _ => (Vec3::ZERO, Quat::IDENTITY),
    }
}

/// Places the zones sorted by entity, so equal-specificity zones blend in the same order the
/// agent probe ([`placed_zones`]) uses, whatever the archetype order.
fn place_zones(zones: &ZoneQuery<'_, '_>) -> ZoneField {
    let mut placed: Vec<(Entity, PlacedZone)> = zones
        .iter()
        .map(|(entity, zone, global, local, portals)| {
            let (centre, rotation) = pose(global, local);
            let portals = portals.map(|p| p.portals.as_slice()).unwrap_or(&[]);
            (entity, PlacedZone::new(zone.clone(), centre, rotation, portals))
        })
        .collect();
    placed.sort_by_key(|(e, _)| *e);
    ZoneField {
        zones: placed.into_iter().map(|(_, z)| z).collect(),
    }
}

/// The zones of `world` as the audio sync places them, with each zone's entity (tools and the
/// agent protocol).
pub fn placed_zones(world: &mut World) -> Vec<(Entity, PlacedZone)> {
    let mut query = world.query::<(
        Entity,
        &AudioReverbZone,
        Option<&GlobalTransform>,
        Option<&Transform>,
        Option<&AudioZonePortals>,
    )>();
    let mut out: Vec<(Entity, PlacedZone)> = query
        .iter(world)
        .map(|(entity, zone, global, local, portals)| {
            let (centre, rotation) = pose(global, local);
            let portals = portals.map(|p| p.portals.as_slice()).unwrap_or(&[]);
            (entity, PlacedZone::new(zone.clone(), centre, rotation, portals))
        })
        .collect();
    out.sort_by_key(|(e, _)| *e);
    out
}

/// Per-frame motion state of the audio sync (velocity estimation).
#[derive(Default)]
pub struct AudioSyncState {
    listeners: MotionTracker,
    emitters: MotionTracker,
    /// Whether the voice-anchor query matched last frame (the stream resets to the flat center
    /// on the frame the last anchor disappears, so a despawned or parked anchor past
    /// `max_distance` cannot leave the loopback silent).
    had_voice_anchor: bool,
    /// Scratch for the anchor's listener list (reused every frame: no per-frame allocation).
    voice_listener_frames: Vec<ListenerFrame>,
}

/// The ECS queries [`audio_sync`] reads.
type ListenerQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static AudioListener,
        Option<&'static GlobalTransform>,
        Option<&'static Transform>,
    ),
>;
type EmitterQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static AudioEmitter,
        Option<&'static GlobalTransform>,
        Option<&'static Transform>,
    ),
    Without<regia_scene::layers::LayerParked>,
>;
type ZoneQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static AudioReverbZone,
        Option<&'static GlobalTransform>,
        Option<&'static Transform>,
        Option<&'static AudioZonePortals>,
    ),
>;

/// The ECS query for the voice loopback stream's anchor (spec 16 § 3b).
type VoiceAnchorQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static crate::voice::VoiceStreamAnchor,
        Option<&'static GlobalTransform>,
        Option<&'static Transform>,
    ),
    Without<regia_scene::layers::LayerParked>,
>;

/// `PostUpdate`/`AudioSync` system (after transform propagation): places the listeners and
/// emitters from their world transforms, derives their velocities from the real frame delta,
/// autoplays/stops entity emitters, exchanges occlusion rays with the provider, applies
/// reverb zones, then advances ducking, snapshots, virtualisation, patches, music bars and the
/// announcer by the frame delta and drains voice capture into the loopback stream.
///
/// The gameplay pause (`Time::paused`, ADR-10105) holds every voice on a game bus in place
/// ([`Audio::set_game_paused`]); while it is on, the mixer bookkeeping (ducking, snapshots, voice
/// chat, the UI and music buses) advances by the real frame delta so a pause screen still sounds
/// right, and the world parts of the tick take a zero step.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(target = "regia_audio", skip_all, level = "trace")]
pub fn audio_sync(
    time: Res<regia_app::Time>,
    real: Option<Res<RealTime>>,
    mut audio: ResMut<Audio>,
    sounds: Option<Res<SoundAssets>>,
    cues: Option<Res<CueAssets>>,
    mut occlusion: Option<ResMut<AudioOcclusion>>,
    rock: Option<Res<AudioRockField>>,
    mut state: Local<AudioSyncState>,
    listeners: ListenerQuery,
    emitters: EmitterQuery,
    zones: ZoneQuery,
    mut origin_shifts: MessageReader<regia_scene::streaming::OriginShifted>,
    follow: Option<Res<AudioListenerFollow>>,
    camera: Option<Res<AudioActiveCamera>>,
    voice_anchors: VoiceAnchorQuery,
) {
    let _span = tracing::debug_span!("audio_sync").entered();
    let dt = time.delta_secs();
    for message in origin_shifts.read() {
        let shift = Vec3::from_array(message.rebase.shift);
        state.listeners.shift_origin(shift);
        state.emitters.shift_origin(shift);
    }

    // Listeners, sorted by entity so player 0 (the first) is stable. With none in the world the
    // ears follow the active camera, or stay at the origin facing −Z.
    let mut frames: Vec<(Option<Entity>, ListenerFrame)> = listeners
        .iter()
        .map(|(entity, listener, global, local)| {
            let (position, rotation) = pose(global, local);
            let mut frame = ListenerFrame::from_rotation(position, rotation);
            frame.velocity = state.listeners.update(entity, position, dt);
            frame.hrtf = listener.hrtf;
            (Some(entity), frame)
        })
        .collect();
    frames.sort_by_key(|(entity, _)| *entity);
    if frames.is_empty()
        && follow.as_deref().copied().unwrap_or_default() == AudioListenerFollow::ActiveCamera
        && let Some(pose) = camera.as_deref().and_then(|camera| camera.0)
    {
        let mut frame = ListenerFrame::from_rotation(pose.position, pose.rotation);
        frame.velocity = state.listeners.update(pose.entity, pose.position, dt);
        frames.push((Some(pose.entity), frame));
    }
    state.listeners.retain_seen();
    audio.set_listeners(&frames);

    // Emitters.
    let mut updates: Vec<EmitterUpdate> = emitters
        .iter()
        .map(|(entity, emitter, global, local)| {
            let (position, rotation) = pose(global, local);
            EmitterUpdate {
                entity,
                emitter: emitter.clone(),
                frame: EmitterFrame {
                    position,
                    velocity: state.emitters.update(entity, position, dt),
                    forward: rotation * Vec3::NEG_Z,
                },
            }
        })
        .collect();
    updates.sort_by_key(|u| u.entity);
    state.emitters.retain_seen();
    // The zones, placed once for the distance scale, mouth routing, rock occlusion and reverb.
    let field = place_zones(&zones);
    // Rock-thickness answers for the rays queued last frame (the physics provider drains the
    // obstacle copies; these are separate).
    if let Some(occ) = occlusion.as_deref_mut() {
        let rays = occ.take_pending_rock();
        if let Some(rock) = rock.as_deref().and_then(AudioRockField::get) {
            let tuning = occ.rock;
            for ray in rays {
                occ.report_thickness(
                    ray.emitter,
                    rock_thickness(rock, &field, &tuning, ray.from, ray.to),
                );
            }
        } else {
            // No rock field (never installed, or cleared): drop stale answers.
            occ.clear_thickness();
        }
    }
    audio.set_zone_field(field);
    audio.set_sound_paths(rock.as_deref().and_then(AudioRockField::shared));
    audio.sync_emitter_frames(
        &updates,
        dt,
        sounds.as_deref(),
        cues.as_deref(),
        occlusion.as_deref_mut(),
    );

    // The voice loopback stream's anchor (spec 16 § 3b): the first anchor by entity id places
    // the running stream like an emitter at the entity's position. On the frame the last anchor
    // disappears (despawned, or parked and filtered out) the stream resets to the flat center:
    // an anchor that went away past max_distance left the placement at gain 0, which would stay
    // silent until something placed the stream again. With no anchor ever, the stream keeps its
    // current placement (a game may place it itself).
    if let Some((_, anchor, global, local)) = voice_anchors.iter().min_by_key(|(entity, ..)| *entity) {
        let (position, _) = pose(global, local);
        state
            .voice_listener_frames
            .extend(audio.listeners().iter().map(|(_, frame)| *frame));
        let placement = crate::voice::stream_placement(anchor, position, &state.voice_listener_frames);
        state.voice_listener_frames.clear();
        audio.set_voice_stream_placement(placement);
        state.had_voice_anchor = true;
    } else if state.had_voice_anchor {
        audio.set_voice_stream_placement(crate::voice_chat::VoicePlacement::Center);
        state.had_voice_anchor = false;
    }

    // Reverb zones follow player 0; a rock field over cooked geometry arms the reverb of the
    // section player 0 stands in (W-25).
    let player0 = audio
        .listeners()
        .first()
        .map(|(_, f)| f.position)
        .unwrap_or(Vec3::ZERO);
    let armed = rock
        .as_deref()
        .and_then(AudioRockField::get)
        .and_then(|r| r.armed_zone(player0));
    let mix = (!audio.zone_field().is_empty() || armed.is_some()).then(|| {
        reverb_mix(
            player0,
            audio
                .zone_field()
                .zones
                .iter()
                .map(|z| (&z.zone, z.centre, z.rotation))
                .chain(armed.as_ref().map(|a| (&a.zone, player0, Quat::IDENTITY))),
        )
    });
    // The most specific zone around player 0 may name an impulse response (ADR-2721).
    let zone_ir = mix.and_then(|_| {
        let listener = audio
            .listeners()
            .first()
            .map(|(_, f)| f.position)
            .unwrap_or(Vec3::ZERO);
        crate::reverb_zone::zone_ir(
            listener,
            audio
                .zone_field()
                .zones
                .iter()
                .map(|z| (&z.zone, z.centre, z.rotation)),
        )
        .map(str::to_string)
    });
    audio.set_reverb_zone_ir(zone_ir.as_deref());
    audio.set_reverb_mix(mix);

    // A gameplay pause holds the world voices; the mixer itself runs on the real frame time
    // (zero while suspended, when `Time` is held for the OS and the output is down anyway).
    let game_paused = time.paused && !time.is_suspended();
    audio.set_game_paused(game_paused);
    let dt = if game_paused {
        real.as_deref().map_or(0.0, |real| real.delta().as_secs_f64())
    } else {
        f64::from(dt)
    };
    audio.tick(dt);
    let _ = audio.tick_voice(dt);
    // Surface cpal stream errors (device change, disconnection; kira restarts the stream itself,
    // spec § 4) so they are visible in the log without game code.
    for error in audio.take_output_errors() {
        tracing::warn!(target: "regia_audio", "audio output stream: {error}");
    }
}

/// `Lifecycle` system: frees the output stream on `Suspended` and rebuilds it on `Resumed`
/// (Android spec § 4; inert when the platform plugin is absent, e.g. unit tests).
///
/// A `Resumed` whose re-open fails (no device yet, e.g. a Bluetooth headset reconnecting) arms
/// [`AudioResumeRetry`]: `audio_resume_retry` tries again every [`RESUME_RETRY_FRAMES`] frames,
/// up to [`RESUME_RETRY_ATTEMPTS`] times, with one warning for the whole episode.
pub fn audio_lifecycle(
    lifecycle: Option<Res<regia_platform::AppLifecycle>>,
    mut audio: Option<ResMut<Audio>>,
    mut retry: Option<ResMut<AudioResumeRetry>>,
) {
    let _span = tracing::debug_span!("audio_lifecycle").entered();
    let Some(lifecycle) = lifecycle else {
        return;
    };
    let Some(audio) = audio.as_deref_mut() else {
        return;
    };
    match lifecycle.last_event {
        Some(LifecycleEvent::Suspended) => {
            audio.suspend_output();
            if let Some(retry) = retry.as_deref_mut() {
                *retry = AudioResumeRetry::default();
            }
        }
        Some(LifecycleEvent::Resumed) => {
            let opened = audio.resume_output();
            // A render-out take defers the resume to its end (`end_mixdown`); nothing to retry.
            if let Some(retry) = retry.as_deref_mut() {
                *retry = AudioResumeRetry::default();
                if !opened && !audio.mixdown_active() {
                    *retry = AudioResumeRetry {
                        failures: 1,
                        next_in_frames: RESUME_RETRY_FRAMES,
                        gave_up: false,
                    };
                    tracing::warn!(
                        target: "regia_audio",
                        "audio output did not re-open at Resumed (no device yet); retrying every {RESUME_RETRY_FRAMES} frames, up to {RESUME_RETRY_ATTEMPTS} times (audio.mix output.resume)"
                    );
                }
            }
        }
        _ => {}
    }
}

/// `PostUpdate`/`AudioSync` system: retries a lifecycle resume that could not re-open the output
/// ([`AudioResumeRetry`], armed by [`audio_lifecycle`]). Counts frames, not time, so a hitch
/// does not skip attempts. Stops when the output is live again (re-opened here, by the output
/// gate or by the host), after [`RESUME_RETRY_ATTEMPTS`] failed retries (`gave_up`), or when the
/// app is suspended again.
pub fn audio_resume_retry(
    lifecycle: Option<Res<regia_platform::AppLifecycle>>,
    mut audio: Option<ResMut<Audio>>,
    mut retry: Option<ResMut<AudioResumeRetry>>,
) {
    let (Some(audio), Some(retry)) = (audio.as_deref_mut(), retry.as_deref_mut()) else {
        return;
    };
    if retry.next_in_frames().is_none() {
        return;
    }
    let _span = tracing::debug_span!("audio_resume_retry").entered();
    let suspended_again = lifecycle
        .as_ref()
        .is_some_and(|l| l.state == regia_platform::LifecycleState::Suspended);
    if !audio.output_suspended() || suspended_again {
        *retry = AudioResumeRetry::default();
        return;
    }
    // A render-out take holds the bus tree; its end applies the deferred resume.
    if audio.mixdown_active() {
        return;
    }
    retry.next_in_frames = retry.next_in_frames.saturating_sub(1);
    if retry.next_in_frames > 0 {
        return;
    }
    if audio.resume_output() {
        tracing::info!(
            target: "regia_audio",
            failures = retry.failures,
            "audio output re-opened after a failed lifecycle resume"
        );
        *retry = AudioResumeRetry::default();
    } else {
        retry.failures += 1;
        if retry.failures > RESUME_RETRY_ATTEMPTS {
            retry.gave_up = true;
            tracing::info!(
                target: "regia_audio",
                failures = retry.failures,
                "audio output still closed; lifecycle resume retries stopped until the next resume"
            );
        } else {
            retry.next_in_frames = RESUME_RETRY_FRAMES;
        }
    }
}

/// A frame slower than this cannot keep a realtime output stream fed: below ~20 fps the frame
/// loop is CPU-bound, which starves kira's realtime audio callback and produces cpal
/// "buffer underrun or overrun" errors. This is what the M18 soak hit on a *visible* Xvfb window
/// when lavapipe (llvmpipe) software rendering saturated every core — the hidden-window path
/// (`SurfacesHidden`) was already covered, this threshold catches the visible-but-slow case.
const THROTTLE_FRAME: Duration = Duration::from_millis(50);
/// Consecutive slow frames before the output pauses (a lone hitch must not silence audio).
const THROTTLE_FRAMES: u32 = 5;

/// Whether the audio output gate (`audio_output_gate`) may pause the output because the frame
/// loop is throttled (sustained slow frames). The hidden-window pause applies in every mode.
///
/// The slow-frame pause exists for software rendering (Xvfb + lavapipe, the M18 soak), where the
/// CPU-bound frame loop starves the realtime audio callback. On a real GPU, slow frames are
/// ordinary hitches (map preload, texture streaming, a camera cut) while the audio thread is fed
/// fine, and pausing there ends every voice: an audible drop-out. Insert this resource before or
/// after adding [`AudioPlugin`] to override the default; it is read every frame.
#[derive(Resource, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Resource, Default, Debug)]
pub enum AudioThrottle {
    /// Arm the slow-frame pause only while frames are rendered by a software (CPU) adapter
    /// ([`SoftwareRenderer`], published by the renderer). Off on a GPU and before any device
    /// exists.
    #[default]
    Auto,
    /// Always arm the slow-frame pause (the v0.1.6 behavior).
    On,
    /// Never pause the output because of slow frames.
    Off,
}

impl AudioThrottle {
    /// True when slow frames may pause the output, given whether a software adapter renders.
    #[must_use]
    pub const fn armed(self, software_renderer: bool) -> bool {
        match self {
            Self::Auto => software_renderer,
            Self::On => true,
            Self::Off => false,
        }
    }
}

/// What [`audio_output_gate`] must do on a pause-state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputAction {
    /// The output stream should now be paused.
    Suspend,
    /// The output stream should now be resumed.
    Resume,
}

/// The action for a pause-state transition (pure; unit-tested). `None` while the state is
/// unchanged or the app starts unpaused.
fn output_action(was_paused: bool, now_paused: bool) -> Option<OutputAction> {
    match (was_paused, now_paused) {
        (false, true) => Some(OutputAction::Suspend),
        (true, false) => Some(OutputAction::Resume),
        _ => None,
    }
}

/// True when the output is suspended and no resume has re-opened it since.
fn resume_pending(audio: &Audio) -> bool {
    audio.output_suspended() && audio.resume_count() < audio.suspend_count()
}

/// `PostUpdate`/`AudioSync` system: pauses the output stream while it cannot be fed in realtime
/// and resumes it when it can again.
///
/// Two conditions pause the stream. (1) Every window surface is hidden (desktop minimise ≈
/// throttled frames, `specs/11` § 2); this applies in every [`AudioThrottle`] mode. (2) The frame
/// loop itself is throttled — [`THROTTLE_FRAMES`] consecutive frames slower than
/// [`THROTTLE_FRAME`] — when [`AudioThrottle`] arms it (`Auto`: only on a software adapter). That
/// is the *visible* Xvfb + lavapipe case
/// from the M18 soak: software rendering saturates every core and starves the realtime audio
/// callback, producing cpal underruns even though no surface is hidden. A minimised or CPU-bound
/// game is effectively silent anyway, so pausing the stream (the same Android suspend/resume path)
/// is the correct behaviour and stops the underruns. Inert when the platform plugin is absent
/// (headless tests) and on Android, where surfaces map to `Suspended`/`Resumed` handled by
/// [`audio_lifecycle`] instead.
///
/// The resume is skipped when the output is already live (`resume_count() >=
/// suspend_count()`): a host that re-opened the stream itself in `Update` keeps its restarted
/// sounds and bus volumes. A resume of the gate's own that fails (no device at that moment, e.g.
/// a headset switch) is retried every [`RESUME_RETRY`] while the output is still suspended.
#[allow(clippy::too_many_arguments)]
pub fn audio_output_gate(
    hidden: Option<Res<SurfacesHidden>>,
    activity: Option<Res<regia_platform::ActivityState>>,
    throttle: Option<Res<AudioThrottle>>,
    software: Option<Res<SoftwareRenderer>>,
    real: Res<RealTime>,
    mut audio: Option<ResMut<Audio>>,
    mut slow_streak: Local<u32>,
    mut was_paused: Local<bool>,
    mut retry_in: Local<Option<Duration>>,
) {
    let _span = tracing::debug_span!("audio_output_gate").entered();
    let Some(audio) = audio.as_deref_mut() else {
        return;
    };
    // A render-out mixdown is not a realtime stream: its frames are as slow as the GPU makes
    // them, and a pause would end every voice mid-take (ADR-1520). They must not count toward
    // the consecutive-slow-frame streak either: reset it, so a hitch before a mixdown cannot
    // suspend the output a frame after the take (ADR-187: *consecutive* slow frames).
    if audio.mixdown_active() {
        *slow_streak = 0;
        return;
    }
    // `onPause` counts as hidden: winit tears the window down only after onStop.
    let surfaces_hidden = hidden.as_ref().is_some_and(|h| h.0) || activity.as_ref().is_some_and(|a| a.paused);
    let armed = throttle
        .as_deref()
        .copied()
        .unwrap_or_default()
        .armed(software.as_ref().is_some_and(|s| s.0));
    let slow = real.delta() >= THROTTLE_FRAME;
    *slow_streak = if slow { slow_streak.saturating_add(1) } else { 0 };
    let throttled = armed && *slow_streak >= THROTTLE_FRAMES;
    match output_action(*was_paused, surfaces_hidden || throttled) {
        Some(OutputAction::Suspend) => {
            let before = audio.suspend_count();
            audio.suspend_output();
            let suspended = audio.suspend_count() > before;
            if surfaces_hidden {
                tracing::info!(
                    target: "regia_audio",
                    suspended,
                    "audio output paused: every window is hidden"
                );
            } else {
                if suspended {
                    audio.note_throttle_suspend();
                }
                tracing::warn!(
                    target: "regia_audio",
                    suspended,
                    streak = *slow_streak,
                    frame_ms = real.delta().as_secs_f64() * 1e3,
                    throttle_suspends = audio.throttle_suspend_count(),
                    "audio output paused: {} consecutive frames slower than {} ms (AudioThrottle armed)",
                    *slow_streak,
                    THROTTLE_FRAME.as_millis()
                );
            }
        }
        // A host that already re-opened the stream this frame (or never let it close) owns the
        // live mixer: a second resume would be a no-op anyway, and is skipped explicitly so the
        // gate never rebuilds what the host restarted.
        Some(OutputAction::Resume) if resume_pending(audio) => {
            *retry_in = (!audio.resume_output()).then_some(RESUME_RETRY);
        }
        Some(OutputAction::Resume) | None => {}
    }
    *was_paused = surfaces_hidden || throttled;
    // Retry only a resume the gate itself attempted: a host's own suspend is left alone.
    if let Some(left) = *retry_in {
        if *was_paused || !resume_pending(audio) {
            *retry_in = None;
        } else if left <= real.delta() {
            *retry_in = (!audio.resume_output()).then_some(RESUME_RETRY);
        } else {
            *retry_in = Some(left - real.delta());
        }
    }
}

/// How often [`audio_output_gate`] retries a resume of its own that could not open the device.
pub const RESUME_RETRY: Duration = Duration::from_secs(1);

/// Frames between two retries of a lifecycle `Resumed` that could not re-open the output.
pub const RESUME_RETRY_FRAMES: u32 = 30;

/// Retries of one failed lifecycle resume before the engine stops trying (about 10 s at 60 fps);
/// the next `Suspended` → `Resumed` cycle starts again.
pub const RESUME_RETRY_ATTEMPTS: u32 = 20;

/// The engine's retry of a lifecycle `Resumed` whose [`Audio::resume_output`] could not open the
/// device (deep-zai audio-mix finding 11). `audio_lifecycle` arms it, `audio_resume_retry`
/// retries every [`RESUME_RETRY_FRAMES`] frames, up to [`RESUME_RETRY_ATTEMPTS`] times. Read by
/// `audio.mix` (`output`).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioResumeRetry {
    failures: u32,
    next_in_frames: u32,
    gave_up: bool,
}

impl AudioResumeRetry {
    /// Failed resume attempts since the last lifecycle `Resumed` (0 while nothing failed or
    /// after the output re-opened).
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Frames until the next retry, while one is scheduled.
    #[must_use]
    pub fn next_in_frames(&self) -> Option<u32> {
        (self.failures > 0 && !self.gave_up).then_some(self.next_in_frames)
    }

    /// True once [`RESUME_RETRY_ATTEMPTS`] retries failed and the engine stopped retrying.
    #[must_use]
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// `"idle"` (nothing to retry), `"retrying"` or `"gave_up"`.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match (self.failures, self.gave_up) {
            (0, _) => "idle",
            (_, false) => "retrying",
            (_, true) => "gave_up",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::{App, TimeUpdateStrategy};
    use std::time::Duration;

    #[test]
    fn output_action_covers_all_transitions() {
        assert_eq!(output_action(false, false), None);
        assert_eq!(output_action(true, true), None);
        assert_eq!(output_action(false, true), Some(OutputAction::Suspend));
        assert_eq!(output_action(true, false), Some(OutputAction::Resume));
    }

    /// Spec 16 § 3b: on the frame the last `VoiceStreamAnchor` disappears the loopback resets to
    /// the flat center (an anchor that went away past `max_distance` left the placement at gain
    /// 0 and would otherwise keep the loopback silent).
    #[test]
    fn a_removed_anchor_resets_the_stream_to_center() {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.add_plugins(AudioPlugin);
        app.world_mut()
            .resource_mut::<Audio>()
            .start_voice_loopback(40)
            .expect("loopback");
        app.world_mut()
            .spawn((crate::AudioListener::default(), GlobalTransform::default()));
        let anchor = app
            .world_mut()
            .spawn((
                crate::voice::VoiceStreamAnchor::default(),
                GlobalTransform::IDENTITY.mul_transform(&Transform::from_translation(
                    regia_core::math::Vec3::new(4.0, 0.0, 0.0),
                )),
            ))
            .id();
        app.update();
        let placement = app
            .world()
            .resource::<Audio>()
            .voice_stream_placement()
            .expect("the anchor placed the stream");
        assert!(
            matches!(placement, crate::voice_chat::VoicePlacement::Spatial { .. }),
            "the anchored stream is spatial: {placement:?}"
        );

        app.world_mut().despawn(anchor);
        app.update();
        let placement = app
            .world()
            .resource::<Audio>()
            .voice_stream_placement()
            .expect("still placed");
        assert_eq!(
            placement,
            crate::voice_chat::VoicePlacement::Center,
            "the frame the last anchor disappears resets to the flat center"
        );
    }

    #[test]
    fn surfaces_hidden_pauses_and_resumes_the_output() {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.insert_resource(SurfacesHidden(false));
        app.add_plugins(AudioPlugin);
        app.update(); // build the plugin and run the first frame

        let suspend = |app: &App| app.world().resource::<Audio>().suspend_count();
        let resume = |app: &App| app.world().resource::<Audio>().resume_count();
        assert_eq!((suspend(&app), resume(&app)), (0, 0));

        // Visible → hidden pauses the stream once.
        app.world_mut().resource_mut::<SurfacesHidden>().0 = true;
        app.update();
        assert_eq!(suspend(&app), 1, "hiding suspends the output");
        assert_eq!(resume(&app), 0);

        // Staying hidden is a no-op (no repeat suspend on every throttled frame).
        app.update();
        assert_eq!(suspend(&app), 1, "a still-hidden surface does not re-suspend");

        // Hidden → visible resumes the stream.
        app.world_mut().resource_mut::<SurfacesHidden>().0 = false;
        app.update();
        assert_eq!(suspend(&app), 1);
        assert_eq!(resume(&app), 1, "showing resumes the output");

        // A second hide/suspend proves the state machine cycles.
        app.world_mut().resource_mut::<SurfacesHidden>().0 = true;
        app.update();
        assert_eq!(suspend(&app), 2, "a second hide suspends again");
    }

    #[test]
    fn slow_frames_pause_and_fast_frames_resume_the_output() {
        // A *visible* window whose frame loop is CPU-bound (the M18 lavapipe underrun case):
        // sustained slow frames pause the output stream, and a recovery resumes it. Driven
        // deterministically with a manual fixed step — no wall-clock dependency. `On` is the
        // v0.1.6 behavior, armed whatever the adapter.
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.insert_resource(SurfacesHidden(false));
        app.insert_resource(AudioThrottle::On);
        app.add_plugins(AudioPlugin);
        app.update(); // build the plugin and run the first frame

        let suspend = |app: &App| app.world().resource::<Audio>().suspend_count();
        let resume = |app: &App| app.world().resource::<Audio>().resume_count();
        assert_eq!((suspend(&app), resume(&app)), (0, 0));

        // Slow frames (100 ms each): fewer than THROTTLE_FRAMES of them must not pause yet.
        let slow = TimeUpdateStrategy::ManualDuration(Duration::from_millis(100));
        app.insert_resource(slow);
        for _ in 0..(THROTTLE_FRAMES - 1) {
            app.update();
        }
        assert_eq!(suspend(&app), 0, "a lone hitch does not silence audio");
        assert_eq!(resume(&app), 0);

        // One more slow frame crosses the streak threshold and pauses the output.
        app.update();
        assert_eq!(suspend(&app), 1, "sustained slow frames pause the output");

        // Staying slow is a no-op.
        app.update();
        assert_eq!(suspend(&app), 1, "still-throttled frames do not re-suspend");

        // A single fast frame clears the streak and resumes.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.update();
        assert_eq!(suspend(&app), 1);
        assert_eq!(resume(&app), 1, "fast frames resume the output");
    }

    /// A headless audio app with a visible window, the given throttle mode and (when `Some`) the
    /// renderer's software-adapter fact, after its first frame.
    fn gate_app(throttle: Option<AudioThrottle>, software: Option<bool>) -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.insert_resource(SurfacesHidden(false));
        if let Some(throttle) = throttle {
            app.insert_resource(throttle);
        }
        if let Some(software) = software {
            app.insert_resource(SoftwareRenderer(software));
        }
        app.add_plugins(AudioPlugin);
        app.update();
        app
    }

    /// Runs `frames` frames of 100 ms each and returns `(suspend_count, throttle_suspend_count)`.
    fn run_slow(app: &mut App, frames: u32) -> (u64, u64) {
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
        for _ in 0..frames {
            app.update();
        }
        let audio = app.world().resource::<Audio>();
        (audio.suspend_count(), audio.throttle_suspend_count())
    }

    #[test]
    fn throttle_modes_arm_as_documented() {
        assert!(AudioThrottle::Auto.armed(true));
        assert!(!AudioThrottle::Auto.armed(false));
        assert!(AudioThrottle::On.armed(false) && AudioThrottle::On.armed(true));
        assert!(!AudioThrottle::Off.armed(false) && !AudioThrottle::Off.armed(true));
        assert_eq!(AudioThrottle::default(), AudioThrottle::Auto);
    }

    #[test]
    fn auto_is_the_plugin_default() {
        let app = gate_app(None, None);
        assert_eq!(*app.world().resource::<AudioThrottle>(), AudioThrottle::Auto);
        // A host's own choice, inserted before the plugin, is kept.
        let app = gate_app(Some(AudioThrottle::Off), None);
        assert_eq!(*app.world().resource::<AudioThrottle>(), AudioThrottle::Off);
    }

    #[test]
    fn auto_on_a_hardware_adapter_ignores_slow_frames() {
        // The DO27 RX 7900 XTX case: hitches on a real GPU must not drop the audio.
        let mut app = gate_app(None, Some(false));
        assert_eq!(
            run_slow(&mut app, 10),
            (0, 0),
            "10 slow frames on a GPU do not suspend"
        );
        // Before any device exists (no fact yet) Auto is disarmed too.
        let mut app = gate_app(None, None);
        assert_eq!(
            run_slow(&mut app, 10),
            (0, 0),
            "no adapter yet: no slow-frame suspend"
        );
    }

    #[test]
    fn auto_on_a_software_adapter_suspends_on_slow_frames() {
        let mut app = gate_app(None, Some(true));
        assert_eq!(
            run_slow(&mut app, THROTTLE_FRAMES - 1),
            (0, 0),
            "a lone hitch is kept"
        );
        assert_eq!(run_slow(&mut app, 1), (1, 1), "lavapipe slow frames suspend once");
        assert_eq!(run_slow(&mut app, 5), (1, 1), "still slow: no repeat suspend");
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.update();
        assert_eq!(
            app.world().resource::<Audio>().resume_count(),
            1,
            "fast frames resume"
        );
    }

    #[test]
    fn auto_follows_a_fact_published_after_startup() {
        // The renderer publishes the fact when its device is created, after the first frames.
        let mut app = gate_app(None, None);
        assert_eq!(run_slow(&mut app, 3), (0, 0));
        app.insert_resource(SoftwareRenderer(true));
        assert_eq!(
            run_slow(&mut app, THROTTLE_FRAMES),
            (1, 1),
            "armed once the fact says CPU"
        );
    }

    #[test]
    fn off_never_suspends_on_slow_frames() {
        for software in [None, Some(false), Some(true)] {
            let mut app = gate_app(Some(AudioThrottle::Off), software);
            assert_eq!(run_slow(&mut app, 20), (0, 0), "Off, software {software:?}");
        }
    }

    #[test]
    fn on_suspends_on_slow_frames_on_any_adapter() {
        for software in [None, Some(false), Some(true)] {
            let mut app = gate_app(Some(AudioThrottle::On), software);
            assert_eq!(
                run_slow(&mut app, THROTTLE_FRAMES - 1),
                (0, 0),
                "On, software {software:?}"
            );
            assert_eq!(run_slow(&mut app, 1), (1, 1), "On, software {software:?}");
        }
    }

    #[test]
    fn slow_frames_never_suspend_a_render_out_mixdown() {
        let mut app = gate_app(Some(AudioThrottle::On), Some(true));
        app.world_mut()
            .resource_mut::<Audio>()
            .begin_mixdown(&crate::MixdownSettings::default())
            .expect("mixdown");
        assert_eq!(run_slow(&mut app, THROTTLE_FRAMES * 3), (0, 0));
        // Once the mixdown ends the throttle applies again.
        assert!(app.world_mut().resource_mut::<Audio>().end_mixdown());
        assert_eq!(run_slow(&mut app, THROTTLE_FRAMES), (1, 1));
    }

    #[test]
    fn a_mixdown_resets_the_slow_frame_streak() {
        // Deep-zai-audio-mix finding 15: slow frames before a mixdown must not count toward
        // the streak after it — the throttle promises *consecutive* frames (ADR-187).
        let mut app = gate_app(Some(AudioThrottle::On), Some(true));
        assert_eq!(run_slow(&mut app, THROTTLE_FRAMES - 1), (0, 0), "a streak builds");
        app.world_mut()
            .resource_mut::<Audio>()
            .begin_mixdown(&crate::MixdownSettings::default())
            .expect("mixdown");
        assert_eq!(
            run_slow(&mut app, THROTTLE_FRAMES),
            (0, 0),
            "mixdown frames do not count"
        );
        assert!(app.world_mut().resource_mut::<Audio>().end_mixdown());
        assert_eq!(
            run_slow(&mut app, THROTTLE_FRAMES - 1),
            (0, 0),
            "the pre-mixdown streak was reset, not frozen"
        );
        assert_eq!(run_slow(&mut app, 1), (1, 1), "a fresh streak suspends");
    }

    #[test]
    fn switching_to_off_while_throttled_resumes() {
        let mut app = gate_app(Some(AudioThrottle::On), Some(false));
        assert_eq!(run_slow(&mut app, THROTTLE_FRAMES), (1, 1));
        app.insert_resource(AudioThrottle::Off);
        app.update();
        assert_eq!(
            app.world().resource::<Audio>().resume_count(),
            1,
            "Off lifts the pause"
        );
    }

    #[test]
    fn hidden_surfaces_suspend_in_every_mode() {
        for throttle in [AudioThrottle::Auto, AudioThrottle::On, AudioThrottle::Off] {
            for software in [None, Some(false), Some(true)] {
                let mut app = gate_app(Some(throttle), software);
                app.world_mut().resource_mut::<SurfacesHidden>().0 = true;
                app.update();
                let audio = app.world().resource::<Audio>();
                assert_eq!(audio.suspend_count(), 1, "{throttle:?}, software {software:?}");
                assert_eq!(
                    audio.throttle_suspend_count(),
                    0,
                    "a hidden pause is not a throttle"
                );
                app.world_mut().resource_mut::<SurfacesHidden>().0 = false;
                app.update();
                assert_eq!(app.world().resource::<Audio>().resume_count(), 1);
            }
        }
    }

    /// A headless app with the platform's lifecycle resource and message, the way
    /// `regia_platform::apply_transition` drives a real `Suspended` / `Resumed`.
    fn lifecycle_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioSettings {
            headless: true,
            ..Default::default()
        });
        app.init_resource::<regia_platform::AppLifecycle>();
        app.add_message::<LifecycleEvent>();
        app.add_plugins(AudioPlugin);
        app.update();
        app
    }

    /// Deep-zai audio-mix finding 11: a `Resumed` whose output re-open fails (a fake output that
    /// fails its first 3 resumes) is retried every [`RESUME_RETRY_FRAMES`] frames, the state
    /// shows in [`AudioResumeRetry`], and the 4th attempt re-opens the output.
    #[test]
    fn a_failed_lifecycle_resume_is_retried_every_n_frames() {
        let mut app = lifecycle_app();
        regia_platform::apply_transition(&mut app, LifecycleEvent::Suspended);
        assert!(app.world().resource::<Audio>().output_suspended());
        app.world_mut().resource_mut::<Audio>().fail_next_resumes(3);
        regia_platform::apply_transition(&mut app, LifecycleEvent::Resumed);
        let retry = |app: &App| *app.world().resource::<AudioResumeRetry>();
        let audio = |app: &App| {
            let a = app.world().resource::<Audio>();
            (a.output_suspended(), a.resume_count())
        };
        assert_eq!(audio(&app), (true, 0), "the Resumed re-open failed");
        assert_eq!((retry(&app).failures(), retry(&app).state()), (1, "retrying"));
        assert_eq!(retry(&app).next_in_frames(), Some(RESUME_RETRY_FRAMES));
        for attempt in 2..=3u32 {
            for _ in 0..RESUME_RETRY_FRAMES - 1 {
                app.update();
            }
            assert_eq!(
                retry(&app).failures(),
                attempt - 1,
                "no retry inside the interval"
            );
            app.update();
            assert_eq!(retry(&app).failures(), attempt, "retry {attempt} ran and failed");
            assert_eq!(audio(&app), (true, 0));
        }
        for _ in 0..RESUME_RETRY_FRAMES {
            app.update();
        }
        assert_eq!(audio(&app), (false, 1), "the 4th attempt re-opened the output");
        assert_eq!((retry(&app).failures(), retry(&app).state()), (0, "idle"));
        assert_eq!(retry(&app).next_in_frames(), None);
    }

    /// The retry is bounded: after [`RESUME_RETRY_ATTEMPTS`] failed retries it stops and reads
    /// `gave_up`; the next suspend/resume cycle starts again (and here succeeds).
    #[test]
    fn a_lifecycle_resume_retry_gives_up_and_the_next_cycle_starts_again() {
        let mut app = lifecycle_app();
        regia_platform::apply_transition(&mut app, LifecycleEvent::Suspended);
        app.world_mut()
            .resource_mut::<Audio>()
            .fail_next_resumes(u32::MAX);
        regia_platform::apply_transition(&mut app, LifecycleEvent::Resumed);
        for _ in 0..(RESUME_RETRY_ATTEMPTS + 2) * RESUME_RETRY_FRAMES {
            app.update();
        }
        let retry = *app.world().resource::<AudioResumeRetry>();
        assert_eq!(retry.state(), "gave_up");
        assert!(retry.gave_up());
        assert_eq!(
            retry.failures(),
            RESUME_RETRY_ATTEMPTS + 1,
            "the first try plus every retry"
        );
        assert_eq!(retry.next_in_frames(), None);
        assert!(app.world().resource::<Audio>().output_suspended());

        app.world_mut().resource_mut::<Audio>().fail_next_resumes(0);
        regia_platform::apply_transition(&mut app, LifecycleEvent::Suspended);
        assert_eq!(app.world().resource::<AudioResumeRetry>().state(), "idle");
        regia_platform::apply_transition(&mut app, LifecycleEvent::Resumed);
        assert!(!app.world().resource::<Audio>().output_suspended());
        assert_eq!(app.world().resource::<AudioResumeRetry>().state(), "idle");
    }
}
