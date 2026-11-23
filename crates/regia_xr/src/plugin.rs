// SPDX-License-Identifier: MPL-2.0
//! [`XrPlugin`]: opens the configured backend, drives the session from runtime events, syncs
//! actions once per fixed step, moves the rig, head and controllers, and runs the XR frame loop
//! (spec 58 §§ 2.2-2.5, 3.3).
//!
//! System sets, in order: [`XrSet::Poll`] (`PreUpdate`: drain runtime events, react to the session
//! state) → [`XrSet::SyncInput`] (`FixedUpdate`, first: script player, retry counter, action sync,
//! held poses) → [`XrSet::UpdateRig`] (`FixedUpdate`: snap turn, vignette, head and controller
//! transforms) → game systems → [`XrSet::Extract`] (`Last`: the frame loop publishes into the
//! mailbox, extraction reads the freshest slot) → [`XrSet::Submit`] (reserved: layers are submitted
//! by the render system on the `Render` schedule, so nothing runs in this set yet).
//!
//! The simulator's frame loop runs on the main thread in `Last`; it never feeds back into the sim
//! (the sim sees only held poses sampled at the fixed-step start). The real binding's dedicated XR
//! frame thread arrives with the OpenXR backend (spec 58 § 3.4 gate).

use regia_app::prelude::*;
use regia_core::glam::{Mat4, Vec3};
use regia_input::InputInjection;
use regia_platform::xr::{
    ControllerState, DepthMode, DepthRange, Eye, Fov, Hand, NullConfig, NullXr, Pose, PoseSample,
    SessionState, SimEvent, Transition, View, XrBackend, XrError, XrEvent, XrRuntime,
};
use regia_scene::Transform;

use crate::budget::{OVER_BUDGET_CODE, XrFrameBudget, XrOverBudget, budget_ms_for, frame_time_ms};
use crate::comfort::{SnapTurn, Vignette, rig_turn};
use crate::config::{Foveation, Mirror, RENDER_SCALE_RANGE, XrBackendKind, XrConfig};
use crate::hands::XrHands;
use crate::input::{ActionPoses, XrActionManifest, XrActionSets, XrActionStates, XrActionValue};
use crate::mailbox::{XrFrameMailbox, XrFrameState};
use crate::sim::{ScriptState, SimPlayer, SimRecorder, XrSimScript, path_to_state};
use crate::space::{TrackingSpace, floor_offset, recenter_offset, resolve_tracking_space};
use crate::state::{XrAppState, XrRenderProfile, XrState, app_state_for};
use crate::stereo::StereoMode;

/// Marks the entity whose world transform is the tracking-space origin. Moving it moves the
/// player.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
pub struct XrRig;

/// The head: a child of the rig; its local transform is the head in rig-local space.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
pub struct XrHead;

/// A controller: a child of the rig at the grip pose.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrController {
    /// Which hand.
    pub hand: Hand,
}

/// A per-eye camera child (0 left, 1 right).
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, Reflect)]
pub struct XrEye {
    /// The eye index.
    pub index: u8,
}

/// The app state changed.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrStateChanged {
    /// The previous app state.
    pub from: XrAppState,
    /// The new app state.
    pub to: XrAppState,
}

/// Input focus changed (pauses nothing: game code decides).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrFocusChanged {
    /// Whether the app has input focus.
    pub focused: bool,
}

/// The tracking origin changed: a recenter, a tracking-space change or a runtime
/// reference-space change.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct XrOriginChanged {
    /// The space in use.
    pub space: TrackingSpace,
    /// The new rig-local offset.
    pub offset: Pose,
}

/// The runtime or device went away; the session was destroyed and will be re-created.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrRuntimeLost;

/// The interaction profile bound to a hand changed.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct XrInteractionProfileChanged {
    /// Which hand.
    pub hand: Hand,
    /// The profile path.
    pub profile: String,
}

/// The display refresh rate changed.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct XrRefreshRateChanged {
    /// The new rate, Hz.
    pub hz: f32,
}

/// The XR system sets, in run order.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XrSet {
    /// `PreUpdate`: runtime events.
    Poll,
    /// `FixedUpdate`, first: action sync and held poses.
    SyncInput,
    /// `FixedUpdate`: rig, head and controller transforms.
    UpdateRig,
    /// `Last`: the frame loop and the mailbox read.
    Extract,
    /// `Last`, after extraction. Reserved and currently empty: layer submission happens in the
    /// render system on the `Render` schedule.
    Submit,
}

/// The engine-side session bookkeeping (manifest, action sets, rig offset, comfort state, the
/// script player, the frame mailbox reader). Read by `xr.get`; changed through the functions of
/// this module.
#[derive(Resource)]
pub struct XrSession {
    /// The action manifest in use.
    pub manifest: XrActionManifest,
    sets: XrActionSets,
    /// The rig-local offset (recenter and emulated floor).
    pub offset: Pose,
    recenter: Pose,
    held_head: Pose,
    held_controllers: [Option<Pose>; 2],
    snap: SnapTurn,
    vignette: Vignette,
    last_rig: Option<Pose>,
    player: Option<SimPlayer>,
    recorder: Option<SimRecorder>,
    tick: u64,
    mailbox: XrFrameMailbox,
    extracted: Option<XrFrameState>,
    /// While true the frame loop publishes nothing (tests: the frame thread is "paused").
    pub frame_paused: bool,
    supported_spaces: Vec<TrackingSpace>,
    runtime_json: Option<String>,
    mask_generation: u64,
    frame_done: bool,
}

impl XrSession {
    fn new(manifest: XrActionManifest, script: Option<XrSimScript>) -> Self {
        XrSession {
            sets: XrActionSets::from_manifest(&manifest),
            manifest,
            offset: Pose::IDENTITY,
            recenter: Pose::IDENTITY,
            held_head: Pose::IDENTITY,
            held_controllers: [None; 2],
            snap: SnapTurn::default(),
            vignette: Vignette::default(),
            last_rig: None,
            player: script.map(SimPlayer::new),
            recorder: None,
            tick: 0,
            mailbox: XrFrameMailbox::new(),
            extracted: None,
            frame_paused: false,
            supported_spaces: TrackingSpace::ALL.to_vec(),
            runtime_json: None,
            mask_generation: 0,
            frame_done: false,
        }
    }

    /// Counts the runtime's visibility mask changes (the eyes upload the mesh again).
    pub fn mask_generation(&self) -> u64 {
        self.mask_generation
    }

