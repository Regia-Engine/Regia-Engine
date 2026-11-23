// SPDX-License-Identifier: MPL-2.0
//! Walk mode (ADR-2457): seated or standing locomotion through a scene in a headset.
//!
//! [`XrWalkPlugin`] adds, on top of [`crate::XrPlugin`]'s snap turn and vignette:
//!
//! - **Smooth locomotion**: the `move` stick moves the rig along the head's heading at
//!   [`WalkSettings::move_speed_mps`], with a dead zone. The head pose itself always comes from
//!   the headset: nothing here adds a bob, a sway or any camera noise.
//! - **Teleport**: push the `move` stick forward past [`WalkSettings::teleport_threshold`] to aim a
//!   ballistic arc from the moving hand (the head when the hand is not tracked), release it to jump
//!   so the head lands over the arc's ground point. A jump never raises the motion vignette.
//! - **Ground follow**: with an [`XrGround`] height function (a terrain), the rig's floor follows
//!   the ground under the head every fixed step.
//! - **Height calibration**: [`calibrate_height`] lifts the virtual floor so the head's current
//!   height becomes a standing eye height (a seated user walks at standing height).
//! - **Comfort presets** ([`ComfortPreset`]) and the **XR budget** ([`walk_budget`]): render scale and
//!   foveation per display refresh rate.
//!
//! Everything runs on the fixed step from the held poses and the synced actions, so a simulator
//! script replays a walk bit for bit. The arc is the standard projectile curve (`docs/provenance.md`).

use std::sync::Arc;

use regia_app::prelude::*;
use regia_core::glam::{Quat, Vec2, Vec3};
use regia_platform::xr::{Hand, Pose, XrError};
use regia_scene::Transform;

use crate::config::{Foveation, RENDER_SCALE_RANGE, XrConfig};
use crate::input::{XrActionStates, XrActionValue};
use crate::plugin::{RigFilter, XrSession, XrSet, xr_update_rig};
use crate::space::yaw_of;
use crate::state::XrState;

/// How the `move` stick moves the rig.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Reflect)]
pub enum Locomotion {
    /// The stick glides the rig along the head's heading.
    #[default]
    Smooth,
    /// The stick aims an arc; releasing it jumps to the arc's end.
    Teleport,
    /// The stick does nothing (room-scale walking only).
    Off,
}

impl Locomotion {
    /// Every mode, in order.
    pub const ALL: [Locomotion; 3] = [Locomotion::Smooth, Locomotion::Teleport, Locomotion::Off];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            Locomotion::Smooth => "smooth",
            Locomotion::Teleport => "teleport",
            Locomotion::Off => "off",
        }
    }

    /// The mode named `name`.
    pub fn from_name(name: &str) -> Option<Locomotion> {
        Locomotion::ALL.into_iter().find(|l| l.name() == name)
    }
}

/// Walk tuning. The numbers are Regia's own starting points, to be tuned with a headset.
#[derive(Debug, Clone, PartialEq, Reflect)]
pub struct WalkSettings {
    /// What the `move` stick does.
    pub locomotion: Locomotion,
    /// Smooth-locomotion speed at full deflection, meters per second (a walking pace by default).
    pub move_speed_mps: f32,
    /// Stick deflection below which the stick counts as centered (rescaled above it).
    pub move_deadzone: f32,
    /// Teleport: the farthest flat-ground reach of the arc, meters.
    pub teleport_range_m: f32,
    /// Teleport: forward deflection that starts aiming.
    pub teleport_threshold: f32,
    /// Teleport: deflection below which a held aim fires the jump.
    pub teleport_release: f32,
    /// Keep the rig's floor on the [`XrGround`] under the head.
    pub ground_follow: bool,
    /// Teleport: show where the arc lands as a flat panel on the ground ([`TELEPORT_MARKER_ID`]) while
    /// aiming (needs [`crate::XrRenderPlugin`]'s panels).
    pub teleport_marker: bool,
}

impl Default for WalkSettings {
    fn default() -> Self {
        WalkSettings {
            locomotion: Locomotion::Smooth,
            move_speed_mps: 1.4,
            move_deadzone: 0.15,
            teleport_range_m: 8.0,
            teleport_threshold: 0.7,
            teleport_release: 0.3,
            ground_follow: true,
            teleport_marker: true,
        }
    }
}

/// The panel id of the teleport target marker (reserved: `xr.set_quad` should not use it).
pub const TELEPORT_MARKER_ID: u32 = u32::MAX - 1;
/// Side of the teleport target marker, meters.
pub const TELEPORT_MARKER_M: f32 = 0.5;
/// The marker's color (sRGB, straight alpha).
pub const TELEPORT_MARKER_COLOR: [u8; 4] = [80, 200, 255, 170];

/// Range of [`WalkSettings::move_speed_mps`].
pub const MOVE_SPEED_RANGE: (f32, f32) = (0.1, 6.0);
/// Range of [`WalkSettings::teleport_range_m`].
pub const TELEPORT_RANGE: (f32, f32) = (1.0, 30.0);
/// Range of a calibration lift, meters (the virtual floor moves at most this far).
pub const LIFT_RANGE: (f32, f32) = (-1.0, 1.5);
/// Range of the eye height [`calibrate_height`] aims for, meters.
pub const EYE_HEIGHT_RANGE: (f32, f32) = (0.5, 2.5);
/// Gravity of the teleport arc, m/s².
pub const ARC_GRAVITY: f32 = 9.81;
/// Time step of the arc's samples, seconds.
pub const ARC_STEP_S: f32 = 0.02;
/// Most arc samples (4 s of flight).
pub const ARC_MAX_SAMPLES: usize = 200;
/// A hit whose ground stands this far above the arc's previous sample is a wall face: no target.
pub const ARC_WALL_M: f32 = 0.5;

fn bad(key: &str, reason: &str) -> XrError {
    XrError::InvalidValue {
        key: key.to_string(),
        reason: reason.to_string(),
    }
}

impl WalkSettings {
    /// Checks the ranges, naming the field at fault (`xr_invalid_value`).
    pub fn validate(&self) -> Result<(), XrError> {
        let within = |v: f32, (lo, hi): (f32, f32)| v.is_finite() && (lo..=hi).contains(&v);
        if !within(self.move_speed_mps, MOVE_SPEED_RANGE) {
            return Err(bad("move_speed_mps", "must be 0.1..=6"));
        }
        if !within(self.move_deadzone, (0.0, 0.9)) {
            return Err(bad("move_deadzone", "must be 0..=0.9"));
        }
        if !within(self.teleport_range_m, TELEPORT_RANGE) {
            return Err(bad("teleport_range_m", "must be 1..=30"));
        }
        if !(within(self.teleport_threshold, (0.0, 1.0))
            && within(self.teleport_release, (0.0, 1.0))
            && self.teleport_release < self.teleport_threshold)
        {
            return Err(bad(
                "teleport_threshold",
                "needs 0 <= teleport_release < teleport_threshold <= 1",
            ));
        }
        Ok(())
    }
}

/// A comfort preset: locomotion, turning and vignette chosen together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComfortPreset {
    /// Teleport, 30-degree snap turns, a strong vignette: for people new to headsets.
    Comfortable,
    /// Smooth locomotion, 30-degree snap turns, the default vignette.
    Moderate,
    /// Smooth locomotion, smooth turning, no vignette: for people used to artificial motion.
    Free,
}

impl ComfortPreset {
    /// Every preset, in order.
    pub const ALL: [ComfortPreset; 3] = [
        ComfortPreset::Comfortable,
        ComfortPreset::Moderate,
        ComfortPreset::Free,
    ];

    /// The lowercase name the agent protocol uses.
    pub fn name(self) -> &'static str {
        match self {
            ComfortPreset::Comfortable => "comfortable",
            ComfortPreset::Moderate => "moderate",
            ComfortPreset::Free => "free",
        }
    }

    /// The preset named `name`.
    pub fn from_name(name: &str) -> Option<ComfortPreset> {
        ComfortPreset::ALL.into_iter().find(|p| p.name() == name)
    }

    /// `(locomotion, snap_turn_deg, vignette_strength)` of the preset.
    pub fn values(self) -> (Locomotion, f32, f32) {
        match self {
            ComfortPreset::Comfortable => (Locomotion::Teleport, 30.0, 0.7),
            ComfortPreset::Moderate => (Locomotion::Smooth, 30.0, 0.5),
            ComfortPreset::Free => (Locomotion::Smooth, 0.0, 0.0),
        }
    }
}

/// The render budget of a walk at a display refresh rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalkBudget {
    /// The display refresh rate it was chosen for, Hz.
    pub refresh_hz: f32,
    /// The whole frame's budget, milliseconds (one display period).
    pub frame_ms: f32,
    /// Eye-image scale against the runtime's recommended size.
    pub render_scale: f32,
    /// Foveation level asked of the runtime (a runtime without it ignores it).
    pub foveation: Foveation,
}