    /// Whether the frame loop publishes nothing (tests: the frame thread is "paused").
    pub fn is_frame_paused(&self) -> bool {
        self.frame_paused
    }

    /// The renderer ran this tick's XR frame ([`crate::render::xr_render_frame`]): [`xr_frame`]
    /// publishes it and runs none of its own.
    pub(crate) fn publish_rendered_frame(&mut self, frame: XrFrameState) {
        self.mailbox.publish(frame);
        self.frame_done = true;
    }

    /// The rig jumped to `rig` (a teleport, a ground placement): the next vignette step measures
    /// motion from there, so an instant move never flashes the vignette (ADR-2457).
    pub(crate) fn note_rig_jump(&mut self, rig: Pose) {
        self.last_rig = Some(rig);
    }

    /// Forgets the rig's last pose (the walk placed the rig): the next step measures no motion.
    pub(crate) fn forget_rig_motion(&mut self) {
        self.last_rig = None;
    }

    /// Fixed steps run.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The head pose held for the current fixed step, in tracking space.
    pub fn held_head(&self) -> Pose {
        self.held_head
    }

    /// The controllers' grip poses held for the current fixed step (left, right).
    pub fn held_controllers(&self) -> [Option<Pose>; 2] {
        self.held_controllers
    }

    /// Whether the action sets are attached.
    pub fn actions_attached(&self) -> bool {
        self.sets.attached()
    }

    /// The motion vignette's current strength.
    pub fn vignette(&self) -> f32 {
        self.vignette.strength()
    }

    /// Snaps fired.
    pub fn snaps(&self) -> u64 {
        self.snap.snaps()
    }

    /// The last frame extraction read from the mailbox.
    pub fn extracted(&self) -> Option<XrFrameState> {
        self.extracted
    }

    /// A clone of the frame mailbox (the frame loop's end).
    pub fn mailbox(&self) -> XrFrameMailbox {
        self.mailbox.clone()
    }

    /// The loader override set by `xr.set_option runtime_json`. Stored only: the OpenXR binding does
    /// not read it yet.
    pub fn runtime_json(&self) -> Option<&str> {
        self.runtime_json.as_deref()
    }

    /// Starts recording the sampled pose stream (record mode).
    pub fn start_recording(&mut self, fov_deg: f32, ipd_m: f32, refresh_hz: f32) {
        self.recorder = Some(SimRecorder::new(fov_deg, ipd_m, refresh_hz));
    }

    /// Stops recording and returns the script.
    pub fn stop_recording(&mut self) -> Option<XrSimScript> {
        self.recorder.take().map(|r| r.script().clone())
    }
}

/// Installs XR (spec 58 § 3.2). Off by default in the engine: add it explicitly.
pub struct XrPlugin {
    /// The project's `config/xr.ron`.
    pub config: XrConfig,
    /// The action manifest (default: [`XrActionManifest::default`]).
    pub manifest: XrActionManifest,
    /// A simulator script to play (simulator backend only).
    pub script: Option<XrSimScript>,
}

impl XrPlugin {
    /// A plugin for `config` with the default manifest and no script.
    pub fn new(config: XrConfig) -> Self {
        XrPlugin {
            config,
            manifest: XrActionManifest::default(),
            script: None,
        }
    }

    /// The simulator backend with no script (headless runs, tests, the editor's PIE simulator).
    pub fn simulator() -> Self {
        XrPlugin::new(XrConfig {
            backend: XrBackendKind::Simulator,
            ..XrConfig::default()
        })
    }
}

/// Opens the backend `config` asks for. The OpenXR binding is not built (spec 58 § 3.4 gate), so
/// `OpenXr` and `Auto` answer [`XrError::NoLoader`]; `Off` answers [`XrError::Unsupported`].
fn open_backend(config: &XrConfig, script: Option<&XrSimScript>) -> Result<Box<dyn XrBackend>, XrError> {
    match config.backend {
        XrBackendKind::Simulator => {
            let mut null_config = script.map(XrSimScript::null_config).unwrap_or_default();
            if let Some(hz) = config.preferred_refresh_hz {
                null_config.display_period_ns = (1.0e9 / f64::from(hz)).round() as i64;
            }
            Ok(Box::new(NullXr::new(null_config)))
        }
        XrBackendKind::OpenXr | XrBackendKind::Auto => Err(XrError::NoLoader),
        XrBackendKind::Off => Err(XrError::Unsupported("backend off")),
    }
}

/// What `last_error` says when `open_backend` cannot open the configured backend. While the
/// OpenXR binding is not built, `NoLoader` is about this build, not the player's install (a working
/// runtime may well be installed), so the message says that and names the fallback. The code stays
/// `xr_no_loader` (spec 58 § 2.3 step 1). When the binding lands, its own loader probe reports.
const NO_BINDING_MESSAGE: &str = "the OpenXR binding is not built in this engine version (spec 58 § 3.4), \
so no OpenXR loader or runtime was opened; the game runs without XR";

fn open_error_message(e: &XrError) -> String {
    match e {
        XrError::NoLoader => NO_BINDING_MESSAGE.to_string(),
        other => other.to_string(),
    }
}

fn refresh_of(config: &XrConfig, script: Option<&XrSimScript>) -> f32 {
    config
        .preferred_refresh_hz
        .or(script.map(|s| s.refresh_hz))
        .unwrap_or(1.0e9 / NullConfig::default().display_period_ns as f32)
}