/// The walk budget for `refresh_hz`: the faster the display, the fewer pixels per eye. 110 Hz and
/// up: scale 0.7, high foveation; 85 Hz and up (90 Hz headsets): 0.8, high; slower (72 Hz): 0.9,
/// medium. A rate that is not finite or not positive is taken as 90 Hz.
pub fn walk_budget(refresh_hz: f32) -> WalkBudget {
    let hz = if refresh_hz.is_finite() && refresh_hz > 0.0 {
        refresh_hz
    } else {
        90.0
    };
    let (render_scale, foveation): (f32, Foveation) = if hz >= 110.0 {
        (0.7, Foveation::High)
    } else if hz >= 85.0 {
        (0.8, Foveation::High)
    } else {
        (0.9, Foveation::Medium)
    };
    let (lo, hi) = RENDER_SCALE_RANGE;
    WalkBudget {
        refresh_hz: hz,
        frame_ms: 1000.0 / hz,
        render_scale: render_scale.clamp(lo, hi),
        foveation,
    }
}

/// The ground height under a world point `(x, z)`, or `None` off the ground (the rig keeps its
/// height there). The host installs it: a terrain's height sampler, a floor plane.
#[derive(Resource, Clone)]
pub struct XrGround(pub Arc<dyn Fn(f32, f32) -> Option<f32> + Send + Sync>);

impl XrGround {
    /// A flat floor at height `y`.
    pub fn flat(y: f32) -> Self {
        XrGround(Arc::new(move |_: f32, _: f32| Some(y)))
    }

    /// The height at `(x, z)`, `None` off the ground or when the sampler returns a non-finite value.
    pub fn height(&self, x: f32, z: f32) -> Option<f32> {
        (self.0)(x, z).filter(|h| h.is_finite())
    }
}

/// The walk's state. Read by `xr.get` (`walk`); changed through [`walk_start`], [`calibrate_height`],
/// [`apply_preset`] and the fields.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct XrWalk {
    /// Whether the walk moves the rig.
    pub active: bool,
    /// Tuning.
    pub settings: WalkSettings,
    /// The calibration lift: the rig's floor sits this far above the ground, meters.
    pub lift_m: f32,
    /// The preset applied last, if any.
    pub preset: Option<ComfortPreset>,
    /// The budget applied last, if any.
    pub budget: Option<WalkBudget>,
    aiming: bool,
    aim_target: Option<Vec3>,
    settled: bool,
    teleports: u64,
    jumps: u64,
    distance_m: f32,
    ground_m: Option<f32>,
    ground_misses: u64,
    steps: u64,
}

impl Default for XrWalk {
    fn default() -> Self {
        XrWalk::new(WalkSettings::default())
    }
}

impl XrWalk {
    /// An inactive walk with `settings`.
    pub fn new(settings: WalkSettings) -> Self {
        XrWalk {
            active: false,
            settings,
            lift_m: 0.0,
            preset: None,
            budget: None,
            aiming: false,
            aim_target: None,
            settled: false,
            teleports: 0,
            jumps: 0,
            distance_m: 0.0,
            ground_m: None,
            ground_misses: 0,
            steps: 0,
        }
    }

    /// Whether a teleport is being aimed (the stick is held forward).
    pub fn aiming(&self) -> bool {
        self.aiming
    }

    /// Where the aimed arc lands (world), `None` while not aiming or when the arc finds no ground.
    pub fn aim_target(&self) -> Option<Vec3> {
        self.aim_target
    }

    /// Instant rig moves so far (teleports, the first ground placement, each [`walk_start`]): anything that
    /// derives a velocity from motion (an audio listener's Doppler) should treat a change here as a cut.
    pub fn jumps(&self) -> u64 {
        self.jumps
    }

    /// Teleports made.
    pub fn teleports(&self) -> u64 {
        self.teleports
    }

    /// Horizontal distance the rig was moved by locomotion (smooth and teleport), meters.
    pub fn distance_m(&self) -> f32 {
        self.distance_m
    }

    /// The ground height under the head at the last step, `None` without ground.
    pub fn ground_m(&self) -> Option<f32> {
        self.ground_m
    }

    /// Steps where the head was off the ground (the rig kept its height).
    pub fn ground_misses(&self) -> u64 {
        self.ground_misses
    }

    /// Fixed steps the walk ran while active.
    pub fn steps(&self) -> u64 {
        self.steps
    }
}

/// Installs the walk. Add after [`crate::XrPlugin`]; the walk starts inactive ([`walk_start`]).
#[derive(Default)]
pub struct XrWalkPlugin {
    /// Starting tuning.
    pub settings: WalkSettings,
}

impl Plugin for XrWalkPlugin {
    fn build(&self, app: &mut App) {
        let mut settings = self.settings.clone();
        if let Err(e) = settings.validate() {
            tracing::warn!(error = %e, "xr walk: invalid settings; the defaults are used");
            settings = WalkSettings::default();
        }
        app.insert_resource(XrWalk::new(settings)).add_systems(
            FixedUpdate,
            (xr_walk, xr_walk_marker)
                .chain()
                .in_set(XrSet::UpdateRig)
                .before(xr_update_rig),
        );
    }
}

/// Rescales a stick past the dead zone to 0..=1 of its length, keeping its direction.
pub fn stick_after_deadzone(stick: Vec2, deadzone: f32) -> Vec2 {
    if !stick.is_finite() {
        return Vec2::ZERO;
    }
    let len = stick.length();
    if len <= deadzone || len == 0.0 {
        return Vec2::ZERO;
    }
    let scaled = ((len - deadzone) / (1.0 - deadzone).max(1e-6)).min(1.0);
    stick * (scaled / len)
}

/// The rig's smooth-locomotion move for one step: the stick (after the dead zone) along the floor
/// heading `head_yaw` (stick +Y forward, +X right), at `move_speed_mps`, for `step_secs`.
pub fn smooth_delta(settings: &WalkSettings, stick: Vec2, head_yaw: f32, step_secs: f32) -> Vec3 {
    let v = stick_after_deadzone(stick, settings.move_deadzone);
    if v == Vec2::ZERO || !head_yaw.is_finite() {
        return Vec3::ZERO;
    }
    let heading = Quat::from_rotation_y(head_yaw);
    let forward = heading * Vec3::NEG_Z;
    let right = heading * Vec3::X;
    (forward * v.y + right * v.x) * (settings.move_speed_mps * step_secs)
}

/// The teleport arc from `origin` along `direction` (normalized here), launched so a flat floor at
/// the origin's height is reached `range_m` away at 45 degrees. Returns the first sample at or under
/// the ground (`ground(x, z)`, else `floor_y`), placed on the ground and pulled back to `range_m` of
/// horizontal reach; `None` when the arc finds no ground within [`ARC_MAX_SAMPLES`] or strikes a wall
/// (ground more than [`ARC_WALL_M`] above the previous sample).
pub fn teleport_target(
    origin: Vec3,
    direction: Vec3,
    range_m: f32,
    ground: Option<&XrGround>,
    floor_y: f32,
) -> Option<Vec3> {
    let dir = direction.try_normalize()?;
    if !origin.is_finite() || !range_m.is_finite() || range_m <= 0.0 {
        return None;
    }
    let height = |x: f32, z: f32| ground.and_then(|g| g.height(x, z)).unwrap_or(floor_y);
    let v0 = (range_m * ARC_GRAVITY).sqrt();
    let velocity = dir * v0;
    let mut last = origin;
    for i in 1..=ARC_MAX_SAMPLES {
        let t = i as f32 * ARC_STEP_S;
        let p = origin + velocity * t + Vec3::new(0.0, -0.5 * ARC_GRAVITY * t * t, 0.0);
        let h = height(p.x, p.z);
        if p.y <= h {
            if h > last.y + ARC_WALL_M {
                // The arc struck a face that rises above where it just was: a wall, not a floor.
                return None;
            }
            let flat = Vec2::new(p.x - origin.x, p.z - origin.z);
            let reach = flat.length();
            let (x, z) = if reach > range_m {
                let back = flat * (range_m / reach);
                (origin.x + back.x, origin.z + back.y)
            } else {
                (p.x, p.z)
            };
            return Some(Vec3::new(x, height(x, z), z));
        }
        last = p;
    }
    None
}

/// The `move` stick and the hand it is bound to (left by default; the dominant-hand swap moves it).
fn move_stick(actions: &XrActionStates) -> (Vec2, Option<Hand>) {
    for (sub, hand) in [
        ("left", Some(Hand::Left)),
        ("right", Some(Hand::Right)),
        ("", None),
    ] {
        if let Some((true, XrActionValue::Vec2(v))) = actions.get("move", sub) {
            return (v, hand);
        }
    }
    (Vec2::ZERO, None)
}