impl Plugin for XrPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<XrStateChanged>()
            .add_message::<XrFocusChanged>()
            .add_message::<XrOriginChanged>()
            .add_message::<XrRuntimeLost>()
            .add_message::<XrInteractionProfileChanged>()
            .add_message::<XrRefreshRateChanged>()
            .add_message::<XrOverBudget>()
            .add_message::<AppExit>()
            .init_resource::<XrFrameBudget>();
        let mut state = XrState {
            backend_kind: self.config.backend,
            tracking_space: self.config.tracking_space,
            render_scale: self.config.render_scale,
            foveation: self.config.foveation,
            mirror: self.config.mirror,
            ..XrState::default()
        };
        if let Err(e) = self.config.validate() {
            tracing::error!(error = %e, "xr: config/xr.ron is invalid; XR stays off");
            state.last_error = Some((e.code().to_string(), e.to_string()));
            app.insert_resource(state);
            app.insert_resource(self.config.clone());
            return;
        }
        let manifest = self
            .manifest
            .with_dominant_hand(self.config.comfort.dominant_hand);
        let mut session = XrSession::new(manifest, self.script.clone());
        let enabled = self.config.backend != XrBackendKind::Off;
        if enabled {
            match open_backend(&self.config, self.script.as_ref()) {
                Ok(backend) => {
                    state.backend = backend.name().to_string();
                    state.sessions_created = 1;
                    state.refresh_hz = refresh_of(&self.config, self.script.as_ref());
                    state.app = XrAppState::Idle;
                    state.actual_tracking_space =
                        chosen_tracking_space(&self.config, &session.supported_spaces);
                    if session.sets.attach().is_err() {
                        tracing::warn!("xr: action sets were already attached");
                    }
                    app.insert_resource(XrRuntime::new(backend));
                }
                Err(e) => {
                    let message = open_error_message(&e);
                    if self.config.required {
                        tracing::error!(code = e.code(), %message, "xr: required is true and no session can be created");
                        app.world_mut()
                            .write_message(AppExit::Error(std::num::NonZeroU8::MIN));
                    } else {
                        tracing::info!(code = e.code(), %message, "xr: no session; the desktop game runs without XR");
                    }
                    state.app = XrAppState::Off;
                    state.last_error = Some((e.code().to_string(), message));
                }
            }
        }
        session.offset = floor_offset_pose(&self.config, state.actual_tracking_space, session.recenter);
        app.insert_resource(XrActionStates::new(&session.sets))
            .insert_resource(session)
            .insert_resource(state)
            .insert_resource(self.config.clone())
            .init_resource::<ActionPoses>()
            .init_resource::<XrHands>()
            .insert_resource(XrRenderProfile::default());
        app.configure_sets(FixedUpdate, (XrSet::SyncInput, XrSet::UpdateRig).chain())
            .configure_sets(Last, (XrSet::Extract, XrSet::Submit).chain())
            .add_systems(PreUpdate, xr_poll.in_set(XrSet::Poll))
            .add_systems(FixedUpdate, xr_sync_input.in_set(XrSet::SyncInput))
            .add_systems(FixedUpdate, xr_update_rig.in_set(XrSet::UpdateRig))
            .add_systems(Last, xr_frame.in_set(XrSet::Extract));
    }
}

/// The space the runtime uses. Seated play is `Local` plus the eye-height offset (spec 58 § 2.8),
/// whatever space the file asked for. Standing play is the requested space, or its fallback.
fn chosen_tracking_space(config: &XrConfig, supported: &[TrackingSpace]) -> TrackingSpace {
    if config.comfort.seated {
        TrackingSpace::Local
    } else {
        resolve_tracking_space(config.tracking_space, supported)
    }
}

fn floor_offset_pose(config: &XrConfig, actual: TrackingSpace, recenter: Pose) -> Pose {
    // Seated always lifts by the eye height. `floor_offset` stays the standing rule: a floor
    // request that fell back to `Local`, and zero when the request itself is `Local`.
    let lift = if config.comfort.seated {
        Vec3::new(0.0, config.comfort.eye_height_m, 0.0)
    } else {
        floor_offset(config.tracking_space, actual, config.comfort.eye_height_m)
    };
    Pose {
        position: recenter.position + lift,
        orientation: recenter.orientation,
    }
}

fn set_app_state(world: &mut World, to: XrAppState) {
    let from = world.resource::<XrState>().app;
    if from != to {
        world.resource_mut::<XrState>().app = to;
        tracing::debug!(from = from.name(), to = to.name(), "xr app state");
        world.write_message(XrStateChanged { from, to });
    }
}

pub(crate) fn record_error(world: &mut World, e: &XrError) {
    let message = open_error_message(e);
    tracing::warn!(code = e.code(), %message, "xr");
    world.resource_mut::<XrState>().last_error = Some((e.code().to_string(), message));
}

fn has_session(world: &World) -> bool {
    world.contains_resource::<XrRuntime>()
}

fn enabled(world: &World) -> bool {
    world.resource::<XrConfig>().backend != XrBackendKind::Off
}

/// Recomputes the app state from the backend's session state.
fn refresh_app_state(world: &mut World) {
    let session = world
        .get_resource::<XrRuntime>()
        .map(|rt| rt.backend().state())
        .unwrap_or(SessionState::Idle);
    world.resource_mut::<XrState>().session = session;
    let to = app_state_for(enabled(world), has_session(world), session);
    // A failed open keeps `Off` (with the reason in `last_error`) until a retry is scheduled.
    let to = if to == XrAppState::NoSession && world.resource::<XrState>().retry_in.is_none() {
        XrAppState::Off
    } else {
        to
    };
    set_app_state(world, to);
}

/// Destroys the session (and the instance): the runtime resource goes away.
fn destroy_session(world: &mut World) {
    world.remove_resource::<XrRuntime>();
    if let Some(mut s) = world.get_resource_mut::<XrSession>() {
        s.mailbox.reset_reader();
        s.extracted = None;
    }
    release_published_input(world);
}

/// Releases everything the last sync held on `regia_input` and empties the pose table: with no
/// session there is no sync left to do it, so a held stick would drive the game for good.
fn release_published_input(world: &mut World) {
    let Some(mut states) = world.remove_resource::<XrActionStates>() else {
        return;
    };
    states.sync(
        false,
        &[
            ControllerState::inactive(Hand::Left),
            ControllerState::inactive(Hand::Right),
        ],
    );
    let mut poses = world.remove_resource::<ActionPoses>().unwrap_or_default();
    states.publish(
        world.get_resource_mut::<InputInjection>().as_deref_mut(),
        &mut poses,
    );
    // `publish` re-inserts every pose action (inactive ones included); with the session gone the
    // table must actually be empty, not full of `(false, identity)` entries.
    poses.poses.clear();
    world.insert_resource(states);
    world.insert_resource(poses);
}