/// `FixedUpdate`, before the rig update: locomotion, teleport and ground follow move the rig.
pub fn xr_walk(
    mut walk: ResMut<XrWalk>,
    session: Option<ResMut<XrSession>>,
    actions: Option<Res<XrActionStates>>,
    ground: Option<Res<XrGround>>,
    fixed: Option<Res<FixedTime>>,
    mut rigs: Query<&mut Transform, RigFilter>,
) {
    let _span = tracing::trace_span!("xr_walk").entered();
    let (Some(mut session), Some(actions)) = (session, actions) else {
        return;
    };
    if !walk.active {
        return;
    }
    let Ok(mut rig_t) = rigs.single_mut() else {
        return;
    };
    walk.steps += 1;
    let step = fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32()).max(1e-6);
    let rig = Pose {
        position: rig_t.translation,
        orientation: rig_t.rotation,
    };
    let head_local = session.offset.compose(&session.held_head());
    let head_world = rig.compose(&head_local);
    let (stick, hand) = move_stick(&actions);
    let settings = walk.settings.clone();
    let mut pose = rig;
    let mut jumped = false;
    match settings.locomotion {
        Locomotion::Smooth => {
            walk.aiming = false;
            walk.aim_target = None;
            let delta = smooth_delta(&settings, stick, yaw_of(head_world.orientation), step);
            if delta != Vec3::ZERO {
                pose.position += delta;
                walk.distance_m += Vec2::new(delta.x, delta.z).length();
            }
        }
        Locomotion::Teleport => {
            if !walk.aiming && stick.is_finite() && stick.y >= settings.teleport_threshold {
                walk.aiming = true;
            }
            if walk.aiming {
                let aim = hand
                    .and_then(|h| session.held_controllers()[h.index()])
                    .map_or(head_world, |grip| rig.compose(&session.offset.compose(&grip)));
                let floor_y = rig.position.y - walk.lift_m;
                walk.aim_target = teleport_target(
                    aim.position,
                    aim.orientation * Vec3::NEG_Z,
                    settings.teleport_range_m,
                    ground.as_deref(),
                    floor_y,
                );
                let released = !stick.is_finite() || stick.length() < settings.teleport_release;
                if released {
                    walk.aiming = false;
                    if let Some(target) = walk.aim_target.take() {
                        let shift = Vec3::new(
                            target.x - head_world.position.x,
                            0.0,
                            target.z - head_world.position.z,
                        );
                        pose.position += shift;
                        pose.position.y = target.y + walk.lift_m;
                        walk.distance_m += Vec2::new(shift.x, shift.z).length();
                        walk.teleports += 1;
                        jumped = true;
                    }
                }
            }
        }
        Locomotion::Off => {
            walk.aiming = false;
            walk.aim_target = None;
        }
    }
    if settings.ground_follow
        && let Some(ground) = ground.as_deref()
    {
        let head_now = pose.compose(&head_local).position;
        match ground.height(head_now.x, head_now.z) {
            Some(h) => {
                pose.position.y = h + walk.lift_m;
                walk.ground_m = Some(h);
                if !walk.settled {
                    // The first placement on the ground is a jump, not motion.
                    walk.settled = true;
                    jumped = true;
                }
            }
            None => {
                walk.ground_m = None;
                walk.ground_misses += 1;
            }
        }
    }
    if pose.position != rig.position {
        rig_t.translation = pose.position;
    }
    if jumped {
        walk.jumps += 1;
        session.note_rig_jump(pose);
    }
}