/// `PreUpdate`: drains the runtime's events and reacts to the session state (begin on ready,
/// end on stopping, tear down on loss pending, exit on exiting).
pub fn xr_poll(world: &mut World) {
    let _span = tracing::trace_span!("xr_poll_events").entered();
    if !world.contains_resource::<XrState>() || !world.contains_resource::<XrSession>() {
        return;
    }
    let Some(mut rt) = world.get_resource_mut::<XrRuntime>() else {
        return;
    };
    let events: Vec<XrEvent> = rt.pump_new();
    for event in events {
        match event {
            XrEvent::StateChanged { state } => {
                if state == SessionState::Running {
                    // A begun session restarts frame indices: forget the old frames (rule 3).
                    world.resource_mut::<XrSession>().mailbox.reset_reader();
                }
                refresh_app_state(world);
            }
            XrEvent::FocusChanged { focused } => {
                world.write_message(XrFocusChanged { focused });
            }
            XrEvent::VisibilityMaskChanged => {
                world.resource_mut::<XrSession>().mask_generation += 1;
            }
            XrEvent::ControllerActive { .. } => {}
        }
    }
    react_to_session(world);
}

fn react_to_session(world: &mut World) {
    let Some(state) = world.get_resource::<XrRuntime>().map(|rt| rt.backend().state()) else {
        return;
    };
    match state {
        SessionState::Ready => {
            // Begin the primary stereo session; frame-order bookkeeping restarts (rule 3).
            let result = world.resource_mut::<XrRuntime>().backend_mut().begin_session();
            if let Err(e) = result {
                record_error(world, &e);
            } else {
                world.resource_mut::<XrSession>().mailbox.reset_reader();
            }
        }
        SessionState::Stopping => {
            let result = world.resource_mut::<XrRuntime>().backend_mut().end_session();
            if let Err(e) = result {
                record_error(world, &e);
            }
        }
        SessionState::LossPending => {
            let result = world.resource_mut::<XrRuntime>().backend_mut().end_session();
            if let Err(e) = result {
                record_error(world, &e);
            }
            destroy_session(world);
            let retry = world.resource::<XrConfig>().retry_frames;
            {
                let mut st = world.resource_mut::<XrState>();
                st.retry_in = Some(retry);
                st.last_error = Some(("xr_runtime_lost".into(), XrError::RuntimeLost.to_string()));
            }
            world.write_message(XrRuntimeLost);
            set_app_state(world, XrAppState::LossPending);
            set_app_state(world, XrAppState::NoSession);
            return;
        }
        SessionState::Exiting => {
            destroy_session(world);
            world.write_message(AppExit::Success);
            set_app_state(world, XrAppState::Exiting);
            return;
        }
        _ => {}
    }
    world.resource_mut::<XrRuntime>().pump();
    refresh_app_state(world);
}

/// Re-creates the session after a loss (or the first time when enabled at runtime).
fn create_session(world: &mut World) -> Result<(), XrError> {
    let config = world.resource::<XrConfig>().clone();
    let script = world
        .resource::<XrSession>()
        .player
        .as_ref()
        .map(|p| p.script().clone());
    let backend = open_backend(&config, script.as_ref())?;
    let name = backend.name().to_string();
    world.insert_resource(XrRuntime::new(backend));
    {
        let mut session = world.resource_mut::<XrSession>();
        // A new session re-creates the action state (attach exactly once per session).
        session.sets = XrActionSets::from_manifest(&session.manifest);
        let _ = session.sets.attach();
        session.mailbox.reset_reader();
        session.mask_generation += 1;
    }
    let sets = XrActionSets::from_manifest(&world.resource::<XrSession>().manifest);
    world.insert_resource(XrActionStates::new(&sets));
    {
        let mut st = world.resource_mut::<XrState>();
        st.backend = name;
        st.retry_in = None;
        st.sessions_created += 1;
        st.refresh_hz = refresh_of(&config, script.as_ref());
    }
    refresh_app_state(world);
    Ok(())
}

fn finite_pose(p: &Pose) -> bool {
    p.position.is_finite() && p.orientation.is_finite()
}

/// `FixedUpdate`, first: the script player, the retry counter, the action sync and the poses held
/// for this step.
pub fn xr_sync_input(world: &mut World) {
    let _span = tracing::trace_span!("xr_sync_actions").entered();
    if !world.contains_resource::<XrSession>() {
        return;
    }
    let tick = {
        let mut s = world.resource_mut::<XrSession>();
        let t = s.tick;
        s.tick += 1;
        t
    };
    // Retry: counter-driven, never the wall clock.
    let retry = world.resource::<XrState>().retry_in;
    if let Some(n) = retry {
        if n <= 1 {
            if let Err(e) = create_session(world) {
                let frames = world.resource::<XrConfig>().retry_frames;
                world.resource_mut::<XrState>().retry_in = Some(frames);
                record_error(world, &e);
            }
        } else {
            world.resource_mut::<XrState>().retry_in = Some(n - 1);
        }
    }
    play_script(world, tick);
    if !has_session(world) {
        return;
    }
    let (state, head, controllers) = {
        let mut rt = world.resource_mut::<XrRuntime>();
        let state = rt.backend().state();
        if state.frames_flow()
            && let Err(e) = rt.backend_mut().sync_actions()
        {
            tracing::debug!(error = %e, "xr sync_actions");
        }
        let b = rt.backend();
        (
            state,
            b.head_sample(),
            [b.controller(Hand::Left), b.controller(Hand::Right)],
        )
    };
    let mut bad = 0;
    {
        let mut s = world.resource_mut::<XrSession>();
        if finite_pose(&head.pose) {
            s.held_head = head.pose;
        } else {
            bad += 1;
        }
        for c in &controllers {
            let slot = &mut s.held_controllers[c.hand.index()];
            if !c.active {
                *slot = None;
            } else if finite_pose(&c.grip.pose) && finite_pose(&c.aim.pose) {
                *slot = Some(c.grip.pose);
            } else {
                bad += 1;
            }
        }
        let held_head = s.held_head;
        let held_controllers = s.held_controllers;
        if let Some(rec) = s.recorder.as_mut() {
            rec.record(tick, &held_head, held_controllers, None);
        }
    }
    world.resource_mut::<XrState>().stats.bad_poses += bad;
    // A controller whose pose is not finite is read as inactive this step.
    let sanitized = controllers.map(|c| {
        if finite_pose(&c.grip.pose) && finite_pose(&c.aim.pose) {
            c
        } else {
            ControllerState::inactive(c.hand)
        }
    });
    world
        .resource_mut::<XrActionStates>()
        .sync(state.input_live(), &sanitized);
    let states = world.resource::<XrActionStates>().clone();
    let mut poses = std::mem::take(&mut *world.resource_mut::<ActionPoses>());
    states.publish(
        world.get_resource_mut::<InputInjection>().as_deref_mut(),
        &mut poses,
    );
    world.insert_resource(poses);
}

fn play_script(world: &mut World, tick: u64) {
    let Some(frame) = world
        .resource::<XrSession>()
        .player
        .as_ref()
        .and_then(|p| p.frame(tick))
    else {
        return;
    };
    for (action, sub, value) in &frame.inputs {
        if let Err(e) = world.resource_mut::<XrActionStates>().inject(action, sub, *value) {
            record_error(world, &e);
        }
    }
    if !has_session(world) {
        return;
    }
    {
        let mut rt = world.resource_mut::<XrRuntime>();
        let _ = rt.backend_mut().simulate(SimEvent::HeadPose(frame.head));
        for hand in Hand::BOTH {
            let mut c = ControllerState::inactive(hand);
            if let Some(p) = frame.controllers[hand.index()] {
                c = ControllerState {
                    active: true,
                    grip: PoseSample::at_rest(p, 0),
                    aim: PoseSample::at_rest(p, 0),
                    ..rt.backend().controller(hand)
                };
            }
            let _ = rt.backend_mut().simulate(SimEvent::Controller(c));
        }
    }
    if let Some(target) = frame.state
        && let Err(e) = drive_session_state(world, target)
    {
        record_error(world, &e);
    }
}

/// The rig entity (not a head or controller).
pub type RigFilter = (With<XrRig>, Without<XrHead>, Without<XrController>);
/// Head entities (not the rig or a controller).
pub type HeadFilter = (With<XrHead>, Without<XrRig>, Without<XrController>);
/// Controller entities (not the rig or the head).
pub type ControllerFilter = (Without<XrRig>, Without<XrHead>);

/// `FixedUpdate`: snap turn from the `turn` action, the motion vignette from rig motion, and the
/// head and controller transforms from the held poses.
pub fn xr_update_rig(
    mut session: ResMut<XrSession>,
    config: Res<XrConfig>,
    actions: Res<XrActionStates>,
    fixed: Option<Res<FixedTime>>,
    mut rigs: Query<&mut Transform, RigFilter>,
    mut heads: Query<&mut Transform, HeadFilter>,
    mut controllers: Query<(&XrController, &mut Transform), ControllerFilter>,
) {
    let step = fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32()).max(1e-6);
    let head_local = session.offset.compose(&session.held_head);
    if let Ok(mut rig) = rigs.single_mut() {
        let turn_x = ["right", "left", ""]
            .iter()
            .find_map(|sub| match actions.get("turn", sub) {
                Some((true, XrActionValue::Vec2(v))) => Some(v.x),
                _ => None,
            })
            .unwrap_or(0.0);
        let comfort = config.comfort.clone();
        let yaw = session.snap.step(&comfort, turn_x, step);
        let mut pose = Pose {
            position: rig.translation,
            orientation: rig.rotation,
        };
        if yaw != 0.0 {
            pose = rig_turn(&pose, &head_local, yaw);
            rig.translation = pose.position;
            rig.rotation = pose.orientation;
        }
        let (lin, ang) = match session.last_rig {
            Some(last) => {
                let lin = (pose.position - last.position).length() / step;
                let ang = (pose.orientation * last.orientation.inverse()).to_axis_angle().1;
                let ang = if ang > std::f32::consts::PI {
                    std::f32::consts::TAU - ang
                } else {
                    ang
                };
                (lin, ang / step)
            }
            None => (0.0, 0.0),
        };
        session.last_rig = Some(pose);
        session.vignette.step(&comfort, lin, ang);
    }
    for mut head in &mut heads {
        head.translation = head_local.position;
        head.rotation = head_local.orientation;
    }
    for (c, mut t) in &mut controllers {
        if let Some(p) = session.held_controllers[c.hand.index()] {
            let local = session.offset.compose(&p);
            t.translation = local.position;
            t.rotation = local.orientation;
        }
    }
}

/// `Last`: one XR frame (wait, begin, locate, render, late latch, end) when frames flow, published
/// into the mailbox; then extraction reads the freshest slot (a repeat counts a stale frame).
pub fn xr_frame(world: &mut World) {
    let _span = tracing::trace_span!("xr_extract").entered();
    let Some(paused) = world.get_resource::<XrSession>().map(|s| s.frame_paused) else {
        return;
    };
    let Some(state) = world.get_resource::<XrRuntime>().map(|rt| rt.backend().state()) else {
        return;
    };
    if !state.frames_flow() {
        return;
    }
    let rendered = std::mem::take(&mut world.resource_mut::<XrSession>().frame_done);
    let mut ran = rendered;
    if !paused && !rendered {
        let result = world.resource_mut::<XrRuntime>().frame(|_, _| {});
        match result {
            Ok(report) => {
                ran = true;
                let mailbox = world.resource::<XrSession>().mailbox.clone();
                mailbox.publish(XrFrameState {
                    frame_index: report.timing.frame_index,
                    predicted_display_time_ns: report.timing.predicted_display_time_ns,
                    period_ns: report.timing.predicted_display_period_ns,
                    should_render: report.timing.should_render,
                });
                world.resource_mut::<XrState>().stats.frames += 1;
            }
            Err(e) => {
                world.resource_mut::<XrState>().stats.dropped_frames += 1;
                record_error(world, &e);
            }
        }
    }
    let fresh = world.resource_mut::<XrSession>().mailbox.take_fresh();
    match fresh {
        Some(f) => world.resource_mut::<XrSession>().extracted = Some(f),
        None => world.resource_mut::<XrState>().stats.stale_frames += 1,
    }
    if ran {
        watch_frame_budget(world, rendered);
    }
}