/// `FixedUpdate`, after [`xr_walk`]: while a teleport is aimed at a target, a flat panel lies on the ground
/// there ([`TELEPORT_MARKER_ID`], world space, facing up, 2 cm above the ground); otherwise it is removed.
/// Without [`crate::XrRenderPlugin`] (no panels) it does nothing.
pub fn xr_walk_marker(walk: Res<XrWalk>, panels: Option<ResMut<crate::render::XrPanels>>) {
    let Some(mut panels) = panels else {
        return;
    };
    let target = walk
        .aim_target
        .filter(|_| walk.active && walk.aiming && walk.settings.teleport_marker);
    match target {
        Some(t) => {
            let panel = crate::render::XrPanel {
                id: TELEPORT_MARKER_ID,
                priority: 0,
                space: regia_platform::xr::LayerSpace::World,
                pose: Pose::new(
                    t + Vec3::new(0.0, 0.02, 0.0),
                    Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
                ),
                size_m: [TELEPORT_MARKER_M, TELEPORT_MARKER_M],
                image: crate::render::PanelImage::Solid(TELEPORT_MARKER_COLOR),
            };
            let moved = panels
                .get(TELEPORT_MARKER_ID)
                .is_none_or(|p| p.pose != panel.pose);
            if moved && !panels.set(panel) {
                tracing::debug!("xr walk: no free panel for the teleport marker");
            }
        }
        None => {
            if panels.get(TELEPORT_MARKER_ID).is_some() {
                panels.remove(TELEPORT_MARKER_ID);
            }
        }
    }
}

/// Whether a session is running (frames flow).
fn running(world: &World) -> bool {
    world
        .get_resource::<XrState>()
        .is_some_and(|s| s.app.is_running())
}

/// Lifts the virtual floor so the head's current height becomes `eye_height_m` (default: the
/// comfort eye height, 1.65 m): a seated user then walks at standing height. Returns the lift,
/// clamped to [`LIFT_RANGE`]. Needs a running session (`xr_not_running`); an eye height outside
/// [`EYE_HEIGHT_RANGE`] is `xr_invalid_value`.
pub fn calibrate_height(world: &mut World, eye_height_m: Option<f32>) -> Result<f32, XrError> {
    if !world.contains_resource::<XrWalk>() {
        return Err(XrError::Unsupported("xr walk"));
    }
    if !running(world) {
        let state = world
            .get_resource::<XrState>()
            .map_or(regia_platform::xr::SessionState::Idle, |s| s.session);
        return Err(XrError::NotRunning {
            call: "calibrate_height",
            state,
        });
    }
    let target = match eye_height_m {
        Some(v) => {
            let (lo, hi) = EYE_HEIGHT_RANGE;
            if !(v.is_finite() && (lo..=hi).contains(&v)) {
                return Err(bad("eye_height_m", "must be 0.5..=2.5"));
            }
            v
        }
        None => world
            .get_resource::<XrConfig>()
            .map_or(1.65, |c| c.comfort.eye_height_m),
    };
    let head_y = world
        .get_resource::<XrSession>()
        .map(|s| s.offset.compose(&s.held_head()).position.y)
        .ok_or(XrError::Unsupported("xr plugin"))?;
    let (lo, hi) = LIFT_RANGE;
    let lift = (target - head_y).clamp(lo, hi);
    let old = std::mem::replace(&mut world.resource_mut::<XrWalk>().lift_m, lift);
    // Without ground follow the rig carries the change itself; with it the next step places it.
    let follows = {
        let walk = world.resource::<XrWalk>();
        walk.settings.ground_follow && world.contains_resource::<XrGround>()
    };
    if !follows {
        let mut rigs = world.query_filtered::<&mut Transform, RigFilter>();
        if let Ok(mut rig) = rigs.single_mut(world) {
            rig.translation.y += lift - old;
        }
    }
    if let Some(mut session) = world.get_resource_mut::<XrSession>() {
        // A calibration is not motion.
        session.forget_rig_motion();
    }
    Ok(lift)
}

/// Applies `preset`: locomotion into the walk, snap turn and vignette into the comfort options.
pub fn apply_preset(world: &mut World, preset: ComfortPreset) -> Result<(), XrError> {
    if !world.contains_resource::<XrWalk>() || !world.contains_resource::<XrConfig>() {
        return Err(XrError::Unsupported("xr walk"));
    }
    let (locomotion, snap, vignette) = preset.values();
    {
        let mut config = world.resource_mut::<XrConfig>();
        config.comfort.snap_turn_deg = snap;
        config.comfort.vignette_strength = vignette;
    }
    let mut walk = world.resource_mut::<XrWalk>();
    walk.settings.locomotion = locomotion;
    walk.preset = Some(preset);
    Ok(())
}