/// Spec 58 § 10: observes the frame that just ran against the budget; on the Nth consecutive
/// overrun raises `xr_over_budget` and, with `auto_scale`, lowers the render scale one step (the
/// eye targets follow at the next frame boundary).
fn watch_frame_budget(world: &mut World, rendered: bool) {
    let simulator = world
        .get_resource::<XrRuntime>()
        .is_some_and(|rt| rt.backend().name() == "null");
    let measured = if rendered {
        world
            .get_resource::<crate::render::XrRenderState>()
            .map(|r| r.stats().last_cpu_ms)
    } else {
        None
    };
    let Some(scripted) = world
        .get_resource::<XrFrameBudget>()
        .map(XrFrameBudget::scripted_ms)
    else {
        return;
    };
    let Some(frame_ms) = frame_time_ms(simulator, scripted, measured) else {
        return;
    };
    let (refresh_hz, scale) = {
        let state = world.resource::<XrState>();
        (state.refresh_hz, state.render_scale)
    };
    let budget_ms = budget_ms_for(refresh_hz);
    let Some(event) = world
        .resource_mut::<XrFrameBudget>()
        .observe(frame_ms, budget_ms, scale)
    else {
        return;
    };
    tracing::warn!(
        code = OVER_BUDGET_CODE,
        frame_ms = event.frame_ms,
        budget_ms = event.budget_ms,
        frames = event.frames,
        scale_from = event.scale_from,
        scale_to = event.scale_to,
        "xr: frames over budget"
    );
    world.resource_mut::<XrState>().stats.over_budget += 1;
    if event.scale_to != event.scale_from {
        world.resource_mut::<XrConfig>().render_scale = event.scale_to;
        world.resource_mut::<XrState>().render_scale = event.scale_to;
    }
    world.write_message(event);
}

/// The projection the engine's renderer uses for an eye: the asymmetric frustum of `fov` with the
/// frame's standard depth (near 0, far 1; ADR-869) and the renderer's stand-in far plane
/// (`regia_render::view_math::INFINITE_FAR_STANDIN`), so an XR eye and a desktop camera of the same
/// field of view build the same matrix (ADR-1676).
pub fn eye_projection(fov: &Fov, near_m: f32) -> Result<Mat4, XrError> {
    let near = if near_m.is_finite() && near_m > 0.0 {
        near_m
    } else {
        regia_render::view_math::FALLBACK_NEAR
    };
    let far = regia_render::view_math::INFINITE_FAR_STANDIN.max(near * 2.0);
    fov.projection(DepthRange { near, far }, DepthMode::Standard)
}

/// Per eye, the view-projection matrix the renderer is fed: [`eye_projection`] times the latched
/// view's view matrix, from the last frame report. `None` before the first frame.
pub fn eye_matrices(world: &World) -> Option<Result<[Mat4; 2], XrError>> {
    let report = world.get_resource::<XrRuntime>()?.last_report()?;
    let near = world.get_resource::<XrConfig>().map_or(0.05, |c| c.near_m);
    let one = |v: &View| eye_projection(&v.fov, near).map(|p| p * v.view_matrix());
    Some(one(&report.views[0]).and_then(|l| one(&report.views[1]).map(|r| [l, r])))
}

/// Recenters: the head's current yaw and floor position become the rig's forward and origin.
/// Sends [`XrOriginChanged`] and returns the new offset.
pub fn recenter(world: &mut World) -> Result<Pose, XrError> {
    let state = world
        .get_resource::<XrState>()
        .ok_or(XrError::Unsupported("xr plugin"))?;
    if !state.app.is_running() && state.app != XrAppState::Idle && state.app != XrAppState::Starting {
        return Err(XrError::NotRunning {
            call: "recenter",
            state: state.session,
        });
    }
    let space = state.actual_tracking_space;
    let config = world.resource::<XrConfig>().clone();
    let offset = {
        let mut session = world.resource_mut::<XrSession>();
        session.recenter = recenter_offset(&session.held_head);
        session.offset = floor_offset_pose(&config, space, session.recenter);
        session.offset
    };
    world.write_message(XrOriginChanged { space, offset });
    Ok(offset)
}

/// Moves the scripted runtime to `target` through the shortest valid path (the simulator's
/// `xr.drive session_state`). A real runtime answers [`XrError::SimulatorOnly`].
pub fn drive_session_state(world: &mut World, target: ScriptState) -> Result<SessionState, XrError> {
    simulator_only(world, "session_state")?;
    let current = world.resource::<XrRuntime>().backend().state();
    let goal = target.session_state();
    let path = path_to_state(current, goal).ok_or(XrError::InvalidTransition {
        from: current,
        transition: match target {
            ScriptState::Ready => Transition::RuntimeReady,
            ScriptState::Synchronized => Transition::RuntimeSynchronized,
            ScriptState::Visible => Transition::RuntimeVisible,
            ScriptState::Focused => Transition::RuntimeFocused,
            ScriptState::Stopping => Transition::RuntimeStop,
            ScriptState::LossPending => Transition::RuntimeLost,
            ScriptState::Exiting => Transition::RuntimeExiting,
        },
    })?;
    for t in path {
        let mut rt = world.resource_mut::<XrRuntime>();
        match t {
            Transition::Begin => rt.backend_mut().begin_session()?,
            Transition::End => rt.backend_mut().end_session()?,
            other => rt.backend_mut().simulate(SimEvent::Runtime(other))?,
        }
    }
    // Apply the reactions (begin, end, tear-down) the plugin would apply at the next poll.
    xr_poll(world);
    Ok(world
        .get_resource::<XrRuntime>()
        .map_or(SessionState::Idle, |rt| rt.backend().state()))
}

fn simulator_only(world: &World, call: &'static str) -> Result<(), XrError> {
    let rt = world.get_resource::<XrRuntime>().ok_or(XrError::NotRunning {
        call,
        state: SessionState::Idle,
    })?;
    if rt.backend().name() != "null" {
        return Err(XrError::SimulatorOnly { call });
    }
    Ok(())
}

/// Sets the scripted head and controllers (the simulator's `xr.drive pose`). A non-finite pose is
/// rejected (the previous one is kept) and counted in `stats.bad_poses`.
pub fn drive_pose(
    world: &mut World,
    head: Option<Pose>,
    controllers: [Option<Pose>; 2],
) -> Result<(), XrError> {
    simulator_only(world, "pose")?;
    let mut bad = 0;
    let result = apply_pose(
        &mut world.resource_mut::<XrRuntime>(),
        head,
        controllers,
        &mut bad,
    );
    world.resource_mut::<XrState>().stats.bad_poses += bad;
    result
}

fn apply_pose(
    rt: &mut XrRuntime,
    head: Option<Pose>,
    controllers: [Option<Pose>; 2],
    bad: &mut u64,
) -> Result<(), XrError> {
    if let Some(h) = head {
        if finite_pose(&h) {
            rt.backend_mut().simulate(SimEvent::HeadPose(h))?;
        } else {
            *bad += 1;
        }
    }
    for hand in Hand::BOTH {
        if let Some(p) = controllers[hand.index()] {
            if !finite_pose(&p) {
                *bad += 1;
                continue;
            }
            let c = ControllerState {
                active: true,
                grip: PoseSample::at_rest(p, 0),
                aim: PoseSample::at_rest(p, 0),
                ..rt.backend().controller(hand)
            };
            rt.backend_mut().simulate(SimEvent::Controller(c))?;
        }
    }
    Ok(())
}

/// Scripts an action value (the simulator's `xr.drive input`), read at the next fixed-step sync.
pub fn drive_input(world: &mut World, action: &str, sub: &str, value: XrActionValue) -> Result<(), XrError> {
    simulator_only(world, "input")?;
    world.resource_mut::<XrActionStates>().inject(action, sub, value)
}

/// When an `xr.set_option` change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedAt {
    /// Immediately.
    Now,
    /// At the next frame boundary (swapchains are re-created between frames).
    NextFrame,
    /// At the next session (instance) creation.
    NextSession,
}

impl AppliedAt {
    /// The name `xr.set_option` reports.
    pub fn name(self) -> &'static str {
        match self {
            AppliedAt::Now => "now",
            AppliedAt::NextFrame => "next_frame",
            AppliedAt::NextSession => "next_session",
        }
    }
}

/// A value for [`set_option`].
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    Text(String),
}

/// The keys [`set_option`] accepts.
pub const OPTION_KEYS: [&str; 14] = [
    "enabled",
    "tracking_space",
    "render_scale",
    "foveation",
    "mirror",
    "snap_turn_deg",
    "vignette_strength",
    "eye_height_m",
    "refresh_hz",
    "runtime_json",
    "stereo_mode",
    "hidden_area",
    "depth_layer",
    "auto_scale",
];

fn bad_value(key: &str, reason: &str) -> XrError {
    XrError::InvalidValue {
        key: key.to_string(),
        reason: reason.to_string(),
    }
}

fn number(key: &str, v: &OptionValue, lo: f64, hi: f64) -> Result<f32, XrError> {
    match v {
        OptionValue::Number(n) if n.is_finite() && (lo..=hi).contains(n) => Ok(*n as f32),
        _ => Err(bad_value(key, &format!("a number {lo}..={hi}"))),
    }
}

fn text<'a>(key: &str, v: &'a OptionValue, expected: &str) -> Result<&'a str, XrError> {
    match v {
        OptionValue::Text(s) => Ok(s),
        _ => Err(bad_value(key, expected)),
    }
}

fn profile(world: &World) -> Result<&XrRenderProfile, XrError> {
    world
        .get_resource::<XrRenderProfile>()
        .ok_or(XrError::Unsupported("xr plugin"))
}

/// The current value of option `key` (as `xr.set_option` reports `old` and `new`).
pub fn option_value(world: &World, key: &str) -> Result<OptionValue, XrError> {
    let config = world
        .get_resource::<XrConfig>()
        .ok_or(XrError::Unsupported("xr plugin"))?;
    Ok(match key {
        "enabled" => OptionValue::Bool(config.backend != XrBackendKind::Off && has_session(world)),
        "tracking_space" => OptionValue::Text(config.tracking_space.name().into()),
        "render_scale" => OptionValue::Number(f64::from(config.render_scale)),
        "foveation" => OptionValue::Text(config.foveation.name().into()),
        "mirror" => OptionValue::Text(config.mirror.name().into()),
        "snap_turn_deg" => OptionValue::Number(f64::from(config.comfort.snap_turn_deg)),
        "vignette_strength" => OptionValue::Number(f64::from(config.comfort.vignette_strength)),
        "eye_height_m" => OptionValue::Number(f64::from(config.comfort.eye_height_m)),
        "refresh_hz" => OptionValue::Number(f64::from(
            config.preferred_refresh_hz.unwrap_or(
                world
                    .get_resource::<XrState>()
                    .ok_or(XrError::Unsupported("xr plugin"))?
                    .refresh_hz,
            ),
        )),
        "runtime_json" => OptionValue::Text(
            world
                .get_resource::<XrSession>()
                .ok_or(XrError::Unsupported("xr plugin"))?
                .runtime_json
                .clone()
                .unwrap_or_default(),
        ),
        "stereo_mode" => OptionValue::Text(profile(world)?.stereo_mode.name().into()),
        "hidden_area" => OptionValue::Bool(profile(world)?.hidden_area),
        "depth_layer" => OptionValue::Bool(profile(world)?.depth_layer),
        "auto_scale" => OptionValue::Bool(
            world
                .get_resource::<XrFrameBudget>()
                .is_some_and(|b| b.auto_scale),
        ),
        other => {
            return Err(XrError::UnknownOption {
                key: other.to_string(),
            });
        }
    })
}