/// Applies [`walk_budget`] for the session's refresh rate: render scale and foveation (applied at
/// the next frame, like `xr.set_option`). Returns the budget.
pub fn apply_budget(world: &mut World) -> Result<WalkBudget, XrError> {
    if !world.contains_resource::<XrWalk>() || !world.contains_resource::<XrConfig>() {
        return Err(XrError::Unsupported("xr walk"));
    }
    let hz = world.get_resource::<XrState>().map_or(90.0, |s| s.refresh_hz);
    let budget = walk_budget(hz);
    {
        let mut config = world.resource_mut::<XrConfig>();
        config.render_scale = budget.render_scale;
        config.foveation = budget.foveation;
    }
    if let Some(mut state) = world.get_resource_mut::<XrState>() {
        state.render_scale = budget.render_scale;
        state.foveation = budget.foveation;
    }
    world.resource_mut::<XrWalk>().budget = Some(budget);
    Ok(budget)
}

/// Where and how a walk starts ([`walk_start`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WalkStart {
    /// Put the head over this world point (x, z; the height comes from the ground or `y`).
    pub position: Option<Vec3>,
    /// Turn the rig so the head faces this heading (radians, 0 looks down −Z, positive turns left).
    pub yaw: Option<f32>,
    /// A comfort preset to apply.
    pub preset: Option<ComfortPreset>,
    /// Apply [`walk_budget`] (render scale and foveation).
    pub budget: bool,
}