/// Applies one `xr.set_option` change. Unknown key: [`XrError::UnknownOption`]; wrong type or
/// out of range: [`XrError::InvalidValue`] with the range. Returns when it takes effect.
pub fn set_option(world: &mut World, key: &str, value: &OptionValue) -> Result<AppliedAt, XrError> {
    if !world.contains_resource::<XrSession>() {
        return Err(XrError::Unsupported("xr plugin"));
    }
    match key {
        "enabled" => {
            let OptionValue::Bool(on) = value else {
                return Err(bad_value(key, "true or false"));
            };
            set_enabled(world, *on)?;
            Ok(AppliedAt::Now)
        }
        "tracking_space" => {
            let name = text(key, value, "local, stage or local_floor")?;
            let space = TrackingSpace::from_name(name)
                .ok_or_else(|| bad_value(key, "local, stage or local_floor"))?;
            world.resource_mut::<XrConfig>().tracking_space = space;
            let actual = {
                let config = world.resource::<XrConfig>().clone();
                let supported = world.resource::<XrSession>().supported_spaces.clone();
                chosen_tracking_space(&config, &supported)
            };
            {
                let mut st = world.resource_mut::<XrState>();
                st.tracking_space = space;
                st.actual_tracking_space = actual;
            }
            let config = world.resource::<XrConfig>().clone();
            let offset = {
                let mut s = world.resource_mut::<XrSession>();
                s.offset = floor_offset_pose(&config, actual, s.recenter);
                s.offset
            };
            world.write_message(XrOriginChanged {
                space: actual,
                offset,
            });
            Ok(AppliedAt::Now)
        }
        "render_scale" => {
            let (lo, hi) = RENDER_SCALE_RANGE;
            let v = number(key, value, f64::from(lo), f64::from(hi))?;
            world.resource_mut::<XrConfig>().render_scale = v;
            world.resource_mut::<XrState>().render_scale = v;
            Ok(AppliedAt::NextFrame)
        }
        "foveation" => {
            let name = text(key, value, "off, low, medium or high")?;
            let f = Foveation::from_name(name).ok_or_else(|| bad_value(key, "off, low, medium or high"))?;
            world.resource_mut::<XrConfig>().foveation = f;
            world.resource_mut::<XrState>().foveation = f;
            Ok(AppliedAt::NextFrame)
        }
        "mirror" => {
            let name = text(key, value, "off, left, right or both")?;
            let m = Mirror::from_name(name).ok_or_else(|| bad_value(key, "off, left, right or both"))?;
            if cfg!(target_os = "android") && m != Mirror::Off {
                return Err(bad_value(key, "Android has no mirror window (off only)"));
            }
            world.resource_mut::<XrConfig>().mirror = m;
            world.resource_mut::<XrState>().mirror = m;
            Ok(AppliedAt::Now)
        }
        "snap_turn_deg" => {
            let v = number(key, value, 0.0, 180.0)?;
            world.resource_mut::<XrConfig>().comfort.snap_turn_deg = v;
            Ok(AppliedAt::Now)
        }
        "vignette_strength" => {
            let v = number(key, value, 0.0, 1.0)?;
            world.resource_mut::<XrConfig>().comfort.vignette_strength = v;
            Ok(AppliedAt::Now)
        }
        "eye_height_m" => {
            let v = number(key, value, 0.5, 2.5)?;
            world.resource_mut::<XrConfig>().comfort.eye_height_m = v;
            let config = world.resource::<XrConfig>().clone();
            let actual = world.resource::<XrState>().actual_tracking_space;
            let mut s = world.resource_mut::<XrSession>();
            s.offset = floor_offset_pose(&config, actual, s.recenter);
            Ok(AppliedAt::Now)
        }
        "refresh_hz" => {
            let v = number(key, value, 30.0, 240.0)?;
            world.resource_mut::<XrConfig>().preferred_refresh_hz = Some(v);
            Ok(AppliedAt::NextSession)
        }
        "runtime_json" => {
            if cfg!(target_os = "android") {
                return Err(bad_value(key, "desktop only (Android uses the system broker)"));
            }
            let path = text(key, value, "a path to a runtime manifest (.json)")?;
            world.resource_mut::<XrSession>().runtime_json = (!path.is_empty()).then(|| path.to_string());
            Ok(AppliedAt::NextSession)
        }
        "stereo_mode" => {
            let name = text(key, value, "two_pass or multiview")?;
            let mode = StereoMode::from_name(name).ok_or_else(|| bad_value(key, "two_pass or multiview"))?;
            world.resource_mut::<XrRenderProfile>().stereo_mode = mode;
            Ok(AppliedAt::NextFrame)
        }
        "hidden_area" => {
            let OptionValue::Bool(on) = value else {
                return Err(bad_value(key, "true or false"));
            };
            world.resource_mut::<XrRenderProfile>().hidden_area = *on;
            Ok(AppliedAt::NextFrame)
        }
        "depth_layer" => {
            let OptionValue::Bool(on) = value else {
                return Err(bad_value(key, "true or false"));
            };
            world.resource_mut::<XrRenderProfile>().depth_layer = *on;
            Ok(AppliedAt::NextFrame)
        }
        "auto_scale" => {
            let OptionValue::Bool(on) = value else {
                return Err(bad_value(key, "true or false"));
            };
            world.get_resource_or_init::<XrFrameBudget>().auto_scale = *on;
            Ok(AppliedAt::Now)
        }
        other => Err(XrError::UnknownOption {
            key: other.to_string(),
        }),
    }
}

fn set_enabled(world: &mut World, on: bool) -> Result<(), XrError> {
    if !on {
        if let Some(mut rt) = world.get_resource_mut::<XrRuntime>() {
            let state = rt.backend().state();
            // Best effort: a clean stop when the runtime allows it; the session is destroyed anyway.
            if state.frames_flow() {
                let _ = rt.backend_mut().request_exit();
                let _ = rt.backend_mut().end_session();
            }
        }
        destroy_session(world);
        world.resource_mut::<XrConfig>().backend = XrBackendKind::Off;
        world.resource_mut::<XrState>().retry_in = None;
        refresh_app_state(world);
        return Ok(());
    }
    if has_session(world) {
        return Ok(());
    }
    if world.resource::<XrConfig>().backend == XrBackendKind::Off {
        let kind = world.resource::<XrState>().backend_kind;
        world.resource_mut::<XrConfig>().backend = if kind == XrBackendKind::Off {
            XrBackendKind::Auto
        } else {
            kind
        };
    }
    match create_session(world) {
        Ok(()) => Ok(()),
        Err(e) => {
            record_error(world, &e);
            refresh_app_state(world);
            Err(e)
        }
    }
}

/// Per eye, the view the last frame rendered (pose and field of view), for `xr.capture_eye`.
pub fn last_views(world: &World) -> Option<[View; 2]> {
    world.get_resource::<XrRuntime>()?.last_report().map(|r| r.views)
}

/// Which eye index `eye` is.
pub fn eye_from_index(index: u8) -> Option<Eye> {
    match index {
        0 => Some(Eye::Left),
        1 => Some(Eye::Right),
        _ => None,
    }
}

/// The world-space position of the rig's head (the rig transform composed with the head's
/// rig-local pose), for tests and tools.
pub fn head_world_position(rig: &Transform, session: &XrSession) -> Vec3 {
    let rig = Pose {
        position: rig.translation,
        orientation: rig.rotation,
    };
    rig.compose(&session.offset.compose(&session.held_head)).position
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::glam::Quat;

    #[test]
    fn head_world_position_composes_rig_offset_and_held_head() {
        let mut session = XrSession::new(XrActionManifest::default(), None);
        session.offset = Pose {
            position: Vec3::new(0.0, 1.0, 0.0),
            orientation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        };
        session.held_head = Pose {
            position: Vec3::new(0.0, 0.0, -1.0),
            orientation: Quat::IDENTITY,
        };
        let rig = Transform::from_translation(Vec3::new(10.0, 0.0, 0.0));
        // offset yaw +90 deg maps local -Z to -X; then the offset height and rig shift apply.
        let p = head_world_position(&rig, &session);
        assert!((p - Vec3::new(9.0, 1.0, 0.0)).length() < 1e-4, "{p:?}");
    }
}