/// Starts the walk: applies the preset and the budget, places the rig so the head stands over
/// `position` facing `yaw`, and activates locomotion. Needs [`XrWalkPlugin`] (else
/// [`XrError::Unsupported`]) and one `XrRig` entity (else `xr_invalid_value` on `rig`).
pub fn walk_start(world: &mut World, start: WalkStart) -> Result<(), XrError> {
    if !world.contains_resource::<XrWalk>() {
        return Err(XrError::Unsupported("xr walk"));
    }
    if let Some(p) = start.position
        && !p.is_finite()
    {
        return Err(bad("position", "three finite numbers"));
    }
    if let Some(y) = start.yaw
        && !y.is_finite()
    {
        return Err(bad("yaw", "a finite number of radians"));
    }
    let head_local = world
        .get_resource::<XrSession>()
        .map(|s| s.offset.compose(&s.held_head()))
        .ok_or(XrError::Unsupported("xr plugin"))?;
    let ground = world.get_resource::<XrGround>().cloned();
    let lift = world.resource::<XrWalk>().lift_m;
    {
        let mut rigs = world.query_filtered::<&mut Transform, RigFilter>();
        let Ok(mut rig) = rigs.single_mut(world) else {
            return Err(bad("rig", "the world needs exactly one XrRig entity"));
        };
        if let Some(yaw) = start.yaw {
            rig.rotation = Quat::from_rotation_y(yaw - yaw_of(head_local.orientation));
        }
        if let Some(p) = start.position {
            let head_offset = rig.rotation * head_local.position;
            let y = ground.as_ref().and_then(|g| g.height(p.x, p.z)).unwrap_or(p.y);
            rig.translation = Vec3::new(p.x - head_offset.x, y + lift, p.z - head_offset.z);
        }
    }
    if let Some(preset) = start.preset {
        apply_preset(world, preset)?;
    }
    if start.budget {
        apply_budget(world)?;
    }
    {
        let mut walk = world.resource_mut::<XrWalk>();
        walk.active = true;
        walk.settled = false;
        walk.jumps += 1;
        walk.aiming = false;
        walk.aim_target = None;
    }
    if let Some(mut session) = world.get_resource_mut::<XrSession>() {
        session.forget_rig_motion();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dead_zone_rescales_and_keeps_direction() {
        assert_eq!(stick_after_deadzone(Vec2::new(0.1, 0.0), 0.15), Vec2::ZERO);
        let full = stick_after_deadzone(Vec2::new(0.0, 1.0), 0.15);
        assert!((full - Vec2::new(0.0, 1.0)).length() < 1e-6);
        let half = stick_after_deadzone(Vec2::new(0.0, 0.575), 0.15);
        assert!((half.y - 0.5).abs() < 1e-5, "{half}");
        assert_eq!(stick_after_deadzone(Vec2::new(f32::NAN, 1.0), 0.15), Vec2::ZERO);
    }

    #[test]
    fn smooth_moves_along_the_head_heading_at_walking_pace() {
        let s = WalkSettings::default();
        let d = smooth_delta(&s, Vec2::new(0.0, 1.0), 0.0, 1.0);
        assert!((d - Vec3::new(0.0, 0.0, -1.4)).length() < 1e-5, "{d}");
        // Heading +90 degrees (turned left) looks down -X; stick right then points down -Z.
        let d = smooth_delta(&s, Vec2::new(1.0, 0.0), std::f32::consts::FRAC_PI_2, 1.0);
        assert!((d - Vec3::new(0.0, 0.0, -1.4)).length() < 1e-5, "{d}");
        assert_eq!(d.y, 0.0, "never vertical: no bob");
    }

    #[test]
    fn the_arc_lands_within_range_on_flat_ground() {
        let origin = Vec3::new(0.0, 1.2, 0.0);
        let dir = Vec3::new(0.0, 1.0, -1.0);
        let t = teleport_target(origin, dir, 8.0, None, 0.0).expect("lands");
        assert_eq!(t.y, 0.0);
        let reach = Vec2::new(t.x, t.z).length();
        assert!(reach <= 8.0 + 1e-4 && reach > 6.0, "{reach}");
        // Aimed down, it lands close.
        let t = teleport_target(origin, Vec3::new(0.0, -1.0, -1.0), 8.0, None, 0.0).expect("lands");
        assert!(Vec2::new(t.x, t.z).length() < 2.0);
        // Straight up comes back down near the origin.
        let t = teleport_target(origin, Vec3::Y, 8.0, None, 0.0).expect("lands");
        assert!(Vec2::new(t.x, t.z).length() < 1e-3);
        assert!(teleport_target(origin, Vec3::ZERO, 8.0, None, 0.0).is_none());
    }

    #[test]
    fn the_arc_follows_a_ground_function() {
        let slope = XrGround(Arc::new(|_: f32, z: f32| Some(-z * 0.25)));
        let t = teleport_target(
            Vec3::new(0.0, 1.2, 0.0),
            Vec3::new(0.0, 0.5, -1.0),
            8.0,
            Some(&slope),
            0.0,
        )
        .expect("lands");
        assert!((t.y - (-t.z * 0.25)).abs() < 1e-5, "on the ground: {t}");
        // A cliff face the arc strikes: no target (never onto the top of a wall).
        let wall = XrGround(Arc::new(|_: f32, z: f32| {
            Some(if z < -0.5 { 100.0 } else { 0.0 })
        }));
        let t = teleport_target(
            Vec3::new(0.0, 1.2, 0.0),
            Vec3::new(0.0, 0.2, -1.0),
            8.0,
            Some(&wall),
            0.0,
        );
        assert_eq!(t, None, "a wall is not a floor");
        // A low step up is fine.
        let step = XrGround(Arc::new(|_: f32, z: f32| Some(if z < -2.0 { 0.3 } else { 0.0 })));
        let t = teleport_target(
            Vec3::new(0.0, 1.2, 0.0),
            Vec3::new(0.0, 0.5, -1.0),
            8.0,
            Some(&step),
            0.0,
        )
        .expect("lands on the step");
        assert_eq!(t.y, 0.3);
    }

    #[test]
    fn the_budget_lowers_resolution_with_the_refresh_rate() {
        let b72 = walk_budget(72.0);
        let b90 = walk_budget(90.0);
        let b120 = walk_budget(120.0);
        assert!(b120.render_scale < b90.render_scale && b90.render_scale < b72.render_scale);
        assert!((b90.frame_ms - 11.111).abs() < 1e-2);
        assert_eq!(b90.foveation, Foveation::High);
        assert_eq!(b72.foveation, Foveation::Medium);
        assert_eq!(walk_budget(f32::NAN).refresh_hz, 90.0);
    }

    #[test]
    fn settings_validation_names_the_field() {
        assert!(WalkSettings::default().validate().is_ok());
        let bad_speed = WalkSettings {
            move_speed_mps: 50.0,
            ..WalkSettings::default()
        };
        assert!(
            matches!(bad_speed.validate(), Err(XrError::InvalidValue { key, .. }) if key == "move_speed_mps")
        );
        let bad_release = WalkSettings {
            teleport_release: 0.9,
            ..WalkSettings::default()
        };
        assert!(
            matches!(bad_release.validate(), Err(XrError::InvalidValue { key, .. }) if key == "teleport_threshold")
        );
    }

    #[test]
    fn names_round_trip() {
        for l in Locomotion::ALL {
            assert_eq!(Locomotion::from_name(l.name()), Some(l));
        }
        for p in ComfortPreset::ALL {
            assert_eq!(ComfortPreset::from_name(p.name()), Some(p));
        }
        assert_eq!(ComfortPreset::from_name("x"), None);
    }
}
